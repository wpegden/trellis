"""Prepare isolated attempt inputs; never reset an already launched workspace."""
from __future__ import annotations

import errno
import fcntl
import json
import os
from pathlib import Path
import shutil
import stat
import subprocess
import tempfile

from .candidate import CandidateError, canonical_json, digest, source_manifest
from .publication import atomic


_EXCLUDED = {'.git', '.lake', '.trellis', '.trellis-history', '.pytest_cache',
             '__pycache__', 'node_modules', '.viewer', 'target', 'last_invalid',
             '.trellis-stop-after-checkpoint'}

_RETRY_RESPONSE_LIMIT = 32 * 1024 * 1024
_PACKAGE_INPUTS = ('lake-manifest.json', 'lean-toolchain', 'lakefile.lean', 'lakefile.toml')
_PACKAGE_MIRRORS = {Path('repo/.lake/packages'),
                    Path('repo/.trellis/supervisor/repo/.lake/packages')}


def _retry_metadata(previous, snapshot):
    """Carry the completed worker's report as bounded, non-authoritative context."""
    from trellis.history_artifacts import LAST_INVALID_METADATA_FILENAME

    metadata = dict(previous_task=previous.name, report_available=False,
                    preserved_source_surfaces={name: (snapshot / name).is_dir()
                                               for name in ('Tablet', 'reference')})

    def read(relative, limit):
        path = previous
        for part in Path(relative).parts:
            path = path / part
            if path.is_symlink():
                raise ValueError('previous worker report contains a symlink')
        if not stat.S_ISREG(path.stat().st_mode):
            raise ValueError('previous worker report is not a regular file')
        with path.open('rb') as stream:
            data = stream.read(limit + 1)
        if len(data) > limit:
            raise ValueError('previous worker report exceeds the handoff size limit')
        return data

    try:
        from .storage import execution_directories, worker_execution_coordinates
        turns = [path for path in execution_directories(previous)
                 if worker_execution_coordinates(path) is not None and (path / 'result.json').exists()]
        execution = max(turns, key=worker_execution_coordinates) if turns else previous / 'execution'
        relative = execution.relative_to(previous)
        receipt = json.loads(read(relative / 'result.json', 65536))
        data = read(relative / 'response.json', _RETRY_RESPONSE_LIMIT)
        if not isinstance(receipt, dict) or receipt.get('response_sha256') != digest(data):
            raise ValueError('previous worker report does not match its completion receipt')
        report = json.loads(data)
        if not isinstance(report, dict) or report.get('kind') not in ('worker', 'worker_submission'):
            raise ValueError('previous attempt has no worker report')
        metadata.update(report_available=True, response_sha256=digest(data))
        if report['kind'] == 'worker_submission':
            turn = worker_execution_coordinates(execution)[0]
            if report.get('turn') != turn or not isinstance(report.get('raw'), dict):
                raise ValueError('previous submission has no matching worker report')
            metadata.update(submission_turn=turn, unaccepted=True)
            report = dict(report['raw'], **{key: report[key] for key in ('request_id', 'cycle') if key in report})
        truncated = []

        def text(key, value, limit=12000):
            if len(value) > limit:
                truncated.append(key)
                return value[:limit] + '... [truncated in retry handoff]'
            return value

        for key in ('request_id', 'cycle'):
            if type(report.get(key)) is int:
                metadata[key] = report[key]
        for key in ('status', 'outcome', 'invalid_kind', 'summary', 'comments'):
            if isinstance(report.get(key), str):
                metadata[key] = text(key, report[key], 12000 if key in ('summary', 'comments') else 128)
        reasons = report.get('deterministic_rejection_reasons')
        if isinstance(reasons, list):
            metadata['deterministic_rejection_reasons'] = [
                text('deterministic_rejection_reasons', reason, 4000)
                for reason in reasons[:8] if isinstance(reason, str)]
            if len(reasons) > 8:
                truncated.append('deterministic_rejection_reasons')
        if truncated:
            metadata['truncated_fields'] = sorted(set(truncated))
    except (OSError, ValueError) as exc:
        metadata['report_available'] = False
        metadata['report_unavailable_reason'] = str(exc)[:1000]
    atomic(snapshot / LAST_INVALID_METADATA_FILENAME, canonical_json(metadata))


