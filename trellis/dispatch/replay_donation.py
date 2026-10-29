"""One fixed host-only replay donor, installed after the candidate overlay."""
from __future__ import annotations

import json
import os
from pathlib import Path
import stat
import tempfile
import time
from dataclasses import dataclass

from . import candidate
from .acceptance_receipts import regular
from .publication import atomic
from .publication_artifacts import lexical_authority, paths
from trellis.checker import portable_replay as portable

MAX_COPY_BYTES = 4 << 30
MAX_OWNERS = 8192
MAX_ENVIRONMENTS = 128
MAX_COPY_SECONDS = 180
MAX_DONOR_HOPS = 8


def _marker(runtime):
    directory = Path(runtime).parent
    if directory.parent.name == 'finalization':
        from .staged_acceptance import donor
        return donor(runtime)
    retry_marker = directory / 'replay-cache-donor.json'
    if retry_marker.exists() and json.loads(regular(retry_marker)).get('donor_kind') == 'owned_capture_retry':
        from .staged_driver import owned_retry_donor
        return owned_retry_donor(runtime)
    if (directory / 'legacy-owner-donor.json').exists():
        from .legacy_acceptance import resolve
        return resolve(runtime)
    if (Path(runtime).name != 'runtime' or directory.parent.name != 'acceptance'
            or not directory.name.isdigit()):
        raise portable.Unavailable('not an isolated host acceptance')
    marker = json.loads(regular(directory / 'replay-cache-donor.json'))
    ordinal = marker.get('donor_round')
    if (marker.get('version') != 1 or not isinstance(ordinal, str) or not ordinal.isdigit()
            or int(ordinal) >= int(directory.name)):
        raise portable.Unavailable('invalid host donor ordinal')
    captured = json.loads(regular(directory / 'canonical.json'))
    if candidate.digest(candidate.canonical_json(captured['key'])) != marker['current_key_sha256']:
        raise portable.Unavailable('host donor preparation differs')
    previous = directory.parent / ordinal
    if marker.get('donor_kind') == 'drained_host_execution':
        from .acceptance_receipts import retry_cache_donor
        if (candidate.digest(regular(previous / 'execution/result.json')) != marker['donor_completion_sha256']
                or retry_cache_donor(directory, captured['key'], captured['config_sha256'], [previous]) != previous):
            raise portable.Unavailable('drained host artifact donor changed')
        # This terminal result is not a successful acceptance/owned receipt.
        # The installer checks actual portable replay evidence independently.
        return previous, marker, None
    if marker.get('donor_kind') is not None:
        raise portable.Unavailable('unknown host artifact donor kind')
    completed = regular(previous / 'completed.json')
    if candidate.digest(completed) != marker['donor_completion_sha256']:
        raise portable.Unavailable('host donor completion changed')
    # Full authentication already ran once outside the coordinator lock. This
    # fixed marker is host-created and inaccessible to workers/elaborators.
    return previous, marker, json.loads(completed)


def _copy(source, destination, budget):
    """Independent regular copies, bounded memory/total bytes even on failure."""
    for path in (source, destination):
        if any(p.is_symlink() for p in (path, *path.parents)):
            raise portable.Unavailable('artifact copy traverses a symlink')
    info = source.stat()
    if not stat.S_ISREG(info.st_mode) or info.st_nlink != 1 or info.st_size > budget[0]:
        raise portable.Unavailable('artifact copy unavailable or over budget')
    destination.parent.mkdir(parents=True, exist_ok=True)
    fd, name = tempfile.mkstemp(prefix='.replay-', dir=destination.parent)
    try:
        with os.fdopen(fd, 'wb') as out, source.open('rb') as inp:
            for chunk in iter(lambda: inp.read(1 << 20), b''):
                if len(chunk) > budget[0] or time.monotonic() > budget[1]:
                    raise portable.Unavailable('artifact copy budget exhausted')
                budget[0] -= len(chunk)
                out.write(chunk)
            out.flush()
            os.fsync(out.fileno())
        os.replace(name, destination)
    finally:
        Path(name).unlink(missing_ok=True)


