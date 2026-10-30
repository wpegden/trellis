"""Claude folder-trust navigation uses the visible choice, never a default."""

from __future__ import annotations

from types import SimpleNamespace

import pytest

from trellis.agents.claude_trust import claude_folder_trust_keys
from trellis.agents.tmux_backend import _detect_dialog
from trellis.agents import tmux_backend
from trellis import quota_snapshots as quota


QUESTION = "Is this a project you created or one you trust?"
YES = "Yes, I trust this folder"
NO = "No, exit"


@pytest.mark.parametrize("labels,selected,keys", [
    ((NO, YES), NO, ("Down", "Enter")),
    ((NO, YES), YES, ("Enter",)),
    ((YES, NO), NO, ("Up", "Enter")),
    ((YES, NO), YES, ("Enter",)),
])
def test_choice_order_and_selection(labels, selected, keys):
    screen = QUESTION + "\n" + "\n".join(
        f"{'❯' if label == selected else ' '} {i}. {label}"
        for i, label in enumerate(labels, 1)
    )
    assert claude_folder_trust_keys(screen) == keys
    assert _detect_dialog("claude", screen) == ("Is this a project you created", keys)
    assert _detect_dialog("antigravity", screen) == ("Is this a project you created", keys)


def test_ansi_decorated_dialog():
    screen = ("\x1b[36m" + QUESTION + "\x1b[0m\n"
              "\x1b[7m❯ 1. No, exit\x1b[0m\n"
              "  2. Yes, I trust this folder")
    assert claude_folder_trust_keys(screen) == ("Down", "Enter")


def test_older_question_and_wrapped_text():
    screen = ("Do you trust the files in this\nfolder?\n"
              "❯ 1. No, exit\n  2. Yes, I trust this folder")
    assert claude_folder_trust_keys(screen) == ("Down", "Enter")
    assert _detect_dialog("claude", screen) == (
        "Is this a project you created", ("Down", "Enter"))


@pytest.mark.parametrize("screen", [
    QUESTION,
    QUESTION + "\n❯ 1. No, exit",
    QUESTION + "\n  1. No, exit\n  2. Yes, I trust this folder",
    QUESTION + "\n❯ 1. No, exit\n❯ 2. Yes, I trust this folder",
    "❯ 1. No, exit\n  2. Yes, I trust this folder",
    "Proceed with dangerous operation?\n❯ 1. No, exit\n  2. Yes, I trust this folder",
    QUESTION + "\nUse the arrow keys\n❯ 1. No, exit",
])
def test_incomplete_or_unrelated_prompt_is_not_accepted(screen):
    assert claude_folder_trust_keys(screen) is None
    assert _detect_dialog("claude", screen) is None
    assert _detect_dialog("antigravity", screen) is None


def test_quota_probe_sends_navigation_once_across_repeated_polls(monkeypatch, tmp_path):
    screen = QUESTION + "\n❯ 1. No, exit\n  2. Yes, I trust this folder"
    sends = []
    monkeypatch.setattr(quota, "_make_throwaway_cwd", lambda _prefix: tmp_path / "probe")
    monkeypatch.setattr(quota.subprocess, "run", lambda *a, **kw: SimpleNamespace(returncode=0, stderr=""))
    monkeypatch.setattr(quota, "_tmux_send", lambda _session, *keys: sends.append(keys))
    monkeypatch.setattr(quota, "_tmux_kill", lambda _session: None)
    monkeypatch.setattr(quota, "_tmux_capture", lambda _session: "usage panel")
    monkeypatch.setattr(quota, "_parse_claude_usage", lambda _screen: {"provider": "claude", "ok": True})
    monkeypatch.setattr(quota.time, "sleep", lambda _seconds: None)

    def wait(_session, matches_any, **kwargs):
        callback = kwargs.get("side_effect_each_poll")
        if callback:
            callback(screen)
            callback(screen)
            callback("Welcome back")
        return "Welcome back" if "Welcome back" in matches_any else "Current week"

    monkeypatch.setattr(quota, "_wait_for", wait)
    assert quota.probe_claude()["ok"] is True
    assert sends == [("Down", "Enter"), ("C-u",), ("/usage",), ("Enter",)]


def test_persistent_session_does_not_navigate_stale_dialog_twice(monkeypatch):
    screen = QUESTION + "\n❯ 1. No, exit\n  2. Yes, I trust this folder"
    sent = []
    ticks = iter(i * 0.6 for i in range(100))
    monkeypatch.setattr(tmux_backend.time, "monotonic", lambda: next(ticks))
    monkeypatch.setattr(tmux_backend.time, "sleep", lambda _seconds: None)
    monkeypatch.setattr(tmux_backend, "pane_dead", lambda _session: False)
    monkeypatch.setattr(tmux_backend, "capture", lambda _session, **_kw: screen)
    monkeypatch.setattr(tmux_backend, "agent_requires_auth", lambda *_args: False)
    monkeypatch.setattr(tmux_backend, "tmux", lambda *args, **_kw: sent.append(args))
    handle = SimpleNamespace(session="fake", provider="claude", preflight_command=None)
    ready, seen = tmux_backend.settle_until_ready(handle, total_timeout=6)
    assert ready is False and "settle_timeout" in seen
    assert [args[-1] for args in sent] == ["Down", "Enter"]


@pytest.mark.parametrize("provider", ["claude", "antigravity"])
def test_ambiguous_trust_screen_cannot_fall_through_to_arrow_hint(monkeypatch, provider):
    screen = QUESTION + "\nUse the arrow keys\n❯ 1. No, exit"
    sent = []
    ticks = iter(i * 0.6 for i in range(100))
    monkeypatch.setattr(tmux_backend.time, "monotonic", lambda: next(ticks))
    monkeypatch.setattr(tmux_backend.time, "sleep", lambda _seconds: None)
    monkeypatch.setattr(tmux_backend, "pane_dead", lambda _session: False)
    monkeypatch.setattr(tmux_backend, "capture", lambda _session, **_kw: screen)
    monkeypatch.setattr(tmux_backend, "agent_requires_auth", lambda *_args: False)
    monkeypatch.setattr(tmux_backend, "tmux", lambda *args, **_kw: sent.append(args))
    handle = SimpleNamespace(session="fake", provider=provider, preflight_command=None)
    ready, seen = tmux_backend.settle_until_ready(handle, total_timeout=6)
    assert ready is False and "settle_timeout" in seen
    assert sent == []