class PreparedInputsChanged(RuntimeError):
    """A receipted, unlaunched preparation can be replaced after revalidation."""


def sync_tree(root):
    """Make copied inputs durable before publishing the preparation receipt."""
    synced_directories = []
    for current, directories, files in os.walk(root, topdown=True, followlinks=False):
        # Package mirrors are reconstructed before a fresh compiler grant.
        # Their copied bytes were flushed while seeding; hardlink bytes already
        # belong to the host cache. Avoid thousands of directory journal waits.
        directories[:] = [name for name in directories
                          if (Path(current) / name).relative_to(root) not in _PACKAGE_MIRRORS]
        synced_directories.append(Path(current))
        for name in files:
            path = Path(current) / name
            if path.is_symlink():
                continue
            relative = path.relative_to(root).parts
            immutable_cache = relative[:3] == ('repo', '.git', 'objects')
            if immutable_cache and path.stat().st_nlink > 1:
                # Existing cache inodes were not written by this preparation.
                # Persist the new hardlink's directory entry below, without
                # flushing every shared Git object for every attempt.
                continue
            with path.open('rb') as stream:
                os.fsync(stream.fileno())
    from .candidate import _sync_dir
    for directory in reversed(synced_directories):
        _sync_dir(directory)


def _package_inputs(repo):
    inputs = {}
    for name in _PACKAGE_INPUTS:
        path = repo / name
        if path.is_symlink():
            raise CandidateError('package preparation inputs cannot be symlinks')
        inputs[name] = digest(path.read_bytes()) if path.exists() else None
    return inputs


def repair_packages(runtime, task_runtime):
    """Restore derived package entries before a new launcher gets channels.

    Existing launchers and completed attempts never pass this grant boundary
    again. The preparation receipt still binds durable sources and metadata;
    package completeness is not acceptance evidence.
    """
    directory = Path(task_runtime).parent
    marker = directory / 'prepared.json'
    if not marker.exists():
        return  # Legacy callers predate reconstructible package mirrors.
    prepared = json.loads(marker.read_text())
    cache = prepared.get('package_cache')
    if cache is None:
        return  # Old preparations flushed the entire package tree.
    metadata = json.loads((Path(runtime) / 'runtime_metadata.json').read_text())
    source, repo = Path(metadata['repo_path']).resolve(), directory / 'repo'
    inputs = cache.get('inputs')
    if (cache.get('version') != 1 or source != Path(prepared['source_repo'])
            or not isinstance(inputs, dict) or set(inputs) != set(_PACKAGE_INPUTS)
            or _package_inputs(source) != inputs or _package_inputs(repo) != inputs):
        raise CandidateError('package repair source or pins changed; retain the prepared workspace')
    from trellis.supervisor_workspace import authoritative_repo_path, _ensure_supervisor_lake_packages
    authority = authoritative_repo_path(repo)
    if authority != repo / '.trellis/supervisor/repo':
        raise CandidateError('package repair must stay inside the private supervisor')
    for destination in (repo, authority):
        _ensure_supervisor_lake_packages(source, destination, durable_copies=True)
    if _package_inputs(source) != inputs:
        raise CandidateError('package repair inputs changed while restoring the mirror')


def _git(repo, *args):
    result = subprocess.run(['git', '-C', str(repo), *args], capture_output=True, text=True)
    if result.returncode:
        raise RuntimeError(f'attempt Git preparation failed: {result.stderr.strip()}')
    return result.stdout.strip()


def same_regular_bytes(source, destination):
    """Only complete independently checked cache files earn resume credit."""
    source, destination = Path(source), Path(destination)
    if destination.is_symlink():
        raise CandidateError('cache resume destination cannot be a symlink')
    if not destination.exists():
        return False
    if (not stat.S_ISREG(source.lstat().st_mode)
            or not stat.S_ISREG(destination.lstat().st_mode)):
        raise CandidateError('cache resume inputs must be regular files')
    if source.stat().st_size != destination.stat().st_size:
        return False
    with source.open('rb') as left, destination.open('rb') as right:
        while True:
            a, b = left.read(1 << 20), right.read(1 << 20)
            if a != b:
                return False
            if not a:
                return True


