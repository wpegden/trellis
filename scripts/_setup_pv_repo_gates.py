#!/usr/bin/env python3
"""Slice-2 acceptance gates for setup_pv_repo.sh. Invoked by the script with
--gate; not a standalone tool. All gates exercise the REAL kernel via the
runtime CLI (TRELLIS_TRELLIS_KERNEL_CMD points at the isolated binary).

Gates:
  (a) node_role[*] == ExtractionModel seeded.
  (b) configured_challenge_targets carry the Def pins with NON-EMPTY
      source_sha256 + extractor_toolchain_sha256.
  (c) approved_extraction_provenance baseline seeded.
  (d) the all-math setup_repo.sh output is byte-identical (the new script is
      additive/separate). Proven structurally: setup_repo.sh is unmodified on
      this branch (git diff vs HEAD is empty), so every artifact it emits is
      byte-identical to HEAD.
  (e) a kernel-CLI accept of a worker payload that EDITS a Model node is
      REJECTED by the byte-pin (challenge_conformance_errors).
  (f) an empty source_sha256 is rejected by extraction_chain_errors.
"""
from __future__ import annotations

import copy
import json
import subprocess
import sys
from pathlib import Path

from trellis.runtime.kernel_cli import run_kernel_cli


def fail(msg: str) -> None:
    print(f"GATE FAILED: {msg}", file=sys.stderr)
    raise SystemExit(1)


def load_state(runtime_root: Path) -> dict:
    res = run_kernel_cli({"action": "show", "root": str(runtime_root)})
    if res.get("status") != "ok":
        fail(f"`show` did not return ok: {json.dumps(res)[:400]}")
    return res["state"]


