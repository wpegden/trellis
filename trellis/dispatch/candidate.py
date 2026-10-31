"""Immutable source candidates and conflict-checked rebasing.

These operations confer no acceptance. The rebased private repository still
goes through ordinary authoritative checking and serialized publication.
Canonical files are never modified here, including on failed/stale attempts.
"""
from __future__ import annotations

import hashlib
import json
import os
from pathlib import Path, PurePosixPath
import shutil
import stat
import tempfile


class CandidateError(RuntimeError):
    pass


def digest(data):
    return hashlib.sha256(data).hexdigest()


def canonical_json(value):
    return json.dumps(value, sort_keys=True, separators=(",", ":"), ensure_ascii=True).encode()


def safe_relative(value):
    if not isinstance(value, str) or not value or "\\" in value or "\0" in value:
        raise CandidateError("candidate path must be a nonempty relative POSIX path")
    p = PurePosixPath(value)
    if p.is_absolute() or any(part in {".", "..", ".git", ".trellis", ".lake"} for part in p.parts) or str(p) != value:
        raise CandidateError(f"unsafe candidate source path: {value!r}")
    return p


def regular_bytes(root, relative):
    p = Path(root)
    for part in safe_relative(relative).parts:
        p = p / part
        try:
            mode = p.lstat().st_mode
        except FileNotFoundError:
            return None
        if stat.S_ISLNK(mode):
            raise CandidateError(f"candidate source cannot traverse a symlink: {relative}")
    if not stat.S_ISREG(mode):
        raise CandidateError(f"candidate source is not a regular file: {relative}")
    return p.read_bytes()


def source_manifest(repo, surfaces=("Tablet", "reference")):
    """Same regular-file surfaces as ordinary worker rollback, including deletions."""
    result = {}
    repo = Path(repo)
    for surface in surfaces:
        safe_relative(surface)
        root = repo / surface
        if not root.exists() and not root.is_symlink():
            continue
        if root.is_symlink() or not root.is_dir():
            raise CandidateError(f"candidate source surface is not a directory: {surface}")
        def visit(path):
            for child in sorted(path.iterdir()):
                relative = child.relative_to(repo).as_posix()
                mode = child.lstat().st_mode
                if stat.S_ISDIR(mode):
                    visit(child)
                elif stat.S_ISREG(mode):
                    result[relative] = digest(regular_bytes(repo, relative))
                else:
                    raise CandidateError(f"non-regular source in candidate: {relative}")
        visit(root)
    return result


def _write(path, data):
    path.parent.mkdir(parents=True, exist_ok=True, mode=0o700)
    with path.open("xb") as stream:
        os.fchmod(stream.fileno(), 0o600)
        stream.write(data)
        stream.flush()
        os.fsync(stream.fileno())


def _sync_dir(path):
    fd = os.open(path, os.O_RDONLY | os.O_DIRECTORY)
    try:
        os.fsync(fd)
    finally:
        os.close(fd)


