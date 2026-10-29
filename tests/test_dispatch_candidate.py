import json
from pathlib import Path
import shutil

import pytest

from trellis.dispatch.candidate import CandidateError, check_scope, check_lean_closure, digest, read, rebase, seal, source_manifest


def put(root, name, content):
    p = root / name
    p.parent.mkdir(parents=True, exist_ok=True)
    p.write_text(content)


def setup_candidate(tmp_path):
    canonical = tmp_path / "canonical"
    for node in ("A", "B", "C"):
        put(canonical, f"Tablet/{node}.lean", f"-- {node} original")
    worker = tmp_path / "worker"
    shutil.copytree(canonical, worker)
    return canonical, worker, source_manifest(canonical)


def test_independent_completion_preserves_already_accepted_work(tmp_path):
    canonical, worker, base = setup_candidate(tmp_path)
    put(worker, "Tablet/A.lean", "-- A proof")
    put(worker, "Tablet/task1_helper.lean", "-- owned helper")
    candidate = tmp_path / "sealed"
    identity = seal(worker, candidate, task_id=1, base_manifest=base,
                    inputs={"Tablet/C.lean": base["Tablet/C.lean"]}, response={"outcome": "valid"})
    check_scope(read(candidate)[0], nodes={"A"}, helper_prefixes={"task1_"})
    put(canonical, "Tablet/B.lean", "-- B accepted meanwhile")
    before = source_manifest(canonical)
    integration = tmp_path / "integration"
    shutil.copytree(canonical, integration)
    receipt = rebase(candidate, canonical, integration)
    assert receipt["candidate_sha256"] == identity
    assert (integration / "Tablet/A.lean").read_text() == "-- A proof"
    assert (integration / "Tablet/B.lean").read_text() == "-- B accepted meanwhile"
    assert source_manifest(canonical) == before
    # A failed acceptance discards this private proposal, never rolls back B.
    shutil.rmtree(integration)
    assert source_manifest(canonical) == before


@pytest.mark.parametrize("changed,reason", [("A", "conflicts with accepted work"), ("C", "stale task input")])
def test_stale_rebase_never_partially_applies(tmp_path, changed, reason):
    canonical, worker, base = setup_candidate(tmp_path)
    put(worker, "Tablet/A.lean", "-- proof")
    candidate = tmp_path / "sealed"
    seal(worker, candidate, task_id=1, base_manifest=base,
         inputs={"Tablet/C.lean": base["Tablet/C.lean"]}, response={})
    put(canonical, f"Tablet/{changed}.lean", "-- newer")
    integration = tmp_path / "integration"
    shutil.copytree(canonical, integration)
    before = source_manifest(integration)
    with pytest.raises(CandidateError, match=reason):
        rebase(candidate, canonical, integration)
    assert source_manifest(integration) == before
    assert source_manifest(canonical) == before


def test_seal_replay_tampering_and_deletion(tmp_path):
    canonical, worker, base = setup_candidate(tmp_path)
    (worker / "Tablet/A.lean").unlink()
    put(worker, "Tablet/B.lean", "-- proof")
    candidate = tmp_path / "sealed"
    kwargs = dict(task_id=1, base_manifest=base, inputs={}, response={})
    first = seal(worker, candidate, **kwargs)
    assert seal(worker, candidate, **kwargs) == first
    put(worker, "Tablet/B.lean", "-- later edit cannot alter sealed evidence")
    assert read(candidate)[1] == first
    with pytest.raises(CandidateError, match="different sealed candidate"):
        seal(worker, candidate, **kwargs)
    integration = tmp_path / "integration"
    shutil.copytree(canonical, integration)
    rebase(candidate, canonical, integration)
    assert not (integration / "Tablet/A.lean").exists()
    assert (canonical / "Tablet/A.lean").exists()
    put(candidate, "files/Tablet/B.lean", "-- tampered")
    with pytest.raises(CandidateError, match="content mismatch"):
        read(candidate)


