"""Atomicity + byte-stability of the canonical history artifact write.

`.trellis-history/supervisor_state.json` is read by the viewer, by the
operator rewind procedure, and by the kernel's recovery path while the supervisor
is mid-checkpoint, and it is swept into a commit by `git add -A` moments
later. A reader must therefore never observe a prefix of the new content,
and the serialized bytes must stay exactly what the historical
(non-atomic) write produced so committed blobs keep their shape.
"""

from __future__ import annotations

import json
import os
import threading
from pathlib import Path
from typing import Any, Dict, List

import pytest

from trellis.history_artifacts import (
    SHARED_STATE_FORMAT,
    encode_shared_state,
    is_shared_state,
    load_supervisor_state,
)
from trellis.runtime import git_checkpoint_hook as hook


def _legacy_bytes(payload: Any) -> bytes:
    """Exactly what `_write_json` did before it became atomic."""
    return (json.dumps(payload, indent=2, sort_keys=False) + "\n").encode("utf-8")


TRICKY_PAYLOAD: Dict[str, Any] = {
    "event_count": 1086,
    "unicode": "α≤ℝ ∀n, x₁ → ∫",
    "literal_backslash_u": "\\u0041 and \\\\ and \"quotes\"",
    "control": "\n\t\r\x00\x1f",
    "z_first": 1,
    "a_second": 2,
    "nested": {"b": [1, 2.5, None, True, False, ""], "a": {"deep": ["x"]}},
}


def test_serialization_is_byte_identical_to_legacy_write(tmp_path: Path) -> None:
    path = tmp_path / "history" / "supervisor_state.json"
    hook._write_json(path, TRICKY_PAYLOAD)
    assert path.read_bytes() == _legacy_bytes(TRICKY_PAYLOAD)


def test_key_order_is_preserved_not_sorted(tmp_path: Path) -> None:
    path = tmp_path / "supervisor_state.json"
    hook._write_json(path, TRICKY_PAYLOAD)
    text = path.read_text(encoding="utf-8")
    assert text.index('"z_first"') < text.index('"a_second"')


def test_non_ascii_is_escaped_not_emitted_raw(tmp_path: Path) -> None:
    """`json_io.save_json` uses ensure_ascii=False; this writer must not."""
    path = tmp_path / "supervisor_state.json"
    hook._write_json(path, {"u": "α"})
    assert path.read_bytes() == b'{\n  "u": "\\u03b1"\n}\n'