def seal(repo, destination, *, task_id, base_manifest, inputs, response,
         bridge_response=None, generated_paths=(), submission=None):
    """Capture a finished attempt into a host-private, content-bound candidate.

    The coordinator reaps the agent before calling this. Sources are copied,
    never hardlinked. A repeated call may only reproduce the identical seal.
    """
    if type(task_id) is not int or task_id < 1:
        raise CandidateError("candidate task ID must be positive")
    repo, destination = Path(repo), Path(destination)
    if destination.resolve().is_relative_to(repo.resolve()):
        raise CandidateError("candidate evidence must be outside the worker repository")
    after = source_manifest(repo)
    paths = sorted(set(base_manifest) | set(after))
    changes = {p: {"before": base_manifest.get(p), "after": after.get(p)}
               for p in paths if base_manifest.get(p) != after.get(p) and p not in generated_paths}
    for path in set(paths) | set(inputs):
        safe_relative(path)
    record = dict(schema=1, task_id=task_id, base_manifest=base_manifest,
                  inputs=inputs, changes=changes, response=response)
    if bridge_response is not None:
        record['bridge_response'] = bridge_response
    if submission is not None:
        if type(submission.get('turn')) is not int or submission['turn'] < 1:
            raise CandidateError('candidate submission requires a positive turn ordinal')
        record['submission'] = submission
    record_bytes = canonical_json(record)
    identity = digest(record_bytes)
    destination.parent.mkdir(parents=True, exist_ok=True, mode=0o700)
    temp = Path(tempfile.mkdtemp(prefix=".candidate-", dir=destination.parent))
    try:
        for relative, change in changes.items():
            if change["after"] is not None:
                content = regular_bytes(repo, relative)
                if content is None or digest(content) != change["after"]:
                    raise CandidateError(f"worker source changed while sealing: {relative}")
                _write(temp / "files" / relative, content)
        if source_manifest(repo) != after:
            raise CandidateError("worker source tree changed while sealing")
        _write(temp / "candidate.json", record_bytes)
        _write(temp / "sha256", (identity + "\n").encode())
        # Persist nested directory entries before the single publication rename.
        for directory, _, _ in os.walk(temp, topdown=False):
            _sync_dir(directory)
        if destination.exists():
            old, old_id = read(destination)
            if old_id != identity or old != record:
                raise CandidateError("task already has a different sealed candidate")
            return identity
        temp.rename(destination)
        _sync_dir(destination.parent)
        return identity
    finally:
        if temp.exists():
            shutil.rmtree(temp)


def read(candidate):
    root = Path(candidate)
    content = regular_bytes(root, "candidate.json")
    identity = regular_bytes(root, "sha256")
    if content is None or identity is None or identity.strip().decode() != digest(content):
        raise CandidateError("candidate manifest identity mismatch")
    record = json.loads(content)
    if record.get("schema") != 1:
        raise CandidateError("unsupported candidate manifest schema")
    for relative, change in record["changes"].items():
        safe_relative(relative)
        if change["before"] != record["base_manifest"].get(relative):
            raise CandidateError(f"candidate base mismatch: {relative}")
        if change["after"] is not None:
            content = regular_bytes(root / "files", relative)
            if content is None or digest(content) != change["after"]:
                raise CandidateError(f"sealed candidate content mismatch: {relative}")
    return record, identity.strip().decode()


def check_scope(record, *, nodes, helper_prefixes=(), shared_paths=(), exclusive=False, compiler="managed"):
    """Check source ownership before spending authoritative compiler resources."""
    if compiler not in ("none", "managed"):
        raise CandidateError("unknown task compiler capability")
    for relative in record["changes"]:
        p = safe_relative(relative)
        if compiler == "none" and not (len(p.parts) == 2 and p.parts[0] == "Tablet" and p.suffix == ".tex"):
            raise CandidateError(f"this task has no Lean capability and may edit only Tablet TeX: {relative}")
        if len(p.parts) == 2 and p.parts[0] == "Tablet" and p.suffix in {".lean", ".tex"}:
            if p.stem in nodes:
                continue
            if (relative not in record["base_manifest"]
                    and any(p.stem.startswith(prefix) for prefix in helper_prefixes)):
                continue
        if exclusive and relative in shared_paths:
            continue
        raise CandidateError(f"candidate edits an unreserved source: {relative}")


def closure_prefix_allows_imports(before, after):
    """Mirror the native closure write guard; semantic checking still follows."""
    def parts(prefix):
        fixed, imports = [], set()
        for line in prefix.splitlines(keepends=True):
            words = line.split('--', 1)[0].split()
            modules = words[1:] if words[:1] == ['import'] else []
            def module_name(module):
                return all(part and (part[0].isalpha() or part[0] == '_')
                           and all(c.isalnum() or c in "_'" for c in part[1:])
                           for part in module.split('.'))
            if modules and all(module_name(module) for module in modules):
                imports.update(modules)
            else:
                fixed.append(line)
        return ''.join(fixed), imports
    before_fixed, before_imports = parts(before)
    after_fixed, after_imports = parts(after)
    return before_fixed == after_fixed and before_imports <= after_imports


