"""Checkpoint prep must not resume abandoned authoring conversations."""
import ast
import re
from pathlib import Path


def test_rewind_starts_authoring_with_fresh_context():
    script = (Path(__file__).resolve().parents[1] / "scripts/prepare_migrated_resume.sh").read_text()
    # Exercise the real embedded helper without running the destructive prep.
    match = re.search(
        r'python3 - "\$ROOT_DIR" "\$REPO_PATH" "\$RUNTIME_ROOT"[^\n]* <<\'PY\'\n(.*?)\nPY',
        script,
        re.S,
    )
    assert match is not None
    source = match.group(1)
    tree = ast.parse(source)
    helpers = ast.Module(body=[node for node in tree.body if isinstance(node, ast.FunctionDef)
        and node.name in {"normalize_label", "force_fresh_authoring_boundary"}], type_ignores=[])
    namespace = {}
    exec(compile(helpers, "prepare_migrated_resume.sh", "exec"), namespace)
    fresh = namespace["force_fresh_authoring_boundary"]
    for phase, key in [("Modeling", "modeling"), ("TheoremStating", "theorem_stating"), ("ProofFormalization", "proof_formalization")]:
        state = {"phase": phase, "pending_task": {"next_worker_context_mode": "resume"}}
        metadata = {"native_history_kinds": [f"worker:{key}", f"review:{key}", "paper"]}
        result = fresh(state, metadata)
        assert result["forced"]
        assert result["previous_pending_task_context"] == "resume"
        assert metadata["native_history_kinds"] == ["paper"]
        assert state["pending_task"]["next_worker_context_mode"] == "fresh"
    state = {"phase": "Cleanup"}
    metadata = {"native_history_kinds": ["worker:cleanup"]}
    assert fresh(state, metadata) == {"forced": False}
    assert metadata["native_history_kinds"] == ["worker:cleanup"]
