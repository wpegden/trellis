"""Portable, evidence-preserving archives of an entire native PV campaign.

This module packages existing kernel and controller evidence. It does not
verify Lean proofs, manufacture approvals, or replace the required-v1 archive.
"""
from __future__ import annotations

import hashlib
import json
import os
from pathlib import Path
import shutil
import stat
import tempfile
import zipfile

from scripts.pv_source_tree import _relative_path, canonical_json
from trellis.pv_revision.store import (
    _write, build_review_package_from_state, digest,
    export_review_package, source_manifest,
)


class CampaignArchiveError(ValueError):
    pass


SCHEMA = 'trellis-pv-campaign-archive/v1'


def _file(path: Path) -> bytes:
    """Read a regular file, refusing links in the evidence path."""
    for part in (path, *path.parents):
        if part.is_symlink():
            raise CampaignArchiveError(f'evidence path contains a symlink: {path}')
    flags = os.O_RDONLY | getattr(os, 'O_NOFOLLOW', 0)
    descriptor = os.open(path, flags)
    try:
        if not stat.S_ISREG(os.fstat(descriptor).st_mode):
            raise CampaignArchiveError(f'evidence is not a regular file: {path}')
        with os.fdopen(descriptor, 'rb', closefd=False) as stream:
            return stream.read()
    finally:
        os.close(descriptor)


def _json(path: Path) -> dict:
    value = json.loads(_file(path))
    if not isinstance(value, dict):
        raise CampaignArchiveError(f'evidence must be an object: {path}')
    return value


def _sha(raw: bytes) -> str:
    return hashlib.sha256(raw).hexdigest()


def _relative(root: Path, name: str) -> Path:
    if not isinstance(name, str):
        raise CampaignArchiveError('evidence relative path must be text')
    _relative_path(tuple(name.split('/')))
    return root / name


def _outcome(runtime: Path, protocol: dict) -> str:
    revision = runtime / 'pv-revision/state.json'
    if revision.is_file() and _json(revision).get('status') == 'all_requirements_retired':
        return 'all_requirements_retired'
    return 'complete' if protocol.get('phase') == 'Complete' else 'incomplete'


def _discover(runtime: Path, *, allow_incomplete: bool) -> tuple[list[dict], list[dict]]:
    """Read only small controller records, following direct predecessor links."""
    generations, gaps, seen = [], [], set()
    expected = None
    current = runtime
    while True:
        if current in seen:
            raise CampaignArchiveError('campaign predecessor chain contains a cycle')
        seen.add(current)
        try:
            protocol = _json(current / 'protocol_state.json')
            metadata = _json(current / 'runtime_metadata.json')
            repo = Path(metadata['repo_path']).absolute()
            manifest = _json(repo / 'modeling-input/manifest.json')
            if manifest.get('provenance_schema') != 'trellis-native-preparation/v1':
                raise CampaignArchiveError('generation lacks native PV preparation provenance')
            preparation = Path(manifest['preparation_root']).absolute()
            if not preparation.is_dir():
                raise CampaignArchiveError(f'linked preparation is missing: {preparation}')
            if expected and (repo != Path(expected['repo']).absolute() or preparation != Path(expected['preparation']).absolute()):
                raise CampaignArchiveError('predecessor ownership disagrees with its recorded generation')
            receipts = [(kind, repo / 'modeling-input' / name)
                        for kind, name in [('revision', 'revision-from.json'), ('reopening', 'reopened-from.json')]
                        if (repo / 'modeling-input' / name).is_file()]
            if len(receipts) > 1:
                raise CampaignArchiveError('generation has ambiguous direct predecessor receipts')
            receipt = _json(receipts[0][1]) if receipts else None
            generations.append({'runtime': current, 'repo': repo, 'preparation': preparation,
                                'protocol': protocol, 'manifest': manifest, 'expected': expected,
                                'link_kind': receipts[0][0] if receipts else None,
                                'link_receipt': receipt, 'link_path': receipts[0][1] if receipts else None})
            if not receipt:
                break
            parent = receipt.get('predecessor') if receipts[0][0] == 'revision' else receipt
            if not isinstance(parent, dict) or any(key not in parent for key in ('runtime', 'repo', 'preparation')):
                raise CampaignArchiveError('predecessor receipt lacks generation identities')
            expected = dict(parent)
            if receipts[0][0] == 'revision':
                expected['_revision_root'] = receipt['revision_root']
                expected['_package_digest'] = receipt['package_digest']
            current = Path(parent['runtime']).absolute()
        except (OSError, ValueError, KeyError) as error:
            if not allow_incomplete:
                raise CampaignArchiveError(f'cannot read campaign predecessor: {error}') from error
            gaps.append({'kind': 'missing_or_invalid_generation', 'detail': str(error), 'runtime': str(current)})
            break
    generations.reverse()
    return generations, gaps