def check_lean_closure(candidate, base_repo, node):
    """Enforce the body-only write contract independently of model prompting.

    Uses the kernel-mirrored BODY splitter and token scan from
    trellis.dispatch.body_contract. Authoritative closure is still required.
    SKETCH nodes are not excluded by this write-scope check.
    """
    from trellis.dispatch.body_contract import body_ban_scan, split_body_marker
    record, _ = read(candidate)
    target = f"Tablet/{node}.lean"
    safe_relative(target)
    if set(record["changes"]) - {target}:
        raise CandidateError("Lean-closure work may change only the assigned theorem's proof body and add imports in its Lean file")
    base = regular_bytes(base_repo, target)
    if base is None or record["base_manifest"].get(target) != digest(base):
        raise CandidateError("Lean-closure task has no authenticated original statement")
    change = record["changes"].get(target)
    if change and change["after"] is None:
        raise CandidateError("Lean-closure work cannot delete its theorem")
    after = regular_bytes(Path(candidate) / "files", target) if change else base
    try:
        prefix, _ = split_body_marker(base.decode("utf-8"))
        proposed_prefix, body = split_body_marker(after.decode("utf-8"))
    except (ValueError, UnicodeError) as exc:
        raise CandidateError(f"Lean-closure BODY marker is invalid: {exc}") from exc
    if not closure_prefix_allows_imports(prefix, proposed_prefix):
        raise CandidateError("Lean-closure work may add imports, but cannot remove existing imports or change preamble options, namespace commands or the theorem statement; a term-local heartbeat option wholly below -- BODY does not change that prefix")
    raw = record["response"]
    if not isinstance(raw, dict):
        raise CandidateError("Lean-closure response must be an object")
    # Ordinary artifact envelopes are handled before this raw-payload gate.
    if raw.get("outcome") not in {"valid", "invalid", "stuck"}:
        raise CandidateError("Lean-closure results must be valid, invalid or stuck; report scope limitations as stuck")
    if raw["outcome"] == "valid":
        banned = body_ban_scan(body)
        if banned is not None:
            raise CandidateError(f"Lean-closure proof body contains banned token: {banned}")
    allowed = {"outcome", "summary", "comments", "system_feedback", "invalid_kind"}
    for key, value in raw.items():
        if key not in allowed and value not in (None, "", [], {}, False):
            raise CandidateError(f"Lean-closure results cannot include {key}; remove it and report any need for broader scope as stuck in summary")


def rebase(candidate, canonical, destination):
    """Overlay onto a prepared private copy of current canonical sources.

    The caller prepares the destination's runtime/build support. All source
    files there must initially match canonical. On any validation failure no
    file is written. Canonical is only read; the coordinator holds its normal
    integration lock throughout rebase, acceptance and publication.
    """
    record, identity = read(candidate)
    canonical, destination = Path(canonical).resolve(), Path(destination).resolve()
    if destination == canonical or destination.is_relative_to(canonical):
        raise CandidateError("rebase requires an isolated sibling repository")
    current = source_manifest(canonical)
    if source_manifest(destination) != current:
        raise CandidateError("rebase destination is not the current canonical source snapshot")
    for relative, expected in record["inputs"].items():
        content = regular_bytes(canonical, relative)
        actual = ('absent' if expected == 'absent' else None) if content is None else digest(content)
        if actual != expected:
            raise CandidateError(f"stale task input: {relative}")
    for relative, change in record["changes"].items():
        if current.get(relative) != change["before"]:
            raise CandidateError(f"candidate source conflicts with accepted work: {relative}")
    for relative, change in record["changes"].items():
        path = destination / relative
        if change["after"] is None:
            path.unlink()
        else:
            content = regular_bytes(Path(candidate) / "files", relative)
            path.parent.mkdir(parents=True, exist_ok=True)
            # Replacing the inode also breaks accidental destination hardlinks.
            fd, temp = tempfile.mkstemp(prefix=".rebase-", dir=path.parent)
            try:
                with os.fdopen(fd, "wb") as stream:
                    stream.write(content)
                os.replace(temp, path)
            finally:
                if os.path.exists(temp):
                    os.unlink(temp)
    return dict(candidate_sha256=identity, canonical_manifest=current,
                proposed_manifest=source_manifest(destination), response=record["response"])
