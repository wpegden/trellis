"""Explicit closure read epochs; independent collectors retain shared admission."""
import logging
import time

from . import admission
from .protocol import CheckerRequest, ProtocolError


def collect(server, request):
    """Caller holds the workspace read gate throughout both fresh read passes.

    This prepared-only operation never builds. Native prefetch already owns an
    exact materialized-environment witness. A cold epoch returns no evidence;
    the caller can retain ordinary preparation/diagnostics. Each node still
    runs its independent collector, with unchanged principal/premise checks.
    """
    from .server import (_sha256_file_or_empty, _local_closure_context_cache_key,
                         LOCAL_CLOSURE_AXIOMS_CACHE_VERSION, store_local_closure_axioms)
    nodes = list(request.nodes)
    deadline = time.monotonic() + request.timeout_secs

    def remaining():
        admission.check_cancelled()
        seconds = deadline - time.monotonic()
        if seconds < 1:
            raise ProtocolError('timeout', 'closure batch deadline exhausted')
        return seconds

    def inputs():
        return (_sha256_file_or_empty(server._local_closure_script_path),
                _sha256_file_or_empty(server._toolchain_path),
                _sha256_file_or_empty(server._lake_manifest_path))

    def read_pass(context):
        # The group is known before compilation: no arrival timer or sharing
        # across subprocess boundaries. Both helpers retain their drift checks.
        ready = server._semantic_replay_readiness(nodes)
        keys = server._concurrent_closure_cache_keys([(node, *context) for node in nodes])
        return ready, {node: keys[(node, *context)] for node in nodes}

    remaining()
    context = inputs()
    ready, keys = read_pass(context)
    if not context[0] or not context[1] or not all(ready.get(n) and keys.get(n) for n in nodes):
        return ({'request_id': request.request_id, 'status': 'cold', 'nodes': {},
                 'reason': 'closure batch requires current replay-attested inputs'}, None,
                {'closure_batch_nodes': len(nodes), 'closure_batch_cold': True})
    owners = sorted(set(request.raw.get('premise_owners', [])))
    modules = set(request.raw.get('module_owners', []))
    principals = request.raw['principal_names']
    no_axcheck = request.raw.get('no_axcheck', False)

    def run(node):
        raw = dict(premise_owners=owners, module_owner=node in modules, no_axcheck=no_axcheck)
        if node not in modules:
            raw['principal_name'] = principals[node]
        child = CheckerRequest('local_closure_axioms', request.request_id,
                               node_name=node, timeout_secs=remaining(), raw=raw)
        response, _, extra = server._handle_local_closure_axioms(
            child, prepared_replay=True, batch_keys=keys)
        return node, dict(response), extra

    parts = admission.map_prepared('local_closure', run, nodes)
    remaining()
    ready_after, keys_after = read_pass(context)
    stable_context = inputs() == context
    responses = {}
    hits = 0
    for node, response, extra in parts:
        hits += int(bool(extra.get('local_closure_axioms_cache_hit')))
        if not stable_context or not ready_after.get(node) or keys_after.get(node) != keys[node]:
            response['status'] = 'internal_error'
            response['errors'] = [*response.get('errors', []),
                                  'closure inputs changed during explicit batch; evidence discarded']
        elif (response.get('returncode') == 0 and response.get('status') == 'ok'
              and not response.get('timed_out') and not response.get('spawn_error')
              and response.get('premise_owners', []) == owners):
            key = _local_closure_context_cache_key(keys[node],
                '<module-owner>' if node in modules else principals[node], owners, no_axcheck)
            # Same best-effort atomic sidecar store as the scalar handler.
            try:
                store_local_closure_axioms(server.local_closure_axioms_cache_dir, key,
                    node_name=node, response=response, cache_version=LOCAL_CLOSURE_AXIOMS_CACHE_VERSION)
            except Exception:
                logging.getLogger(__name__).exception('closure cache store failed for %s', node)
        responses[node] = response
    return ({'request_id': request.request_id, 'status': 'ok', 'nodes': responses}, 0,
            {'closure_batch_nodes': len(nodes), 'closure_batch_cache_hits': hits,
             'closure_batch_read_passes': 2})
