"""Drift guard: `--model` / `--effort` must reach every lane a shipped
template dispatches.

The create wizard rewrites agent specs by walking a hand-listed tuple of
config roots (`AGENT_ROOTS` in scripts/trellis_create_run.sh). A hand list
is exactly what failed before: `verification` was missing from it, so a run
started with `--model gpt-5.6-sol` dispatched 107 of 141 reviewer-role
bursts on gpt-5.5 (58 correspondence, 28 soundness, 21 substantiveness) —
invisible in a role tally, because every verifier pool logs as role
"reviewer".

So this test holds the list against the templates rather than against a
second hand list: every model-bearing spec in a shipped template must sit
under some `AGENT_ROOTS` entry. Add a differentiated lane to a template
(`blockered_worker`, `easy_close_worker`, a `worker_rules` binding, a
`workflow.phase_overrides` entry) and this fails, naming the path — which
is the signal to extend the tuple.

Deliberately a STATIC check, not a wizard-time gate. Run creation from the
browser is a rarely-exercised cold path; a runtime guard that hard-fails
there would trade a silent-wrong-model bug for a can't-start-a-run bug, and
a false positive would surface only when someone was trying to launch. The
staleness is caught here, in the suite, where a false positive costs a test
run.
"""

from __future__ import annotations

import ast
import json
import re
from pathlib import Path

import pytest

REPO_ROOT = Path(__file__).resolve().parents[1]
CREATE_SCRIPT = REPO_ROOT / "scripts" / "trellis_create_run.sh"
TEMPLATES = (
    REPO_ROOT / "examples" / "trellis.config.json",
    REPO_ROOT / "examples" / "trellis.isabelle.config.json",
)


def agent_roots() -> tuple[str, ...]:
    """The wizard's override roots, read from the script that owns them."""
    text = CREATE_SCRIPT.read_text(encoding="utf-8")
    match = re.search(r"^AGENT_ROOTS = (\([^)]*\))", text, re.MULTILINE)
    assert match, "AGENT_ROOTS tuple not found in scripts/trellis_create_run.sh"
    roots = ast.literal_eval(match.group(1))
    assert isinstance(roots, tuple) and roots, f"unexpected AGENT_ROOTS: {roots!r}"
    return roots


def model_specs(obj, path: str = ""):
    """Every (dotted-path, spec) that names a model the way an agent does.

    Mirrors the wizard's own predicate — a dict carrying both `model` and
    `provider` — so this test and the walker agree on what an agent spec is.
    """
    if isinstance(obj, dict):
        if "model" in obj and "provider" in obj:
            yield (path or "(root)", obj)
        for key, value in obj.items():
            child = f"{path}.{key}" if path else key
            yield from model_specs(value, child)
    elif isinstance(obj, list):
        for i, value in enumerate(obj):
            yield from model_specs(value, f"{path}[{i}]")


@pytest.mark.parametrize("template", TEMPLATES, ids=lambda p: p.name)
def test_every_model_spec_sits_under_an_agent_root(template: Path) -> None:
    config = json.loads(template.read_text(encoding="utf-8"))
    roots = agent_roots()
    uncovered = [
        path
        for path, _spec in model_specs(config)
        if path.split(".")[0].split("[")[0] not in roots
    ]
    assert not uncovered, (
        f"{template.name} carries model-bearing specs the create wizard's "
        f"--model/--effort override cannot reach: {uncovered}. A run started "
        f"with an override would silently dispatch these on the template's "
        f"model. Add the owning root to AGENT_ROOTS in "
        f"scripts/trellis_create_run.sh (or drop the spec from the template)."
    )


@pytest.mark.parametrize("template", TEMPLATES, ids=lambda p: p.name)
def test_templates_carry_no_inert_verification_scalars(template: Path) -> None:
    """`verification.provider`/`.model` are not a lane and must not look like one.

    The kernel's verification config holds the three agent pools and
    nothing else, so these scalars never reached dispatch — but they read
    as authoritative sitting beside the pools that do, and a stale value
    there is exactly the kind of thing a reviewer's eye slides past. The
    parser still ACCEPTS them (configs on disk in live runs carry them;
    rejecting would break those runs on resume) — they just must not ship
    in a template.
    """
    verification = json.loads(template.read_text(encoding="utf-8")).get("verification", {})
    inert = [
        key
        for key in ("provider", "model", "thinking_budget", "max_context_tokens")
        if key in verification
    ]
    assert not inert, (
        f"{template.name} verification block carries inert scalars {inert}. "
        f"No dispatch path reads them (kernel: RuntimeBridgeVerificationConfig "
        f"has only the *_agents pools). Put per-lane selection in the pools."
    )


@pytest.mark.parametrize("template", TEMPLATES, ids=lambda p: p.name)
def test_verification_pools_are_the_dispatching_surface(template: Path) -> None:
    """The pools the kernel actually binds from are present and complete.

    `substantiveness_agents` is the one that hid: absent from Python's
    parser entirely while the kernel dispatched 21 bursts a run from it.
    Pin all three so a template can't ship a pool the tooling won't see.
    """
    verification = json.loads(template.read_text(encoding="utf-8")).get("verification", {})
    for pool in ("correspondence_agents", "soundness_agents", "substantiveness_agents"):
        agents = verification.get(pool)
        assert isinstance(agents, list) and agents, (
            f"{template.name}: verification.{pool} must be a non-empty list; "
            f"got {agents!r}"
        )
        for agent in agents:
            assert agent.get("provider") and agent.get("model"), (
                f"{template.name}: every {pool} entry needs provider+model, "
                f"or the wizard's override predicate skips it: {agent!r}"
            )


def test_sidecar_model_is_deliberately_outside_the_override() -> None:
    """The grunt model is excluded from `--model` ON PURPOSE. Pin that.

    Grunts are a high-volume cheap lane (gpt-5.6-luna is ~25x cheaper than
    the sol lanes); pointing them at the worker model would multiply the
    run's cost for no gain. Two independent things keep them out: no
    shipped template has a `sidecar.model` block at all, and the sidecar
    names its model `sidecar.model.name`, not `.model`, so even the
    wizard's recursive walk would not match it.

    This test exists because that exclusion was undocumented and therefore
    indistinguishable from the oversight that put `verification` outside
    the walker. If you intend to make `--model` cover grunts, delete this
    test in the same commit — don't let it fail silently into a fix.
    """
    for template in TEMPLATES:
        config = json.loads(template.read_text(encoding="utf-8"))
        sidecar_model = (config.get("sidecar") or {}).get("model")
        assert sidecar_model is None, (
            f"{template.name} now ships a sidecar.model block ({sidecar_model!r}). "
            f"Decide explicitly whether --model should rewrite the grunt model, "
            f"then update this test to match the decision."
        )
    assert "sidecar" not in agent_roots(), (
        "sidecar was added to AGENT_ROOTS. If that is intended, note that the "
        "grunt model lives at sidecar.model.name and the walker's predicate "
        "(model+provider) will not match it — the override would silently no-op."
    )
