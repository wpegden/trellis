"""Stage 3 (PV-framework rebuild, plan doc 32): the checkpoint hook's Q1
gate-decision transaction half — tracked record file written before the
commit, lightweight ``supervisor2/trust-decision-*`` tag written last, and
the never-delete rule (the checkpoint/clean delete-then-recreate pattern is
a pinned trap that must not be reused for decision tags)."""

import json
import subprocess
from pathlib import Path

import pytest

import trellis.runtime.git_checkpoint_hook as hook_module
from trellis.runtime.git_checkpoint_hook import (
    TRUST_DECISION_RECORD_DIR,
    TRUST_DECISION_TAG_PREFIX,
    commit_checkpoint,
)


def _git(repo: Path, *args: str) -> str:
    result = subprocess.run(
        ["git", *args], cwd=str(repo), capture_output=True, text=True, check=True
    )
    return result.stdout


def _init_repo(tmp_path: Path) -> Path:
    repo = tmp_path / "repo"
    repo.mkdir()
    _git(repo, "init", "-q")
    _git(repo, "config", "user.name", "Trellis Test")
    _git(repo, "config", "user.email", "trellis@example.invalid")
    (repo / ".keep").write_text("keep", encoding="utf-8")
    _git(repo, "add", "-A")
    _git(repo, "commit", "-q", "-m", "init")
    return repo


def _payload(repo: Path, tmp_path: Path, trust_record=None) -> dict:
    root = tmp_path / "runtime"
    root.mkdir(exist_ok=True)
    payload = {
        "root": str(root),
        "state_path": str(root / "protocol_state.json"),
        "event_log_dir": str(repo / ".trellis-history" / "event-log"),
        "checkpoint_path": str(root / "checkpoint.json"),
        "metadata_path": str(root / "runtime_metadata.json"),
        "metadata": {"repo_path": str(repo)},
        "state": {"phase": "proof_formalization", "stage": "start"},
        # cycle 0 keeps the chat-history operations (a second repo) out of
        # this unit's scope.
        "checkpoint": {"cycle": 0, "phase": "proof_formalization"},
        "commands": [],
        "event_count": 7,
        "is_clean": False,
    }
    if trust_record is not None:
        payload["trust_record"] = trust_record
    return payload


DIGEST = "ab" * 32
LINE = json.dumps({"index": 7, "event": {"event": "start_cycle"}, "trust_record": {"record_sha256": DIGEST}})


def test_decision_payload_writes_record_file_before_commit_and_tag_last(tmp_path):
    repo = _init_repo(tmp_path)
    carrier = {"line_json": LINE, "record_sha256": DIGEST}
    head = commit_checkpoint(_payload(repo, tmp_path, trust_record=carrier))
    assert head

    record_path = repo / TRUST_DECISION_RECORD_DIR / f"{DIGEST}.json"
    assert record_path.read_text(encoding="utf-8") == LINE + "\n"
    # The record file is COMMITTED (inside the checkpoint commit's tree).
    tracked = _git(repo, "ls-tree", "-r", "--name-only", "HEAD")
    assert f"{TRUST_DECISION_RECORD_DIR}/{DIGEST}.json" in tracked

    tag = f"{TRUST_DECISION_TAG_PREFIX}{DIGEST[:12]}"
    assert tag in _git(repo, "tag", "-l", tag)
    # Recovery contract: `git show <tag>:<path>` returns the exact bytes.
    shown = _git(repo, "show", f"{tag}:{TRUST_DECISION_RECORD_DIR}/{DIGEST}.json")
    assert shown == LINE + "\n"


def test_same_digest_same_content_is_idempotent(tmp_path):
    repo = _init_repo(tmp_path)
    carrier = {"line_json": LINE, "record_sha256": DIGEST}
    commit_checkpoint(_payload(repo, tmp_path, trust_record=carrier))
    # A re-run with the identical decision (e.g. a crash retry) succeeds and
    # never deletes or force-moves the tag.
    payload = _payload(repo, tmp_path, trust_record=carrier)
    payload["event_count"] = 8  # a new checkpoint tag, same decision
    commit_checkpoint(payload)
    tag = f"{TRUST_DECISION_TAG_PREFIX}{DIGEST[:12]}"
    assert tag in _git(repo, "tag", "-l", tag)


