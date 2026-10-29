"""Isolated source candidates and hash-bound review evidence.

This is an evidence store, not a version-control system. Full regular-file
copies use the canonical PV source manifest; diffs are derived review aids.
Nothing here authorizes a kernel decision or changes the original run.
"""
from __future__ import annotations

import difflib
import hashlib
import json
import os
from pathlib import Path
import shutil
import tempfile
import zipfile
from typing import Any

from scripts.pv_source_tree import canonical_json, source_tree_manifest, source_partition_digest, _relative_path

REVIEW_SCHEMA = 'trellis-pv-revision-review/v2'
LEGACY_REVIEW_SCHEMA = 'trellis-pv-revision-review/v1'


class RevisionStoreError(ValueError):
    pass


def digest(value: Any) -> str:
    return hashlib.sha256(canonical_json(value)).hexdigest()


def _read(path: Path) -> Any:
    if path.is_symlink():
        raise RevisionStoreError(f"evidence path is a symlink: {path}")
    return json.loads(path.read_bytes())


def _write(path: Path, value: Any, *, immutable: bool = False) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    if path.is_symlink():
        raise RevisionStoreError(f"evidence path is a symlink: {path}")
    raw = canonical_json(value)
    if immutable and path.exists():
        if path.read_bytes() != raw:
            raise RevisionStoreError(f"immutable evidence already exists: {path}")
        return
    fd, temporary = tempfile.mkstemp(prefix=".write-", dir=path.parent)
    try:
        with os.fdopen(fd, "wb") as stream:
            stream.write(raw)
            stream.flush()
            os.fsync(stream.fileno())
        os.replace(temporary, path)
        if immutable:
            path.chmod(0o444)
    finally:
        if os.path.exists(temporary):
            os.unlink(temporary)


def _copy(source: Path, destination: Path, *, readonly: bool = False) -> dict:
    before = source_tree_manifest(source, allow_empty=True)
    destination.mkdir(parents=True, exist_ok=False)
    for entry in before["files"]:
        relative = entry["relative_path"]
        target = destination / relative
        target.parent.mkdir(parents=True, exist_ok=True)
        # Manifest validation rejects links; verification below detects changes
        # between hashing and copying. Failed copies remain as evidence.
        shutil.copy2(source / relative, target, follow_symlinks=False)
    if source_tree_manifest(destination, allow_empty=True) != before:
        raise RevisionStoreError("source changed while copying snapshot")
    if source_tree_manifest(source, allow_empty=True) != before:
        raise RevisionStoreError("source changed while copying snapshot")
    if readonly:
        for entry in before["files"]:
            (destination / entry["relative_path"]).chmod(0o444)
    return before


def _goals(collection: dict | list) -> dict:
    if isinstance(collection, list):
        collection = {"context": "", "goals": collection}
    if not isinstance(collection, dict) or not isinstance(collection.get("goals"), list):
        raise RevisionStoreError("requirements must contain context and goals")
    if not isinstance(collection.get("context", ""), str):
        raise RevisionStoreError("requirement context must be text")
    result = json.loads(canonical_json(collection))
    result.setdefault("context", "")
    ids = []
    for goal in result["goals"]:
        if not isinstance(goal, dict) or not isinstance(goal.get("id"), str) or not goal["id"]:
            raise RevisionStoreError("each goal requires a nonempty id")
        if not isinstance(goal.get("text"), str) or not goal["text"].strip():
            raise RevisionStoreError("each goal requires explicit text")
        ids.append(goal["id"])
    if len(ids) != len(set(ids)):
        raise RevisionStoreError("goal ids must be unique")
    return result


def _scope_path(value: str) -> str:
    if not isinstance(value, str) or not value:
        raise RevisionStoreError("task scope requires relative paths")
    _relative_path(tuple(value.rstrip("/").split("/")))
    return value


def tree_diff(before: Path, after: Path, *, paths=None) -> dict:
    """A deletion retains its original bytes in the referenced base snapshot."""
    old = source_tree_manifest(before, allow_empty=True)
    new = source_tree_manifest(after, allow_empty=True)
    checked_old, checked_new = old, new
    if paths is not None:
        old, new = select_manifest(old, paths), select_manifest(new, paths)
    old_files = {row["relative_path"]: row for row in old["files"]}
    new_files = {row["relative_path"]: row for row in new["files"]}
    changes, patches = [], []
    for relative in sorted(old_files.keys() | new_files.keys()):
        left, right = old_files.get(relative), new_files.get(relative)
        if left == right:
            continue
        operation = "add" if left is None else "delete" if right is None else "modify"
        a = (before / relative).read_bytes() if left else b""
        b = (after / relative).read_bytes() if right else b""
        if (left and hashlib.sha256(a).hexdigest() != left['sha256_of_raw_bytes']) or (right and hashlib.sha256(b).hexdigest() != right['sha256_of_raw_bytes']):
            raise RevisionStoreError('source changed while deriving its patch')
        binary = b"\0" in a or b"\0" in b
        try:
            a_text, b_text = a.decode("utf-8"), b.decode("utf-8")
        except UnicodeDecodeError:
            binary = True
        changes.append({"path": relative, "operation": operation, "before": left, "after": right, "binary": binary})
        if binary:
            patches.append(f"Binary {operation}: {relative} ({left and left['sha256_of_raw_bytes']} -> {right and right['sha256_of_raw_bytes']})\n")
        else:
            patches.extend(difflib.unified_diff(a_text.splitlines(keepends=True), b_text.splitlines(keepends=True), fromfile=f"a/{relative}" if left else "/dev/null", tofile=f"b/{relative}" if right else "/dev/null"))
    if source_tree_manifest(before, allow_empty=True) != checked_old or source_tree_manifest(after, allow_empty=True) != checked_new:
        raise RevisionStoreError("source changed while deriving its patch")
    return {"base_source_digest": old["source_tree_sha256"], "result_source_digest": new["source_tree_sha256"], "changes": changes, "patch": "".join(patches)}


