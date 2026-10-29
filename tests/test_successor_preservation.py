import hashlib
import json
import shutil
from pathlib import Path

import pytest

from scripts.extract_pv_model import split_generated_lean
from trellis.modeling.preservation import plan_preservation, preserve_successor_dag
from trellis.pv_model_support import TYPE_LAYOUT, compose_model_support, empty_model_support
from trellis.pv_revision.store import source_manifest


def write(path, value):
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(json.dumps(value))


@pytest.fixture
def successor(tmp_path):
    old, repo, revision = (tmp_path / n for n in ("old", "new", "revision"))
    raw = ("import Aeneas\nnamespace demo\n/-- [demo::Carrier] -/\nstructure Carrier where\n  value : Nat\n"
           "/-- [demo::stable] -/\ndef stable : Nat := 1\n"
           "/-- [demo::changed] -/\ndef changed : Nat := 2\nend demo\n")
    for root, text in [(old, raw), (repo, raw.replace("changed : Nat := 2", "changed : Nat := 3"))]:
        root.mkdir(); (root / "Tablet").mkdir()
        (root / "generated.lean").write_text(text)
        composition = compose_model_support(text, empty_model_support(), repo=root, expose_types=True)
        (root / "Tablet/Preamble.lean").write_text(composition["preamble"])
        (root / "Tablet/Preamble.tex").write_text("")
        split = split_generated_lean(text)
        models = []
        for node in split.nodes:
            (root / "Tablet" / f"{node.name}.lean").write_text(node.text)
            (root / "Tablet" / f"{node.name}.tex").write_text("model statement")
            models.append({"node": node.name, "lean": node.text, "namespace_context": []})
        # Extracted types intentionally have no paired NL source.
        (root / "Tablet/Carrier.lean").write_text("structure Carrier where\n value : Nat\n")
        models.append({"node": "Carrier", "lean": "structure Carrier where\n value : Nat\n", "namespace_context": []})
        write(root / "trellis.config.json", {"pv_tablet": {"extraction_models": models}})
        write(root / "Tablet/model-support.json", empty_model_support())
        write(root / "modeling-input/manifest.json", {"model_support_layout": TYPE_LAYOUT})
    graph = {"StableProof": ["stable"], "AffectedProof": ["changed"],
             "AffectedConsumer": ["AffectedProof"], "TypeProof": ["Carrier"],
             "ReviewHelper": ["stable"], "AssumptionProof": ["stable"]}
    for node, deps in graph.items():
        (old / "Tablet" / f"{node}.lean").write_text(
            "".join(f"import Tablet.{d}\n" for d in deps) + f"theorem {node} : True := by trivial\n")
        (old / "Tablet" / f"{node}.tex").write_text("paired statement")
    nodes = {p.stem for p in (old / "Tablet").glob("*.lean")}
    state = {"live": {"present_nodes": sorted(nodes)}, "deps": graph,
             "node_kinds": {n: "Proof" if n in graph else "Definition" for n in nodes},
             "target_claims": {}, "local_closure_records": {n: {"kernel_axioms": []} for n in nodes}}
    state["local_closure_records"]["AssumptionProof"]["kernel_axioms"] = ["old_environment_axiom"]
    frozen = revision / "predecessor"
    shutil.copytree(old / "Tablet", frozen / "Tablet")
    write(frozen / "protocol_state.json", state)
    write(revision / "reviews/abc/review.json", {"digest": "abc", "audit_invalidation": [
        {"node": "ReviewHelper", "lanes": ["soundness"], "reason": "Recheck interpretation"}],
        "original_collection": {"goals": []}, "proposed_collection": {"goals": []}})
    write(repo / "modeling-input/revision-from.json", {"schema": "trellis-pv-revision-successor/v1",
        "package_digest": "abc", "revision_root": str(revision), "predecessor": {
            "repo": str(old), "protocol_state_sha256": hashlib.sha256((frozen / "protocol_state.json").read_bytes()).hexdigest(),
            "tablet_manifest": source_manifest(frozen / "Tablet")}})
    return repo, frozen


def test_automatic_source_selection_keeps_independent_proofs_and_unpaired_types(successor):
    repo, _ = successor
    plan = plan_preservation(repo)
    assert set(plan["authored_nodes"]) == {"StableProof", "TypeProof", "ReviewHelper", "AffectedProof", "AffectedConsumer"}
    assert plan["authoring_requires_revalidation"]["changed"]["reason"].startswith("extracted declaration changed")
    assert plan["excluded"]["AssumptionProof"]["reason"] == "requires predecessor assumptions"
    assert "Tablet/Carrier.tex" not in plan["files"]
    # A verifier finding invalidates evidence, not the unchanged authoring.
    assert "ReviewHelper" in plan["authored_nodes"]


