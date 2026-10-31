"""Read-only identity queries survive transport loss without replaying builds."""
import json
import struct

import pytest

from trellis import check_job


@pytest.fixture
def clock(monkeypatch):
    now = [0.0]
    monkeypatch.setattr(check_job.time, "monotonic", lambda: now[0])
    monkeypatch.setattr(check_job.time, "sleep", lambda seconds: now.__setitem__(0, now[0] + seconds))
    return now


def completed(state="succeeded"):
    return dict(job_id="same-job", state=state,
                result=dict(returncode=0, stdout="/trusted/toolchain\n", stderr=""))


def test_metadata_reconnect_preserves_submit_and_status_identity(monkeypatch, clock):
    calls = []
    replies = iter([
        check_job.JobTransportError("lost submit acknowledgement"),
        dict(job_id="same-job", state="running"),
        check_job.JobTransportError("checker restarting"),
        completed(),
    ])
    def rpc(request, *, timeout):
        calls.append((dict(request), timeout))
        reply = next(replies)
        if isinstance(reply, Exception):
            raise reply
        return reply
    monkeypatch.setattr(check_job, "rpc", rpc)
    assert check_job.tool_info("lean", "--print-prefix") == completed()
    assert calls[0][0] == calls[1][0]
    assert calls[0][0]["kind"] == "tool_info"
    assert calls[0][0]["target"] == "lean/--print-prefix"
    assert calls[0][0]["request_id"]
    assert calls[2][0] == calls[3][0] == dict(action="status", job_id="same-job")
    assert all(0 < timeout <= 30 for _, timeout in calls)
    assert calls[-1][1] < calls[0][1]


@pytest.mark.parametrize("message", ["invalid capability", "invalid response length from checker", "request ID has different parameters"])
def test_metadata_does_not_retry_auth_or_protocol_failure(monkeypatch, clock, message):
    calls = []
    def rpc(request, **kwargs):
        calls.append(request)
        raise check_job.JobError(message)
    monkeypatch.setattr(check_job, "rpc", rpc)
    with pytest.raises(check_job.JobError, match=message):
        check_job.tool_info("lean", "--print-prefix")
    assert len(calls) == 1
    assert clock[0] == 0


def test_metadata_continuous_transport_loss_has_one_deadline(monkeypatch, clock):
    calls = []
    def rpc(request, *, timeout):
        calls.append(dict(request))
        clock[0] += min(4, timeout)
        raise check_job.JobTransportError("connection refused")
    monkeypatch.setattr(check_job, "rpc", rpc)
    with pytest.raises(check_job.JobError, match="unavailable for 30 seconds") as error:
        check_job.tool_info("lean", "--print-prefix")
    assert isinstance(error.value.__cause__, check_job.JobTransportError)
    assert clock[0] == 30
    assert len(calls) > 1 and all(call == calls[0] for call in calls)


def test_metadata_healthy_admission_wait_can_exceed_reconnect_deadline(monkeypatch, clock):
    calls = []
    def rpc(request, **kwargs):
        calls.append(dict(request))
        if clock[0] >= 65:
            return completed()
        return dict(job_id="same-job", state="queued" if clock[0] < 40 else "running")
    monkeypatch.setattr(check_job, "rpc", rpc)
    assert check_job.tool_info("lean", "--print-prefix") == completed()
    assert clock[0] == 65
    assert sum(call["action"] == "submit" for call in calls) == 1
    assert len(calls) == 131, "healthy polling must retain its sleep"


def test_metadata_success_resets_the_next_transport_outage_deadline(monkeypatch, clock):
    calls = []
    def rpc(request, *, timeout):
        calls.append((dict(request), timeout))
        if len(calls) in (1, 3):
            clock[0] += 20
            raise check_job.JobTransportError("checker restarting")
        if len(calls) == 2:
            return dict(job_id="same-job", state="running")
        return completed()
    monkeypatch.setattr(check_job, "rpc", rpc)
    assert check_job.tool_info("lean", "--print-prefix") == completed()
    assert clock[0] == 41
    assert calls[0][1] == calls[2][1] == 30
    assert calls[1][1] == calls[3][1] == 9.75
    assert calls[2][0] == calls[3][0] == dict(action="status", job_id="same-job")


@pytest.mark.parametrize("state", ["interrupted", "cancelled", "failed", "infrastructure_error"])
def test_metadata_terminal_failure_is_not_resubmitted(monkeypatch, clock, state):
    calls = []
    def rpc(request, **kwargs):
        calls.append(request)
        return completed(state)
    monkeypatch.setattr(check_job, "rpc", rpc)
    assert check_job.tool_info("lean", "--print-prefix")["state"] == state
    assert len(calls) == 1


