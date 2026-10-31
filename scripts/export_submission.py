#!/usr/bin/env python3
"""Export a completed challenge run as a lean-eval submission repo.

Standalone post-completion operator tool (not coupled to the pipeline).
Takes the finished run repo, its challenge_targets.json, and the original
problem download (verified against the recorded provenance hashes) and
produces a directly submittable artifact:

- `Submission/` mirrored from `Tablet/*.lean` with `import Tablet.`
  rewritten to `import Submission.` and kernel-managed files dropped
  (the `Tablet.lean` root aggregator is outside `Tablet/`; `Axioms.lean`
  is excluded here).
- `Submission.lean` declaring each prescribed theorem under the
  `Submission` namespace with the spec's statement text, stated in the
  target's recorded `namespace_context`, closed `by exact` of the
  covering node's declaration (byte-equal defs make the two statements
  definitionally equal, which is what lets `exact` close the bridge).
- `lakefile.toml` copied from the problem download with `name` set to
  the problem id from provenance (CI identifies the problem by it).

Local validation replicates CI: overlay ONLY `Submission.lean` and
`Submission/**/*.lean` onto a pristine copy of the problem download and
run the comparator (`lake test`) there; any failure exits nonzero, so a
zero exit means push + open the pre-filled issue and nothing else.
`--skip-lake` keeps the overlay + structural checks but skips the lake
invocation. On success the pre-filled GitHub issue URL for
leanprover/lean-eval is printed; opening it stays a human action.

Usage:
    python3 scripts/export_submission.py <run_repo> <challenge_targets.json> \
        <problem_download_dir> --out <submission_dir> [--model-id <id>] \
        [--skip-lake] [--work-dir <overlay_dir>]
"""

from __future__ import annotations

import argparse
import hashlib
import json
import re
import shutil
import subprocess
import sys
import urllib.parse
from pathlib import Path

ISSUE_REPO = "leanprover/lean-eval"
SUBMISSION_REPO_URL_PLACEHOLDER = "<SUBMISSION_REPO_URL>"
MODEL_ID_PLACEHOLDER = "<MODEL_ID>"

# FILESPEC marker conventions (see kernel/src/filespec_split.rs).
TABLET_NODE_MARKER_PREFIX = "-- [TABLET NODE:"
BODY_MARKER_TRIMMED = "-- BODY"

# Kernel-managed file inside Tablet/ that must not ship in Submission/.
KERNEL_MANAGED_TABLET_FILES = {"Axioms.lean"}

IMPORT_TABLET_RE = re.compile(r"^(\s*)import\s+Tablet\.", re.MULTILINE)
NAMESPACE_RE = re.compile(r"^\s*namespace\s+([\w.']+)")
END_RE = re.compile(r"^\s*end\s+([\w.']+)")
LAKEFILE_NAME_RE = re.compile(r'^name\s*=\s*"[^"]*"', re.MULTILINE)


def die(message: str) -> None:
    print(f"error: {message}", file=sys.stderr)
    sys.exit(1)


def sha256_file(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as handle:
        for chunk in iter(lambda: handle.read(1 << 20), b""):
            digest.update(chunk)
    return digest.hexdigest()


def load_spec(path: Path) -> dict:
    try:
        spec = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError) as exc:
        die(f"cannot read challenge targets spec {path}: {exc}")
    if spec.get("schema_version") != 1:
        die(
            f"unsupported challenge_targets schema_version "
            f"{spec.get('schema_version')!r} in {path} (expected 1)"
        )
    for key in ("problem_id", "source_hashes", "targets"):
        if key not in spec:
            die(f"challenge targets spec {path} is missing {key!r}")
    return spec


def verify_provenance(spec: dict, download_dir: Path) -> None:
    """Every file in source_hashes must exist in the download with a
    matching SHA-256 — the download must be the exact problem the run
    was imported from."""
    source_hashes = spec["source_hashes"]
    if not source_hashes:
        die("challenge targets spec has an empty source_hashes map")
    for filename in sorted(source_hashes):
        expected = source_hashes[filename]
        candidate = download_dir / filename
        if not candidate.is_file():
            die(
                f"provenance: {filename} is recorded in source_hashes but "
                f"missing from {download_dir}"
            )
        actual = sha256_file(candidate)
        if actual != expected:
            die(
                f"provenance: SHA-256 mismatch for {filename}: spec records "
                f"{expected}, download has {actual}; this download is not "
                f"the problem the run was imported from"
            )


def rewrite_tablet_imports(text: str) -> str:
    return IMPORT_TABLET_RE.sub(r"\1import Submission.", text)