def test_rename_source_is_a_sibling_temp_and_target_never_holds_a_prefix(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    path = tmp_path / "supervisor_state.json"
    old_payload = {"generation": "old", "filler": ["o" * 256] * 64}
    hook._write_json(path, old_payload)
    old_bytes = path.read_bytes()

    observed: Dict[str, Any] = {}
    real_replace = os.replace

    def spy_replace(src, dst):  # type: ignore[no-untyped-def]
        # Everything for the new generation must still be in the temp file:
        # the target is untouched right up to the instant of the rename.
        observed["target_at_rename"] = Path(dst).read_bytes()
        observed["src"] = Path(src)
        observed["src_bytes"] = Path(src).read_bytes()
        return real_replace(src, dst)

    monkeypatch.setattr(hook.os, "replace", spy_replace)

    new_payload = {"generation": "new", "filler": ["n" * 256] * 64}
    hook._write_json(path, new_payload)

    assert observed["target_at_rename"] == old_bytes
    assert observed["src"].parent == path.parent  # same filesystem => atomic
    assert observed["src"] != path
    assert observed["src_bytes"] == _legacy_bytes(new_payload)
    assert path.read_bytes() == _legacy_bytes(new_payload)


def test_concurrent_reader_never_sees_partial_content(tmp_path: Path) -> None:
    path = tmp_path / "supervisor_state.json"
    old_payload = {"generation": "old", "filler": ["o" * 1024] * 1024}
    new_payload = {"generation": "new", "filler": ["n" * 1024] * 1024}
    hook._write_json(path, old_payload)
    old_bytes = path.read_bytes()
    new_bytes = _legacy_bytes(new_payload)
    assert old_bytes != new_bytes

    # A set, so memory stays bounded no matter how fast the reader spins.
    observed: set[bytes] = set()
    samples: List[int] = []
    stop = threading.Event()
    started = threading.Event()

    def reader() -> None:
        started.set()
        while not stop.is_set():
            try:
                observed.add(path.read_bytes())
            except FileNotFoundError:
                observed.add(b"<missing>")
            samples.append(1)

    thread = threading.Thread(target=reader, daemon=True)
    thread.start()
    started.wait(timeout=5)
    try:
        hook._write_json(path, new_payload)
    finally:
        stop.set()
        thread.join(timeout=5)

    assert samples, "reader thread never sampled the path"
    # Every sample is one complete generation or the other -- never a prefix,
    # never a truncated/missing file.
    assert observed <= {old_bytes, new_bytes}
    assert path.read_bytes() == new_bytes


def test_no_temp_files_left_behind_on_success(tmp_path: Path) -> None:
    path = tmp_path / "supervisor_state.json"
    hook._write_json(path, {"a": 1})
    hook._write_json(path, {"a": 2})
    assert sorted(p.name for p in tmp_path.iterdir()) == ["supervisor_state.json"]


def test_no_temp_files_left_behind_when_rename_fails(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    path = tmp_path / "supervisor_state.json"
    hook._write_json(path, {"a": 1})

    def boom(src, dst):  # type: ignore[no-untyped-def]
        raise OSError("rename failed")

    monkeypatch.setattr(hook.os, "replace", boom)
    with pytest.raises(OSError):
        hook._write_json(path, {"a": 2})

    # Original content survives intact and no scratch file is orphaned in
    # the tracked history directory.
    assert path.read_bytes() == _legacy_bytes({"a": 1})
    assert sorted(p.name for p in tmp_path.iterdir()) == ["supervisor_state.json"]


def test_no_lock_sidecar_is_created(tmp_path: Path) -> None:
    """A `.lock` sidecar in .trellis-history would be committed by `git add -A`."""
    path = tmp_path / "supervisor_state.json"
    hook._write_json(path, {"a": 1})
    assert not list(tmp_path.glob("*.lock"))


def test_new_file_gets_group_readable_mode(tmp_path: Path) -> None:
    path = tmp_path / "supervisor_state.json"
    hook._write_json(path, {"a": 1})
    assert (path.stat().st_mode & 0o777) == hook.HISTORY_JSON_FILE_MODE


def test_existing_file_mode_is_preserved(tmp_path: Path) -> None:
    path = tmp_path / "supervisor_state.json"
    hook._write_json(path, {"a": 1})
    os.chmod(path, 0o600)
    hook._write_json(path, {"a": 2})
    assert (path.stat().st_mode & 0o777) == 0o600


def _canonical_payload(tmp_path: Path) -> Dict[str, Any]:
    repo = tmp_path / "repo"
    repo.mkdir(exist_ok=True)
    runtime_root = tmp_path / "runtime"
    bridge = runtime_root / "bridge"
    bridge.mkdir(parents=True, exist_ok=True)
    return {
        "root": str(runtime_root),
        "event_count": 7,
        "metadata": {"repo_path": str(repo)},
        "checkpoint": {"cycle": 3},
        "state": {
            "phase": "ProofFormalization",
            # Repeated enough to be worth pooling, so the encoded form
            # actually exercises $pool/$strings rather than passing through.
            "nodes": [{"id": "Preamble", "status": "closed"}] * 8,
        },
        "commands": [],
    }


def _expected_document(payload: Dict[str, Any]) -> Dict[str, Any]:
    return {
        "event_count": payload["event_count"],
        "metadata": payload["metadata"],
        "checkpoint": payload["checkpoint"],
        "state": payload["state"],
        "commands": payload["commands"],
    }


def test_write_canonical_history_publishes_both_artifact_kinds(tmp_path: Path) -> None:
    payload = _canonical_payload(tmp_path)
    repo = Path(payload["metadata"]["repo_path"])
    review = {"decision": "approve", "note": "∀ ok"}
    (Path(payload["root"]) / "bridge" / "latest_review.json").write_text(
        json.dumps(review), encoding="utf-8"
    )

    hook._write_canonical_history(payload, repo)

    # The state artifact is the only one the shared-state codec touches; the
    # bridge-derived artifacts stay plain.
    state = load_supervisor_state(hook.supervisor_state_path(repo))
    assert state == _expected_document(payload)
    assert hook.review_result_path(repo).read_bytes() == _legacy_bytes(review)
    assert not list(hook.project_history_dir(repo).glob("*.tmp"))
    assert not list(hook.project_history_dir(repo).glob("*.lock"))


def test_supervisor_state_is_encoded_by_default_and_round_trips(tmp_path: Path) -> None:
    """Default (no reachable config): shared form, and decode == the document."""
    payload = _canonical_payload(tmp_path)
    repo = Path(payload["metadata"]["repo_path"])
    hook._write_canonical_history(payload, repo)

    path = hook.supervisor_state_path(repo)
    raw = json.loads(path.read_text(encoding="utf-8"))
    assert is_shared_state(raw)
    assert raw["$format"] == SHARED_STATE_FORMAT
    assert load_supervisor_state(path) == _expected_document(payload)
    # Passthrough keys stay readable without decoding: `segment_event_log`
    # and prepare_migrated_resume.sh's first phase read `event_count` raw.
    assert raw["event_count"] == payload["event_count"]


def test_knob_false_writes_the_plain_form_byte_for_byte(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    payload = _canonical_payload(tmp_path)
    repo = Path(payload["metadata"]["repo_path"])
    monkeypatch.setattr(hook, "SHARED_STATE_DEFAULT", False)
    hook._write_canonical_history(payload, repo)

    path = hook.supervisor_state_path(repo)
    assert path.read_bytes() == _legacy_bytes(_expected_document(payload))
    assert not is_shared_state(json.loads(path.read_text(encoding="utf-8")))


def test_oversized_plain_payload_latches_on_despite_the_knob(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """The backstop against the 100 MB wall, and its observability."""
    assert hook.SHARED_STATE_LATCH_BYTES == 90 * 1024 * 1024
    payload = _canonical_payload(tmp_path)
    repo = Path(payload["metadata"]["repo_path"])
    monkeypatch.setattr(hook, "SHARED_STATE_DEFAULT", False)
    # Shrink the threshold rather than building a 90 MiB document: the
    # comparison is the behaviour under test, the constant is asserted above.
    monkeypatch.setattr(hook, "SHARED_STATE_LATCH_BYTES", 256)

    hook._write_canonical_history(payload, repo)

    path = hook.supervisor_state_path(repo)
    assert is_shared_state(json.loads(path.read_text(encoding="utf-8")))
    assert load_supervisor_state(path) == _expected_document(payload)

    events = [
        json.loads(line)
        for line in hook._history_format_log_path(repo)
        .read_text(encoding="utf-8")
        .splitlines()
        if line.strip()
    ]
    assert [e["kind"] for e in events] == ["shared_state_auto_latch"]
    assert events[0]["latch_bytes"] == 256
    assert events[0]["plain_bytes"] >= 256


def test_a_lossy_encode_aborts_the_write_instead_of_publishing(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """Round-trip fidelity is enforced at the write site, not assumed."""
    payload = _canonical_payload(tmp_path)
    repo = Path(payload["metadata"]["repo_path"])

    def lossy(document):  # type: ignore[no-untyped-def]
        encoded = encode_shared_state(document)
        encoded["state"] = {}
        return encoded

    monkeypatch.setattr(hook, "encode_shared_state", lossy)
    with pytest.raises(RuntimeError, match="did not round-trip"):
        hook._write_canonical_history(payload, repo)

    assert not hook.supervisor_state_path(repo).exists()
    assert not list(hook.project_history_dir(repo).glob("*.tmp"))