@pytest.mark.parametrize("kind,target", [("build", "N"), ("incremental", "N"), ("scratch", ".trellis/scratch/p.lean"), ("local_closure", "N")])
def test_compiler_job_transport_failure_is_not_automatically_retried(monkeypatch, kind, target):
    calls = []
    def rpc(request):
        calls.append(request)
        raise check_job.JobTransportError("checker restarting")
    monkeypatch.setattr(check_job, "rpc", rpc)
    with pytest.raises(check_job.JobTransportError):
        check_job.check(kind, target)
    assert len(calls) == 1


def test_managed_shim_uses_metadata_helper_only_for_exact_whitelist(monkeypatch):
    monkeypatch.setenv("TRELLIS_COMPILER_CAPABILITY", "managed")
    calls = []
    def metadata(tool, argument):
        calls.append((tool, argument))
        return completed()
    monkeypatch.setattr(check_job, "tool_info", metadata)
    monkeypatch.setattr(check_job, "render", lambda job: 0)
    for tool in ("lean", "lake"):
        for flag in ("--version", "--help", "-h"):
            assert check_job.shim(tool, [flag]) == 0
    assert check_job.shim("lean", ["--print-prefix"]) == 0
    assert check_job.shim("lake", ["env", "lean", "--print-prefix"]) == 0
    assert len(calls) == 8
    for tool, args in [("lean", ["--print-prefix", "Tablet/N.lean"]), ("lake", ["--print-prefix"]), ("lake", ["--version", "build"])]:
        with pytest.raises(check_job.JobError):
            check_job.shim(tool, args)
    assert len(calls) == 8
    monkeypatch.setattr(check_job, "main", lambda args: calls.append(tuple(args)) or 0)
    assert check_job.shim("lake", ["build", "Tablet.N"]) == 0
    assert calls[-1] == ("build", "Tablet.N")


class ReplySocket:
    def __init__(self, reply):
        self.reply = reply
    def __enter__(self):
        return self
    def __exit__(self, *args):
        return False
    def settimeout(self, timeout):
        pass
    def connect(self, address):
        pass
    def sendall(self, data):
        pass
    def recv(self, size):
        result, self.reply = self.reply[:size], self.reply[size:]
        return result


@pytest.mark.parametrize("payload,transport", [(None, True), (b"not JSON", False), (json.dumps({"error": "invalid capability"}).encode(), False)])
def test_rpc_classifies_only_connection_loss_as_transport(monkeypatch, payload, transport):
    reply = b"" if payload is None else struct.pack(">I", len(payload)) + payload
    monkeypatch.setattr(check_job.socket, "socket", lambda *args: ReplySocket(reply))
    with pytest.raises(check_job.JobError) as error:
        check_job.rpc(dict(action="status"), socket_path="/tmp/test.sock", token="test")
    assert isinstance(error.value, check_job.JobTransportError) is transport


@pytest.mark.parametrize("failure,transport", [(ConnectionRefusedError(), True), (FileNotFoundError(), True), (TimeoutError(), True), (PermissionError(), False)])
def test_rpc_does_not_treat_socket_permission_denial_as_reconnectable(monkeypatch, failure, transport):
    class FailedSocket(ReplySocket):
        def connect(self, address):
            raise failure
    monkeypatch.setattr(check_job.socket, "socket", lambda *args: FailedSocket(b""))
    with pytest.raises(check_job.JobError) as error:
        check_job.rpc(dict(action="status"), socket_path="/tmp/test.sock", token="test")
    assert isinstance(error.value, check_job.JobTransportError) is transport


def test_rpc_deadline_covers_all_receive_chunks(monkeypatch, clock):
    payload = json.dumps(completed()).encode()
    class SlowSocket(ReplySocket):
        def settimeout(self, timeout):
            self.timeout = timeout
        def recv(self, size):
            clock[0] += min(2, self.timeout)
            if self.timeout < 2:
                raise TimeoutError("checker connection timed out")
            return super().recv(min(size, 1))
    monkeypatch.setattr(check_job.socket, "socket", lambda *args: SlowSocket(struct.pack(">I", len(payload)) + payload))
    with pytest.raises(check_job.JobTransportError, match="timed out"):
        check_job.rpc(dict(action="status"), socket_path="/tmp/test.sock", token="test", timeout=3)
    assert clock[0] == 3, "all receive chunks must share the same deadline"
