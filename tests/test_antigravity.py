"""Tests for the Antigravity (`agy`) provider pane-scrape helpers.

Pane fixtures are real `agy -i` captures (tmux capture-pane) taken with the
pane-scrape-friendly TUI settings (altScreenMode=never, runningLightSpeed=off,
verbosity=low) that run_antigravity_burst seeds.
"""

from __future__ import annotations

from pathlib import Path

from trellis.agents import antigravity as agy


# Real completed-turn pane (idle, two turns rendered).
PANE_IDLE = """\
      ▄▀▀▄        Antigravity CLI 1.0.10
     ▀▀▀▀▀▀       primary@example.com (Google AI Ultra)
    ▀▀▀▀▀▀▀▀      Gemini 3.1 Pro (High)
   ▄▀▀    ▀▀▄     /tmp/x/agytest2
  ▄▀▀      ▀▀▄
────────────────────────────────────────────────────────────
> reply with exactly: HELLO_TUNED
  HELLO_TUNED
────────────────────────────────────────────────────────────
> Write a haiku about Lean theorem proving, then write FINISHED_HAIKU
  Types check and goals close,
  Tactics guide the mind to truth,
  Proofs stand firm and bright.
  FINISHED_HAIKU
────────────────────────────────────────────────────────────
>
────────────────────────────────────────────────────────────
? for shortcuts                                  Gemini 3.1 Pro (High)
"""

# Real mid-generation pane (busy).
PANE_BUSY = """\
> Count slowly from 1 to 5 then say DONE_COUNTING
⣾  Generating...
────────────────────────────────────────────────────────────
>
────────────────────────────────────────────────────────────
esc to cancel                                    Gemini 3.1 Pro (High)
"""


def test_input_box_empty_true_when_idle() -> None:
    assert agy.input_box_is_empty(PANE_IDLE) is True


def test_input_box_empty_false_when_busy() -> None:
    assert agy.input_box_is_empty(PANE_BUSY) is False


def test_last_assistant_message_extracts_last_reply() -> None:
    msg = agy.last_assistant_message(PANE_IDLE)
    assert "FINISHED_HAIKU" in msg
    assert "Types check and goals close," in msg
    # Must not bleed the earlier turn's reply or any banner chrome.
    assert "HELLO_TUNED" not in msg
    assert "Antigravity CLI" not in msg
    assert "? for shortcuts" not in msg


def test_default_model_string() -> None:
    assert agy.DEFAULT_MODEL == "Gemini 3.1 Pro (High)"


def test_seed_settings_idempotent_and_tuned(tmp_path: Path) -> None:
    import json

    home = tmp_path
    agy.seed_antigravity_settings(home, model="Gemini 3.1 Pro (High)")
    agy.seed_antigravity_settings(home, model="Gemini 3.1 Pro (High)")  # twice
    data = json.loads(agy.antigravity_settings_path(home).read_text())
    assert data["altScreenMode"] == "never"
    assert data["runningLightSpeed"] == "off"
    assert data["verbosity"] == "low"
    assert data["toolPermission"] == "always-proceed"
    assert data["model"] == "Gemini 3.1 Pro (High)"


def test_latest_conversation_id_workspace_scoped(tmp_path: Path) -> None:
    import json

    home = tmp_path
    state = agy.antigravity_state_dir(home)
    state.mkdir(parents=True, exist_ok=True)
    ws = (tmp_path / "repo").resolve()
    ws.mkdir()
    other = (tmp_path / "other").resolve()
    other.mkdir()
    hist = agy.antigravity_history_path(home)
    hist.write_text(
        "\n".join(
            json.dumps(rec)
            for rec in [
                {"workspace": str(other), "conversationId": "other-1"},
                {"workspace": str(ws), "conversationId": "ws-old"},
                {"workspace": str(ws), "conversationId": "ws-new"},
            ]
        )
        + "\n",
        encoding="utf-8",
    )
    assert agy.latest_conversation_id(ws, home=home) == "ws-new"
    # No history for an unseen workspace -> None (fresh launch).
    assert agy.latest_conversation_id(tmp_path / "unseen", home=home) is None
