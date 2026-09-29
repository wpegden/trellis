"""Repo discovery for the terminal monitor.

The monitor is read-only, so a wrong answer costs no data -- but a silent
wrong answer (binding to a non-repo and rendering an empty run) is worse than
an error. These tests pin that a bare `.trellis/` directory is not accepted as
a repo, and that a real project repo still is.
"""

from __future__ import annotations

import pytest

# `rich` is an optional pip dependency (see INSTALLATION.md); cli_monitor
# imports it at module scope, so skip rather than force it into the test env.
pytest.importorskip("rich")

from trellis.cli_monitor import _find_repo, main  # noqa: E402


def _make_repo(root):
    """A repo the way a real run leaves it: project config + state dir."""
    root.mkdir(parents=True, exist_ok=True)
    (root / "trellis.config.json").write_text("{}\n")
    (root / ".trellis").mkdir()
    return root


def test_bare_trellis_dir_does_not_capture_discovery(tmp_path) -> None:
    # A stray empty `.trellis/` (a setup bug once left one in ~/math) sits
    # above the cwd. It carries no project config, so it is not a repo and
    # must not be returned.
    stray = tmp_path / "math"
    (stray / ".trellis").mkdir(parents=True)
    start = stray / "sibling"
    start.mkdir()

    assert _find_repo(start) is None


def test_real_repo_is_found_from_a_subdirectory(tmp_path) -> None:
    repo = _make_repo(tmp_path / "math" / "alpha")
    deep = repo / "Tablet" / "nested"
    deep.mkdir(parents=True)

    assert _find_repo(deep) == repo.resolve()


def test_real_repo_wins_over_a_nearer_bare_trellis_dir(tmp_path) -> None:
    # Walking outward, the bare `.trellis/` is encountered first. Positive
    # evidence must carry the decision, not proximity.
    repo = _make_repo(tmp_path / "runs")
    decoy = repo / "scratch"
    (decoy / ".trellis").mkdir(parents=True)

    assert _find_repo(decoy) == repo.resolve()


def test_config_without_state_dir_is_not_a_repo(tmp_path) -> None:
    # The monitor reads `.trellis/`; a config alone is not something it can
    # render, so it must not be silently bound either.
    half = tmp_path / "half"
    half.mkdir()
    (half / "trellis.config.json").write_text("{}\n")

    assert _find_repo(half) is None


def test_failure_message_names_what_it_looked_for_and_where(tmp_path, capsys) -> None:
    stray = tmp_path / "math"
    (stray / ".trellis").mkdir(parents=True)

    assert main(["--repo", str(stray), "--once"]) == 2

    err = capsys.readouterr().err
    assert "trellis.config.json" in err
    assert ".trellis" in err
    assert str(stray.resolve()) in err
    assert "--repo" in err


def test_once_snapshot_runs_against_a_discovered_repo(tmp_path, monkeypatch) -> None:
    repo = _make_repo(tmp_path / "math" / "alpha")
    monkeypatch.chdir(repo)

    assert main(["--once"]) == 0