def effective_invalidation(audit_ids, before_fingerprints, after_fingerprints,
                           dependencies, source_preservation_ids=(), *, source_changed=False) -> list[str]:
    """Union the audit with mechanical changes and transitive dependents."""
    invalid = set(audit_ids)
    invalid.update(key for key in before_fingerprints.keys() | after_fingerprints.keys()
                   if before_fingerprints.get(key) != after_fingerprints.get(key))
    if source_changed:
        invalid.update(source_preservation_ids)
    while True:
        expanded = invalid | {key for key, requirements in dependencies.items() if invalid.intersection(requirements)}
        if expanded == invalid:
            return sorted(invalid)
        invalid = expanded


INVALIDATION_LANES = ("correspondence", "soundness", "goal_faithfulness", "substantiveness", "closure")


def validate_invalidation(value: Any, known_ids=None) -> list[dict]:
    """Normalize legacy node ids and explicit audit node/lane/reason rows."""
    if not isinstance(value, list):
        raise RevisionStoreError("audit invalidation must be a list")
    normalized = []
    for row in value:
        if isinstance(row, str):
            row = {"node": row, "lanes": list(INVALIDATION_LANES), "reason": "Audit requested invalidation"}
        if not isinstance(row, dict) or not isinstance(row.get("node"), str) or not row["node"]:
            raise RevisionStoreError("audit invalidation requires a node identity")
        if known_ids is not None and row["node"] not in known_ids:
            raise RevisionStoreError(f"audit invalidation names unknown node: {row['node']}")
        if not isinstance(row.get("reason"), str) or not row["reason"].strip():
            raise RevisionStoreError("audit invalidation requires a reason")
        lanes = row.get("lanes")
        if not isinstance(lanes, list) or not lanes or any(lane not in INVALIDATION_LANES for lane in lanes):
            raise RevisionStoreError("audit invalidation names an unknown or empty lane set")
        normalized.append({"node": row["node"], "lanes": sorted(set(lanes)), "reason": row["reason"]})
    return normalized


def source_manifest(path: Path | str) -> dict:
    return source_tree_manifest(Path(path), allow_empty=True)


def select_manifest(manifest: dict, paths) -> dict:
    files = [row for row in manifest['files'] if row['relative_path'] in paths]
    return dict(manifest, files=files, source_tree_sha256=source_partition_digest(files))


def harness_paths(authorizations) -> set[str]:
    paths = set()
    for authorization in authorizations:
        for path in authorization.get('evidence_paths', []):
            _relative_path(tuple(path.split('/')))
            paths.add(path)
        for check in authorization.get('checks', []):
            for path in check.get('harness_paths', []):
                _relative_path(tuple(path.split('/')))
                paths.add(path)
    return paths


def review_partition(state: dict) -> tuple[dict, dict]:
    """Only explicitly declared new harness files can leave the program tree."""
    authorizations = [task['authorization'] if isinstance(task.get('authorization'), dict) else task for task in state['tasks'].values()]
    paths = harness_paths(authorizations)
    original = {row['relative_path'] for row in state['original_manifest']['files']}
    candidate = {row['relative_path'] for row in state['source_manifest']['files']}
    if paths & original:
        raise RevisionStoreError('Evidence/harness paths include original production files; use separate copies: ' + ', '.join(sorted(paths & original)))
    if paths - candidate:
        raise RevisionStoreError('Declared evidence/harness files are missing: ' + ', '.join(sorted(paths - candidate)))
    return select_manifest(state['source_manifest'], candidate - paths), select_manifest(state['source_manifest'], paths)


def ratified_source_manifest(package: dict) -> dict:
    return package['ratification']['source_manifest'] if package['schema'] == REVIEW_SCHEMA else package['source_manifest']


def copy_manifest_files(source: Path, destination: Path, manifest: dict) -> None:
    """Copy a verified projection, preserving raw candidate evidence elsewhere."""
    destination.mkdir(parents=True, exist_ok=False)
    for row in manifest['files']:
        path = _state_source(source, row['relative_path'])
        raw = path.read_bytes()
        if len(raw) != row['byte_length'] or hashlib.sha256(raw).hexdigest() != row['sha256_of_raw_bytes']:
            raise RevisionStoreError('Source bytes changed while separating production and evidence')
        target = destination / row['relative_path']
        target.parent.mkdir(parents=True, exist_ok=True)
        target.write_bytes(raw)
        target.chmod(path.stat().st_mode & 0o777)
    if source_manifest(destination) != manifest:
        raise RevisionStoreError('Separated source manifest does not match')


def ratified_source(root: Path, package: dict) -> Path:
    source = _state_source(root, package['source'])
    if package['schema'] == LEGACY_REVIEW_SCHEMA:
        return source
    folder = write_review_package(root, package)
    target = folder / 'production'
    expected = ratified_source_manifest(package)
    if not target.exists():
        temporary = Path(tempfile.mkdtemp(prefix='.production-', dir=folder))
        try:
            copy_manifest_files(source, temporary / 'source', expected)
            os.rename(temporary / 'source', target)
        finally:
            shutil.rmtree(temporary)
    if source_manifest(target) != expected:
        raise RevisionStoreError('Stored production snapshot changed')
    return target


