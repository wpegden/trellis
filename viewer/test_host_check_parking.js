const assert = require('assert');
const fs = require('fs');
const os = require('os');
const path = require('path');
const { parallelState } = require('./parallel_dispatch');
const root = fs.mkdtempSync(path.join(os.tmpdir(), 'trellis-host-parking-view-'));
function write(name, value) {
  const file = path.join(root, name);
  fs.mkdirSync(path.dirname(file), { recursive: true });
  fs.writeFileSync(file, JSON.stringify(value));
}
(async () => { try {
  write('protocol_state.json', { phase: 'ProofFormalization', dispatch: {
    config: { worker_slots: 3, profiles: {} }, tasks: { 1: { id: 1,
      binding: {}, assignment: { scope: { nodes: ['A'] } },
      status: { status: 'checking', candidate_sha256: 'candidate' } } }, verifiers: { jobs: {} } } });
  const marker = { version: 1, state: 'parked', task_id: 1, candidate_sha256: 'candidate',
    reason: 'capture unavailable', evidence: '1/acceptance/4/finalization/3',
    bindings: { private: 'not-for-viewer' } };
  write('dispatch/1/host-check-blocked.json', marker);
  let state = await parallelState(root);
  assert.strictEqual(state.available_slots, 2);
  assert.strictEqual(state.tasks[0].status.status, 'checking');
  assert.deepStrictEqual(state.tasks[0].host_check_blocked, {
    summary: 'Host check blocked: capture unavailable', evidence: marker.evidence, slot_retained: true });
  assert(!JSON.stringify(state).includes('not-for-viewer'));
  // Host-marker updates must be visible even without a new native snapshot.
  write('dispatch/1/host-check-blocked.json', { ...marker, state: 'resuming' });
  assert.strictEqual((await parallelState(root)).tasks[0].host_check_blocked, null);
  write('dispatch/1/host-check-blocked.json', { ...marker, candidate_sha256: 'different' });
  assert.strictEqual((await parallelState(root)).tasks[0].host_check_blocked, null);
  console.log('host-check parking projection passed');
} finally { fs.rmSync(root, { recursive: true, force: true }); } })()
  .catch(error => { console.error(error); process.exitCode = 1; });