def test_helper_prefix_does_not_authorize_existing_node_or_shared_files(tmp_path):
    canonical, worker, base = setup_candidate(tmp_path)
    put(worker, "Tablet/A.lean", "-- forbidden")
    candidate = tmp_path / "sealed"
    seal(worker, candidate, task_id=1, base_manifest=base, inputs={}, response={})
    with pytest.raises(CandidateError, match="unreserved"):
        check_scope(read(candidate)[0], nodes={"B"}, helper_prefixes={"A"})
    put(worker, "reference/source.md", "-- shared source edit")
    second = tmp_path / "second"
    seal(worker, second, task_id=2, base_manifest=base, inputs={}, response={})
    with pytest.raises(CandidateError, match="reference/source.md"):
        check_scope(read(second)[0], nodes={"A"})


def test_symlink_sources_and_canonical_destination_are_refused(tmp_path):
    canonical, worker, base = setup_candidate(tmp_path)
    put(worker, "Tablet/A.lean", "-- proof")
    candidate = tmp_path / "sealed"
    seal(worker, candidate, task_id=1, base_manifest=base, inputs={}, response={})
    with pytest.raises(CandidateError, match="isolated"):
        rebase(candidate, canonical, canonical)
    (worker / "Tablet/A.lean").unlink()
    (worker / "Tablet/A.lean").symlink_to(canonical / "Tablet/A.lean")
    with pytest.raises(CandidateError, match="non-regular"):
        source_manifest(worker)


@pytest.mark.parametrize("edit,message", [
    ("statement", "theorem statement"), ("nl", "only the assigned theorem"),
    ("helper", "only the assigned theorem"), ("metadata", "cannot include"),
])
def test_weak_closure_task_cannot_decompose_or_restate(tmp_path, edit, message):
    canonical, worker, _ = setup_candidate(tmp_path)
    before = "theorem A : True :=\n-- BODY\nby sorry\n"
    put(canonical, "Tablet/A.lean", before)
    put(worker, "Tablet/A.lean", before.replace("by sorry", "by trivial"))
    raw = {"outcome": "valid"}
    if edit == "statement":
        put(worker, "Tablet/A.lean", "theorem A : 1 = 1 :=\n-- BODY\nby rfl\n")
    elif edit == "nl":
        put(worker, "Tablet/A.tex", "new decomposition")
    elif edit == "helper":
        put(worker, "Tablet/Helper.lean", "theorem Helper : True := by trivial")
    else:
        raw["semantic_dep_updates"] = {"A": ["B"]}
    candidate = tmp_path / "sealed"
    seal(worker, candidate, task_id=1, base_manifest=source_manifest(canonical), inputs={}, response=raw)
    with pytest.raises(CandidateError, match=message):
        check_lean_closure(candidate, canonical, "A")


def test_body_only_closure_of_a_sketch_is_allowed(tmp_path):
    canonical, worker, _ = setup_candidate(tmp_path)
    put(canonical, "Tablet/A.lean", "theorem A : True :=\n-- BODY\nby sorry\n")
    put(worker, "Tablet/A.lean", "theorem A : True :=\n-- BODY\nby trivial\n")
    candidate = tmp_path / "sealed"
    seal(worker, candidate, task_id=1, base_manifest=source_manifest(canonical), inputs={}, response={"outcome": "valid"})
    check_lean_closure(candidate, canonical, "A")


@pytest.mark.parametrize('preamble_option', [False, True])
def test_closure_heartbeat_option_scope(tmp_path, preamble_option):
    canonical, worker, _ = setup_candidate(tmp_path)
    prefix = '-- [TABLET NODE: A]\ntheorem A : True :=\n-- BODY\n'
    put(canonical, 'Tablet/A.lean', prefix + 'by sorry\n')
    proposed = ('set_option maxHeartbeats 400000\n' + prefix + 'by trivial\n'
                if preamble_option else prefix + 'set_option maxHeartbeats 400000 in by trivial\n')
    put(worker, 'Tablet/A.lean', proposed)
    sealed = tmp_path / 'sealed'
    seal(worker, sealed, task_id=1, base_manifest=source_manifest(canonical), inputs={},
         response={'outcome': 'valid'})
    if preamble_option:
        with pytest.raises(CandidateError, match='preamble options'):
            check_lean_closure(sealed, canonical, 'A')
    else:
        check_lean_closure(sealed, canonical, 'A')