def _environment(source, identity, environment_source):
    try:
        return portable.environment_for(source, identity), False
    except FileNotFoundError:
        if environment_source is None:
            raise
        return portable.environment_for(environment_source, identity), True


def _owned_observation(runtime, previous, marker, completed):
    """Only a completed owned stage may supply an omitted replay inventory."""
    if Path(runtime).parent.parent.name != 'finalization':
        return None
    return _saved_owned_observation(previous, marker, completed)


def _saved_owned_observation(previous, marker, completed):
    raw = regular(previous / 'execution/response.json')
    if len(raw) > 32 << 20 or candidate.digest(raw) != completed['files']['execution/response.json']:
        raise portable.Unavailable('saved owner facts changed')
    output = json.loads(raw)
    facts = output.get('facts') or {}
    owners = marker.get('owner_nodes')
    observed = facts.get('observation_environment')
    if (output.get('stage') != 'owned' or output.get('status') != 'facts'
            or facts.get('schema_version') != 1 or facts.get('owner_ok') is not True
            or not isinstance(owners, list) or not owners or len(owners) > MAX_OWNERS
            or facts.get('owner_nodes') != owners or not isinstance(observed, dict)
            or not isinstance(observed.get('artifact_files'), dict)
            or not set(owners) <= set(observed.get('environment_sha256s', {}))):
        raise portable.Unavailable('saved owner environment coverage unavailable')
    return owners, observed


def _fresh_owned_capture(runtime):
    directory = Path(runtime).parent
    return (directory.parent.name == 'acceptance' and directory.name.isdigit()
            and 'staged_plan' in json.loads(regular(directory / 'canonical.json')))


def _artifact_witness(view, repo, nodes):
    return {str(portable.bound_path(label, repo, view.environment['sysroot'])):
            binding['sha256'] if binding is not None else None
            for label, binding in view.files_for(nodes).items()}


def _bounded_retry_chain(directory):
    # Existing owned-retry authentication recursively authenticates its prior
    # completion. Bound that depth before entering it, not merely afterward.
    for _ in range(MAX_DONOR_HOPS + 1):
        path = directory / 'replay-cache-donor.json'
        if not path.exists():
            return
        marker = json.loads(regular(path))
        if marker.get('donor_kind') != 'owned_capture_retry':
            return
        ordinal = marker.get('donor_round')
        if not isinstance(ordinal, str) or not ordinal.isdigit() or int(ordinal) >= int(directory.name):
            raise portable.Unavailable('saved owner donor chain changed')
        directory = directory.parent / ordinal
    raise portable.Unavailable('saved owner donor chain limit')



@dataclass(frozen=True)
class _OwnerBinding:
    """Compact comparison operands; never a replacement for the file witness."""
    digest: str
    imports: str
    adapter: str | None
    adapter_label: str
    adapter_error: str | None

    @classmethod
    def capture(cls, view, node):
        # Preserve same_owner's raw-equality fast path: a legacy manifest with
        # no relocatable adapter is still usable at its exact original binding.
        try:
            adapter, label, error = view.adapter_comparison(), view._adapter_label, None
        except portable.Unavailable as exc:
            adapter, label, error = None, '', str(exc)
        return cls(view.owner_digest(node), view._module_digest('Tablet.' + node),
                   adapter, label, error)

    def matches(self, other):
        if self.digest == other.digest:
            return True
        if self.adapter_error or other.adapter_error:
            raise portable.Unavailable(self.adapter_error or other.adapter_error)
        return self.adapter == other.adapter and self.imports == other.imports


