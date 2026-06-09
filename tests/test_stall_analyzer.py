from __future__ import annotations

import json
import os
from pathlib import Path

from trellis.stall_analyzer import analyze_inflight_request


def _write_json(path: Path, payload: object) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(json.dumps(payload, indent=2) + "\n", encoding="utf-8")


def _touch(path: Path, ts: float, text: str = "") -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(text, encoding="utf-8")
    os.utime(path, (ts, ts))


def _worker_layout(tmp_path: Path) -> tuple[Path, Path]:
    repo = tmp_path / "repo"
    (repo / "Tablet").mkdir(parents=True)
    (repo / "trellis.config.json").write_text("{}", encoding="utf-8")
    _write_json(
        repo / "trellis.policy.json",
        {"timing": {"stall_threshold_seconds": 60}},
    )
    runtime_root = tmp_path / "run-runtime"
    _write_json(
        runtime_root / "runtime_metadata.json",
        {
            "repo_path": str(repo),
            "config_path": str(repo / "trellis.config.json"),
        },
    )
    _write_json(
        runtime_root / "protocol_state.json",
        {
            "cycle": 31,
            "phase": "ProofFormalization",
            "active_node": "SubcriticalIsolation",
            "in_flight_request": {
                "id": 77,
                "kind": "Worker",
                "cycle": 31,
                "phase": "ProofFormalization",
                "active_node": "SubcriticalIsolation",
            },
        },
    )
    (runtime_root / "event_log.jsonl").write_text("", encoding="utf-8")
    state_dir = repo / ".trellis" / "runtime" / runtime_root.name
    return repo, runtime_root


def test_worker_stable_without_recent_progress_is_stalled(tmp_path: Path, monkeypatch) -> None:
    repo, runtime_root = _worker_layout(tmp_path)
    state_dir = repo / ".trellis" / "runtime" / runtime_root.name
    stem = "trellis_worker_77_result"
    _touch(state_dir / "staging" / f"{stem}.request.json", 100, "{}\n")
    _touch(state_dir / "logs" / f"{stem}-prompt.txt", 110, "prompt\n")
    _touch(repo / ".trellis" / "chats" / "live" / stem / "prompt.txt", 110, "prompt\n")
    _touch(repo / ".trellis" / "scratch" / "test_lemmas.lean", 140, "lemma foo : True := by trivial\n")
    _touch(repo / "Tablet" / "SubcriticalIsolation.lean", 90, "theorem SubcriticalIsolation : True := by\n  sorry\n")
    monkeypatch.setattr("trellis.stall_analyzer._backend_session_status_for_role", lambda role: "stable")

    report = analyze_inflight_request(repo, runtime_root=runtime_root, now=250)

    assert report["ok"] is True
    assert report["stalled"] is True
    assert report["status"] == "stalled"
    assert report["reason_code"] == "stable_after_work_no_handoff"
    assert report["request"]["id"] == 77
    assert report["last_progress"]["label"] == "scratch"
    assert report["last_substantive_progress"]["label"] == "scratch"


def test_worker_running_is_not_stalled(tmp_path: Path, monkeypatch) -> None:
    repo, runtime_root = _worker_layout(tmp_path)
    state_dir = repo / ".trellis" / "runtime" / runtime_root.name
    stem = "trellis_worker_77_result"
    _touch(state_dir / "staging" / f"{stem}.request.json", 100, "{}\n")
    _touch(state_dir / "logs" / f"{stem}-prompt.txt", 110, "prompt\n")
    _touch(repo / ".trellis" / "chats" / "live" / stem / "prompt.txt", 110, "prompt\n")
    monkeypatch.setattr("trellis.stall_analyzer._backend_session_status_for_role", lambda role: "running")

    report = analyze_inflight_request(repo, runtime_root=runtime_root, now=250)

    assert report["ok"] is True
    assert report["stalled"] is False
    assert report["status"] == "active"
    assert report["reason_code"] == "backend_running"


def test_worker_prompt_only_quiet_time_gets_extra_grace(tmp_path: Path, monkeypatch) -> None:
    repo, runtime_root = _worker_layout(tmp_path)
    state_dir = repo / ".trellis" / "runtime" / runtime_root.name
    stem = "trellis_worker_77_result"
    _touch(state_dir / "staging" / f"{stem}.request.json", 100, "{}\n")
    _touch(state_dir / "logs" / f"{stem}-prompt.txt", 110, "prompt\n")
    _touch(repo / ".trellis" / "chats" / "live" / stem / "prompt.txt", 110, "prompt\n")
    monkeypatch.setattr("trellis.stall_analyzer._backend_session_status_for_role", lambda role: "stable")

    report = analyze_inflight_request(repo, runtime_root=runtime_root, now=205)
    assert report["stalled"] is False
    assert report["status"] == "idle"
    assert report["reason_code"] == "backend_stable"

    report = analyze_inflight_request(repo, runtime_root=runtime_root, now=235)
    assert report["stalled"] is True
    assert report["status"] == "stalled"
    assert report["reason_code"] == "stable_prompt_only_too_long"


def test_post_handoff_unconsumed_is_stalled(tmp_path: Path, monkeypatch) -> None:
    repo, runtime_root = _worker_layout(tmp_path)
    state_dir = repo / ".trellis" / "runtime" / runtime_root.name
    stem = "trellis_worker_77_result"
    _touch(state_dir / "staging" / f"{stem}.request.json", 100, "{}\n")
    _touch(state_dir / "staging" / f"{stem}.raw.json", 120, "{}\n")
    _touch(state_dir / "staging" / f"{stem}.done", 121, "")
    _touch(state_dir / "logs" / f"{stem}-prompt.txt", 110, "prompt\n")
    monkeypatch.setattr("trellis.stall_analyzer._backend_session_status_for_role", lambda role: "stable")

    report = analyze_inflight_request(repo, runtime_root=runtime_root, now=250)

    assert report["ok"] is True
    assert report["stalled"] is True
    assert report["status"] == "stalled"
    assert report["reason_code"] == "post_handoff_unconsumed"
