"""Ordinary host response hydration, with optional compilation admitted centrally."""
import json
import os
from pathlib import Path

from .candidate import canonical_json
from .publication import atomic


def enrich(runtime, response):
    runtime = Path(runtime)
    if response.get('rust_witness_artifact'):
        from trellis.atomic_actions.checker_client import _round_trip, _next_request_id
        atomic(runtime / 'host-response.json', canonical_json(response))
        result = _round_trip(Path(os.environ['TRELLIS_CHECKER_SOCKET']),
            dict(op='host_response_enrichment', request_id=_next_request_id()), timeout_secs=3600)
        return result['response']
    from trellis.runtime.kernel_cli import run_kernel_cli
    return run_kernel_cli(dict(action='dispatch_enrich_response', root=str(runtime), response=response))['output']
