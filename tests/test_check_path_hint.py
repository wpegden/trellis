"""The --context-json path tax (dec2flt resume: 136 of 151 post-resume worker
bursts, 90%, mistyped the artifact-check path once): the runtime state dir and
the repo-side bridge dir share a basename, so ``<runtime>/<name>/staging/f``
is a natural mistyping of ``<repo>/.trellis/runtime/<name>/staging/f``. The
checker's file-not-found error must state the correct path when it can find
exactly one same-basename artifact under the repo's bridge staging tree."""

from __future__ import annotations

import json
from pathlib import Path

from trellis.checking import _load_json_artifact


def _bridge_file(repo: Path, run: str, name: str) -> Path:
    staging = repo / ".trellis" / "runtime" / run / "staging"
    staging.mkdir(parents=True, exist_ok=True)
    path = staging / name
    path.write_text(json.dumps({"ok": True}), encoding="utf-8")
    return path


def test_not_found_error_names_the_bridge_staging_twin(tmp_path: Path) -> None:
    repo = tmp_path / "campaign"
    runtime = tmp_path / "runtime-root"
    correct = _bridge_file(repo, "run-20260101T000000Z", "w_1.context.json")
    mistyped = runtime / "run-20260101T000000Z" / "staging" / "w_1.context.json"

    data, errors = _load_json_artifact(mistyped, repo=repo)

    assert data is None
    assert errors, "a missing artifact still errors"
    joined = "\n".join(errors)
    assert str(correct) in joined, joined


def test_not_found_error_without_a_twin_stays_plain(tmp_path: Path) -> None:
    repo = tmp_path / "campaign"
    missing = tmp_path / "nowhere" / "staging" / "w_9.context.json"

    data, errors = _load_json_artifact(missing, repo=repo)

    assert data is None
    assert errors == [f"{missing} not found"]


def test_ambiguous_twins_are_not_guessed(tmp_path: Path) -> None:
    repo = tmp_path / "campaign"
    _bridge_file(repo, "run-a", "w_1.context.json")
    _bridge_file(repo, "run-b", "w_1.context.json")
    mistyped = tmp_path / "elsewhere" / "staging" / "w_1.context.json"

    data, errors = _load_json_artifact(mistyped, repo=repo)

    assert data is None
    assert errors == [f"{mistyped} not found"]