def campaign_status(runtime: Path) -> dict | None:
    """Cheap viewer status; saved export identities are not reverified on poll."""
    runtime = Path(runtime).absolute()
    metadata = runtime / 'runtime_metadata.json'
    if not metadata.is_file():
        return None
    outcome = 'unknown'
    try:
        protocol_raw = _file(runtime / 'protocol_state.json')
        protocol = json.loads(protocol_raw)
        outcome = _outcome(runtime, protocol)
        repo = Path(_json(metadata)['repo_path']).absolute()
        provenance = repo / 'modeling-input/manifest.json'
        if not provenance.is_file() or _json(provenance).get('provenance_schema') != 'trellis-native-preparation/v1':
            return None
        generations, gaps = _discover(runtime, allow_incomplete=True)
        result = {'schema': SCHEMA, 'can_export_final': outcome != 'incomplete' and not gaps,
                  'ready_for_final': outcome != 'incomplete' and not gaps, 'terminal_outcome': outcome,
                  'generation_count': len(generations), 'history_completeness': 'not_checked', 'gaps': gaps}
        if gaps:
            result['error'] = gaps[0]['detail']
        if protocol.get('trust_base', {}).get('mode') == 'required_v1' and outcome == 'complete' and not (runtime / 'trust_package.archive').is_file():
            result.update(can_export_final=False, ready_for_final=False, error='required-v1 final trust archive is missing')
        latest = runtime / 'campaign-archives/latest.json'
        if latest.is_file():
            receipt = _json(latest)
            if receipt.get('protocol_state_sha256') == _sha(protocol_raw) and receipt.get('terminal_outcome') == outcome:
                result['history_completeness'] = receipt.get('history_completeness', 'not_checked')
                result['gaps'] = receipt.get('gaps', gaps)
                if receipt.get('final'):
                    archive_file = _relative(runtime / 'campaign-archives', receipt['archive'])
                    if not archive_file.is_file() or archive_file.is_symlink():
                        raise CampaignArchiveError('saved campaign archive is missing or unsafe')
                    result['final_archive'] = {'path': str(archive_file),
                                               'sha256': receipt['archive_sha256'],
                                               'verification': 'saved_export_receipt'}
        failure = runtime / 'campaign-archives/last-error.json'
        if failure.is_file() and ('final_archive' not in result or failure.stat().st_mtime_ns > latest.stat().st_mtime_ns):
            result.pop('final_archive', None)
            result.update(can_export_final=False, ready_for_final=False)
            result['last_export_error'] = _json(failure)
            result['error'] = result['last_export_error'].get('error', 'The last campaign export failed')
        return result
    except (OSError, ValueError, KeyError) as error:
        return {'schema': SCHEMA, 'can_export_final': False, 'ready_for_final': False,
                'terminal_outcome': outcome, 'gaps': [{'kind': 'invalid_native_evidence', 'detail': str(error)}],
                'error': str(error)}