@pytest.mark.parametrize('imports', [
    'import Tablet.Helper\n', 'import Mathlib.Data.Nat.Basic -- existing support\n',
    'import Mathlib Tablet.Helper\n', 'import Mathlib.Αλφα\n',
])
def test_closure_can_import_existing_support(tmp_path, imports):
    canonical, worker, _ = setup_candidate(tmp_path)
    prefix = 'import Tablet.Preamble\n-- [TABLET NODE: A]\ntheorem A : True :=\n-- BODY\n'
    put(canonical, 'Tablet/A.lean', prefix + 'by sorry\n')
    put(worker, 'Tablet/A.lean', imports + prefix + 'by trivial\n')
    sealed = tmp_path / 'sealed'
    seal(worker, sealed, task_id=1, base_manifest=source_manifest(canonical), inputs={}, response={'outcome': 'valid'})
    check_lean_closure(sealed, canonical, 'A')


@pytest.mark.parametrize('change', ['remove_import', 'namespace', 'options', 'statement', 'extra_command'])
def test_import_permission_does_not_expand_other_closure_authority(tmp_path, change):
    canonical, worker, _ = setup_candidate(tmp_path)
    prefix = 'import Tablet.Preamble\nset_option autoImplicit false\nnamespace N\n-- [TABLET NODE: A]\ntheorem A : True :=\n-- BODY\n'
    put(canonical, 'Tablet/A.lean', prefix + 'by sorry\n')
    proposed = {
        'remove_import': prefix.replace('import Tablet.Preamble\n', ''),
        'namespace': prefix.replace('namespace N', 'namespace M'),
        'options': prefix.replace('false', 'true'),
        'statement': prefix.replace('True', 'False'),
        'extra_command': 'import Mathlib; axiom hidden : False\n' + prefix,
    }[change]
    put(worker, 'Tablet/A.lean', 'import Tablet.Helper\n' + proposed + 'by trivial\n')
    sealed = tmp_path / 'sealed'
    seal(worker, sealed, task_id=1, base_manifest=source_manifest(canonical), inputs={}, response={'outcome': 'valid'})
    with pytest.raises(CandidateError, match='may add imports'):
        check_lean_closure(sealed, canonical, 'A')


def test_unfinished_weak_attempt_can_report_stuck_without_claiming_closure(tmp_path):
    canonical, worker, _ = setup_candidate(tmp_path)
    for repo in (canonical, worker):
        put(repo, "Tablet/A.lean", "theorem A : True :=\n-- BODY\nby sorry\n")
    candidate = tmp_path / "stuck"
    seal(worker, candidate, task_id=1, base_manifest=source_manifest(canonical), inputs={},
         response={"outcome": "stuck", "summary": "Could not finish this proof."})
    check_lean_closure(candidate, canonical, "A")
    invalid = tmp_path / "invalid"
    seal(worker, invalid, task_id=2, base_manifest=source_manifest(canonical), inputs={}, response={"outcome": "valid"})
    with pytest.raises(CandidateError, match="banned token: sorry"):
        check_lean_closure(invalid, canonical, "A")


@pytest.mark.parametrize('created_elsewhere', [False, True])
def test_exact_evidence_addition_requires_exclusive_scope_and_captured_absence(tmp_path, created_elsewhere):
    canonical, worker, base = setup_candidate(tmp_path)
    name = 'reference/model-evidence/new-probe.rs'
    put(worker, name, 'fn main() {}')
    sealed = tmp_path / 'sealed'
    seal(worker, sealed, task_id=1, base_manifest=base, inputs={name: 'absent'}, response={'outcome': 'valid'})
    record, _ = read(sealed)
    with pytest.raises(CandidateError, match='unreserved'):
        check_scope(record, nodes={'A'}, helper_prefixes={'new-'})
    check_scope(record, nodes={'A'}, shared_paths={name}, exclusive=True)
    with pytest.raises(CandidateError, match='unreserved'):
        check_scope(record, nodes={'A'}, shared_paths={'reference/model-evidence/other.rs'}, exclusive=True)
    if created_elsewhere:
        put(canonical, name, 'different accepted evidence')
    destination = tmp_path / 'private'; shutil.copytree(canonical, destination)
    before = source_manifest(canonical)
    if created_elsewhere:
        with pytest.raises(CandidateError, match='stale task input'):
            rebase(sealed, canonical, destination)
        assert source_manifest(destination) == before
    else:
        rebase(sealed, canonical, destination)
        assert (destination / name).read_text() == 'fn main() {}'
    assert source_manifest(canonical) == before
