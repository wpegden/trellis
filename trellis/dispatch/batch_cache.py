"""One frozen optional build-cache donor for a fresh compact batch.

Every assignment's complete source/config/history preparation still precedes
native reservation. Only regenerable build expansion waits until launch.
"""
from pathlib import Path
import json
import os
import stat
import tempfile

from . import workspace
from .candidate import CandidateError, canonical_json, digest, source_manifest, _sync_dir
from .publication import atomic

FORMAT = 'trellis-shared-batch-cache/1'


def read(path):
    from .acceptance_receipts import regular
    return json.loads(regular(path))


def inventory(root, *, hashes=True, exclude=()):
    """Account ordinary independent copies, with filesystem-block rounding."""
    root = Path(root)
    if root.is_symlink():
        raise CandidateError('batch cache input cannot be a symlink')
    entries, allocated, inodes = {}, 0, 0
    if not root.exists():
        return entries, allocated, inodes
    for current, directories, files in os.walk(root, followlinks=False):
        directories[:] = [name for name in directories if name not in exclude]
        for name in directories + files:
            if name in exclude:
                continue
            path = Path(current) / name
            info = path.lstat()
            if not (stat.S_ISDIR(info.st_mode) or stat.S_ISREG(info.st_mode)):
                raise CandidateError('batch cache input must be regular: ' + str(path))
            inodes += 1
            allocated += 4096 if path.is_dir() else ((info.st_size + 4095) // 4096) * 4096
            if hashes and path.is_file():
                # Streaming avoids a second large private protocol allocation.
                import hashlib
                checksum = hashlib.sha256()
                with path.open('rb') as stream:
                    for block in iter(lambda: stream.read(1 << 20), b''):
                        checksum.update(block)
                entries[str(path.relative_to(root))] = dict(bytes=info.st_size, sha256=checksum.hexdigest())
    return entries, allocated, inodes


def allocation(repo, workers, pending, *, new_capture, workspace_bytes, workspace_inodes):
    # Price durable source/config copies separately from the common optional
    # build and one launch expansion. Packages/Git remain existing immutable
    # hardlink caches. This is initial allocation, not an active-growth bound.
    _, source_bytes, source_inodes = inventory(repo, hashes=False, exclude=workspace._EXCLUDED)
    auxiliary_bytes = auxiliary_inodes = 0
    for relative in ('.trellis/scripts', '.trellis/artifacts', '.trellis/stuck-math-audit'):
        _, size, count = inventory(repo / relative, hashes=False)
        auxiliary_bytes += size
        auxiliary_inodes += count
    # Existing per-workspace bounds: statement transforms 64 MiB, reviewer
    # scratch 32 MiB, worker findings 32 MiB; additional config/history margin.
    extras = 192 << 20
    carriers = sum(3 * workers.references[id]['bytes'] for id in pending)
    durable = len(pending) * (2 * source_bytes + auxiliary_bytes + extras) + carriers
    # Reserve one full cache expansion and a shared capture. Count common
    # cache copies by their bytes as well, so unusually large caches do not
    # silently inherit the ordinary 3 GiB estimate.
    build_bytes = build_inodes = 0
    for relative in ('.lake/build', '.lake/config', '.trellis/supervisor/repo/.lake/build',
                     '.trellis/supervisor/repo/.trellis/kernel-replay-environments'):
        _, size, count = inventory(repo / relative, hashes=False)
        build_bytes += size
        build_inodes += count
    cache_bytes = max(workspace_bytes, build_bytes + 2 * source_bytes + auxiliary_bytes)
    cache_inodes = max(workspace_inodes, build_inodes + 2 * source_inodes + auxiliary_inodes)
    return dict(bytes=durable + cache_bytes * (2 if new_capture else 1),
        inodes=len(pending) * (2 * source_inodes + auxiliary_inodes + workspace_inodes)
               + cache_inodes * (2 if new_capture else 1),
        durable_bytes=durable, cache_bytes=cache_bytes, pending=len(pending))


def capture(root, repo, binding):
    marker = root / 'capture.json'
    if marker.exists():
        receipt = read(marker)
        if receipt['binding'] != binding:
            raise CandidateError('shared batch cache binding changed')
        validate(root, receipt)
        return receipt
    if root.exists():
        raise CandidateError('unfinished batch cache retained for inspection')
    root.parent.mkdir(parents=True, exist_ok=True)
    temp = Path(tempfile.mkdtemp(prefix='.batch-cache-', dir=root.parent))
    target = temp / 'repo'
    before = source_manifest(repo)
    workspace._copy_tree(repo, target, exclude=workspace._EXCLUDED)
    scripts = repo / '.trellis/scripts'
    if scripts.is_dir():
        workspace._copy_tree(scripts, target / '.trellis/scripts')
    for relative in ('.lake/build', '.lake/config'):
        if (repo / relative).is_dir():
            workspace._copy_tree(repo / relative, target / relative, clone_builds=True,
                                 omit_module_setup=relative == '.lake/build')
    seeded = workspace._seed_authoritative_build(repo, target)
    if source_manifest(repo) != before or source_manifest(target) != before:
        raise CandidateError('source changed during shared cache capture')
    entries, size, count = inventory(target)
    receipt = dict(format=FORMAT, binding=binding, sources=before, files=entries,
                   allocated_copy_bytes=size, inodes=count, seeded_oleans=seeded)
    workspace.sync_tree(temp)
    atomic(temp / 'capture.json', canonical_json(receipt))
    os.rename(temp, root)
    _sync_dir(root.parent)
    return receipt


def validate(root, receipt):
    if receipt.get('format') != FORMAT or inventory(root / 'repo')[0] != receipt['files']:
        raise CandidateError('shared batch cache contents changed; retain original capture')


def reference(root, dispatch, receipt):
    return dict(path=str(root.relative_to(dispatch)), sha256=digest(canonical_json(receipt)))


def remaining_hydration(donor, repo):
    """Price missing or incomplete cache outputs on an owned pre-launch retry.

    Complete equal files are skipped by resumable build copying. Regular
    source copies inside the supervisor are independently writable, and can
    be overwritten without allocating another whole source tree.
    """
    groups = [(donor / relative, repo / relative, ())
              for relative in ('.lake/build', '.lake/config')]
    authority = Path('.trellis/supervisor/repo')
    if (donor / authority / '.lake/build/lib/lean/Tablet').is_dir():
        groups += [(repo, repo / authority, workspace._EXCLUDED),
                   (repo / '.trellis/scripts', repo / authority / '.trellis/scripts', ())]
        for relative in ('.lake/build/lib/lean/Tablet', '.lake/build/ir/Tablet',
                         '.trellis/kernel-replay-environments'):
            groups.append((donor / authority / relative, repo / authority / relative, ()))
    needed = inodes = 0
    for source, destination, excluded in groups:
        entries, _, _ = inventory(source, exclude=excluded)
        if source.exists() and not destination.exists():
            needed += 4096
            inodes += 1
        for relative, value in entries.items():
            origin, target = source / relative, destination / relative
            if workspace.same_regular_bytes(origin, target):
                continue
            # Partial files receive no content credit. Their existing physical
            # allocation is retained by the in-place copy fallback; a complete
            # conflicting output receives the full replacement allocation charge.
            existing = target.stat().st_blocks * 512 if target.exists() else 0
            if target.exists() and target.stat().st_size >= value['bytes']:
                existing = 0
            needed += max(0, ((value['bytes'] + 4095) // 4096) * 4096 - existing)
            inodes += int(not target.exists())
            parent = target.parent
            while parent != destination and not parent.exists():
                needed += 4096
                inodes += 1
                parent = parent.parent
    # Package payloads use existing immutable hardlinks on this same-filesystem
    # path. Allow directory-entry/inode metadata for links still to be created.
    package_source = repo / '.lake/packages'
    package_target = repo / authority / '.lake/packages'
    if package_source.is_dir():
        for current, directories, files in os.walk(package_source, followlinks=False):
            relative = Path(current).relative_to(package_source)
            target = package_target / relative
            if not target.exists():
                needed += 4096
                inodes += 1
            missing = sum(not (target / name).exists() for name in files)
            needed += ((missing * 256 + 4095) // 4096) * 4096
    return needed, inodes


def hydrate(directory, dispatch, capacity):
    prepared = read(directory / 'prepared.json')
    if prepared.get('host_only'):
        raise CandidateError('host-only preparation cannot hydrate a provider workspace')
    ref = prepared.get('deferred_build_cache')
    if ref is None:
        return
    marker = directory / 'build-cache-ready.json'
    if marker.exists():
        if read(marker)['capture'] != ref:
            raise CandidateError('worker build cache readiness binding changed')
        return  # Never overwrite an adopted worker's legitimate edits/builds.
    if any(directory.rglob('recipe.json')):
        raise CandidateError('worker recipe exists before deferred build preparation')
    relative = Path(ref['path'])
    if relative.is_absolute() or '..' in relative.parts or relative.parts[:1] != ('batch-caches',):
        raise CandidateError('invalid host batch cache reference')
    root = dispatch / relative
    for path in (root, *root.parents):
        if path.is_symlink():
            raise CandidateError('shared cache reference cannot traverse links')
    receipt = read(root / 'capture.json')
    if digest(canonical_json(receipt)) != ref['sha256']:
        raise CandidateError('shared cache receipt changed')
    repo = directory / 'repo'
    if source_manifest(repo) != read(directory / 'base-manifest.json'):
        raise CandidateError('unlaunched deferred workspace source changed')
    validate(root, receipt)
    intent = directory / 'build-cache-intent.json'
    resumed = intent.exists()
    if resumed and read(intent) != dict(capture=ref):
        raise CandidateError('interrupted cache preparation has another identity')
    if resumed:
        for relative in ('.lake/build', '.lake/config', '.trellis/supervisor'):
            inventory(repo / relative, hashes=False, exclude=('packages',))
    if capacity is not None:
        if resumed:
            size, count = remaining_hydration(root / 'repo', repo)
        else:
            size = max(capacity.workspace_bytes, receipt['allocated_copy_bytes'])
            count = max(capacity.workspace_inodes, receipt['inodes'])
        capacity.require_allocation(size, count, check_floor=True)
    if not resumed:
        for relative in ('.lake/build', '.lake/config', '.trellis/supervisor'):
            if (repo / relative).exists() or (repo / relative).is_symlink():
                raise CandidateError('deferred cache has unowned preexisting outputs')
        atomic(intent, canonical_json(dict(capture=ref)))
    # Runs with coordinator.lock; all admission/copy operations are serialized.
    # An interrupted copy is independently writable optional data. Resuming
    # before any recipe may overwrite these cache files, never source/config.
    donor = root / 'repo'
    for relative in ('.lake/build', '.lake/config'):
        if (donor / relative).is_dir():
            workspace._copy_tree(donor / relative, repo / relative, clone_builds=True,
                                 reuse_builds=resumed, omit_module_setup=relative == '.lake/build')
    seeded = workspace._seed_authoritative_build(donor, repo, resume=resumed)
    workspace.repair_packages(dispatch.parent, directory / 'runtime')
    workspace.sync_tree(directory)
    atomic(marker, canonical_json(dict(capture=ref, seeded_oleans=seeded)))