def copy_source_tree(source: Path | str, destination: Path | str) -> dict:
    return _copy(Path(source), Path(destination))


def changed_paths(before_manifest: dict, after_manifest: dict) -> list[str]:
    old = {row['relative_path']: row for row in before_manifest['files']}
    new = {row['relative_path']: row for row in after_manifest['files']}
    return sorted(key for key in old.keys() | new.keys() if old.get(key) != new.get(key))


def _state_source(root: Path, value: str) -> Path:
    path = Path(value)
    # Existing driver attempts record base_source as an absolute confined path.
    if not path.is_absolute():
        _relative_path(tuple(value.split('/')))
        path = root / path
    if not path.resolve().is_relative_to(root.resolve()):
        raise RevisionStoreError('source evidence path escapes the revision')
    current = path
    while current != root:
        if current.is_symlink():
            raise RevisionStoreError('source evidence path contains a symlink')
        if current.parent == current:
            raise RevisionStoreError('source evidence path is outside revision root')
        current = current.parent
    return path


def validate_check_evidence(root: Path, checks: dict | None) -> None:
    """Bind production check stdout/stderr and exact result receipt bytes."""
    if not checks or not ("artifacts" in checks or "evidence" in checks):
        return  # Injected test executors can return an in-memory receipt.
    artifacts = checks.get("artifacts")
    if not isinstance(artifacts, dict) or not isinstance(checks.get("evidence"), str):
        raise RevisionStoreError("check evidence requires artifact hashes and a receipt")
    for value, expected in {**artifacts, checks["evidence"]: checks.get("evidence_sha256")}.items():
        path = _state_source(root, value)
        if not path.is_relative_to(root / "checks") or not path.is_file():
            raise RevisionStoreError("check evidence must be a regular file under checks/")
        if hashlib.sha256(path.read_bytes()).hexdigest() != expected:
            raise RevisionStoreError("check evidence artifact changed")
    receipt = _read(_state_source(root, checks["evidence"]))
    expected = {key: value for key, value in checks.items() if key not in {"evidence", "evidence_sha256"}}
    if receipt != expected:
        raise RevisionStoreError("check receipt differs from controller evidence")


def validate_predecessor_evidence(root: Path, handoff: dict) -> dict:
    """Keep preserved negative proofs and protocol bytes inside the approval."""
    evidence = root / 'predecessor'
    required = handoff.get('schema') == 'trellis-pv-revision-predecessor/v1'
    if not evidence.exists() and not required:
        return {}
    result = {}
    record = evidence / 'record.json'
    if record.exists():
        if _read(_state_source(root, str(record))) != handoff:
            raise RevisionStoreError('preserved predecessor record changed')
        result['record_digest'] = digest(handoff)
    protocol = evidence / 'protocol_state.json'
    if protocol.exists() or required:
        protocol = _state_source(root, str(protocol))
        if not protocol.is_file():
            raise RevisionStoreError('preserved predecessor protocol evidence is missing')
        actual = hashlib.sha256(protocol.read_bytes()).hexdigest()
        expected = handoff.get('protocol_state_sha256')
        if (required or expected is not None) and actual != expected:
            raise RevisionStoreError('preserved predecessor protocol evidence changed')
        result['protocol_state_sha256'] = actual
    tablet = evidence / 'Tablet'
    if tablet.exists() or required:
        manifest_path = evidence / 'tablet-manifest.json'
        if not manifest_path.is_file():
            raise RevisionStoreError('preserved predecessor proofs lack a manifest')
        manifest = source_manifest(_state_source(root, str(tablet)))
        expected = handoff.get('tablet_manifest', _read(manifest_path))
        if manifest != expected or manifest != _read(manifest_path):
            raise RevisionStoreError('preserved predecessor proof snapshot changed')
        result['tablet_manifest'] = manifest
    return result


