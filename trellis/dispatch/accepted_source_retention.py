"""Retire exact Git source copies only after accepted publication and sealing.

Failed workers retain their retry source. Accepted workers retain their complete
published candidate, unique outer source bytes, and all handoff inputs. Future
findings select the same seal as this policy and never fall back to outer source.
"""
from pathlib import Path

from . import auxiliary_retention as engine, candidate, handoffs
from . import retained_files as retained, terminal_retention as terminal

FORMAT = 'trellis-accepted-worker-source-retirement/1'
RECEIPT = 'accepted-sources-retired.json'


def roots(directory):
    for surface in ('Tablet', 'reference'):
        yield directory, directory / 'repo', surface


def _seal_admission(directory, publication):
    sealed = handoffs._selected_candidate(directory)
    if sealed is None:
        raise ValueError('accepted source retirement requires a selected sealed candidate')
    signatures = {}

    def observe(path):
        path = retained.regular_path(path)
        signatures[path] = (retained._signature(path.stat()), engine._parents(path, directory))

    observe(sealed / 'candidate.json')
    observe(sealed / 'sha256')
    manifest = terminal._json(sealed / 'candidate.json')
    for relative, change in manifest['changes'].items():
        candidate.safe_relative(relative)
        if change['after'] is not None:
            observe(sealed / 'files' / relative)
    record, sha = candidate.read(sealed)  # Full byte verification stays off-lock.
    if (record != manifest or record['task_id'] != publication['task_id']
            or sha != publication['candidate_sha256']):
        raise ValueError('accepted source candidate differs from committed publication')

    def fence():
        if handoffs._selected_candidate(directory) != sealed:
            raise ValueError('accepted source handoff candidate selection changed')
        for path, (signature, parents) in signatures.items():
            if (retained._signature(retained.regular_path(path).stat()) != signature
                    or engine._parents(path, directory) != parents):
                raise ValueError('accepted source candidate changed before retirement')

    fence()
    return dict(publication, candidate=str(sealed.relative_to(directory))), fence


def retire(collector, directory, observed, *, apply=False, accepted=None):
    result = dict(status='ineligible', removed=0, removed_bytes=0,
                  removed_allocated_bytes=0, retained_files=0)
    _, directory = retained.owner_directory(collector.root, Path(directory))
    if (retained.owner_identity(directory)['kind'] != 'worker'
            or directory.name not in observed or collector.cancelled()):
        return result
    witness = collector.witness()
    publication = witness.get('accepted_source_candidates', {}).get(directory.name)
    if (publication is None or accepted is None
            or accepted(directory.name) != (observed[directory.name], publication)):
        return result
    if handoffs._selected_candidate(directory) is None:
        return dict(result, status='no_sealed_candidate')

    def admission(current):
        # Re-evaluate on every restart, never authorize from our private receipt.
        if current.get('accepted_source_candidates', {}).get(directory.name) != publication:
            raise ValueError('accepted source publication changed')
        record, seal_fence = _seal_admission(directory, publication)
        def fence():
            if accepted(directory.name) != (observed[directory.name], publication):
                raise ValueError('accepted source native observation changed')
            seal_fence()
        fence()
        return record, fence

    return engine._retire(collector, directory, observed, apply=apply,
        roots_fn=roots, format=FORMAT, receipt_name=RECEIPT,
        git_surfaces=('Tablet', 'reference'), file_surfaces=(), admission=admission)