def _recover_owned_alias(previous, marker, completed, current, *, environment_source=None):
    """Match both halves of a historical aliased observation without rewriting it."""
    owners, observed = _saved_owned_observation(previous, marker, completed)
    _bounded_retry_chain(previous)
    from trellis.atomic_actions import observations as obs
    from . import staged_acceptance as stages
    seen = set()
    def inventories(directory, receipt):
        repo = lexical_authority(directory / 'repo')
        identities = set()
        for node in owners:
            record = obs._read_olean_provenance(repo, node)
            identity = (record.get('kernel_replay') or {}).get('portable_environment_sha256')
            if identity is not None:
                identities.add(identity)
        raw = regular(directory / 'execution/response.json')
        if len(raw) > 32 << 20 or candidate.digest(raw) != receipt['files']['execution/response.json']:
            raise portable.Unavailable('saved donor response changed')
        captures = (json.loads(raw).get('replay_donation') or {}).get('captures', [])
        if not isinstance(captures, list) or len(captures) > MAX_ENVIRONMENTS:
            raise portable.Unavailable('saved donor capture inventory limit')
        identities.update(row['environment_sha256'] for row in captures
                          if isinstance(row, dict) and row.get('status') == 'captured'
                          and 'environment_sha256' in row)
        for identity in sorted(identities):
            seen.add(identity)
            if len(seen) > MAX_ENVIRONMENTS:
                raise portable.Unavailable('environment inventory limit')
            # One individually bounded, digest-verified document at a time.
            environment, _ = _environment(repo, identity, environment_source)
            yield portable.EnvironmentView(environment)

    # New owner observations once inherited an older donor's commitment while
    # retaining their fresh file witness. Require the full immediate witness,
    # including absences, before considering any historical commitment.
    source = lexical_authority(previous / 'repo')
    witnesses, immediate = [], []
    for view in inventories(previous, completed):
        bindings = {node: _OwnerBinding.capture(view, node) for node in owners
                    if 'Tablet.' + node in view.modules}
        immediate.append(bindings)
        # Check the complete immediate witness, including absent files, before
        # dropping the manifest. A compact digest alone cannot attest this.
        if len(bindings) == len(owners) and _artifact_witness(view, source, owners) == observed['artifact_files']:
            witnesses.append(bindings)
    if not witnesses:
        raise portable.Unavailable('saved owner file witness has no retained manifest')
    commitments = observed['environment_sha256s']
    current_bindings = {node: _OwnerBinding.capture(current, node) for node in owners}
    matched, relocations = {}, []
    directory, receipt, candidates = previous, completed, immediate
    for hop in range(MAX_DONOR_HOPS + 1):
        for bindings in candidates:
            for node in owners:
                if node in matched or node not in bindings:
                    continue
                binding, now = bindings[node], current_bindings[node]
                if binding.digest != commitments[node]:
                    continue
                if not binding.matches(now) or not any(binding.matches(witness[node]) for witness in witnesses):
                    raise portable.Unavailable('saved owner imported environment changed: ' + node)
                # Historical alias recovery also requires a unique adapter.
                if binding.adapter_error or now.adapter_error:
                    raise portable.Unavailable(binding.adapter_error or now.adapter_error)
                matched[node] = commitments[node]
                relocation = dict(previous=binding.adapter_label[:512], current=now.adapter_label[:512])
                if relocation not in relocations:
                    relocations.append(relocation)
        if len(matched) == len(owners):
            if len(relocations) != 1:
                raise portable.Unavailable('saved owner adapter bindings disagree')
            return matched, relocations[0]
        if hop == MAX_DONOR_HOPS:
            break
        retry = directory / 'replay-cache-donor.json'
        legacy = directory / 'legacy-owner-donor.json'
        selected = retry if retry.exists() and json.loads(regular(retry)).get('donor_kind') == 'owned_capture_retry' else legacy
        if not selected.exists():
            break
        if candidate.digest(regular(selected)) != receipt['files'].get(selected.name):
            raise portable.Unavailable('saved owner donor marker changed')
        if selected == legacy:
            from .legacy_acceptance import resolve
            from .acceptance_receipts import implementation_identity
            consumer = receipt['key']['implementation_identity']
            next_directory, _next_marker, next_receipt = resolve(directory / 'runtime',
                historical_consumer=consumer if consumer != implementation_identity() else None)
        else:
            next_directory, _next_marker, next_receipt = _marker(directory / 'runtime')
        if (next_directory.parent != previous.parent or not next_directory.name.isdigit()
                or int(next_directory.name) >= int(directory.name)
                or any(next_receipt['key'].get(name) != completed['key'].get(name)
                       for name in stages._STABLE_KEY_FIELDS)):
            raise portable.Unavailable('saved owner donor chain changed')
        directory, receipt = next_directory, next_receipt
        candidates = ({node: _OwnerBinding.capture(view, node) for node in owners
                       if 'Tablet.' + node in view.modules}
                      for view in inventories(directory, receipt))
    raise portable.Unavailable('saved owner commitment has no retained donor manifest')