def main() -> int:
    runtime_root = Path(sys.argv[1]).resolve()
    config_path = Path(sys.argv[2]).resolve()
    repo = Path(sys.argv[3]).resolve()

    state = load_state(runtime_root)
    roles = state.get("node_role", {})
    targets = state.get("configured_challenge_targets", {})
    approved = state.get("approved_extraction_provenance", {})

    # ── (a) node_role seeded ExtractionModel ──
    model_nodes = [n for n, r in roles.items() if r == "extraction_model"]
    if not model_nodes:
        fail("(a) no node carries node_role=extraction_model")
    print(f"GATE (a) PASS: {len(model_nodes)} node(s) seeded node_role=ExtractionModel "
          f"({', '.join(model_nodes[:6])}{' ...' if len(model_nodes) > 6 else ''})")

    # ── (b) challenge targets with non-empty source + toolchain sha ──
    # Scope: EXTRACTION-MODEL targets only. The crate-extracted model `def`s
    # carry extraction provenance (source_sha256 + extractor_toolchain_sha256)
    # and that provenance is their trust anchor. Worker-authored goal targets
    # are not extracted and therefore must be excluded from this gate.
    if not targets:
        fail("(b) configured_challenge_targets is empty")
    # Extraction-model target ids carry the `model:` prefix (seeded from the
    # crate-extracted defs); the tolerant fallback also matches by the target's
    # `name`/`node` against the extraction-model node set (mirrors gate (e)).
    model_node_set = {n for n, r in roles.items() if r == "extraction_model"}

    def is_model_target(tid, spec):
        if tid.startswith("model:"):
            return True
        # A prose `goal:<fn>` pair (and its `__refutation` twin) is an
        # AUTHORED goal target, never an extracted def, whatever it is
        # named. The tolerant fallback below would otherwise capture it:
        # a goal target is named after the crate function it verifies, and
        # that function is itself an extraction-model node. At launch a
        # prose goal statement does not exist yet, so it is a `theorem`
        # skeleton with no source_sha256 — and the provenance check below
        # would fail on every prose campaign.
        if tid.startswith("goal:"):
            return False
        return (spec.get("name") in model_node_set
                or spec.get("node") in model_node_set
                or tid.split(":", 1)[-1] in model_node_set)

    model_targets = {tid: spec for tid, spec in targets.items()
                     if is_model_target(tid, spec)}
    if not model_targets:
        fail("(b) no extraction-model challenge targets to check provenance on")
    bad = []
    for tid, spec in model_targets.items():
        if spec.get("kind") != "def":
            bad.append(f"{tid}: kind={spec.get('kind')} (expected def)")
        prov = spec.get("provenance", {})
        if not str(prov.get("source_sha256", "")).strip():
            bad.append(f"{tid}: empty source_sha256")
        if not str(prov.get("extractor_toolchain_sha256", "")).strip():
            bad.append(f"{tid}: empty extractor_toolchain_sha256")
    if bad:
        fail("(b) challenge-target provenance incomplete:\n  " + "\n  ".join(bad))
    print(f"GATE (b) PASS: {len(model_targets)} extraction-model Def challenge "
          f"target(s) carry non-empty source_sha256 + extractor_toolchain_sha256 "
          f"({len(targets) - len(model_targets)} operator-supplied byte-pinned "
          f"contract target(s) excluded)")

    # ── (c) approved_extraction_provenance baseline ──
    # The baseline is the EXTRACTION provenance snapshot — one entry per
    # extraction-model target. Mode-B operator-supplied contract targets are
    # byte-pinned, not extracted, so they correctly have no baseline entry;
    # compare against the extraction-model targets, not all challenge targets.
    if not approved:
        fail("(c) approved_extraction_provenance baseline not seeded")
    if set(approved.keys()) != set(model_targets.keys()):
        fail(f"(c) baseline keys {sorted(approved)} != extraction-model target keys "
             f"{sorted(model_targets)}")
    print(f"GATE (c) PASS: approved_extraction_provenance baseline seeded for "
          f"{len(approved)} extraction-model target(s) (matches the extraction-model "
          f"challenge targets; {len(targets) - len(model_targets)} byte-pinned "
          f"contract target(s) have no extraction baseline)")

    # ── (d) all-math setup_repo.sh byte-identical (additive proof) ──
    source_root = Path(__file__).resolve().parents[1]
    diff = subprocess.run(
        ["git", "-C", str(source_root), "diff", "--stat", "HEAD", "--", "scripts/setup_repo.sh"],
        capture_output=True, text=True,
    )
    if diff.stdout.strip():
        fail(f"(d) scripts/setup_repo.sh was MODIFIED on this branch:\n{diff.stdout}")
    print("GATE (d) PASS: scripts/setup_repo.sh is byte-unchanged vs HEAD (the PV "
          "setup is a separate, additive script) -> all-math setup output unaffected")

    # ── (e) byte-pin rejects a Model edit ──
    # Pick one model node + its target; hand-edit the on-disk Tablet file below
    # `-- BODY`, then run the REAL acceptance (normalize_worker) claiming it.
    model_node = model_nodes[0]
    target_id = next(t for t, s in targets.items()
                     if s.get("name") == model_node or s.get("node") == model_node
                     or t.split(":", 1)[-1] == model_node)
    node_file = repo / "Tablet" / f"{model_node}.lean"
    original = node_file.read_text(encoding="utf-8")
    # Inject a benign edit AFTER the `-- BODY` marker (the byte-pinned region).
    lines = original.split("\n")
    bidx = next(i for i, ln in enumerate(lines) if ln.strip() == "-- BODY")
    edited = lines[: bidx + 1] + ["-- worker tampered here"] + lines[bidx + 1 :]
    node_file.write_text("\n".join(edited), encoding="utf-8")
    try:
        out = _normalize_claim(repo, targets, model_node, target_id, mark_model=True)
        errs = "\n".join(out.get("output", {}).get("contract_errors", []))
        if "challenge byte-conformance rule failed" not in errs:
            fail(f"(e) byte-pin did NOT reject a hand-edited Model node. errors:\n{errs}")
        print("GATE (e) PASS: editing a Model node is REJECTED by the byte-pin "
              "(challenge_conformance_errors)")
    finally:
        node_file.write_text(original, encoding="utf-8")

    # ── (f) empty source_sha256 rejected by extraction_chain_errors ──
    targets_f = copy.deepcopy(targets)
    targets_f[target_id]["provenance"]["source_sha256"] = ""
    out = _normalize_claim(repo, targets_f, model_node, target_id, mark_model=True)
    errs = "\n".join(out.get("output", {}).get("contract_errors", []))
    if "extraction-chain rule failed" not in errs or "empty `source_sha256`" not in errs:
        fail(f"(f) empty source_sha256 NOT rejected by extraction_chain_errors. errors:\n{errs}")
    print("GATE (f) PASS: an empty source_sha256 is REJECTED by extraction_chain_errors")

    # ── (g) every pinned namespace_context round-trips (model AND goal) ──
    # The kernel byte-checks each claiming node's extracted namespace context
    # against the target's pinned `namespace_context` at acceptance
    # (challenge_conformance_errors), normalising BOTH sides to the
    # `namespace …`/`end …` scope lines (kernel
    # `filespec_split::namespace_context{,_from_preamble}`: opens free, namespace
    # pinned). The extract_pv_model.py (c2) gate round-trips this for the
    # extraction MODELS only. This gate runs the SAME normalization the kernel
    # uses over every challenge target with a non-empty normalized pin and an
    # on-disk node file, so a structurally-uncoverable pin fails setup
    # instead of halting the run. Gate-pass <=> kernel-accepts.
    ctx_mismatches = []
    checked = 0
    for tid, spec in targets.items():
        pinned_norm = _namespace_only(str(spec.get("namespace_context", "")))
        if not pinned_norm:
            # Empty / open-only pin -> the kernel block is inert (math + any
            # un-namespaced node); nothing to round-trip.
            continue
        node = spec.get("node") or spec.get("name")
        if not node:
            continue
        node_file = repo / "Tablet" / f"{node}.lean"
        if not node_file.exists():
            # No on-disk node yet (the worker authors it later); the runtime
            # acceptance is where it is checked. Setup can only round-trip the
            # nodes that already exist, which is exactly the failure mode here.
            continue
        actual = _file_namespace_context(node_file.read_text(encoding="utf-8"))
        checked += 1
        if actual != pinned_norm:
            ctx_mismatches.append(
                f"{tid} (node `{node}`): pinned namespace_context is "
                f"structurally uncoverable\n"
                f"    pinned(normalised)={pinned_norm!r} "
                f"file-context={actual!r}"
            )
    if ctx_mismatches:
        fail("(g) goal/model namespace_context does not round-trip "
             "(uncoverable at runtime):\n  " + "\n  ".join(ctx_mismatches))
    print(f"GATE (g) PASS: all {checked} non-empty pinned namespace_context value(s) "
          "round-trip namespace-line-to-namespace-line against their on-disk node "
          "(matches the kernel's opens-free/namespace-pinned acceptance)")

    print("\nAll Slice-2 gates PASS.")
    return 0