def build_review_package_from_state(root: Path | str, state: dict, *, schema=None) -> dict:
    """Build the driver review from its durable, immutable attempt copies.

    Recomputing this package at the human gate rejects stale manifests, task
    authorizations, goals, receipts, and both accepted and rejected evidence.
    """
    root = Path(root).resolve()
    schema = schema or state.get('human_package', {}).get('schema', REVIEW_SCHEMA)
    if schema not in {REVIEW_SCHEMA, LEGACY_REVIEW_SCHEMA}:
        raise RevisionStoreError('Unknown revision review schema')
    original = _state_source(root, 'sources/original')
    candidate = _state_source(root, state['source'])
    if source_manifest(original) != state['original_manifest'] or source_manifest(candidate) != state['source_manifest']:
        raise RevisionStoreError('source evidence changed before review')
    old_collection = _goals(state['original_collection'])
    new_collection = _goals(state['proposed_collection'])
    if old_collection['goals'] != state['original_goals'] or new_collection['goals'] != state['proposed_goals']:
        raise RevisionStoreError('goal inventory differs from its requirement collection')
    aggregate = tree_diff(original, candidate)
    task_diffs = []
    for attempt in state['attempts']:
        before = _state_source(root, attempt['base_source'])
        after = _state_source(root, attempt['source'])
        if source_manifest(before) != attempt['base'] or source_manifest(after) != attempt['manifest']:
            raise RevisionStoreError('task snapshot source evidence changed')
        validate_check_evidence(root, attempt.get('checks'))
        authorization = attempt['authorization']
        delta = tree_diff(before, after)
        paths = authorization.get('allowed_paths', authorization.get('paths'))
        if paths is not None:
            for path in paths:
                _scope_path(path)
        violations = [] if paths is None else [row['path'] for row in delta['changes']
                     if not any(row['path'] == path or row['path'].startswith(path.rstrip('/') + '/') for path in paths)]
        task_diffs.append({'attempt_id': attempt['id'], 'task_id': attempt['task_id'],
                           'base_source': attempt['base_source'], 'result_source': attempt['source'],
                           'task_authorization_digest': digest(authorization), 'diff': delta,
                           'scope_passed': attempt.get('scope', {}).get('decision') == 'pass' and not violations,
                           'mechanical_scope_violations': violations, 'accepted': attempt.get('accepted', False),
                           'implementation_alternatives': attempt.get('response', {}).get('alternatives', []),
                           'checks': attempt.get('checks'), 'checks_are_proofs': False})
    aggregate_check_history = state.get('aggregate_check_history', [])
    for observation in aggregate_check_history:
        for checks in observation['checks'].values():
            validate_check_evidence(root, checks)
            if checks.get('source') is not None and checks['source'] != observation['source_manifest']:
                raise RevisionStoreError('historical aggregate check receipt names a different source')
    comment_base = None
    if state.get('comment_amendment'):
        from trellis.pv_revision.comment_amendments import validate
        comment_base = validate(root, state, reviewed=True)
    checked_manifest = state['source_manifest']
    ancestor = comment_base
    while ancestor:
        checked_manifest = ancestor['source_manifest']
        from trellis.pv_revision.comment_amendments import base_package
        ancestor = base_package(root, ancestor['comment_amendment']['base_review']) if ancestor.get('comment_amendment') else None
    aggregate_checks = state.get('aggregate_checks', {})
    for checks in aggregate_checks.values():
        validate_check_evidence(root, checks)
        if checks.get('source') is not None and checks['source'] != checked_manifest:
            raise RevisionStoreError('aggregate check receipt names a different final source')
    handoff = state.get('handoff', {})
    predecessor_evidence = validate_predecessor_evidence(root, handoff)
    before_fingerprints = handoff.get('before_fingerprints', {})
    after_fingerprints = handoff.get('after_fingerprints', dict(before_fingerprints))
    # Requirement edits are mechanically invalidating even when an audit omits
    # them. No proof dependency can disappear merely by leaving its key out.
    before_fingerprints = dict(before_fingerprints, **{goal['id']: digest({'context': old_collection['context'], 'goal': goal}) for goal in old_collection['goals']})
    after_fingerprints = dict(after_fingerprints, **{goal['id']: digest({'context': new_collection['context'], 'goal': goal}) for goal in new_collection['goals']})
    for removed in set(goal['id'] for goal in old_collection['goals']) - set(goal['id'] for goal in new_collection['goals']):
        after_fingerprints.pop(removed, None)
    plan = state.get('plan') or {}
    audit_value = plan.get('invalidation', plan.get('invalidate', plan.get('invalidation_ids', [])))
    baseline = handoff.get('baseline', {})
    known_ids = None
    if 'nodes' in baseline:
        known_ids = set(baseline['nodes']) | set(before_fingerprints) | set(after_fingerprints)
    audit_invalidation = validate_invalidation(audit_value, known_ids)
    audit_ids = [item['node'] for item in audit_invalidation]
    preservation = handoff.get('source_preservation_ids', handoff.get('baseline', {}).get('nodes', []))
    # A fresh source successor must recheck all known goal/fingerprint
    # identities as well as the explicitly inventoried preservation nodes.
    preservation = sorted(set(preservation) | set(before_fingerprints) | set(after_fingerprints))
    invalidation = effective_invalidation(audit_ids, before_fingerprints, after_fingerprints,
                                         handoff.get('dependencies', handoff.get('baseline', {}).get('dependencies', {})), preservation,
                                         source_changed=bool(aggregate['changes']))
    mechanical = effective_invalidation([], before_fingerprints, after_fingerprints,
                                         handoff.get('dependencies', baseline.get('dependencies', {})), preservation,
                                         source_changed=bool(aggregate['changes']))
    # Fresh modeling inherits no approvals. Mechanical changes and their
    # dependent closure invalidate every proof/review lane; the audit may add
    # explicit lane invalidations even when no source or goal bytes changed.
    per_lane = {lane: set(mechanical) for lane in INVALIDATION_LANES}
    audit_dependents = set(invalidation) - set(audit_ids) - set(mechanical)
    for affected in per_lane.values():
        affected.update(audit_dependents)
    for item in audit_invalidation:
        for lane in item['lanes']:
            per_lane[lane].add(item['node'])
    per_lane = {lane: sorted(nodes) for lane, nodes in per_lane.items()}
    fields = ('round', 'original_collection', 'proposed_collection', 'original_goals', 'proposed_goals',
              'negated_goals', 'original_manifest', 'source_manifest', 'source', 'plan', 'tasks', 'attempts', 'aggregate_scope')
    package = {key: state[key] for key in fields}
    history = [item for item in state.get('history', []) if item.get('task', {}).get('role') != 'aggregate_scope']
    package.update(schema='trellis-pv-revision-review/v1', aggregate_diff=aggregate, task_diffs=task_diffs,
                   controller_evidence=history, operator_actions=state.get('operator_actions', []),
                   aggregate_checks=aggregate_checks, aggregate_check_history=aggregate_check_history,
                   predecessor_evidence=predecessor_evidence,
                   effective_invalidation=invalidation, per_lane_invalidation=per_lane,
                   audit_invalidation=audit_invalidation, original_evidence=handoff,
                   audit_remedy_alternatives=plan.get('alternatives', []), tests_are_proofs=False,
                   retirement_is_proof=False, proof_status='requires_modeling_and_reproof',
                   scope_passed=state['aggregate_scope'].get('decision') == 'pass' and all(
                       item['scope_passed'] for item in task_diffs if item['accepted']))
    from trellis.pv_revision.code_edits import references
    requested_edits = references(root, state)
    if requested_edits:
        package['human_code_edits'] = requested_edits
    if comment_base:
        package['comment_amendment'] = state['comment_amendment']
        package['scope_passed'] = package['scope_passed'] and state['comment_amendment']['review']['decision'] == 'pass'
    package['candidate_digest'] = digest({'source': state['source_manifest'], 'goals': new_collection,
                                          'task_authorizations': state['tasks'], 'audit': plan,
                                          'attempts': state['attempts'], 'aggregate_scope': state['aggregate_scope'],
                                          'aggregate_checks': aggregate_checks,
                                          'aggregate_check_history': aggregate_check_history,
                                          'original_evidence': handoff, 'effective_invalidation': invalidation})
    if schema == REVIEW_SCHEMA:
        if state.get('review_repackaging'):
            legacy = dict(package, digest=digest(package))
            prior = state['review_repackaging']['from_digest']
            if legacy['digest'] != prior or _read(_state_source(root, f'reviews/{prior}/review.json')) != legacy:
                raise RevisionStoreError('Repackaged review no longer matches its original checked evidence')
        production, evidence = review_partition(state)
        production_paths = {row['relative_path'] for row in production['files']} | {row['relative_path'] for row in state['original_manifest']['files']}
        evidence_paths = {row['relative_path'] for row in evidence['files']}
        package.update(schema=schema,
                       aggregate_diff=tree_diff(original, candidate, paths=production_paths),
                       evidence_diff=tree_diff(original, candidate, paths=evidence_paths),
                       ratification={'original_manifest': state['original_manifest'], 'source_manifest': production,
                                     'original_collection': old_collection, 'proposed_collection': new_collection},
                       evidence={'source_manifest': evidence, 'checks': {
                           key: (task['authorization'] if isinstance(task.get('authorization'), dict) else task).get('checks', [])
                           for key, task in state['tasks'].items()},
                           'tools': state.get('tools', {}), 'ratified': False})
        package['candidate_digest'] = digest(package['ratification'])
        if state.get('review_repackaging'):
            package['review_repackaging'] = state['review_repackaging']
        package['evidence_digest'] = digest({key: value for key, value in package.items()
            if key not in {'schema', 'ratification', 'candidate_digest', 'aggregate_diff'}})
    package['digest'] = digest(package)
    return json.loads(canonical_json(package))


