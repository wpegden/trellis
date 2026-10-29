import hashlib
import io
from itertools import combinations
import json
import os
from pathlib import Path
import subprocess

import pytest

from trellis.runtime import checkpoint_hook_actions as actions
from trellis.runtime import git_checkpoint_hook as hook


@pytest.fixture
def fixture(tmp_path, monkeypatch):
    repo, root, templates = (tmp_path / name for name in ("repo", "runtime", "templates"))
    for path in (repo, root, templates):
        path.mkdir()
    monkeypatch.setenv("TRELLIS_AB_TEMPLATES_DIR", str(templates))
    payload = dict(root=str(root), metadata=dict(repo_path=str(repo)), is_clean=True,
                   checkpoint=dict(cycle=0), state=dict(stage="Start", in_flight_request=None))
    return payload, repo, root, templates


@pytest.mark.parametrize("status", [0, 1])
def test_clean_stop_preserves_success_and_failure_behavior(fixture, status):
    payload, repo, root, _ = fixture
    trigger = root / ".stop_at_next_clean"
    trigger.touch()
    actions.run(payload, status)
    assert (repo / ".trellis-stop-after-checkpoint").is_file()
    assert not trigger.exists()


def test_unclean_checkpoint_retains_trigger(fixture):
    payload, repo, root, _ = fixture
    payload["is_clean"] = False
    (root / ".stop_at_next_clean").touch()
    actions.run(payload, 0)
    assert (root / ".stop_at_next_clean").exists()
    assert not (repo / ".trellis-stop-after-checkpoint").exists()


def test_failed_stop_sentinel_preserves_trigger_for_retry(fixture, monkeypatch, capsys):
    payload, _, root, _ = fixture
    trigger = root / ".stop_at_next_clean"
    trigger.touch()
    def fail_touch(path, *args, **kwargs):
        raise PermissionError("stop destination denied")
    monkeypatch.setattr(Path, "touch", fail_touch)
    actions.run(payload, 0)
    assert trigger.exists()
    assert "stop destination denied" in capsys.readouterr().err


def test_direct_hook_does_not_enable_launcher_actions(fixture, monkeypatch, capsys):
    payload, repo, root, templates = fixture
    (root / ".stop_at_next_clean").touch()
    for name in ("a", "b"):
        (templates / f"{name}.config.json").write_text(name)
    monkeypatch.setattr(hook.sys, "stdin", io.StringIO(json.dumps(payload)))
    monkeypatch.setattr(hook, "commit_checkpoint", lambda document: "committed")
    assert hook.main() == 0
    assert json.loads(capsys.readouterr().out)["ok"] is True
    assert (root / ".stop_at_next_clean").exists()
    assert not (repo / ".trellis-stop-after-checkpoint").exists()
    assert not (repo / "trellis.config.json").exists()
    assert not (templates / "swap_log.jsonl").exists()


@pytest.mark.parametrize("pair", [("a", "b"), ("codex", "gemini")])
def test_config_selection_matches_original_balanced_blocks_and_retry(fixture, pair):
    payload, repo, _, templates = fixture
    for name in pair:
        (templates / f"{name}.config.json").write_text(name)
    choices = []
    for cycle in range(12):
        payload["checkpoint"]["cycle"] = cycle
        block, pos = divmod(cycle, 6)
        arrangements = list(combinations(range(6), 3))
        digest = hashlib.sha256(f"trellis-ab:{templates}:{block}".encode()).digest()
        expected = pair[0] if pos in arrangements[int.from_bytes(digest[:4], "big") % 20] else pair[1]
        actions.run(payload, 0)
        choices.append((repo / "trellis.config.json").read_text())
        assert choices[-1] == expected
        actions.run(payload, 0)
        assert json.loads((templates / "swap_log.jsonl").read_text().splitlines()[-1])["swapped"] is False
    assert choices[:6].count(pair[0]) == choices[6:].count(pair[0]) == 3


@pytest.mark.parametrize("status,stage,in_flight", [(1, "Start", None), (0, "Review", None), (0, "Start", {})])
def test_config_switch_requires_successful_idle_start(fixture, status, stage, in_flight):
    payload, repo, _, templates = fixture
    for name in ("a", "b"):
        (templates / f"{name}.config.json").write_text(name)
    payload["state"].update(stage=stage, in_flight_request=in_flight)
    actions.run(payload, status)
    assert not (repo / "trellis.config.json").exists()
    assert not (templates / "swap_log.jsonl").exists()


def test_optional_action_failure_does_not_fail_committed_checkpoint(fixture, monkeypatch, capsys):
    payload, _, _, _ = fixture
    monkeypatch.setattr(hook.sys, "stdin", io.StringIO(json.dumps(payload)))
    monkeypatch.setattr(hook, "commit_checkpoint", lambda document: "committed")
    def fail(document, status):
        assert status == 0
        raise OSError("optional action failed")
    assert hook.main(after_checkpoint=fail) == 0
    captured = capsys.readouterr()
    assert json.loads(captured.out) == {"ok": True, "commit": "committed"}
    assert "optional action failed" in captured.err


@pytest.mark.parametrize("fails", [False, True])
def test_payload_is_parsed_once_and_same_object_reaches_actions(fixture, monkeypatch, capsys, fails):
    payload, _, _, _ = fixture
    monkeypatch.setattr(hook.sys, "stdin", io.StringIO(json.dumps(payload)))
    seen = []
    def commit(document):
        seen.append(document)
        if fails:
            raise RuntimeError("checkpoint failed")
        return "committed"
    def after(document, status):
        assert document is seen[0]
        assert status == int(fails)
    monkeypatch.setattr(hook, "commit_checkpoint", commit)
    assert hook.main(after_checkpoint=after) == int(fails)
    assert len(seen) == 1
    assert json.loads(capsys.readouterr().out)["ok"] is not fails


def test_shell_launcher_commits_without_temporary_payload(fixture):
    payload, repo, root, templates = fixture
    for args in (("init", "-q"), ("config", "user.name", "Test"), ("config", "user.email", "test@example.invalid")):
        subprocess.run(["git", "-C", str(repo), *args], check=True)
    (root / ".stop_at_next_clean").touch()
    payload.update(event_count=7, commands=[])
    script = Path(__file__).resolve().parents[1] / "scripts" / "trellis_git_checkpoint_hook.sh"
    environment = dict(os.environ, TMPDIR=str(root / "does-not-exist"),
                       TRELLIS_AB_TEMPLATES_DIR=str(templates), PYTHONDONTWRITEBYTECODE="1")
    result = subprocess.run([str(script)], input=json.dumps(payload), text=True,
                            capture_output=True, env=environment, timeout=30)
    assert result.returncode == 0, result.stderr + result.stdout
    assert json.loads(result.stdout)["ok"] is True
    assert (repo / ".trellis-stop-after-checkpoint").exists()
    assert not (root / ".stop_at_next_clean").exists()
    committed = subprocess.check_output(["git", "-C", str(repo), "show", "HEAD:.trellis-history/supervisor_state.json"], text=True)
    assert json.loads(committed)["$format"] == "trellis-shared-state/1"