def recover_owned_alias(runtime, current_view, *, environment_source=None, historical_consumer=None):
    """Read-only preflight; current finalization must still capture actual bytes."""
    if historical_consumer is None:
        previous, marker, completed = _marker(runtime)
    else:
        # Only the driver's already-authenticated completed-Cold preflight
        # supplies this existing historical evidence-authentication seam.
        from .staged_acceptance import donor
        previous, marker, completed = donor(runtime, historical_consumer=historical_consumer)
    return _recover_owned_alias(previous, marker, completed, current_view,
                                environment_source=environment_source)


def _match_owned_alias(previous, marker, completed, current, observed, owners, *, environment_source=None):
    try:
        return current.match_owned_observation(observed, owners)
    except portable.Unavailable:
        return _recover_owned_alias(previous, marker, completed, current,
                                    environment_source=environment_source)


def current_owner_aliases(workspace, current, nodes, *, environment_source=None):
    """Reattest only an equal donor environment with relocated host adapter."""
    directory = Path(workspace.runtime_root).parent
    if not any((directory / name).is_file() for name in
               ('staged-donor.json', 'legacy-owner-donor.json', 'replay-cache-donor.json')):
        return {}, []
    if _fresh_owned_capture(workspace.runtime_root):
        return {}, []  # New facts bind this capture, not their artifact donor.
    from trellis.atomic_actions import observations as obs
    previous, marker, completed = _marker(workspace.runtime_root)
    source = lexical_authority(previous / 'repo')
    owned = _owned_observation(workspace.runtime_root, previous, marker, completed)
    allowed = None
    if 'owner_nodes' in marker:
        allowed = set(obs.SourceClosureSnapshot(source).materialization_order(marker['owner_nodes']))
    groups, aliases, relocations = {}, {}, []
    missing = set()
    for node in sorted(set(nodes)):
        paths(node)
        if allowed is not None and node not in allowed:
            continue
        record = obs._read_olean_provenance(source, node)
        identity = (record.get('kernel_replay') or {}).get('portable_environment_sha256')
        if not identity:
            missing.add(node)
            continue
        groups.setdefault(identity, []).append(node)
        if len(groups) > MAX_ENVIRONMENTS:
            raise portable.Unavailable('environment inventory limit')
    for identity, members in groups.items():
        environment, _ = _environment(source, identity, environment_source)
        old = portable.EnvironmentView(environment)
        for node in members:
            historical = old.owner_digest(node)
            if historical != current.owner_digest(node) and old.same_owner(current, node):
                aliases[node] = historical
                relocation = dict(previous=old._adapter_label[:512], current=current._adapter_label[:512])
                if len(relocations) < 8 and relocation not in relocations:
                    relocations.append(relocation)
        del old, environment
    if owned is not None:
        owners, observed = owned
        if not missing <= set(owners):
            raise portable.Unavailable('saved owner capture exceeds observed roots')
        needed = set(nodes) & set(owners)
        if missing or any(aliases.get(node, current.owner_digest(node)) != observed['environment_sha256s'][node]
                          for node in needed):
            commitments, relocation = _match_owned_alias(previous, marker, completed, current, observed, owners,
                                                         environment_source=environment_source)
            aliases.update({node: commitments[node] for node in needed})
            if relocation not in relocations and len(relocations) < 8:
                relocations.append(relocation)
    return aliases, relocations