def _copy_build_file(source, destination, *, reuse_existing=False):
    """Seed an independent writable cache, sharing extents where supported.

    Linux FICLONE shares data blocks, never writable inodes. Unsupported or
    cross-filesystem copies retain the ordinary byte-copy path. Real storage
    failures propagate; cloning does not reserve space for later COW writes.
    All copied artifact/source/replay validation remains with the caller.
    """
    if reuse_existing and same_regular_bytes(source, destination):
        return destination  # Preserve existing extents instead of rewriting COW.
    with open(source, 'rb') as src:
        try:
            dst = open(destination, 'xb')
        except FileExistsError:
            # An existing private cache keeps ordinary copy2 overwrite
            # semantics. Never truncate it merely to probe clone support.
            return shutil.copy2(source, destination)
        with dst:
            try:
                fcntl.ioctl(dst.fileno(), 0x40049409, src.fileno())  # FICLONE
            except OSError as error:
                if error.errno not in (errno.EXDEV, errno.EOPNOTSUPP, errno.ENOTTY,
                                        errno.EINVAL, errno.ENOSYS):
                    raise
                # Leave the empty independent destination for copy2 below.
                cloned = False
            else:
                cloned = True
    if cloned:
        shutil.copystat(source, destination)
        return destination
    return shutil.copy2(source, destination)


def _copy_tree(source, destination, *, exclude=(), clone_builds=False, reuse_builds=False,
               omit_module_setup=False):
    """Copy writable files; reject special files and links outside the input."""
    source, destination = Path(source), Path(destination)
    if source.is_symlink():
        raise CandidateError(f'attempt inputs cannot traverse symlinks: {source}')
    destination.mkdir(parents=True, exist_ok=True)
    for current, directories, filenames in os.walk(source, followlinks=False):
        relative = Path(current).relative_to(source)
        directories[:] = [n for n in directories if n not in exclude]
        target = destination / relative
        target.mkdir(parents=True, exist_ok=True)
        for name in list(directories) + filenames:
            if name in exclude:
                continue
            origin = Path(current) / name
            mode = origin.lstat().st_mode
            if stat.S_ISLNK(mode):
                raise CandidateError(f'attempt inputs cannot traverse symlinks: {origin}')
            if stat.S_ISREG(mode):
                # Lake writes ir/**/*.setup.json immediately before compiling.
                # These invocation inputs are not cached output artifacts; avoid
                # multiplying their large import maps in optional build seeds.
                # General/source copies remain exact, and existing private files
                # are never removed when resuming an interrupted hydration.
                if (omit_module_setup and relative.parts[:1] == ('ir',)
                        and name.endswith('.setup.json')):
                    continue
                # No hardlinks for writable source or project build outputs.
                if clone_builds:
                    _copy_build_file(origin, target / name, reuse_existing=reuse_builds)
                else:
                    shutil.copy2(origin, target / name)
            elif not stat.S_ISDIR(mode):
                raise CandidateError(f'attempt input is not a regular file: {origin}')


def _relocate(value, old_repo, new_repo, old_runtime, new_runtime):
    if isinstance(value, dict):
        return {k: _relocate(v, old_repo, new_repo, old_runtime, new_runtime) for k, v in value.items()}
    if isinstance(value, list):
        return [_relocate(v, old_repo, new_repo, old_runtime, new_runtime) for v in value]
    if isinstance(value, str):
        for before, after in [(old_runtime, new_runtime), (old_repo, new_repo)]:
            if value == str(before) or value.startswith(str(before) + '/'):
                return str(after) + value[len(str(before)):]
    return value