class _Bundle:
    def __init__(self, folder: Path):
        self.folder = folder
        self.rows: dict[str, dict] = {}
        self.inputs: dict[Path, str] = {}
        self.trees: dict[Path, dict] = {}
        self.external_staging: list[Path] = []

    def bytes(self, name: str, raw: bytes, *, mode: int = 0o644) -> None:
        target = _relative(self.folder, name)
        row = {'path': name, 'sha256': _sha(raw), 'byte_length': len(raw), 'unix_mode': mode}
        if name in self.rows and self.rows[name] != row:
            raise CampaignArchiveError('archive evidence path collision')
        target.parent.mkdir(parents=True, exist_ok=True)
        target.write_bytes(raw)
        self.rows[name] = row

    def file(self, path: Path, name: str, *, expected: str | None = None) -> None:
        raw = _file(path)
        actual = _sha(raw)
        if expected is not None and actual != expected:
            raise CampaignArchiveError(f'evidence hash mismatch: {path}')
        self.inputs[path] = actual
        self.bytes(name, raw, mode=path.stat().st_mode & 0o777)

    def tree(self, path: Path, name: str, *, expected: dict | None = None) -> dict:
        manifest = source_manifest(path)
        if expected is not None and manifest != expected:
            raise CampaignArchiveError(f'source or proof snapshot changed: {path}')
        self.trees[path] = manifest
        for row in manifest['files']:
            self.file(path / row['relative_path'], name + '/' + row['relative_path'], expected=row['sha256_of_raw_bytes'])
        return manifest

    def revalidate(self) -> None:
        for path, expected in self.inputs.items():
            if _sha(_file(path)) != expected:
                raise CampaignArchiveError(f'evidence changed during export: {path}')
        for path, expected in self.trees.items():
            if source_manifest(path) != expected:
                raise CampaignArchiveError(f'evidence tree changed during export: {path}')