def install(workspace, *, environment_source=None):
    """Called only under the checker workspace writer and sync exclusion locks."""
    from trellis.atomic_actions import observations as obs
    start = time.monotonic()
    result = dict(version=1, status='cold', copied_nodes=[], covered_nodes=[], captures=[])
    touched = set()
    retained = set()
    authority = workspace.supervisor_repo
    def skipped(node, reason):
        summary = result.setdefault('skipped', {})
        summary[reason] = summary.get(reason, 0) + 1
        samples = result.setdefault('skipped_samples', [])
        if len(samples) < 8:
            samples.append(dict(node=node[:80], reason=reason))
    try:
        previous, marker, _completed = _marker(workspace.runtime_root)
        if not portable.acceptance_workspace(authority):
            raise portable.Unavailable('destination is not host acceptance authority')
        source = lexical_authority(previous / 'repo')
        try:
            owned = _owned_observation(workspace.runtime_root, previous, marker, _completed)
        except (ValueError, KeyError, TypeError):
            result['failure_class'] = 'saved_owner_binding'
            raise
        unstamped = set()
        source_snapshot = obs.SourceClosureSnapshot(source)
        snapshot = obs.SourceClosureSnapshot(authority)
        allowed = None
        if 'owner_nodes' in marker:
            owners = marker['owner_nodes']
            if not isinstance(owners, list) or not owners or len(owners) > MAX_OWNERS:
                raise portable.Unavailable('invalid proof donor artifact roots')
            for node in owners:
                paths(node)
            allowed = set(source_snapshot.materialization_order(owners))
        records, environments = {}, set()
        root = source / '.lake/build/lib/lean/Tablet'
        with os.scandir(root) as entries:
            for index, entry in enumerate(entries):
                if index >= MAX_OWNERS * 16:
                    raise portable.Unavailable('artifact inventory limit')
                if not entry.name.endswith('.olean'):
                    continue
                node = entry.name[:-6]
                paths(node)  # Exact ordinary owner/path allowlist.
                if allowed is not None and node not in allowed:
                    continue
                if len(records) >= MAX_OWNERS:
                    raise portable.Unavailable('artifact owner limit')
                record = obs._read_olean_provenance(source, node)
                replay = record.get('kernel_replay')
                identity = replay.get('portable_environment_sha256') if isinstance(replay, dict) else None
                if (not isinstance(identity, str) or len(identity) != 64
                        or any(c not in '0123456789abcdef' for c in identity)):
                    if owned is None or identity is not None:
                        skipped(node, 'no portable replay attestation')
                        continue
                    unstamped.add(node)
                if (not obs.olean_is_content_current(source, node, source_snapshot=source_snapshot)
                        or not obs.olean_has_current_kernel_replay(source, node)
                        or snapshot.closure_hash(node) != source_snapshot.closure_hash(node)):
                    skipped(node, 'source or replay attestation changed')
                    continue
                if identity is not None and identity not in environments:
                    if len(environments) >= MAX_ENVIRONMENTS:
                        raise portable.Unavailable('environment inventory limit')
                    environments.add(identity)
                records[node] = record
        if unstamped and not set(owned[0]) <= set(records):
            result['failure_class'] = 'saved_owner_binding'
            raise portable.Unavailable('saved owner lacks current ordinary replay coverage')
        unstamped.intersection_update(records)
        if not records:
            raise portable.Unavailable('no portable, source-current host artifacts')
        # Authenticate every bounded document before copying or compiling.
        # Retain only its owners' comparison operands, not the large inventory.
        bindings = {}
        for identity in sorted(environments):
            environment, recovered = _environment(source, identity, environment_source)
            if recovered:
                result.setdefault('canonical_environment_manifests', []).append(identity)
            view = portable.EnvironmentView(environment)
            for node, record in records.items():
                if record['kernel_replay'].get('portable_environment_sha256') == identity:
                    bindings[node] = _OwnerBinding.capture(view, node)
            del view, environment
        workspace._invalidate_compile_cache_for_sync([node + '.lean' for node in records])
        budget = [MAX_COPY_BYTES, time.monotonic() + MAX_COPY_SECONDS]
        for node, record in records.items():
            touched.add(node)
            # Never expose a half-copied replay capability, including on crash.
            stripped = dict(record)
            stripped.pop('kernel_replay', None)
            atomic(obs._olean_srcclosure_sidecar_path(authority, node), candidate.canonical_json(stripped))
            for name in paths(node):
                if name.endswith('.olean.srcclosure'):
                    continue
                old, new = source / '.lake/build' / name, authority / '.lake/build' / name
                if old.exists() or old.is_symlink():
                    _copy(old, new, budget)
                elif new.exists() or new.is_symlink():
                    if new.is_symlink():
                        raise portable.Unavailable('artifact destination is symlinked')
                    new.unlink()
        files = {}
        owner_environments = {}
        # All historical replay batches share ONE current resolver inventory.
        # Owner DAG projections exclude irrelevant batch siblings afterward.
        fresh, stats = portable.capture(authority, list(records),
            timeout_secs=portable.CAPTURE_SECONDS, bwrap_role='lake_compiler',
            telemetry_origin='donation_install')
        result['captures'].append(stats)
        if fresh is None:
            raise portable.Unavailable('current imported environment unavailable')
        fresh_identity = portable.digest(fresh)
        atomic(authority / portable.DIRECTORY / (fresh_identity + '.json'), portable._json(fresh))
        current_view = portable.EnvironmentView(fresh)
        recovered_commitments = {}
        if unstamped:
            owners, observed = owned
            try:
                recovered_commitments, relocation = _match_owned_alias(previous, marker, _completed,
                    current_view, observed, owners, environment_source=environment_source)
            except (ValueError, KeyError, TypeError):
                result['failure_class'] = 'saved_owner_binding'
                raise
            covered = current_view.tablet_import_closure(owners)
            for node in unstamped - covered:
                skipped(node, 'outside saved owner import closure')
                records.pop(node)
            unstamped.intersection_update(covered)
            for node in unstamped:
                # A destination capability is granted only after exact original
                # owned commitments and ordinary replay/source checks agree.
                records[node] = dict(records[node], kernel_replay=dict(records[node]['kernel_replay'],
                    portable_environment_sha256=fresh_identity))
            for node in unstamped:
                bindings[node] = _OwnerBinding.capture(current_view, node)
            environments.add(fresh_identity)
            result['owned_environment_recovered_nodes'] = sorted(unstamped)
            result.setdefault('adapter_relocations', []).append(relocation)
        for identity in sorted(environments):
            owners = [node for node, record in records.items()
                      if record['kernel_replay'].get('portable_environment_sha256') == identity]
            group_retained = []
            for node in owners:
                record = records[node]
                before = bindings[node]
                now = _OwnerBinding.capture(current_view, node)
                commitment = before.digest
                if not before.matches(now):
                    skipped(node, 'import environment changed')
                    continue
                if current_view.owner_digest(node) != commitment:
                    result.setdefault('adapter_relocation_nodes', []).append(node)
                    relocation = dict(previous=before.adapter_label[:512], current=now.adapter_label[:512])
                    relocations = result.setdefault('adapter_relocations', [])
                    if len(relocations) < 8 and relocation not in relocations:
                        relocations.append(relocation)
                if (obs._artifact_bundle(authority, node) != record['kernel_replay']['artifact_bundle']
                        or not obs.olean_is_content_current(authority, node, source_snapshot=snapshot)):
                    skipped(node, 'artifact or source changed after copy')
                    continue
                # Rewrite path labels from the freshly resolved environment.
                # These labels are not evidence; ordered bytes remain authority.
                current = portable.rebind_record(authority, node, record, fresh)
                current['kernel_replay']['portable_environment_sha256'] = fresh_identity
                if obs._write_olean_provenance(authority, node, current):
                    retained.add(node)
                    group_retained.append(node)
                    owner_environments[node] = recovered_commitments.get(node, commitment)
                else:
                    skipped(node, 'provenance write failed')
            for label, binding in current_view.files_for(group_retained).items():
                path = str(portable.bound_path(label, authority, fresh['sysroot']))
                value = binding['sha256'] if binding is not None else None
                if path in files and files[path] != value:
                    raise portable.Unavailable('inconsistent portable environment bindings')
                files[path] = value
        # Missing dependency owners cannot borrow this root's capability.
        covered, dependency_checks = _dependency_coverage(authority, snapshot, retained)
        result['dependency_checks'] = dependency_checks
        for node in retained - covered:
            skipped(node, 'dependency replay unavailable')
            retained.remove(node)
            owner_environments.pop(node, None)
        if not snapshot.is_current() or not source_snapshot.is_current():
            raise portable.Unavailable('source changed during dependency validation')
        result.update(status='installed' if retained else 'cold', copied_nodes=sorted(retained),
            covered_nodes=sorted(retained), bytes_copied=MAX_COPY_BYTES - budget[0],
            donor_round=marker['donor_round'], donor_completion_sha256=marker['donor_completion_sha256'],
            environment_sha256s=owner_environments, artifact_files=files)
        if not retained:
            result['reason'] = 'no replay artifacts retained: ' + ', '.join(sorted(result.get('skipped', {})))
        if len(portable._json(files)) > portable.MAX_MANIFEST_BYTES:
            result.pop('artifact_files')
            result['fact_reuse_unavailable'] = 'artifact witness byte limit'
        return result
    except (OSError, ValueError, KeyError, TypeError, RecursionError, candidate.CandidateError,
            obs.SourceClosureSnapshotTooLarge) as exc:
        retained.clear()
        result.update(status='cold', reason=str(exc)[:240], copied_nodes=[], covered_nodes=[])
        return result
    finally:
        for node in touched - retained:
            obs._purge_olean_artifacts(authority, node)
        result['wall_secs'] = time.monotonic() - start


