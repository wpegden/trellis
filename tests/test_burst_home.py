from __future__ import annotations

from pathlib import Path

from trellis.burst_home import seed_burst_home


def test_seed_prunes_transcript_and_scratch_subtrees(tmp_path: Path) -> None:
    source = tmp_path / "home"
    (source / ".claude" / "jobs" / "j1").mkdir(parents=True)
    (source / ".claude" / "jobs" / "j1" / "scratch.txt").write_text("x")
    (source / ".claude" / "projects" / "p").mkdir(parents=True)
    (source / ".claude" / "projects" / "p" / "t.jsonl").write_text("x")
    (source / ".claude" / "plugins").mkdir()
    (source / ".claude" / "plugins" / "list.json").write_text("[]")
    (source / ".claude" / ".credentials.json").write_text("{}")
    (source / ".claude" / "settings.json").write_text("{}")
    (source / ".codex").mkdir()
    (source / ".codex" / "auth.json").write_text("{}")
    (source / ".codex" / "config.toml").write_text("")
    (source / ".codex" / "logs_2.sqlite").write_text("big")
    (source / ".codex" / "logs_2.sqlite-wal").write_text("big")
    (source / ".codex" / "sessions" / "2026").mkdir(parents=True)
    (source / ".codex" / "sessions" / "2026" / "r.jsonl").write_text("x")
    (source / ".codex" / "state_5.sqlite").write_text("s")

    home = seed_burst_home(tmp_path / "runtime", "worker", source_home=source, persistent=True)

    assert (home / ".claude" / ".credentials.json").is_file()
    assert (home / ".claude" / "settings.json").is_file()
    assert (home / ".claude" / "plugins" / "list.json").is_file()
    assert not (home / ".claude" / "jobs").exists()
    assert not (home / ".claude" / "projects").exists()
    assert (home / ".codex" / "auth.json").is_file()
    assert (home / ".codex" / "config.toml").is_file()
    assert not (home / ".codex" / "state_5.sqlite").exists()
    assert not (home / ".codex" / "logs_2.sqlite").exists()
    assert not (home / ".codex" / "logs_2.sqlite-wal").exists()
    assert not (home / ".codex" / "sessions").exists()
    # auth stays a hard link so a token refresh inside the burst reaches the host
    assert (home / ".codex" / "auth.json").stat().st_ino == (source / ".codex" / "auth.json").stat().st_ino