def test_same_name_different_content_is_a_hard_error(tmp_path):
    repo = _init_repo(tmp_path)
    carrier = {"line_json": LINE, "record_sha256": DIGEST}
    commit_checkpoint(_payload(repo, tmp_path, trust_record=carrier))
    different = {"line_json": LINE.replace('"index": 7', '"index": 9'), "record_sha256": DIGEST}
    with pytest.raises(RuntimeError, match="different content"):
        commit_checkpoint(_payload(repo, tmp_path, trust_record=different))
    # The original committed record survives untouched.
    record_path = repo / TRUST_DECISION_RECORD_DIR / f"{DIGEST}.json"
    assert record_path.read_text(encoding="utf-8") == LINE + "\n"


def test_math_mode_payload_writes_no_decision_artifacts(tmp_path):
    repo = _init_repo(tmp_path)
    commit_checkpoint(_payload(repo, tmp_path))
    assert not (repo / TRUST_DECISION_RECORD_DIR).exists()
    assert _git(repo, "tag", "-l", f"{TRUST_DECISION_TAG_PREFIX}*").strip() == ""


# ---------------------------------------------------------------------------
# Stage-3 fix 3 (Codex 3): a same-answer retry after each real pre-tag
# failure point converges — the runtime reuses the surviving prospective
# line verbatim (timestamp included), so the retried carrier is
# byte-identical, and the hook resumes chat/tag completion when HEAD
# already contains the identical record but lacks the decision tag.
# All three tests run with cycle > 0 so the chat-history operations (the
# plan's named fallible window) are in scope.
# ---------------------------------------------------------------------------


def _cycle_payload(repo: Path, tmp_path: Path, carrier: dict) -> dict:
    payload = _payload(repo, tmp_path, trust_record=carrier)
    payload["checkpoint"] = {"cycle": 1, "phase": "proof_formalization"}
    return payload


def _decision_tag_exists(repo: Path) -> bool:
    tag = f"{TRUST_DECISION_TAG_PREFIX}{DIGEST[:12]}"
    return bool(_git(repo, "tag", "-l", tag).strip())


def _main_commit_count(repo: Path) -> int:
    return int(_git(repo, "rev-list", "--count", "HEAD").strip())


def test_same_answer_retry_after_commit_failure_converges(tmp_path, monkeypatch):
    # Failure point 1 — post-record-file: the main-repo commit fails after
    # the record file is written into the worktree.
    repo = _init_repo(tmp_path)
    carrier = {"line_json": LINE, "record_sha256": DIGEST}
    real_git = hook_module._git

    def failing_git(target, *args, **kwargs):
        if args and args[0] == "commit":
            raise RuntimeError("injected main-repo commit failure")
        return real_git(target, *args, **kwargs)

    monkeypatch.setattr(hook_module, "_git", failing_git)
    with pytest.raises(RuntimeError, match="injected main-repo commit failure"):
        commit_checkpoint(_cycle_payload(repo, tmp_path, carrier))
    monkeypatch.undo()

    record_path = repo / TRUST_DECISION_RECORD_DIR / f"{DIGEST}.json"
    assert record_path.read_text(encoding="utf-8") == LINE + "\n"
    assert not _decision_tag_exists(repo), "hook failed => no decision tag"

    # Same answer, byte-identical carrier: the retry stages the surviving
    # record file and completes the whole transaction.
    head = commit_checkpoint(_cycle_payload(repo, tmp_path, carrier))
    assert head
    assert _decision_tag_exists(repo)
    assert record_path.read_text(encoding="utf-8") == LINE + "\n"
    assert _main_commit_count(repo) == 2  # init + the checkpoint commit