def test_changed_frozen_evidence_rejected_before_any_copy(successor):
    repo, frozen = successor
    (frozen / "Tablet/StableProof.lean").write_text("theorem forged : False := by sorry")
    with pytest.raises(ValueError, match="Tablet differs"):
        preserve_successor_dag(repo)
    assert not (repo / "Tablet/StableProof.lean").exists()


def test_source_reuse_does_not_require_an_old_certificate(successor):
    repo, frozen = successor
    path = frozen / "protocol_state.json"
    state = json.loads(path.read_text())
    state["local_closure_records"].pop("StableProof")
    state["local_closure_records"]["TypeProof"]["kernel_axioms"] = ["sorryAx"]
    write(path, state)
    marker = repo / "modeling-input/revision-from.json"
    receipt = json.loads(marker.read_text())
    receipt["predecessor"]["protocol_state_sha256"] = hashlib.sha256(path.read_bytes()).hexdigest()
    write(marker, receipt)
    assert {"StableProof", "TypeProof"} <= set(plan_preservation(repo)["authored_nodes"])


def test_existing_successor_work_is_not_overwritten(successor):
    repo, _ = successor
    (repo / "Tablet/StableProof.lean").write_text("new authored work")
    with pytest.raises(ValueError, match="overwrite successor work"):
        preserve_successor_dag(repo)
    assert not (repo / "Tablet/TypeProof.lean").exists()


def test_first_generation_has_no_preservation(tmp_path):
    assert preserve_successor_dag(tmp_path) is None


def revise_frozen_state(repo, frozen, update):
    path = frozen / "protocol_state.json"
    state = json.loads(path.read_text())
    update(state)
    write(path, state)
    marker = repo / "modeling-input/revision-from.json"
    receipt = json.loads(marker.read_text())
    receipt["predecessor"]["protocol_state_sha256"] = hashlib.sha256(path.read_bytes()).hexdigest()
    write(marker, receipt)


def test_audit_recheck_keeps_unchanged_principal_authoring(successor):
    repo, frozen = successor
    revise_frozen_state(repo, frozen, lambda s: s["target_claims"].update(ReviewHelper=["same"]))
    review = frozen.parent / "reviews/abc/review.json"
    package = json.loads(review.read_text())
    package.update(original_collection={"context": "shared", "goals": [{"id": "same", "text": "same statement"}]},
                   proposed_collection={"context": "shared", "goals": [{"id": "same", "text": "same statement"}]},
                   audit_invalidation=[{"node": "same", "lanes": ["correspondence"], "reason": "Changed source"}],
                   plan={"dispositions": [{"goal_id": "same", "remedy": "repair"}]})
    write(review, package)
    plan = plan_preservation(repo)
    assert "ReviewHelper" in plan["authored_nodes"]
    assert plan["authoring_requires_revalidation"]["ReviewHelper"]["goals"] == ["same"]
    # Requirement replacement changes authoring eligibility, not just a verdict.
    package["proposed_collection"]["goals"][0]["text"] = "new statement"
    write(review, package)
    assert "ReviewHelper" not in plan_preservation(repo)["authored_nodes"]


def test_negative_carrier_is_historical_even_for_same_goal_text(successor):
    repo, frozen = successor
    def negative(s):
        s["target_claims"]["StableProof"] = ["same"]
        s["authored_decide_goals"] = {"primary": "same"}
        s["configured_challenge_targets"] = {"primary": {"name": "Positive"}}
        s["pv_live_polarity"] = {"primary": "disprove"}
    revise_frozen_state(repo, frozen, negative)
    plan = plan_preservation(repo)
    assert "StableProof" not in plan["authored_nodes"]
    assert plan["excluded"]["StableProof"]["reason"].startswith("predecessor negative carrier")


def test_removed_dependency_still_excludes_authored_consumers(successor):
    repo, _ = successor
    config = json.loads((repo / "trellis.config.json").read_text())
    config["pv_tablet"]["extraction_models"] = [m for m in config["pv_tablet"]["extraction_models"] if m["node"] != "changed"]
    write(repo / "trellis.config.json", config)
    plan = plan_preservation(repo)
    assert {"AffectedProof", "AffectedConsumer"}.isdisjoint(plan["authored_nodes"])
    assert plan["excluded"]["changed"]["reason"] == "extracted declaration removed"