def _dependency_coverage(authority, snapshot, roots):
    """Deduplicate overlapping checks within the caller's writer/sync locks.

    No artifact writes or compilation may occur in this interval. A fresh
    second pass re-reads content provenance and the full replay artifact bundle
    before any cached success is used. The cache never survives this call.
    Small or oversized inventories retain the scalar path.
    """
    from trellis.atomic_actions import observations as obs
    roots = sorted(roots)
    unique, references = set(), 0
    bounded = True
    for node in roots:
        dependencies = snapshot.materialization_order([node])
        references += len(dependencies)
        unique.update(dependencies)
        if len(unique) > MAX_OWNERS:
            bounded = False
            break
    checks = 0

    def current(dep):
        nonlocal checks
        checks += 1
        return (obs.olean_is_content_current(authority, dep, source_snapshot=snapshot)
                and obs.olean_has_current_kernel_replay(authority, dep))

    if not bounded or references <= 4 * len(unique):
        covered = {node for node in roots
                   if all(current(dep) for dep in snapshot.materialization_order([node]))}
        return covered, dict(mode='scalar', validations=checks)

    valid = {}
    def available(dep):
        if dep not in valid:
            valid[dep] = current(dep)
        return valid[dep]
    # Keep the scalar path's early exit on each failed closure. In particular,
    # a missing common leaf need not trigger reads of every dependent artifact.
    eligible = {node for node in roots
                if all(available(dep) for dep in snapshot.materialization_order([node]))}
    # Re-read every successful dependency; failures remain failures for this
    # operation. This preserves exact optional-part and replay-policy checks.
    for dep in valid:
        if valid[dep]:
            valid[dep] = current(dep)
    covered = {node for node in eligible
               if all(valid[dep] for dep in snapshot.materialization_order([node]))}
    return covered, dict(mode='shared_dependencies', references=references,
                         unique=len(unique), validations=checks)