def _seed_authoritative_build(repo, work_repo, *, resume=False):
    """Reuse coherent build outputs and replay evidence from a trusted supervisor.

    The caller selects canonical authority or an authenticated drained host
    acceptance. Worker build files are never authority. Only that supervisor's
    replay-attested bundles can seed this new private supervisor workspace.
    Copy first, validate the exact copied bytes and complete source closures,
    then retain their bound replay attestations. Ordinary materialization checks
    these again and rebuilds or replays any changed or untrusted module.
    """
    from trellis.atomic_actions import observations as obs
    from trellis.supervisor_workspace import authoritative_repo_path

    source = authoritative_repo_path(repo)
    destination = authoritative_repo_path(work_repo)
    relative = Path('.lake/build/lib/lean/Tablet')
    if (not source.is_relative_to(repo) or not destination.is_relative_to(work_repo)
            or not (source / relative).is_dir() or (destination.exists() and not resume)):
        return 0
    # An optional cache must not follow links into another authority or expose
    # shared writable inodes. The attempt has no running processes yet.
    for path in [repo / '.trellis', repo / '.trellis/supervisor', repo / '.trellis/supervisor/repo',
                 *[(source / Path(*relative.parts[:n])) for n in range(1, len(relative.parts) + 1)],
                 source / '.lake/build/ir', source / '.lake/build/ir/Tablet']:
        if path.is_symlink():
            return 0
    try:
        _copy_tree(work_repo, destination, exclude=_EXCLUDED)
        scripts = work_repo / '.trellis/scripts'
        if scripts.is_dir():
            _copy_tree(scripts, destination / '.trellis/scripts')
        (destination / relative).mkdir(parents=True, exist_ok=resume)
        for artifact in (source / relative).iterdir():
            if not any(artifact.name.endswith(suffix) for suffix in obs._OLEAN_ARTIFACT_SUFFIXES):
                continue
            if not stat.S_ISREG(artifact.lstat().st_mode):
                raise CandidateError('authoritative build cache contains a nonregular artifact')
            _copy_build_file(artifact, destination / relative / artifact.name, reuse_existing=resume)
        snapshot = obs.SourceClosureSnapshot(destination)
        nodes = {p.stem for p in (destination / relative).glob('*.olean')}
        valid = {name for name in nodes
                 if obs.olean_is_content_current(destination, name, source_snapshot=snapshot)
                 and obs.olean_has_current_kernel_replay(destination, name)}
        # A coherent node cannot keep imports whose own seed was rejected.
        reusable = {name for name in valid
                    if set(snapshot.materialization_order([name])) <= valid}
        if not snapshot.is_current():
            reusable.clear()
        # Lake checks all declared module outputs, including generated C,
        # before reusing its Lean artifacts. Keep these cache outputs too;
        # no native object/library is transferred or treated as proof evidence.
        source_ir = source / '.lake/build/ir/Tablet'
        destination_ir = destination / '.lake/build/ir/Tablet'
        for name in reusable:
            for suffix in ('.c', '.c.hash'):
                artifact = source_ir / (name + suffix)
                if not artifact.exists() and not artifact.is_symlink():
                    continue
                if not stat.S_ISREG(artifact.lstat().st_mode):
                    raise CandidateError('authoritative build cache contains a nonregular artifact')
                destination_ir.mkdir(parents=True, exist_ok=True)
                _copy_build_file(artifact, destination_ir / artifact.name, reuse_existing=resume)
        for name in nodes:
            if name not in reusable:
                obs._purge_olean_artifacts(destination, name)
        # Seeded imports can retain a replay environment captured by an older
        # acceptance. Copy the referenced manifests with their bundles, so a
        # later donor can establish portability without replaying those imports.
        # This is optional content-bound metadata, never a new attestation.
        from trellis.checker import portable_replay as portable
        attempted_environments = set()
        environment_bytes = 0
        for name in sorted(reusable):
            identity = (obs._read_olean_provenance(destination, name).get('kernel_replay')
                        or {}).get('portable_environment_sha256')
            if not isinstance(identity, str) or not identity or identity in attempted_environments:
                continue
            if len(attempted_environments) >= 128:
                break
            attempted_environments.add(identity)
            try:
                data = portable.environment_bytes_for(
                    source, identity, max_bytes=(64 << 20) - environment_bytes)
                if environment_bytes + len(data) > 64 << 20:
                    continue
                from .publication import atomic
                target = destination / portable.DIRECTORY / (identity + '.json')
                if not (resume and target.is_file() and not target.is_symlink()
                        and target.read_bytes() == data):
                    atomic(target, data)
                environment_bytes += len(data)
            except (OSError, ValueError, CandidateError):
                pass  # Keep ordinary replay when portability is unavailable.
        return len(reusable)
    except (OSError, CandidateError, obs.SourceClosureSnapshotTooLarge):
        # Incoherent or unavailable cache data only makes this attempt cold.
        # Removing the whole fresh workspace also handles partial copies.
        if destination.exists():
            shutil.rmtree(destination)
        return 0


