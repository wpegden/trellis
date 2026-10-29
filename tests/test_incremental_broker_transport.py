"""Real UNIX socket and independent-process regressions; no Lean required."""
import json
import os
from pathlib import Path
import socket
import subprocess
import sys
import time

import pytest

from trellis import incremental_check as ic


@pytest.mark.parametrize("component", ["deep-checkout-" * 8, "λ" * 55])
def test_long_address_reaches_original_socket_and_closes_directory_fd(tmp_path, component):
    directory = tmp_path / component
    directory.mkdir()
    path = directory / "broker.sock"
    assert len(os.fsencode(path)) >= 108
    cwd = Path.cwd()
    with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as server:
        with ic._socket_address(path) as address:
            assert len(os.fsencode(address)) < 108
            fd = int(address.split("/")[-2])
            server.bind(address)
        with pytest.raises(OSError):
            os.fstat(fd)
        assert path.is_socket()
        server.listen()
        with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as client:
            ic._connect_socket(client, path)
            conn, _ = server.accept()
            with conn:
                ic._send_frame(client, {"method": "ping"})
                assert ic._recv_frame(conn) == {"method": "ping"}
    assert Path.cwd() == cwd


def test_parallel_cold_clients_share_one_detached_broker_and_can_restart(tmp_path):
    repo = tmp_path / ("nested-generation-" * 7)
    repo.mkdir()
    client = (
        "from pathlib import Path; import sys; "
        "from trellis import incremental_check as ic; "
        "repo=Path(sys.argv[1]); assert ic._ensure_broker(repo); "
        "print(ic._broker_pidfile(repo).read_text())"
    )
    children = []
    try:
        for _ in range(12):
            children.append(subprocess.Popen(
                [sys.executable, "-c", client, str(repo)],
                stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True,
            ))
        pids = set()
        for child in children:
            out, err = child.communicate(timeout=15)
            assert child.returncode == 0, err
            pids.add(int(out.strip()))
        assert len(pids) == 1
        assert ic._broker_reachable(repo)
        assert ic.main(["--repo", str(repo), "--shutdown"]) == 0
        assert not ic._socket_path(repo).exists()
        assert not ic._broker_pidfile(repo).exists()
        restarted = subprocess.run(
            [sys.executable, "-c", client, str(repo)],
            capture_output=True, text=True, timeout=15, check=True,
        )
        assert int(restarted.stdout.strip()) not in pids
    finally:
        for child in children:
            if child.poll() is None:
                child.kill()
                child.wait()
        ic.main(["--repo", str(repo), "--shutdown"])


def test_timing_preserves_verdict_and_records_fallback_reason(tmp_path, monkeypatch):
    monkeypatch.setattr(ic.subprocess, "run", lambda *a, **kw: subprocess.CompletedProcess(a, 7))
    assert ic._lake_build_fallback(tmp_path, "N", reason="broker unavailable") == 7
    row = json.loads((ic._server_state_dir(tmp_path) / "checks.jsonl").read_text())
    assert row["backend"] == "lake_fallback"
    assert row["fallback_reason"] == "broker unavailable"
    assert row["exit_code"] == 7
    assert row["elapsed_ms"] >= 0
    # A broken telemetry destination cannot change the build result.
    (ic._server_state_dir(tmp_path) / "checks.jsonl").unlink()
    (ic._server_state_dir(tmp_path) / "checks.jsonl").mkdir()
    assert ic._lake_build_fallback(tmp_path, "N", reason="test") == 7


def test_failed_bind_is_reported_instead_of_silently_succeeding(tmp_path):
    path = tmp_path / "absent" / "broker.sock"
    with pytest.raises(OSError):
        ic._Broker(tmp_path).serve(path)


@pytest.mark.parametrize("changed", ["legacy", "pid_namespace", "start_ticks", "boot_id"])
def test_reaping_a_stale_pid_never_signals_a_replacement_process(tmp_path, changed):
    child = subprocess.Popen([sys.executable, "-c", "import time; time.sleep(60)"])
    try:
        ic._server_state_dir(tmp_path).mkdir(parents=True)
        ic._broker_pidfile(tmp_path).write_text(str(child.pid))
        record = dict(ic._process_identity(child.pid))
        if changed != "legacy":
            record[changed] = "different"
            ic._broker_identity_file(tmp_path).write_text(json.dumps(record))
        ic._reap_stale_broker(tmp_path)
        assert child.poll() is None
        assert not ic._broker_pidfile(tmp_path).exists()
    finally:
        child.terminate()
        child.wait(timeout=5)


def test_reaping_stops_the_identity_matched_process(tmp_path):
    child = subprocess.Popen([sys.executable, "-c", "import time; time.sleep(60)"])
    try:
        ic._server_state_dir(tmp_path).mkdir(parents=True)
        ic._broker_pidfile(tmp_path).write_text(str(child.pid))
        ic._broker_identity_file(tmp_path).write_text(json.dumps(ic._process_identity(child.pid)))
        ic._reap_stale_broker(tmp_path)
        assert child.wait(timeout=5) < 0
    finally:
        if child.poll() is None:
            child.kill()
            child.wait()