def write_review_package(root: Path | str, package: dict) -> Path:
    """Write immutable JSON/patch/README without changing controller state."""
    root = Path(root)
    unsigned = {key: value for key, value in package.items() if key != 'digest'}
    if package.get('digest') != digest(unsigned):
        raise RevisionStoreError('cannot publish an invalid review digest')
    folder = root / 'reviews' / package['digest']
    _write(folder / 'review.json', package, immutable=True)
    text = review_readme(package)
    artifacts = [('README.md', text), ('candidate.patch', package['aggregate_diff']['patch'])]
    if package['schema'] == REVIEW_SCHEMA:
        artifacts.append(('evidence.patch', package['evidence_diff']['patch']))
    for name, raw in artifacts:
        target = folder / name
        if target.exists() and target.read_text() != raw:
            raise RevisionStoreError('immutable review artifact changed')
        if not target.exists():
            target.write_text(raw)
            target.chmod(0o444)
    return folder



def review_readme(package: dict) -> str:
    """Render the review itself, including concrete requirements and evidence."""
    lines = ["# Program verification remediation review", "",
             f"Exact candidate digest: `{package['candidate_digest']}`", "",
             f"{'Review' if package['schema'] == REVIEW_SCHEMA else 'Approval'} package digest: `{package['digest']}`", "",
             "Tests do not constitute proofs. Retirement is not proof. This candidate requires "
             "fresh modeling and reproof after ratification; surviving conditional obligations remain explicit goals.", ""]
    separated = package['schema'] == REVIEW_SCHEMA
    if separated:
        lines.extend(['Ratification covers the production source changes and proposed requirements, including shared context. '
                      'Harness files, check receipts and audit reports are attached evidence, not approved changes.', '',
                      f"Evidence digest: `{package['evidence_digest']}`", '', '## Production changes', ''])
        lines.extend(f"- {row['operation']}: `{row['path']}`" for row in package['aggregate_diff']['changes'])
        lines.extend(['', '## Evidence and harness (not ratified)', ''])
        lines.extend(f"- `{row['relative_path']}` — SHA-256 `{row['sha256_of_raw_bytes']}`"
                     for row in package['evidence']['source_manifest']['files'])
        lines.append('')
        if package.get('review_repackaging'):
            repackaging = package['review_repackaging']
            lines.extend([f"Repackaged from review `{repackaging['from_digest']}`. Source bytes, requirements, checks and scope verdicts are unchanged. "
                          "Recorded checks used the combined snapshot; repackaging did not rerun them.", '',
                          'Separation review: ' + repackaging['separation_reason'], ''])
    if package.get('comment_amendment'):
        change = package['comment_amendment']
        lines.extend(['## Comment-only amendment', '',
            f"Prior reviewed proposal: `{change['base_review']}`.",
            'Only requested comments changed. Earlier checks and scope verdicts retain their original source bindings and were not rerun against the edited source.',
            'Focused comment review: ' + change['review']['decision'] + ' — ' + change['review']['reason'], ''])
        lines.extend('Editorial note: ' + note for note in change['review'].get('editorial_notes', []))
        lines.append('')
    for request in package.get('human_code_edits', []):
        lines.extend(['## Human code amendment', '',
                      f"Requested changes to review `{request['package_digest']}`: `{request['path']}`.",
                      'These are requested edits, not evidence that this candidate implements them.', ''])
        lines.extend(f"- {edit['operation']}: `{edit['path']}`" for edit in request['files'])
        lines.append('')
    for title, collection in (("Original requirements", package['original_collection']),
                               ("Proposed requirements", package['proposed_collection'])):
        lines.extend([f"## {title}", "", "Shared context:", "", collection.get('context') or "(none)", ""])
        for goal in collection['goals']:
            lines.extend([f"### {goal['id']}", "", goal['text'], ""])
            if goal.get('source_refs'):
                lines.extend(["Source references: " + json.dumps(goal['source_refs'], ensure_ascii=False), ""])
        if not collection['goals']:
            lines.extend(["All requirements were retired. No successor proof or successful verification is claimed.", ""])
    plan = package.get('plan') or {}
    lines.extend(["## Audit remedies", "", plan.get('summary', '(no summary)'), ""])
    for row in plan.get('dispositions', []):
        lines.extend([f"### {row['goal_id']}: {row['remedy']}", "", row.get('reason', ''), ""])
        for key, label in (('replacement_goals', 'Replacement'), ('surviving_obligations', 'Surviving conditional obligation')):
            for goal in row.get(key, []):
                lines.extend([f"{label} {goal['id']}: {goal['text']}", ""])
        if row.get('task_ids'):
            lines.extend(["Authorized tasks: " + ', '.join(row['task_ids']), ""])
    def alternatives(rows):
        if not rows:
            lines.extend(["No alternatives recorded.", ""])
        for row in rows:
            lines.extend([f"- {row.get('remedy', row.get('approach', 'Alternative'))}: {row.get('rationale', '')}",
                          "  Status: " + row.get('status', 'audit assessment') + ". Evidence: " +
                          '; '.join(str(item) for item in row.get('evidence', [])), ""])
    lines.extend(["### Audit remedy alternatives", ""])
    alternatives(package.get('audit_remedy_alternatives', []))
    lines.extend(["## Task scope and checks", "",
                  "Each per-attempt check receipt concerns its named task source version. Final aggregate " +
                  ("check results below retain their original source bindings and were not rerun for the comment amendment."
                   if package.get("comment_amendment") else "check results are reported separately below and are bound to the final candidate."), ""])
    for ident, task in package.get('tasks', {}).items():
        authorization = task['authorization'] if isinstance(task.get('authorization'), dict) else task
        lines.extend([f"### {ident}", "", authorization.get('authorization', '(scope recorded in review.json)'), "",
                      "Allowed paths: " + ', '.join(authorization.get('allowed_paths', authorization.get('paths', ['semantic scope only']))), ""])
    for index, attempt in enumerate(package['task_diffs']):
        lines.extend([f"### Attempt {attempt['attempt_id']} for {attempt['task_id']}", "",
                      f"Accepted: {attempt['accepted']}. Scope passed: {attempt['scope_passed']}.", "",
                      f"Base source: `{attempt['diff']['base_source_digest']}`", "",
                      f"Result source: `{attempt['diff']['result_source_digest']}`", "",
                      f"Authorization digest: `{attempt['task_authorization_digest']}`", "",
                      "Mechanical scope violations: " + ', '.join(attempt['mechanical_scope_violations']) if attempt['mechanical_scope_violations'] else "Mechanical scope violations: none.", ""])
        checks = attempt.get('checks') or {}
        lines.extend([f"Independent checks passed: {checks.get('ok', False)}. {checks.get('reason', '')}", ""])
        for check in checks.get('checks', []):
            lines.extend([f"- {check.get('id', 'check')} ({check.get('side', 'candidate')}): "
                          f"exit {check.get('exit_code')}, expected {check.get('expected_exit')}, "
                          f"matched {check.get('matched')}, timeout {check.get('timeout', False)}.", ""])
        lines.extend(["Implementation alternatives:", ""])
        alternatives(attempt.get('implementation_alternatives', []))
    check_label = "Prior implementation checks (not rerun)" if package.get("comment_amendment") else "Final candidate checks"
    lines.extend(["## " + check_label, ""])
    if not package.get('aggregate_checks'):
        lines.extend(["No final candidate check receipts are recorded (requirement-only revisions need no source checks).", ""])
    for ident, checks in package.get('aggregate_checks', {}).items():
        lines.extend([f"### {ident}", "", f"{check_label} passed: {checks.get('ok', False)}.", ""])
        if package.get('comment_amendment') and checks.get('source'):
            lines.extend([f"Checked source: `{checks['source']['source_tree_sha256']}`", ""])
        for check in checks.get('checks', []):
            lines.extend([f"- {check.get('id', 'check')} ({check.get('side', 'candidate')}): "
                          f"exit {check.get('exit_code')}, expected {check.get('expected_exit')}, "
                          f"matched {check.get('matched')}, timeout {check.get('timeout', False)}.", ""])
    if package.get('aggregate_check_history'):
        lines.extend(["### Previous aggregate check observations", ""])
        for observation in package['aggregate_check_history']:
            outcomes = ', '.join(f"{ident}: passed={checks.get('ok', False)}"
                                 for ident, checks in observation['checks'].items())
            lines.extend([f"- Sequence {observation['sequence']}, round {observation['round']}, "
                          f"source `{observation['source_manifest']['source_tree_sha256']}`: {outcomes}", ""])
    lines.extend(["## Aggregate scope and required revalidation", "",
                  f"Scope passed: {package['scope_passed']}.", "",
                  package.get('aggregate_scope', {}).get('reason', ''), ""])
    for lane, nodes in package.get('per_lane_invalidation', {}).items():
        lines.extend([f"- {lane}: {', '.join(nodes) or '(none)'}", ""])
    if package.get('operator_actions'):
        lines.extend(['## Operator recovery decisions', ''])
        for action in package['operator_actions']:
            lines.extend([json.dumps(action, ensure_ascii=False, sort_keys=True), ''])
    lines.extend(["## Evidence layout", "",
                  "The exported ZIP is self-contained: `review.json` contains the exact structured review; "
                  "`candidate.patch` is the aggregate patch; `sources/original/` and `sources/candidate/` "
                  "contain full source bytes, including deleted and binary originals. "
                  "`sources/snapshots/<digest>/` and `patches/` retain task source versions and diffs. "
                  "`checks/` contains only the recorded check outputs and receipts. "
                  "`predecessor/` retains available original proof sources and protocol state. "
                  "`archive-manifest.json` maps every bundled file to its byte hash and each task to its source snapshots.", "",
                  "The on-disk review directory contains README, review JSON and aggregate patch; source snapshots "
                  "are under the revision root named in its metadata until exported. "
                  + ("`candidate.patch` and `sources/production/` contain only production code. "
                   "`evidence.patch` and `evidence/harness/` contain the attached harness. "
                   "`sources/candidate/` retains the complete candidate and evidence files needed to reproduce checks; "
                   "it is not the ratified production tree. The package digest binds all evidence against tampering; "
                   "the candidate digest binds only production changes and requirements. Evidence changes require "
                   "reopening the review, not approval of the evidence as program code."
                   if separated else "Approval binds source, requirements and shared context, task authorizations, receipts, "
                   "and scope decisions. Changed contents require a fresh review."), ""])
    notes = [(note, 'Audit goal note') for note in plan.get('goal_notes', [])]
    notes += [(note, 'Final scope code note') for note in package.get('aggregate_scope', {}).get('code_notes', [])]
    for attempt in package.get('attempts', []):
        notes += [(note, f"Task scope code note ({attempt['id']})") for note in attempt.get('scope', {}).get('code_notes', [])]
    if notes:
        lines.extend(['## Structured review notes', ''])
        for note, label in notes:
            lines.extend([f"### {label} — {note['author_role']}", '',
                          'Related goals: ' + ', '.join(note['goal_ids']), ''])
            if 'characterization' in note:
                lines.extend([note['characterization'], ''])
            if 'path' in note:
                lines.extend(['File: ' + note['path'], ''])
            if 'range' in note:
                span = note['range']
                lines.extend([f"{span['side']} lines {span['start']}–{span['end']} in this review's snapshot.", ''])
            lines.extend([note['reason'], ''])
    return '\n'.join(lines)


