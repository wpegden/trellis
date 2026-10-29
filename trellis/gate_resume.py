"""Read-only binding checks for a viewer-requested parked-gate restart."""
import hashlib
import json
from pathlib import Path
import sys


def check(runtime, repo, response_sha256):
    runtime, repo = Path(runtime), Path(repo)
    raw = (runtime / 'human_gate_response.json').read_bytes()
    if len(raw) > 4096 or hashlib.sha256(raw).hexdigest() != response_sha256:
        raise ValueError('saved gate decision changed')
    response = json.loads(raw)
    state = json.loads((runtime / 'protocol_state.json').read_bytes())
    request = state.get('in_flight_request') or {}
    pause = json.loads((runtime / 'pause_request.json').read_bytes())
    if (state.get('stage') != 'HumanGate'
            or request.get('kind') not in ('HumanGate', 'human_gate')
            or type(request.get('id')) is not int or type(state.get('cycle')) is not int
            or type(request.get('cycle')) is not int or request['cycle'] != state['cycle']
            or response.get('choice') not in ('approve', 'feedback')
            or type(response.get('cycle')) is not int or type(response.get('request_id')) is not int
            or response['cycle'] != state['cycle'] or response['request_id'] != request['id']):
        raise ValueError('saved decision does not bind the current human gate')
    if pause.get('kind') != 'gate_parked' or pause.get('armed_at_cycle') != state['cycle']:
        raise ValueError('only the current gate parking pause can be resumed')
    sentinel = repo / '.trellis-stop-after-checkpoint'
    if sentinel.exists() and not sentinel.read_text().startswith('[trellis-pause] gate_parked:'):
        raise ValueError('an unrelated stop marker is present')


if __name__ == '__main__':
    check(*sys.argv[1:])