def export_submission_tree(run_repo: Path, out_dir: Path) -> list[Path]:
    """Mirror Tablet/*.lean into <out>/Submission/ with imports rewritten,
    dropping kernel-managed files. Returns the relative paths written."""
    tablet_dir = run_repo / "Tablet"
    if not tablet_dir.is_dir():
        die(f"run repo {run_repo} has no Tablet/ directory")
    submission_dir = out_dir / "Submission"
    if submission_dir.exists():
        shutil.rmtree(submission_dir)
    written: list[Path] = []
    for source in sorted(tablet_dir.rglob("*.lean"), key=str):
        rel = source.relative_to(tablet_dir)
        if str(rel) in KERNEL_MANAGED_TABLET_FILES:
            continue
        dest = submission_dir / rel
        dest.parent.mkdir(parents=True, exist_ok=True)
        dest.write_text(
            rewrite_tablet_imports(source.read_text(encoding="utf-8")),
            encoding="utf-8",
        )
        written.append(rel)
    if not written:
        die(f"no Tablet node files found under {tablet_dir}")
    return written


def covering_namespace_chain(node_text: str) -> list[str]:
    """Namespace components opened in the node file's preamble (above the
    `-- [TABLET NODE: ...]` marker). The prescribed declaration text is
    namespace-free by importer construction, so any namespace wrapper the
    node uses sits in the free preamble and determines the fully
    qualified name of the covering declaration."""
    chain: list[str] = []
    for line in node_text.splitlines():
        if line.lstrip().startswith(TABLET_NODE_MARKER_PREFIX):
            break
        match = NAMESPACE_RE.match(line)
        if match:
            chain.extend(match.group(1).split("."))
            continue
        match = END_RE.match(line)
        if match:
            parts = match.group(1).split(".")
            if chain[-len(parts):] == parts:
                del chain[-len(parts):]
    return chain


def strip_proof_boundary(lean_text: str, target_id: str) -> str:
    """Take the spec statement text up to its final `:=` / `:= by`
    boundary (the importer re-terminates statements per FILESPEC: final
    line ends `:=` or `by`)."""
    text = lean_text.rstrip()
    if text.endswith(":= by"):
        return text[: -len(":= by")].rstrip()
    if text.endswith(":="):
        return text[: -len(":=")].rstrip()
    if text.endswith("by"):
        base = text[: -len("by")].rstrip()
        if base.endswith(":="):
            base = base[: -len(":=")].rstrip()
        return base
    die(
        f"target {target_id}: prescribed statement does not end at a "
        f"`:=` or `by` proof boundary; cannot build the bridge declaration"
    )
    raise AssertionError("unreachable")


def build_submission_root(spec: dict, run_repo: Path, download_dir: Path) -> str:
    """Compose Submission.lean: one bridge theorem per prescribed theorem
    target, closed `by exact` of the covering node's declaration."""
    theorem_targets = [t for t in spec["targets"] if t.get("kind") == "theorem"]
    if not theorem_targets:
        die("challenge targets spec has no theorem targets; nothing to bridge")

    imports: list[str] = []
    if (download_dir / "ChallengeDeps.lean").is_file():
        imports.append("import ChallengeDeps")
    imports.extend(
        f"import Submission.{name}"
        for name in sorted({t["name"] for t in theorem_targets})
    )

    blocks: list[str] = []
    for target in theorem_targets:
        name = target["name"]
        node_path = run_repo / "Tablet" / f"{name}.lean"
        if not node_path.is_file():
            die(
                f"target {target['id']}: no covering Tablet node "
                f"(expected {node_path}; name parity is kernel-enforced)"
            )
        node_text = node_path.read_text(encoding="utf-8")
        if not any(
            line.lstrip().startswith(TABLET_NODE_MARKER_PREFIX)
            for line in node_text.splitlines()
        ):
            die(f"{node_path} has no `-- [TABLET NODE: ...]` marker line")
        if not any(
            line.strip() == BODY_MARKER_TRIMMED for line in node_text.splitlines()
        ):
            die(f"{node_path} has no `-- BODY` marker line")

        qualified = "_root_." + ".".join(
            covering_namespace_chain(node_text) + [name]
        )
        statement = strip_proof_boundary(target["lean"], target["id"])
        declaration = f"{statement} := by\n  exact {qualified}"
        context = target.get("namespace_context") or ""
        if context:
            declaration = f"open {context} in\n{declaration}"
        blocks.append(declaration)

    return (
        "\n".join(imports)
        + "\n\nnamespace Submission\n\n"
        + "\n\n".join(blocks)
        + "\n\nend Submission\n"
    )


def write_lakefile(download_dir: Path, out_dir: Path, problem_id: str) -> None:
    """Model the lakefile on the problem download's, with the package
    `name` (the first top-level name, which CI uses to identify the
    problem) replaced by the provenance problem id."""
    source = download_dir / "lakefile.toml"
    if source.is_file():
        text = source.read_text(encoding="utf-8")
        replaced, count = LAKEFILE_NAME_RE.subn(
            f'name = "{problem_id}"', text, count=1
        )
        if count == 0:
            replaced = f'name = "{problem_id}"\n' + text
    else:
        replaced = (
            f'name = "{problem_id}"\n'
            f'defaultTargets = ["Submission"]\n'
            f"\n"
            f"[[lean_lib]]\n"
            f'name = "Submission"\n'
        )
    (out_dir / "lakefile.toml").write_text(replaced, encoding="utf-8")