def _seed_statement_transforms(runtime, work_runtime):
    """Copy optional derivations from canonical or authenticated host authority.

    Never use agent repositories or candidates as a cache source. The normal
    statement-transform consumer still establishes current kernel replay and
    recomputes the full input/helper/environment key before returning a hit.
    These independent copies confer no authority on the preparation receipt.
    """
    from trellis.checker.sync import load_local_closure_axioms

    source = Path(runtime) / 'checker-state/statement-transforms'
    destination = Path(work_runtime) / 'checker-state/statement-transforms'
    if any(path.is_symlink() for path in (Path(runtime), source.parent, source)):
        return 0
    try:
        from itertools import islice
        entries = list(islice(source.iterdir(), 8192))
    except OSError:
        return 0
    copied, remaining = 0, 64 << 20
    for path in entries:
        if remaining <= 0:
            break
        key = path.stem
        if path.suffix != '.json' or len(key) != 64 or any(c not in '0123456789abcdef' for c in key):
            continue
        try:
            info = path.lstat()
            if not stat.S_ISREG(info.st_mode) or info.st_nlink != 1 or info.st_size > min(16 << 20, remaining):
                continue
            if (destination / path.name).exists():
                continue
            record = load_local_closure_axioms(source, key, expected_version=1)
            if not record or not isinstance(record.get('response'), dict) or record['response'].get('status') != 'ok':
                continue
            data = canonical_json(record)
            remaining -= len(data)
            atomic(destination / path.name, data)
            copied += 1
        except (OSError, ValueError):
            # An absent, interrupted or corrupt optional cache remains cold.
            continue
    return copied


def _copy_completion_evidence(repo, destination, state, request):
    """Deliver diagnostic bytes for a NEW final audit, never scratch history."""
    if (str(request.get('kind', '')).lower() != 'audit'
            or str(state.get('phase', '')).lower() != 'modeling'
            or state.get('modeling_request') is not None
            or state.get('modeling_route_review') is not None):
        return {}

    def read(relative):
        relative = Path(relative)
        if relative.is_absolute() or '..' in relative.parts:
            raise CandidateError('completion evidence path must stay inside the repository')
        path = repo
        for part in relative.parts:
            path /= part
            if path.is_symlink():
                raise CandidateError('completion evidence cannot traverse symlinks')
        if not path.is_file():
            raise CandidateError(f'missing completion evidence: {relative}')
        return path.read_bytes()

    completion = Path('.trellis/scratch/modeling-completion-check.json')
    data = read(completion)
    report = json.loads(data)
    paths = report.get('provenance_reports')
    if not isinstance(paths, list) or any(not isinstance(path, str) for path in paths):
        raise CandidateError('completion report lacks current-call provenance paths')
    evidence = {completion: data}
    allowed_parents = {Path('.trellis/scratch'), Path('.trellis/supervisor/repo/.trellis/scratch')}
    for path in paths:
        relative = Path(path)
        if (relative.name != 'provenance.json' or relative.parent.parent not in allowed_parents
                or not relative.parent.name.startswith('provenance-')):
            raise CandidateError('invalid completion provenance report path')
        content = read(relative)
        evidence[relative] = content
        provenance = json.loads(content)
        # Native provenance declares its source-tree diagnostic explicitly.
        # Retain its original bytes/path fields and provide the corresponding
        # repository-relative file, without importing unrelated scratch files.
        manifest = provenance.get('source_digest', {}).get('manifest')
        if manifest:
            companion = Path(manifest)
            if companion.is_absolute():
                try:
                    companion = companion.relative_to(repo)
                except ValueError as exc:
                    raise CandidateError('completion companion is outside the repository') from exc
            if companion.parent != relative.parent or companion.name != 'source-tree.json':
                raise CandidateError('invalid completion provenance companion path')
            evidence[companion] = read(companion)
    for relative, content in evidence.items():
        atomic(destination / relative, content)
    return {str(relative): digest(content) for relative, content in evidence.items()}