def _generation(bundle: _Bundle, row: dict, index: int, *, allow_incomplete: bool, gaps: list) -> dict:
    from scripts.extract_pv_model import source_input_manifest
    repo, preparation, runtime = row['repo'], row['preparation'], row['runtime']
    prefix = f'generations/{index:04d}'
    protocol, manifest, expected = row['protocol'], row['manifest'], row['expected'] or {}
    bundle.file(runtime / 'protocol_state.json', prefix + '/protocol_state.json', expected=expected.get('protocol_state_sha256'))
    # One shared object tree serves every generation in this portable bundle.
    # Exact repeated contracts are not copied under each generation prefix.
    from trellis.dispatch.history import referenced_requests, object_bytes, DIRECTORY
    for reference in referenced_requests(protocol).values():
        path, raw = object_bytes(repo, reference)
        # The digest binds uncompressed request bytes. Equivalent ZIP encodings
        # share one object; still revalidate every generation's source input.
        bundle.inputs[path] = _sha(raw)
        name = f"{DIRECTORY}/{reference['sha256']}.zip"
        if name not in bundle.rows:
            bundle.bytes(name, raw)

    bundle.file(repo / 'modeling-input/manifest.json', prefix + '/modeling-input/manifest.json')
    raw_source = bundle.tree(repo / 'upstream', prefix + '/source/original')
    adapted_source = bundle.tree(repo / 'crate', prefix + '/source/extracted')
    if raw_source != source_input_manifest(preparation / 'upstream') or adapted_source != source_input_manifest(preparation / 'work/crate'):
        raise CampaignArchiveError('native generation source differs from its preparation')
    if manifest.get('source_basis') is not None:
        from trellis.modeling.driver import source_fingerprint
        from trellis.pv_toolchain import setup_basis
        if not isinstance(manifest.get('resource_identity'), str):
            raise CampaignArchiveError('native provenance lacks its frozen resource identity')
        basis = source_fingerprint(preparation, setup=setup_basis(preparation, resource_identity=manifest['resource_identity']))
        if basis != manifest['source_basis']:
            raise CampaignArchiveError('preparation source or extraction setup differs from its frozen basis')
    if expected.get('source_basis') is not None and expected['source_basis'] != manifest.get('source_basis'):
        raise CampaignArchiveError('predecessor extraction basis changed')
    tablet = bundle.tree(repo / 'Tablet', prefix + '/Tablet', expected=expected.get('tablet_manifest'))
    proof_files = {entry['relative_path']: entry['sha256_of_raw_bytes'] for entry in tablet['files']}
    present = set(protocol.get('live', {}).get('present_nodes', []))
    for node, closure in protocol.get('local_closure_records', {}).items():
        if node in present and closure.get('active_decl_hash') and proof_files.get(node + '.lean') != closure['active_decl_hash']:
            raise CampaignArchiveError(f'proof bytes differ from recorded closure: {node}')
    for name in ('goals.json', 'tool-identities.json', 'extraction.json', 'SETUP.md', 'axiom-compatibility.json'):
        path = repo / 'modeling-input' / name
        if path.is_file():
            bundle.file(path, prefix + '/modeling-input/' + name)
    goals = _json(repo / 'modeling-input/goals.json')
    if _file(repo / 'modeling-input/goals.json') != _file(preparation / 'goals.json'):
        raise CampaignArchiveError('native generation requirement collection changed')
    if row['link_kind'] == 'revision':
        accepted = _json(Path(row['link_receipt']['revision_root']) / 'state.json')
        from trellis.pv_revision.store import ratified_source_manifest
        if raw_source != ratified_source_manifest(accepted['human_package']) or goals != accepted['proposed_collection']:
            raise CampaignArchiveError('successor source or requirements differ from the ratified revision')
    bundle.file(repo / 'GOAL.md', prefix + '/GOAL.md', expected=protocol.get('pv_goal_prose_sha256'))
    model_name = manifest.get('native_model', 'generated.lean')
    bundle.file(_relative(repo, model_name), prefix + '/mechanical-extraction.lean',
                expected=manifest.get('bootstrap_reproduction', {}).get('native_sha256'))
    for name in ('pv_tablet.json', 'tcb_manifest.json', 'lean-toolchain', 'lakefile.lean', 'lakefile.toml',
                 'lake-manifest.json', 'SOURCE_TREE_MANIFEST.json', 'SOURCE_PARTITION_MANIFEST.json'):
        if (repo / name).is_file():
            bundle.file(repo / name, prefix + '/manifests/' + name)
    if (repo / 'pv_tablet.json').is_file():
        for model in _json(repo / 'pv_tablet.json').get('extraction_models', []):
            source_sha = model.get('provenance', {}).get('source_sha256')
            if source_sha is not None and source_sha != adapted_source['source_tree_sha256']:
                raise CampaignArchiveError('extracted source differs from registered model provenance')
    if (repo / 'tcb_manifest.json').is_file():
        for item in _json(repo / 'tcb_manifest.json').get('extraction_provenance', {}).get('source_digests', []):
            if item.get('source_sha256') != adapted_source['source_tree_sha256']:
                raise CampaignArchiveError('extracted source differs from the TCB provenance')
    archive = runtime / 'trust_package.archive'
    finalized = protocol.get('trust_base', {}).get('package_finalization') or {}
    if archive.is_file():
        bundle.file(archive, prefix + '/trust_package.archive', expected=finalized.get('archive_sha256'))
    elif protocol.get('phase') == 'Complete' and protocol.get('trust_base', {}).get('mode') == 'required_v1':
        raise CampaignArchiveError('completed required-v1 generation lacks its original trust archive')
    if protocol.get('phase') == 'Complete' and protocol.get('trust_base', {}).get('mode') == 'required_v1' and not finalized.get('archive_sha256'):
        raise CampaignArchiveError('required-v1 finalization lacks the archive identity')
    if row['link_path']:
        bundle.file(row['link_path'], prefix + '/predecessor-receipt.json')
    history_dir = bundle.folder / '.history-staging' / str(index)
    if history_dir.resolve().is_relative_to(repo.resolve()) or history_dir.resolve().is_relative_to(preparation.resolve()):
        history_dir = Path(tempfile.mkdtemp(prefix='pv-campaign-history-'))
        bundle.external_staging.append(history_dir)
    from trellis.modeling.history import export_history, snapshot_model_lineage
    history = export_history(preparation, history_dir / 'extractions')
    lineage = snapshot_model_lineage(repo, history_dir / 'models')
    bundle.tree(history_dir / 'extractions', prefix + '/extraction-history')
    bundle.tree(history_dir / 'models', prefix + '/model-lineage')
    complete = history.get('history_complete', history.get('complete', history.get('coverage') == 'complete'))
    if not complete:
        gaps.append({'kind': 'historical_extraction_coverage', 'generation': index,
                     'detail': history.get('coverage', 'partial_or_unknown'), 'final_export_blocking': False})
    if not lineage.get('history_complete', lineage.get('complete', lineage.get('coverage') == 'complete')):
        gaps.append({'kind': 'historical_model_lineage_coverage', 'generation': index,
                     'detail': lineage.get('coverage', 'partial_or_unknown'), 'final_export_blocking': False})
    revisions = []
    revision_root = Path(expected.get('_revision_root', runtime / 'pv-revision'))
    if expected.get('_revision_root') and not (revision_root / 'state.json').is_file():
        raise CampaignArchiveError('linked revision evidence is missing')
    if (revision_root / 'state.json').is_file():
        revision = _json(revision_root / 'state.json')
        if raw_source != revision['original_manifest'] or goals != revision['original_collection']:
            raise CampaignArchiveError('predecessor source or requirements differ from preserved revision evidence')
        package = build_review_package_from_state(revision_root, revision) if revision.get('human_package') else None
        if package is not None:
            if expected.get('_package_digest') and expected['_package_digest'] != package['digest']:
                raise CampaignArchiveError('linked human revision approval package changed')
            decisions = revision.get('human_decisions', [])
            if (expected.get('_revision_root') or revision.get('status') == 'all_requirements_retired') and (
                    not decisions or decisions[-1].get('action') != 'accept' or decisions[-1].get('digest') != package['digest']):
                raise CampaignArchiveError('revision link has no exact human acceptance')
            review_zip = export_review_package(revision_root, revision)
            bundle.file(review_zip, prefix + '/revision/review.zip')
            bundle.file(review_zip.with_suffix('.json'), prefix + '/revision/export-receipt.json')
        elif not allow_incomplete:
            raise CampaignArchiveError('generation revision lacks its review evidence')
        projection = {key: revision.get(key) for key in ('status', 'phase', 'original_collection', 'proposed_collection',
                      'human_decisions', 'operator_actions', 'plan', 'aggregate_scope', 'comment_amendment')}
        bundle.bytes(prefix + '/revision/decisions.json', canonical_json(projection))
        from trellis.pv_revision.code_edits import references
        for request in references(revision_root, revision):
            bundle.file(revision_root / request['path'], prefix + '/revision/' + request['path'], expected=request['sha256'])
        revisions = (revision.get('plan') or {}).get('dispositions', [])
    return {'generation': index, 'path': prefix, 'phase': protocol.get('phase'), 'cycle': protocol.get('cycle'),
            'raw_source_digest': raw_source['source_tree_sha256'], 'extracted_source_digest': adapted_source['source_tree_sha256'],
            'requirements': goals, 'protocol_state_sha256': _sha(_file(runtime / 'protocol_state.json')),
            'dispatch_request_history': '../../.trellis-history/dispatch-requests',
            'recorded_refutations': protocol.get('pv_refutation_findings', {}),
            'revision_handoff': protocol.get('pv_revision_handoff'), 'requirement_dispositions': revisions,
            'authored_goal_bindings': protocol.get('authored_decide_goals', {}),
            'recorded_polarities': protocol.get('pv_live_polarity', {}), 'recorded_closures': protocol.get('local_closure_records', {}),
            'mechanical_model_sha256': _sha(_file(_relative(repo, model_name))),
            'predecessor_kind': row['link_kind'], 'extraction_history': history, 'model_lineage': lineage,
            'trust_archive': prefix + '/trust_package.archive' if archive.is_file() else None,
            'trust_archive_status': ('recorded_finalization_bound' if protocol.get('phase') == 'Complete' and finalized.get('archive_sha256')
                                     else 'unfinalized' if archive.is_file() else 'absent')}