def replicate_ci_overlay(
    out_dir: Path,
    download_dir: Path,
    overlay_dir: Path,
    skip_lake: bool,
) -> None:
    """Replicate CI locally: copy the pristine download, overlay ONLY
    Submission.lean and Submission/**/*.lean, then run the comparator."""
    if overlay_dir.exists():
        shutil.rmtree(overlay_dir)
    overlay_dir.parent.mkdir(parents=True, exist_ok=True)
    shutil.copytree(download_dir, overlay_dir)

    overlay_rel = [Path("Submission.lean")]
    overlay_rel.extend(
        Path("Submission") / p.relative_to(out_dir / "Submission")
        for p in sorted((out_dir / "Submission").rglob("*.lean"), key=str)
    )
    for rel in overlay_rel:
        dest = overlay_dir / rel
        dest.parent.mkdir(parents=True, exist_ok=True)
        shutil.copyfile(out_dir / rel, dest)

    # Structural checks: the overlay files landed byte-identically and
    # the pristine download files were left untouched.
    for rel in overlay_rel:
        if (overlay_dir / rel).read_bytes() != (out_dir / rel).read_bytes():
            die(f"overlay: {rel} differs from the submission copy")
    for source in sorted(download_dir.rglob("*"), key=str):
        if not source.is_file():
            continue
        rel = source.relative_to(download_dir)
        copied = overlay_dir / rel
        if rel in overlay_rel:
            continue
        if not copied.is_file() or copied.read_bytes() != source.read_bytes():
            die(f"overlay: pristine download file {rel} was not preserved")

    if skip_lake:
        print(f"overlay validated structurally (lake skipped): {overlay_dir}")
        return
    result = subprocess.run(["lake", "test"], cwd=overlay_dir)
    if result.returncode != 0:
        die(
            f"comparator failed: `lake test` exited {result.returncode} "
            f"in {overlay_dir}"
        )
    print(f"comparator passed (`lake test`) in {overlay_dir}")


def prefilled_issue_url(problem_id: str, model_id: str) -> str:
    title = f"Submission: {problem_id}"
    body = (
        f"Problem: {problem_id}\n"
        f"Submission repo: {SUBMISSION_REPO_URL_PLACEHOLDER}\n"
        f"Model: {model_id}\n"
    )
    query = urllib.parse.urlencode(
        {"title": title, "body": body}, quote_via=urllib.parse.quote
    )
    return f"https://github.com/{ISSUE_REPO}/issues/new?{query}"


def main() -> None:
    parser = argparse.ArgumentParser(
        description="Export a completed challenge run as a lean-eval "
        "submission repo and validate it the way CI will."
    )
    parser.add_argument("run_repo", type=Path, help="finished run repo")
    parser.add_argument(
        "challenge_targets", type=Path, help="challenge_targets.json from the importer"
    )
    parser.add_argument(
        "problem_download_dir", type=Path, help="original problem download directory"
    )
    parser.add_argument(
        "--out", type=Path, required=True, help="submission repo output directory"
    )
    parser.add_argument(
        "--model-id",
        default=MODEL_ID_PLACEHOLDER,
        help="model identifier for the pre-filled issue",
    )
    parser.add_argument(
        "--skip-lake",
        action="store_true",
        help="skip the `lake test` comparator run (overlay + structural checks still run)",
    )
    parser.add_argument(
        "--work-dir",
        type=Path,
        default=None,
        help="overlay workspace directory (default: <out>-overlay beside --out)",
    )
    args = parser.parse_args()

    if not args.run_repo.is_dir():
        die(f"run repo {args.run_repo} is not a directory")
    if not args.problem_download_dir.is_dir():
        die(f"problem download {args.problem_download_dir} is not a directory")

    spec = load_spec(args.challenge_targets)
    problem_id = spec["problem_id"]
    verify_provenance(spec, args.problem_download_dir)

    args.out.mkdir(parents=True, exist_ok=True)
    export_submission_tree(args.run_repo, args.out)
    root_text = build_submission_root(spec, args.run_repo, args.problem_download_dir)
    (args.out / "Submission.lean").write_text(root_text, encoding="utf-8")
    write_lakefile(args.problem_download_dir, args.out, problem_id)
    print(f"submission repo written: {args.out}")

    overlay_dir = (
        args.work_dir
        if args.work_dir is not None
        else args.out.parent / f"{args.out.name}-overlay"
    )
    replicate_ci_overlay(
        args.out, args.problem_download_dir, overlay_dir, args.skip_lake
    )

    print("push the submission repo, then open the pre-filled issue:")
    print(prefilled_issue_url(problem_id, args.model_id))


if __name__ == "__main__":
    main()