def export_review_package(root: Path | str, state: dict) -> Path:
    """Atomically export exact review inputs without runtime/toolchain material."""
    root = Path(root).resolve()
    package = build_review_package_from_state(root, state)
    if state.get('human_package') is not None and package != state['human_package']:
        raise RevisionStoreError('cannot export a stale human review package')
    files: dict[str, bytes] = {
        'README.md': review_readme(package).encode('utf-8'),
        'review.json': canonical_json(package),
        'candidate.patch': package['aggregate_diff']['patch'].encode('utf-8'),
    }
    if package['schema'] == REVIEW_SCHEMA:
        files['evidence.patch'] = package['evidence_diff']['patch'].encode('utf-8')
    modes: dict[str, int] = {}
    def include_file(path: Path, name: str, expected: str | None = None):
        path = _state_source(root, str(path))
        if not path.is_file():
            raise RevisionStoreError('export evidence file is missing')
        raw = path.read_bytes()
        if expected is not None and hashlib.sha256(raw).hexdigest() != expected:
            raise RevisionStoreError('export evidence changed while reading')
        if name in files and files[name] != raw:
            raise RevisionStoreError('export contains conflicting evidence paths')
        files[name] = raw
        modes[name] = path.stat().st_mode & 0o777
    def include_tree(path: Path, prefix: str, expected: dict | None = None):
        manifest = source_manifest(path)
        if expected is not None and manifest != expected:
            raise RevisionStoreError('export source snapshot changed')
        for row in manifest['files']:
            include_file(path / row['relative_path'], prefix + '/' + row['relative_path'], row['sha256_of_raw_bytes'])
        return manifest['source_tree_sha256']
    include_tree(root / 'sources/original', 'sources/original', state['original_manifest'])
    if package.get('review_repackaging'):
        prior = package['review_repackaging']['from_digest']
        include_file(root / 'reviews' / prior / 'review.json', f'reviews/{prior}/review.json')
    for request in package.get('human_code_edits', []):
        include_file(root / request['path'], request['path'], request['sha256'])
    include_tree(_state_source(root, state['source']), 'sources/candidate', state['source_manifest'])
    if package['schema'] == REVIEW_SCHEMA:
        for prefix, manifest in [('sources/production', ratified_source_manifest(package)),
                                 ('evidence/harness', package['evidence']['source_manifest'])]:
            for row in manifest['files']:
                include_file(_state_source(root, state['source']) / row['relative_path'],
                             prefix + '/' + row['relative_path'], row['sha256_of_raw_bytes'])
    ancestor = package
    while ancestor.get('comment_amendment'):
        from trellis.pv_revision.comment_amendments import base_package
        ancestor = base_package(root, ancestor['comment_amendment']['base_review'])
        if ancestor.get('review_repackaging'):
            prior = ancestor['review_repackaging']['from_digest']
            base_package(root, prior)
            include_file(root / 'reviews' / prior / 'review.json', f'reviews/{prior}/review.json')
        prior = ancestor['digest']
        include_file(root / 'reviews' / prior / 'review.json', f'reviews/{prior}/review.json')
        include_tree(_state_source(root, ancestor['source']), 'sources/snapshots/' + ancestor['source_manifest']['source_tree_sha256'], ancestor['source_manifest'])
    snapshots = []
    for index, attempt in enumerate(state['attempts']):
        before, after = attempt['base']['source_tree_sha256'], attempt['manifest']['source_tree_sha256']
        include_tree(_state_source(root, attempt['base_source']), f'sources/snapshots/{before}', attempt['base'])
        include_tree(_state_source(root, attempt['source']), f'sources/snapshots/{after}', attempt['manifest'])
        patch = f'patches/{index:04d}.patch'
        files[patch] = package['task_diffs'][index]['diff']['patch'].encode('utf-8')
        snapshots.append({'attempt_id': attempt['id'], 'base': f'sources/snapshots/{before}',
                          'result': f'sources/snapshots/{after}', 'patch': patch})
        checks = attempt.get('checks') or {}
        for value, expected in checks.get('artifacts', {}).items():
            path = _state_source(root, value)
            include_file(path, path.relative_to(root).as_posix(), expected)
        if checks.get('evidence'):
            path = _state_source(root, checks['evidence'])
            include_file(path, path.relative_to(root).as_posix(), checks['evidence_sha256'])
    aggregate_receipts = list(state.get('aggregate_checks', {}).values())
    for observation in state.get('aggregate_check_history', []):
        aggregate_receipts.extend(observation['checks'].values())
    for checks in aggregate_receipts:
        for value, expected in checks.get('artifacts', {}).items():
            path = _state_source(root, value)
            include_file(path, path.relative_to(root).as_posix(), expected)
        if checks.get('evidence'):
            path = _state_source(root, checks['evidence'])
            include_file(path, path.relative_to(root).as_posix(), checks['evidence_sha256'])
    predecessor = root / 'predecessor'
    for name in ('protocol_state.json', 'record.json', 'tablet-manifest.json', 'README.md'):
        path = predecessor / name
        if path.exists():
            expected = state.get('handoff', {}).get('protocol_state_sha256') if name == 'protocol_state.json' else None
            include_file(path, 'predecessor/' + name, expected)
    if (predecessor / 'Tablet').exists():
        manifest_path = predecessor / 'tablet-manifest.json'
        if not manifest_path.is_file():
            raise RevisionStoreError('preserved proof sources lack their manifest')
        include_tree(predecessor / 'Tablet', 'predecessor/Tablet', _read(manifest_path))
    manifest = {'schema': 'trellis-pv-revision-export/v1', 'review_digest': package['digest'],
                'candidate_digest': package['candidate_digest'], 'task_snapshots': snapshots,
                'files': [{'path': name, 'byte_length': len(raw), 'sha256': hashlib.sha256(raw).hexdigest(),
                           'unix_mode': modes.get(name, 0o644)}
                          for name, raw in sorted(files.items())]}
    files['archive-manifest.json'] = canonical_json(manifest)
    if build_review_package_from_state(root, state) != package:
        raise RevisionStoreError('review evidence changed while preparing export')
    output = root / 'exports'
    output.mkdir(exist_ok=True)
    _state_source(root, str(output))
    target = output / (package['digest'] + '.zip')
    fd, temporary = tempfile.mkstemp(prefix='.export-', suffix='.zip', dir=output)
    os.close(fd)
    try:
        with zipfile.ZipFile(temporary, 'w', compression=zipfile.ZIP_DEFLATED) as archive:
            for name, raw in sorted(files.items()):
                info = zipfile.ZipInfo(name, date_time=(1980, 1, 1, 0, 0, 0))
                info.compress_type = zipfile.ZIP_DEFLATED
                info.external_attr = (0o100000 | modes.get(name, 0o644)) << 16
                archive.writestr(info, raw)
        with open(temporary, 'rb') as stream:
            os.fsync(stream.fileno())
        archive_digest = hashlib.sha256(Path(temporary).read_bytes()).hexdigest()
        if target.exists():
            if target.is_symlink() or hashlib.sha256(target.read_bytes()).hexdigest() != archive_digest:
                raise RevisionStoreError('immutable review export changed')
        else:
            os.replace(temporary, target)
            target.chmod(0o444)
        _write(target.with_suffix('.json'), {'schema': manifest['schema'], 'review_digest': package['digest'],
                                           'candidate_digest': package['candidate_digest'],
                                           'archive_sha256': archive_digest, 'file_count': len(files)}, immutable=True)
    finally:
        if os.path.exists(temporary):
            os.unlink(temporary)
    return target