def prepare(runtime, repo, config_path, destination, private_state, *, expected_manifest,
            previous=None, seed_packages=True, handoff_repo=None, host_cache_donor=None,
            deferred_build_cache=None, measurements=None, host_only=False):
    """Call under the run's publication lock, before the launch permit.

    The marker is a durable preparation receipt, not acceptance evidence.
    Once it exists this function never overwrites any attempt files. Interrupted
    temporary trees remain evidence until the caller confirms they are unused.
    Host-only checks compile in the authoritative inner repository, so omit the
    outer agent build seed while retaining configuration and trusted inner caches.
    Record this mode to prevent later provider adoption of the same workspace.
    """
    from .telemetry import phase
    runtime, repo, destination = map(lambda p: Path(p).resolve(), (runtime, repo, destination))
    config_path = Path(config_path).resolve()
    if not destination.is_relative_to(runtime / 'dispatch') or destination.is_relative_to(repo):
        raise ValueError('attempt workspaces must be inside this runtime dispatch directory and outside canonical sources')
    request = private_state.get('in_flight_request')
    if not isinstance(request, dict):
        raise ValueError('attempt workspace requires its native prepared request')
    with phase(measurements, 'workspace.identity'):
        identity = digest(canonical_json(dict(state=private_state, sources=expected_manifest,
            config_sha256=digest(config_path.read_bytes()))))
    if host_only and deferred_build_cache is not None:
        raise CandidateError('host-only preparation cannot use a provider build cache')
    marker = destination / 'prepared.json'
    if marker.exists():
        prepared = json.loads(marker.read_text())
        if prepared.get('host_only') and not host_only:
            raise CandidateError('host-only preparation cannot be adopted as a provider workspace')
        if prepared['identity'] != identity:
            raise PreparedInputsChanged('attempt already has different prepared inputs; retain its workspace')
        return destination
    if destination.exists():
        raise RuntimeError('attempt directory exists without a preparation receipt; preserve it for recovery')
    with phase(measurements, 'workspace.source_validation'):
        if source_manifest(repo) != expected_manifest:
            raise CandidateError('canonical sources changed before workspace preparation')
    with phase(measurements, 'workspace.copy'):
        package_inputs = _package_inputs(repo) if seed_packages else None
        destination.parent.mkdir(parents=True, exist_ok=True)
        temp = Path(tempfile.mkdtemp(prefix='.workspace-', dir=destination.parent))
        work_repo, work_runtime = temp / 'repo', temp / 'runtime'
        final_repo, final_runtime = destination / 'repo', destination / 'runtime'
        # Local Git objects are immutable and may be hardlinked. Avoid expanding
        # the entire checkpoint history in every worker's working directory.
        result = subprocess.run(['git', 'clone', '--local', '--no-tags', '--no-checkout',
                                 str(repo), str(work_repo)], capture_output=True, text=True)
        if result.returncode:
            raise RuntimeError(f'attempt clone failed: {result.stderr.strip()}; retained {temp}')
        _git(work_repo, 'config', 'core.sparseCheckout', 'true')
        patterns = work_repo / '.git/info/sparse-checkout'
        # A historical checkpoint may track the operational stop marker even after
        # canonical resume removed it. Copy exclusions alone cannot undo checkout.
        patterns.write_text('/*\n!/.trellis-history/\n!/.lake/\n!/.trellis/\n'
                            '!/.trellis-stop-after-checkpoint\n')
        _git(work_repo, 'read-tree', '-mu', 'HEAD')
        # Checkout is a cache seed. Replace source surfaces so dirty accepted
        # bytes and deletions, including untracked helpers, match canonical exactly.
        for surface in ('Tablet', 'reference'):
            if (work_repo / surface).exists():
                shutil.rmtree(work_repo / surface)
        _copy_tree(repo, work_repo, exclude=_EXCLUDED)
        if (repo / '.trellis').is_symlink():
            raise CandidateError(f'attempt inputs cannot traverse symlinks: {repo / ".trellis"}')
        for relative in ('.trellis/scripts', '.trellis/artifacts', '.trellis/stuck-math-audit'):
            # Hosts run acceptance, not audit/provider bursts. Explicit audit
            # notes in their frozen request are still captured just below.
            if host_only and relative == '.trellis/stuck-math-audit':
                continue
            if (repo / relative).is_dir():
                _copy_tree(repo / relative, work_repo / relative)
        # Only explicitly referenced advisory notes cross an attempt boundary.
        # A parallel review's frozen workspace is the sole source for its notes;
        # legacy adopted requests use the canonical reviewer's scratch directory.
        from trellis.worker_scratch import capture_reviewer_scratch, reviewer_scratch_paths
        comments = str(request.get('reviewer_comments') or '')
        include_audit = request.get('kind') in ('Review', 'Worker', 'StuckMathAudit')
        if handoff_repo is not None and include_audit:
            for relative in reviewer_scratch_paths(comments, include_audit=True):
                # Canonical audit history may contain a different file under the
                # same name. An unavailable producer note must remain unavailable.
                copied = work_repo / relative
                if copied.is_file():
                    copied.unlink()
        handoffs, missing_handoffs = capture_reviewer_scratch(
            Path(handoff_repo) if handoff_repo is not None else repo,
            comments, include_audit=include_audit)
        for relative, content in handoffs.items():
            atomic(work_repo / relative, content)
        history = repo / '.trellis/logs/burst-history.jsonl'
        if history.is_file():
            (work_repo / '.trellis/logs').mkdir(parents=True, exist_ok=True)
            shutil.copy2(history, work_repo / '.trellis/logs/burst-history.jsonl')
        if str(request.get('kind', '')).lower() in ('worker', 'review', 'stuckmathaudit', 'stuck_math_audit'):
            # Existing archived chats need only private refs and read-only shared
            # objects, not another checkout/copy of every transcript per attempt.
            from trellis.chat_history import chat_repo_path
            chats = chat_repo_path(repo)
            if (chats / '.git').is_dir():
                if (chats / '.git').resolve() != chats / '.git':
                    raise CandidateError('chat history cannot traverse a symlink')
                _git(repo, 'clone', '--shared', '--no-checkout', str(chats), str(chat_repo_path(work_repo)))
            from .transcripts import prepare_history
            prepare_history(runtime, repo, work_repo, request=request)
        (work_repo / '.trellis').mkdir(exist_ok=True)
        if seed_packages:
            from trellis.supervisor_workspace import _ensure_supervisor_lake_packages
            _ensure_supervisor_lake_packages(repo, work_repo, durable_copies=True)
        build_seeds = ('.lake/config',) if host_only else ('.lake/build', '.lake/config')
        for relative in (() if deferred_build_cache else build_seeds):
            if (repo / relative).is_dir():
                _copy_tree(repo / relative, work_repo / relative,
                           clone_builds=relative == '.lake/build',
                           omit_module_setup=relative == '.lake/build')
        # A retry donor is a host-only sibling selected by receipt authentication,
        # never the prior worker repository or a payload-provided artifact path.
        cache_repo = Path(host_cache_donor) / 'repo' if host_cache_donor is not None else repo
        seeded_oleans = 0 if deferred_build_cache else _seed_authoritative_build(cache_repo, work_repo)
        if seed_packages and not deferred_build_cache:
            from trellis.supervisor_workspace import authoritative_repo_path
            private_supervisor = authoritative_repo_path(work_repo)
            if not private_supervisor.is_relative_to(work_repo):
                raise CandidateError('private supervisor workspace must stay inside the attempt')
            # Host observations also run for text-only verifiers, which do not
            # perform the worker's later supervisor sync. Seed packages even when
            # no trusted project build was reusable; compiler sandboxes cannot
            # fetch missing package sources into their read-only package tree.
            _ensure_supervisor_lake_packages(repo, private_supervisor, durable_copies=True)
        seeded_statement_transforms = _seed_statement_transforms(runtime, work_runtime)
        if host_cache_donor is not None:
            seeded_statement_transforms += _seed_statement_transforms(Path(host_cache_donor) / 'runtime', work_runtime)
        if previous is not None:
            previous = Path(previous).resolve()
            previous_repo = previous / 'repo'
            if not previous_repo.is_relative_to(runtime / 'dispatch'):
                raise ValueError('retry evidence must belong to this run')
            snapshot = work_repo / 'last_invalid'
            from .source_recovery import Sources
            recovered_sources = Sources(previous, repo=repo)
            for surface in ('Tablet', 'reference'):
                if (previous_repo / surface).is_dir():
                    _copy_tree(previous_repo / surface, snapshot / surface)
            recovered_sources.materialize(snapshot)
            _retry_metadata(previous, snapshot)
        # Copy diagnostics after cache seeding and advisory handoffs: creating a
        # supervisor scratch directory earlier would prevent build-cache seeding.
        completion_evidence = _copy_completion_evidence(repo, work_repo, private_state, request)
        config = _relocate(json.loads(config_path.read_text()), repo, final_repo, runtime, final_runtime)
        config['repo_path'] = str(final_repo)
        config['state_dir'] = '.trellis'
        atomic(work_repo / 'trellis.config.json', canonical_json(config))
        metadata = _relocate(json.loads((runtime / 'runtime_metadata.json').read_text()),
                             repo, final_repo, runtime, final_runtime)
        metadata.update(repo_path=str(final_repo), config_path=str(final_repo / 'trellis.config.json'))
        atomic(work_runtime / 'runtime_metadata.json', canonical_json(metadata))
    from trellis.private_state import write_private_state
    with phase(measurements, 'workspace.private_state_write'):
        write_private_state(work_runtime / 'protocol_state.json', private_state)
    atomic(temp / 'base-manifest.json', canonical_json(expected_manifest))
    with phase(measurements, 'workspace.final_source_validation'):
        if source_manifest(work_repo) != expected_manifest or source_manifest(repo) != expected_manifest:
            raise CandidateError(f'workspace source capture changed; retained {temp}')
    with phase(measurements, 'workspace.audit_materialization'):
        from .coarse_audits import materialize_recent, restrict_evidence
        materialize_recent(work_repo, private_state)
        if request.get('scoped_audit') is not None:
            restrict_evidence(work_repo, request['scoped_audit'])
    with phase(measurements, 'workspace.sync'):
        sync_tree(temp)
    with phase(measurements, 'workspace.publish'):
        if seed_packages and (_package_inputs(repo) != package_inputs or _package_inputs(work_repo) != package_inputs):
            raise CandidateError(f'package preparation inputs changed; retained {temp}')
        atomic(temp / 'prepared.json', canonical_json(dict(identity=identity, request_id=request['id'],
            prepared_config_sha256=digest(canonical_json(config)),
            history={key: request.get(key) for key in ('cycle', 'kind', 'active_node', 'mode')},
            source_repo=str(repo), source_head=_git(repo, 'rev-parse', 'HEAD'), seeded_oleans=seeded_oleans,
            package_cache=dict(version=1, inputs=package_inputs) if seed_packages else None,
            seeded_statement_transforms=seeded_statement_transforms,
            **({'host_only': True} if host_only else {}),
            **({"deferred_build_cache": deferred_build_cache} if deferred_build_cache else {}),
            **({'host_cache_donor_round': Path(host_cache_donor).name} if host_cache_donor is not None else {}),
            **({"completion_evidence": completion_evidence} if completion_evidence else {}),
            reviewer_handoffs={name: digest(data) for name, data in handoffs.items()},
            missing_reviewer_handoffs=missing_handoffs)))
        os.rename(temp, destination)
        from .candidate import _sync_dir
        _sync_dir(destination.parent)
    return destination
