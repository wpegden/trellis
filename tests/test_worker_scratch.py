from __future__ import annotations

import json
import os
from pathlib import Path
import pytest

from trellis.worker_scratch import (
    ensure_worker_scratch_workspace,
    worker_scratch_dir,
    worker_scratch_example_path,
    worker_scratch_notes_path,
    worker_scratch_readme_path,
)


def _write_target(repo: Path, target: str) -> None:
    repo.mkdir(parents=True, exist_ok=True)
    (repo / "trellis.config.json").write_text(
        json.dumps({"workflow": {"default_target": target}}), encoding="utf-8"
    )


def test_ensure_worker_scratch_workspace_creates_scaffold(tmp_path: Path) -> None:
    repo = tmp_path / "repo"
    scratch = ensure_worker_scratch_workspace(repo, reset=True)

    assert scratch["workspace_path"] == worker_scratch_dir(repo)
    assert worker_scratch_readme_path(repo).is_file()
    assert worker_scratch_notes_path(repo).is_file()
    assert worker_scratch_example_path(repo).is_file()
    assert "fresh" in str(scratch["status_text"])


def test_ensure_worker_scratch_workspace_reset_clears_extra_files(tmp_path: Path) -> None:
    repo = tmp_path / "repo"
    ensure_worker_scratch_workspace(repo, reset=True)
    worker_scratch_notes_path(repo).write_text("carry over\n", encoding="utf-8")
    extra = worker_scratch_dir(repo) / "scratch-proof.lean"
    extra.write_text("example : True := trivial\n", encoding="utf-8")

    ensure_worker_scratch_workspace(repo, reset=False)
    assert worker_scratch_notes_path(repo).read_text(encoding="utf-8") == "carry over\n"
    assert extra.exists()

    ensure_worker_scratch_workspace(repo, reset=True)
    assert "carry over" not in worker_scratch_notes_path(repo).read_text(encoding="utf-8")
    assert not extra.exists()


def test_isabelle_scratch_workspace_is_thy_native(tmp_path: Path) -> None:
    repo = tmp_path / "repo"
    _write_target(repo, "isabelle_hol")
    ensure_worker_scratch_workspace(repo, reset=True)

    example = worker_scratch_example_path(repo)
    assert example.name == "example.thy"
    body = example.read_text(encoding="utf-8")
    assert "theory Scratch" in body and "imports Main" in body and body.rstrip().endswith("end")
    assert "import Tablet.Preamble" not in body and "lake" not in body

    readme = worker_scratch_readme_path(repo).read_text(encoding="utf-8")
    assert "isabelle build" in readme.lower() or "isabelle session" in readme.lower()
    assert "lake env lean" not in readme


def test_lean_scratch_workspace_unchanged_with_config(tmp_path: Path) -> None:
    repo = tmp_path / "repo"
    _write_target(repo, "lean")
    ensure_worker_scratch_workspace(repo, reset=True)

    example = worker_scratch_example_path(repo)
    assert example.name == "example.lean"
    assert ".trellis/scripts/check-job scratch" in worker_scratch_readme_path(repo).read_text(encoding="utf-8")


def test_fresh_context_preserves_only_referenced_reviewer_files(tmp_path):
    repo = tmp_path / 'repo'
    ensure_worker_scratch_workspace(repo)
    scratch = worker_scratch_dir(repo)
    (scratch / 'nested').mkdir()
    (scratch / 'nested/plan.md').write_text('Explicit reviewer handoff')
    (scratch / 'notes.md').write_text('Explicit reviewer notes')
    (scratch / 'old-probe.lean').write_text('Discard me')
    result = ensure_worker_scratch_workspace(repo, reset=True, reviewer_comments=
        'Read `.trellis/scratch/nested/plan.md:12` and .trellis/scratch/notes.md. '
        'Also .trellis/scratch/missing.md.')
    assert (scratch / 'nested/plan.md').read_text() == 'Explicit reviewer handoff'
    assert (scratch / 'notes.md').read_text() == 'Explicit reviewer notes'
    assert not (scratch / 'old-probe.lean').exists()
    assert 'Reviewer notes unavailable: .trellis/scratch/missing.md' in result['handoff_status_text']