def _readme(manifest: dict) -> str:
    lines = ['# Program verification campaign', '', f"Terminal outcome: **{manifest['terminal_outcome']}**.", '',
             f"Final archive: {manifest['final']}. Historical coverage: {manifest['history_completeness']}.", '',
             'This archive preserves recorded evidence across source and requirement versions. It does not '
             'supply new approvals or proof results. A retired or replaced requirement is not proved by its '
             'removal. Original disproofs and later proofs remain attached to their own source and goal versions.', '',
             'Each `generations/NNNN/` contains original and extraction-adapted source, the requirement collection '
             'and shared context, raw mechanical Lean, proof sources, protocol state, and tool/library manifests. '
             '`extraction-history/` records preserved extraction observations; `model-lineage/` records the '
             'mechanical baseline and Trellis model changes. `revision/review.zip` retains task patches, '
             'checks and scoped source snapshots; `revision/decisions.json` retains human decisions.', '',
             'A nested `trust_package.archive` preserves existing kernel-produced bytes unchanged. The generation '
             'index distinguishes `recorded_finalization_bound` from `unfinalized` artifacts; file presence alone '
             'does not establish ratification. This outer archive neither replaces its verifier nor grants its approval '
             'to another generation. `campaign.json` contains lineage and byte hashes for portable relative paths.', '',
             'Tools and libraries are declared by their exact manifests and identities; their binaries are not '
             'bundled. Reproduction requires those dependencies. Original lakefiles and source are unchanged '
             'and may contain host-specific absolute library paths that must be relocated. This is a '
             'self-contained evidence and source history, not a claim of an offline rebuildable environment.', '']
    if manifest['terminal_outcome'] == 'all_requirements_retired':
        lines.extend(['All remaining requirements were explicitly retired. No final successful verification '
                      'of those original requirements is claimed.', ''])
    for row in manifest['generations']:
        lines.extend([f"## Generation {row['generation']}", '',
                      f"Recorded phase: {row['phase']}; original source `{row['raw_source_digest']}`.", '',
                      row['requirements'].get('context', ''), ''])
        for goal in row['requirements']['goals']:
            lines.extend([f"### {goal['id']}", '', goal['text'], ''])
        for disposition in row['requirement_dispositions']:
            lines.extend([f"- {disposition['goal_id']}: {disposition['remedy']}. {disposition.get('reason', '')}", ''])
    if manifest['gaps']:
        lines.extend(['## History gaps', ''])
        for gap in manifest['gaps']:
            lines.extend([f"- {gap['kind']}: {gap['detail']}", ''])
    return '\n'.join(lines)