def test_same_answer_retry_after_chat_ops_failure_resumes_completion(tmp_path, monkeypatch):
    # Failure point 2 — post-main-commit: the cycle-chat operations fail
    # after the record file is COMMITTED. The retry finds nothing to stage
    # and must RESUME (not reject) the tag/chat completion.
    repo = _init_repo(tmp_path)
    # Steady-state repo shape: the nested chat-state tree is not tracked by
    # the main repo, so a resumed attempt has nothing new to stage.
    (repo / ".gitignore").write_text(".trellis/\n", encoding="utf-8")
    _git(repo, "add", "-A")
    _git(repo, "commit", "-q", "-m", "ignore chat state")
    carrier = {"line_json": LINE, "record_sha256": DIGEST}

    def failing_chat_checkpoint(*args, **kwargs):
        raise RuntimeError("injected chat-history failure")

    monkeypatch.setattr(hook_module, "commit_chat_checkpoint", failing_chat_checkpoint)
    with pytest.raises(RuntimeError, match="injected chat-history failure"):
        commit_checkpoint(_cycle_payload(repo, tmp_path, carrier))
    monkeypatch.undo()

    # The record file is committed at HEAD; the decision tag is missing.
    tracked = _git(repo, "ls-tree", "-r", "--name-only", "HEAD")
    assert f"{TRUST_DECISION_RECORD_DIR}/{DIGEST}.json" in tracked
    assert not _decision_tag_exists(repo)
    commits_after_failure = _main_commit_count(repo)

    head = commit_checkpoint(_cycle_payload(repo, tmp_path, carrier))
    assert head
    assert _decision_tag_exists(repo)
    # Resume: no second checkpoint commit was created.
    assert _main_commit_count(repo) == commits_after_failure
    record_path = repo / TRUST_DECISION_RECORD_DIR / f"{DIGEST}.json"
    assert record_path.read_text(encoding="utf-8") == LINE + "\n"


def test_same_answer_retry_after_tag_write_crash_resumes_completion(tmp_path, monkeypatch):
    # Failure point 3 — post-chat-ops: the process dies at the decision-tag
    # write itself, after every earlier operation succeeded.
    repo = _init_repo(tmp_path)
    (repo / ".gitignore").write_text(".trellis/\n", encoding="utf-8")
    _git(repo, "add", "-A")
    _git(repo, "commit", "-q", "-m", "ignore chat state")
    carrier = {"line_json": LINE, "record_sha256": DIGEST}

    def crashing_tag_write(*args, **kwargs):
        raise RuntimeError("injected crash at the decision-tag write")

    monkeypatch.setattr(hook_module, "_write_trust_decision_tag", crashing_tag_write)
    with pytest.raises(RuntimeError, match="injected crash at the decision-tag write"):
        commit_checkpoint(_cycle_payload(repo, tmp_path, carrier))
    monkeypatch.undo()

    assert not _decision_tag_exists(repo)
    commits_after_failure = _main_commit_count(repo)

    head = commit_checkpoint(_cycle_payload(repo, tmp_path, carrier))
    assert head
    assert _decision_tag_exists(repo)
    assert _main_commit_count(repo) == commits_after_failure
    # The decision tag points at the commit carrying the record file.
    tag = f"{TRUST_DECISION_TAG_PREFIX}{DIGEST[:12]}"
    shown = _git(repo, "show", f"{tag}:{TRUST_DECISION_RECORD_DIR}/{DIGEST}.json")
    assert shown == LINE + "\n"


def test_nothing_to_commit_without_committed_record_stays_a_hard_error(tmp_path):
    # The resume branch is gated on HEAD carrying the IDENTICAL record
    # bytes: a decision-bearing payload that stages nothing while HEAD has
    # no committed record file keeps the fail-closed hard error.
    repo = _init_repo(tmp_path)
    (repo / ".gitignore").write_text(
        ".trellis-history/\n.trellis/\n", encoding="utf-8"
    )
    _git(repo, "add", "-A")
    _git(repo, "commit", "-q", "-m", "pathological ignore of the history tree")
    carrier = {"line_json": LINE, "record_sha256": DIGEST}
    with pytest.raises(RuntimeError, match="nothing to commit"):
        commit_checkpoint(_cycle_payload(repo, tmp_path, carrier))
    assert not _decision_tag_exists(repo)


def test_decision_tag_is_never_deleted_by_checkpoint_tag_rewrites(tmp_path):
    # The checkpoint/clean tags use delete-then-recreate; the decision tag
    # writer is a separate path that never deletes.  Re-running the SAME
    # event_count (which rewrites the checkpoint tag) must leave the
    # decision tag exactly where it was.
    repo = _init_repo(tmp_path)
    carrier = {"line_json": LINE, "record_sha256": DIGEST}
    commit_checkpoint(_payload(repo, tmp_path, trust_record=carrier))
    tag = f"{TRUST_DECISION_TAG_PREFIX}{DIGEST[:12]}"
    before = _git(repo, "rev-parse", tag).strip()
    payload = _payload(repo, tmp_path)
    (repo / "unrelated.txt").write_text("change", encoding="utf-8")
    commit_checkpoint(payload)
    after = _git(repo, "rev-parse", tag).strip()
    assert before == after
