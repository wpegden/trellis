import gzip
import hashlib
import io
import json
import signal
import stat
import subprocess

import pytest

from trellis.runtime import kernel_cli as cli


class Sink(io.BytesIO):
    def close(self):
        self.captured = self.getvalue()
        super().close()


def fake_child(monkeypatch, *, code, stdout=b"", stderr=b""):
    children = []

    def popen(command, **kwargs):
        class Child:
            pid = 2718
            args = command
            returncode = code

            def wait(self):
                return self.returncode

        child = Child()
        child.stdin, child.stdout, child.stderr = Sink(), io.BytesIO(stdout), io.BytesIO(stderr)
        children.append(child)
        return child

    monkeypatch.setattr(cli.subprocess, "Popen", popen)
    monkeypatch.setattr(cli, "kernel_cli_command", lambda: ["unused-native-fixture"])
    return children


def test_abort_retains_exact_input_and_progress_before_cleanup(tmp_path, monkeypatch):
    monkeypatch.setattr(cli.Path, "home", lambda: tmp_path)
    progress = tmp_path / "progress.log"
    progress.write_bytes(b"x" * 70000 + b"last progress\n")
    monkeypatch.setattr(cli, "_setup_progress_tail", lambda: (
        progress, {cli._PROGRESS_LOG_ENV: str(progress)}, None, None))
    children = fake_child(monkeypatch, code=-signal.SIGABRT, stderr=b"stack overflow\n")
    payload = {"action": "dispatch_acceptance_state", "state": {"source": "unicode λ"}}
    with pytest.raises(cli.KernelCliError, match="stack overflow") as error:
        cli.run_kernel_cli(payload)
    assert len(children) == 1, "an abort must never retry"
    assert not progress.exists(), "ordinary progress cleanup is unchanged"
    directory, = (tmp_path / ".local/state/trellis/kernel-failures").iterdir()
    receipt = json.loads((directory / "receipt.json").read_text())
    original = children[0].stdin.captured
    assert original == json.dumps(payload).encode()
    assert gzip.decompress((directory / "input.json.gz").read_bytes()) == original
    assert receipt["input_sha256"] == hashlib.sha256(original).hexdigest()
    assert receipt["action"] == payload["action"]
    assert receipt["child_pid"] == 2718
    assert receipt["returncode"] == -signal.SIGABRT
    assert (directory / "progress.tail").read_bytes() == (b"x" * 70000 + b"last progress\n")[-65536:]
    assert (directory / "stderr.tail").read_bytes() == b"stack overflow\n"
    assert stat.S_IMODE(directory.stat().st_mode) == 0o700
    assert all(stat.S_IMODE(p.stat().st_mode) == 0o600 for p in directory.iterdir())
    assert str(directory / "receipt.json") in str(error.value)


@pytest.mark.parametrize("code,stdout,stderr", [
    (0, b'{"status":"dispatch_ok","output":17}', b""),
    (2, b'{"error":"ordinary refusal"}', b"diagnostic"),
])
def test_success_and_ordinary_refusal_create_no_crash_evidence(tmp_path, monkeypatch, code, stdout, stderr):
    monkeypatch.setattr(cli.Path, "home", lambda: tmp_path)
    monkeypatch.setattr(cli, "_setup_progress_tail", lambda: (None, {}, None, None))
    fake_child(monkeypatch, code=code, stdout=stdout, stderr=stderr)
    if code:
        with pytest.raises(cli.KernelCliError, match="^ordinary refusal$"):
            cli.run_kernel_cli({"action": "dispatch_control"})
    else:
        assert cli.run_kernel_cli({"action": "dispatch_control"}) == {"status": "dispatch_ok", "output": 17}
    assert not (tmp_path / ".local").exists()


def test_diagnostic_write_failure_preserves_abort_and_does_not_retry(tmp_path, monkeypatch):
    monkeypatch.setattr(cli.Path, "home", lambda: tmp_path)
    (tmp_path / ".local").write_text("not a directory")
    monkeypatch.setattr(cli, "_setup_progress_tail", lambda: (None, {}, None, None))
    children = fake_child(monkeypatch, code=-signal.SIGSEGV, stderr=b"original native failure")
    with pytest.raises(cli.KernelCliError, match="original native failure") as error:
        cli.run_kernel_cli({"action": "dispatch_control"})
    assert "diagnostic_unavailable=" in str(error.value)
    assert len(children) == 1


def test_partial_json_on_abort_still_reports_the_receipt(tmp_path, monkeypatch):
    monkeypatch.setattr(cli.Path, "home", lambda: tmp_path)
    monkeypatch.setattr(cli, "_setup_progress_tail", lambda: (None, {}, None, None))
    fake_child(monkeypatch, code=-signal.SIGABRT, stdout=b'{"state":', stderr=b"stack overflow")
    with pytest.raises(cli.KernelCliError, match="invalid kernel CLI response") as error:
        cli.run_kernel_cli({"action": "dispatch_control"})
    assert "stack overflow" in str(error.value)
    assert "receipt=" in str(error.value)


@pytest.mark.parametrize("restricted", ["unknown_action", "too_large"])
def test_payload_retention_is_restricted(tmp_path, monkeypatch, restricted):
    monkeypatch.setattr(cli.Path, "home", lambda: tmp_path)
    payload = json.dumps({"action": "dispatch_control" if restricted == "too_large" else "run",
                          "value": "private launch configuration"}).encode()
    if restricted == "too_large":
        monkeypatch.setattr(cli, "_FAILURE_INPUT_LIMIT", 1)
    cli._retain_native_failure(payload, 22, -6, b"", None)
    directory, = (tmp_path / ".local/state/trellis/kernel-failures").iterdir()
    receipt = json.loads((directory / "receipt.json").read_text())
    assert receipt["input_sha256"] == hashlib.sha256(payload).hexdigest()
    assert not receipt["input_retained"]
    assert not (directory / "input.json.gz").exists()


def test_symlink_diagnostic_directory_is_refused(tmp_path, monkeypatch):
    monkeypatch.setattr(cli.Path, "home", lambda: tmp_path)
    target = tmp_path / "target"
    target.mkdir(mode=0o700)
    parent = tmp_path / ".local/state/trellis"
    parent.mkdir(parents=True)
    (parent / "kernel-failures").symlink_to(target, target_is_directory=True)
    result = cli._retain_native_failure(b'{"action":"dispatch_control"}', 22, -6, b"", None)
    assert "diagnostic_unavailable=" in result
    assert not list(target.iterdir())


def test_existing_empty_sigterm_retry_policy_is_unchanged(monkeypatch):
    attempts = []

    def once(payload):
        attempts.append(payload)
        return subprocess.CompletedProcess([], -15, "", "")

    monkeypatch.setattr(cli, "_run_kernel_cli_once", once)
    monkeypatch.setattr(cli.time, "sleep", lambda _: None)
    with pytest.raises(cli.KernelCliError, match="status -15"):
        cli.run_kernel_cli({"action": "dispatch_control"})
    assert len(attempts) == 3
    assert len(set(attempts)) == 1