def _namespace_only(preamble: str) -> str:
    """Keep only `namespace …`/`end …` scope lines from a preamble string,
    joined by `\\n`, trimmed. Marker-free — mirrors the kernel
    `filespec_split::namespace_context_from_preamble`, the normalization applied
    to the PINNED side of the byte-compare so a full-preamble pin (with `open`
    lines) reduces to the same namespace-only value a node file extracts.
    """
    ctx = [
        ln.rstrip("\r")
        for ln in preamble.split("\n")
        if ln.lstrip().startswith("namespace ")
        or ln.strip() == "namespace"
        or ln.lstrip().startswith("end ")
        or ln.strip() == "end"
    ]
    return "\n".join(ctx).strip()


def _file_namespace_context(content: str) -> str:
    """The namespace context of a node FILE: the `namespace …`/`end …` lines in
    the PREAMBLE (above the `-- [TABLET NODE: …]` marker). Mirrors the kernel
    `filespec_split::namespace_context` (marker-aware preamble scan), then the
    same scope-line filter as `_namespace_only`.
    """
    lines = content.split("\n")
    marker_idx = next(
        (i for i, ln in enumerate(lines)
         if ln.strip().startswith("-- [TABLET NODE:") and ln.strip().endswith("]")),
        len(lines),
    )
    return _namespace_only("\n".join(lines[:marker_idx]))


def _normalize_claim(repo: Path, targets: dict, node: str, target_id: str, *, mark_model: bool) -> dict:
    """Run the REAL acceptance normalization for a single node claiming a target.

    Mirrors the worker_normalization.rs challenge_input/extraction_input test
    fixtures: the node is present and claims `target_id`; `extraction_model_nodes`
    is the node when mark_model. `current_present_nodes` includes Preamble (the
    repo always has it) so the structural snapshot derives cleanly.
    """
    payload = {
        "action": "normalize_worker",
        "input": {
            "repo_path": str(repo),
            "configured_targets": [],
            "configured_challenge_targets": targets,
            "current_present_nodes": ["Preamble"],
            "current_proof_nodes": [],
            "current_node_kinds": {},
            "current_deps": {},
            "current_target_claims": {},
            "current_challenge_claims": {},
            "extraction_model_nodes": ([node] if mark_model else []),
            "approved_paper_fingerprints": {},
            "target_claim_updates": {node: []},
            "challenge_claim_updates": {node: [target_id]},
            "target_fingerprints": {},
            "sound_current_fingerprints": {},
        },
    }
    res = run_kernel_cli(payload)
    if res.get("status") != "normalize_worker_ok":
        fail(f"normalize_worker did not return ok: {json.dumps(res)[:600]}")
    return res


if __name__ == "__main__":
    raise SystemExit(main())