def export_campaign(runtime: Path, *, allow_incomplete: bool = False) -> Path:
    """Write a deterministic archive and atomic receipt after byte revalidation."""
    runtime = Path(runtime).absolute()
    protocol_raw = _file(runtime / 'protocol_state.json')
    protocol = json.loads(protocol_raw)
    outcome = _outcome(runtime, protocol)
    if outcome == 'incomplete' and not allow_incomplete:
        raise CampaignArchiveError('campaign has no Complete or all_requirements_retired terminal outcome')
    generations, gaps = _discover(runtime, allow_incomplete=allow_incomplete)
    if not generations:
        raise CampaignArchiveError('no native PV generations are available to archive')
    output = runtime / 'campaign-archives'
    output.mkdir(exist_ok=True)
    if output.is_symlink():
        raise CampaignArchiveError('campaign export directory must not be a symlink')
    staging = Path(tempfile.mkdtemp(prefix='.campaign-', dir=output))
    try:
        bundle = _Bundle(staging)
        records = []
        for index, row in enumerate(generations):
            try:
                records.append(_generation(bundle, row, index, allow_incomplete=allow_incomplete, gaps=gaps))
            except (OSError, ValueError, KeyError) as error:
                if not allow_incomplete:
                    raise CampaignArchiveError(f'generation {index} cannot be archived: {error}') from error
                gaps.append({'kind': 'invalid_generation_evidence', 'generation': index, 'detail': str(error),
                             'final_export_blocking': True})
        final = outcome != 'incomplete' and not any(gap.get('final_export_blocking', True) for gap in gaps)
        manifest = {'schema': SCHEMA, 'terminal_outcome': outcome, 'final': final,
                    'protocol_state_sha256': _sha(protocol_raw), 'generation_count': len(generations),
                    'history_completeness': 'complete' if not gaps else 'partial', 'gaps': gaps,
                    'generations': records,
                    'claim_policy': 'Only recorded kernel outcomes for each exact goal/source version; retirement and replacement are not proof.'}
        manifest['goal_lineage'] = [
            {'generation': row['generation'], 'goal_id': goal['id'],
             'goal_version_digest': digest({'goal': goal, 'context': row['requirements'].get('context', ''),
                                            'source': row['raw_source_digest'], 'model': row['mechanical_model_sha256']}),
             'text': goal['text'], 'shared_context': row['requirements'].get('context', ''),
             'raw_source_digest': row['raw_source_digest'], 'mechanical_model_sha256': row['mechanical_model_sha256'],
             'protocol_evidence': row['path'] + '/protocol_state.json',
             'recorded_dispositions': [item for item in row['requirement_dispositions'] if item.get('goal_id') == goal['id']]}
            for row in records for goal in row['requirements']['goals']]
        bundle.bytes('README.md', _readme(manifest).encode('utf-8'))
        manifest['files'] = sorted(bundle.rows.values(), key=lambda row: row['path'])
        manifest['digest'] = digest(manifest)
        bundle.bytes('campaign.json', canonical_json(manifest))
        bundle.revalidate()
        if _file(runtime / 'protocol_state.json') != protocol_raw:
            raise CampaignArchiveError('campaign protocol changed during export')
        archive_path = output / (manifest['digest'] + '.zip')
        temporary = staging / 'campaign.zip'
        with zipfile.ZipFile(temporary, 'w', compression=zipfile.ZIP_DEFLATED) as archive:
            for name, entry in sorted(bundle.rows.items()):
                info = zipfile.ZipInfo(name, date_time=(1980, 1, 1, 0, 0, 0))
                info.compress_type = zipfile.ZIP_DEFLATED
                info.external_attr = (0o100000 | entry['unix_mode']) << 16
                archive.writestr(info, _file(_relative(staging, name)))
        archive_sha = _sha(_file(temporary))
        if archive_path.exists():
            if _sha(_file(archive_path)) != archive_sha:
                raise CampaignArchiveError('existing immutable campaign archive differs')
        else:
            with temporary.open('rb') as stream:
                os.fsync(stream.fileno())
            os.replace(temporary, archive_path)
            archive_path.chmod(0o444)
        receipt = {key: manifest[key] for key in ('schema', 'digest', 'terminal_outcome', 'final',
                   'protocol_state_sha256', 'generation_count', 'history_completeness', 'gaps')}
        receipt.update(archive=archive_path.name, archive_sha256=archive_sha)
        _write(archive_path.with_suffix('.json'), receipt, immutable=True)
        _write(output / 'latest.json', receipt)
        return archive_path
    finally:
        for external in bundle.external_staging:
            shutil.rmtree(external)
        shutil.rmtree(staging)