@pytest.mark.parametrize('kind', ['traversal', 'file_symlink', 'parent_symlink', 'hardlink', 'fifo'])
@pytest.mark.parametrize('namespace', ['scratch', 'stuck-math-audit'])
@pytest.mark.parametrize('suffix', ['', '/'])
def test_handoff_never_reads_external_or_special_files(tmp_path, kind, namespace, suffix):
    from trellis.worker_scratch import capture_reviewer_scratch
    repo = tmp_path / 'repo'; scratch = repo / '.trellis' / namespace; scratch.mkdir(parents=True)
    secret = tmp_path / 'secret.md'; secret.write_text('outside input')
    relative = f'.trellis/{namespace}/note.md'
    if kind == 'traversal':
        relative = f'.trellis/{namespace}/../../../secret.md'
    elif kind == 'file_symlink':
        (repo / relative).symlink_to(secret)
    elif kind == 'parent_symlink':
        scratch.rmdir(); scratch.symlink_to(tmp_path, target_is_directory=True)
        relative = f'.trellis/{namespace}/secret.md'
    elif kind == 'hardlink':
        os.link(secret, repo / relative)
    else:
        os.mkfifo(repo / relative)
    with pytest.raises(ValueError):
        capture_reviewer_scratch(repo, f'Read `{relative}{suffix}`', include_audit=namespace != 'scratch')
    assert secret.read_text() == 'outside input'


@pytest.mark.parametrize('namespace', ['scratch', 'stuck-math-audit'])
@pytest.mark.parametrize('suffix', ['', '/'])
def test_directory_reference_is_unavailable_without_recursive_capture(tmp_path, namespace, suffix):
    from trellis.worker_scratch import capture_reviewer_scratch
    relative = f'.trellis/{namespace}/worker-handoffs/task-12'
    directory = tmp_path / relative
    directory.mkdir(parents=True)
    (directory / 'unrequested.lean').write_text('not explicitly handed off')
    (directory / 'notes.md').write_bytes(b'exact advisory bytes\n')
    captured, missing = capture_reviewer_scratch(tmp_path,
        f'Consult `{relative}{suffix}` and `{relative}/notes.md`.', include_audit=True)
    assert captured == {relative + '/notes.md': b'exact advisory bytes\n'}
    assert missing == [relative + suffix + ' (directory reference; name an exact advisory file)']


@pytest.mark.parametrize('namespace', ['scratch', 'stuck-math-audit'])
@pytest.mark.parametrize('exists', [False, True])
def test_directory_spelling_never_captures_a_regular_file(tmp_path, namespace, exists):
    from trellis.worker_scratch import capture_reviewer_scratch
    relative = f'.trellis/{namespace}/note.md/'
    if exists:
        note = tmp_path / relative
        note.parent.mkdir(parents=True)
        note.write_text('not an explicitly referenced file')
    captured, missing = capture_reviewer_scratch(tmp_path, relative, include_audit=True)
    assert captured == {}
    assert missing == [relative + ' (directory reference; name an exact advisory file)' if exists else relative]


@pytest.mark.parametrize('suffix', ['a//notes.md/', 'a/./notes.md/', 'a/../notes.md/', 'a//'])
def test_directory_spelling_does_not_relax_other_path_normalization(suffix):
    from trellis.worker_scratch import reviewer_scratch_paths
    with pytest.raises(ValueError):
        reviewer_scratch_paths('.trellis/scratch/' + suffix)


def test_handoff_size_limits_report_unavailable_without_loading_large_files(tmp_path, monkeypatch):
    import trellis.worker_scratch as scratch
    monkeypatch.setattr(scratch, '_MAX_HANDOFF_FILE_BYTES', 8)
    monkeypatch.setattr(scratch, '_MAX_HANDOFF_TOTAL_BYTES', 10)
    directory = tmp_path / '.trellis/scratch'; directory.mkdir(parents=True)
    for name, data in [('a.md', 'a' * 9), ('b.md', 'b' * 7), ('c.md', 'c' * 4)]:
        (directory / name).write_text(data)
    captured, missing = scratch.capture_reviewer_scratch(tmp_path,
        'Read .trellis/scratch/a.md, .trellis/scratch/b.md, .trellis/scratch/c.md.')
    assert captured == {'.trellis/scratch/b.md': b'b' * 7}
    assert missing == ['.trellis/scratch/a.md (exceeds handoff limit of 8 bytes)',
                       '.trellis/scratch/c.md (exceeds handoff limit of 3 bytes)']


def test_audit_namespace_is_opt_in_and_keeps_default_scratch_reset(tmp_path):
    from trellis.worker_scratch import capture_reviewer_scratch
    relative = '.trellis/stuck-math-audit/cycle-928/probe.lean'
    note = tmp_path / relative
    note.parent.mkdir(parents=True)
    note.write_bytes(b'exact advisory\n')
    comments = f'Read `{relative}`.'
    assert capture_reviewer_scratch(tmp_path, comments) == ({}, [])
    assert capture_reviewer_scratch(tmp_path, comments, include_audit=True) == ({relative: b'exact advisory\n'}, [])
    ensure_worker_scratch_workspace(tmp_path, reset=True, reviewer_comments=comments)
    assert note.read_bytes() == b'exact advisory\n'
    assert not (tmp_path / '.trellis/scratch/cycle-928/probe.lean').exists()