def install_via_checker(runtime):
    from trellis.atomic_actions.checker_client import _round_trip, _next_request_id
    runtime = Path(runtime)
    if not any((runtime.parent / name).is_file()
               for name in ('replay-cache-donor.json', 'staged-donor.json', 'legacy-owner-donor.json')):
        return None
    result = _round_trip(Path(os.environ['TRELLIS_CHECKER_SOCKET']),
        dict(op='seed_acceptance_replay', request_id=_next_request_id()), timeout_secs=3600)
    # Keep counters and content-bound provenance in the fresh host response.
    return result.get('replay_donation')


def validation_fact_donor(runtime, witness):
    """Only host-selected successful facts; native rechecks current operands."""
    try:
        if (not isinstance(witness, dict) or witness.get('status') != 'installed'
                or not isinstance(witness.get('artifact_files'), dict)):
            return None
        previous, marker, completed = _marker(runtime)
        if marker.get('donor_kind') in ('drained_host_execution', 'owned_capture_retry'):
            return None  # No old response, validation facts or verdict authority.
        if any(witness.get(key) != marker[key] for key in ('donor_round', 'donor_completion_sha256')):
            return None
        path = previous / 'execution/response.json'
        from .retained_files import logical_size
        if logical_size(path) > 32 << 20:
            return None
        raw = regular(path)
        if candidate.digest(raw) != completed['files']['execution/response.json']:
            return None
        response = json.loads(raw)
        receipt = response.get('validation_fact_receipt')
        results = response.get('validation_step_results')
        if (not isinstance(receipt, dict) or receipt.get('schema_version') != 3
                or not isinstance(results, list)):
            return None
        # Native emits these roots from the selector used by the actual plan
        # (active owner AND approved correspondence targets). Imported bytes
        # are bound transitively; unrelated materialized owners are not reads.
        owners = receipt.get('artifact_owners')
        if (not isinstance(owners, list) or not owners or len(owners) > MAX_OWNERS
                or not all(isinstance(node, str) for node in owners)
                or not set(owners) <= set(witness['covered_nodes'])):
            return None
        for node in owners:
            paths(node)
        return dict(receipt=receipt, step_results=results, environment=witness)
    except (OSError, ValueError, KeyError, TypeError, candidate.CandidateError):
        return None


def validation_fact_context(runtime):
    """Bind named host relocation carriers, never worker-supplied path rules."""
    try:
        directory = Path(runtime).parent
        canonical = json.loads(regular(directory / 'canonical.json'))
        prepared = json.loads(regular(directory / 'prepared.json'))
        context = dict(canonical_config_sha256=canonical['config_sha256'],
                       prepared_config_sha256=prepared['prepared_config_sha256'])
        worker = directory / 'repo'
        authority = lexical_authority(worker)
        marker = authority / '.trellis-supervisor.json'
        if marker.exists() or marker.is_symlink():
            raw = regular(marker)
            expected = dict(worker_repo_path=str(worker.absolute()),
                supervisor_repo_path=str(authority), supervisor_home=str(authority.parent / 'home'),
                supervisor_cache=str(authority.parent / 'cache'))
            if json.loads(raw) != expected:
                raise candidate.CandidateError('supervisor marker differs from its host workspace')
            context['supervisor_marker'] = dict(
                canonical_sha256=canonical['source']['input_hashes'].get('.trellis-supervisor.json'),
                prepared_sha256=candidate.digest(raw), expected=expected)
        return context
    except (OSError, ValueError, KeyError, TypeError, candidate.CandidateError):
        return None
