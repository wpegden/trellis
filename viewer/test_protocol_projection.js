// Hot protocol readers share a compact asynchronous projection, including when
// the snapshot changes mid-read. A slow helper must never stall HTTP health.
const assert = require('assert');
const fs = require('fs');
const http = require('http');
const os = require('os');
const path = require('path');
const cp = require('child_process');
const root = fs.mkdtempSync(path.join(os.tmpdir(), 'trellis-protocol-projection-'));
process.env.PROJECTS_ROOT = root;
process.env.STATIC_OUT = path.join(root, 'static');
process.env.TRELLIS_VIEWER_READ_TOKEN = '0';
process.env.TRELLIS_VIEWER_BIND = '127.0.0.1';
function write(file, value) {
  fs.mkdirSync(path.dirname(file), { recursive: true });
  fs.writeFileSync(file, JSON.stringify(value));
}
const repo = path.join(root, 'example'), runtime = `${repo}-runtime`;
const stateFile = path.join(runtime, 'protocol_state.json');
write(path.join(repo, 'trellis.config.json'), {});
write(path.join(runtime, 'runtime_metadata.json'), { repo_path: repo });
write(stateFile, { cycle: 3 });
const canonical = { phase: 'ProofFormalization', stage: 'Worker', cycle: 3,
  in_flight_request: { id: 90, kind: 'Worker', cycle: 3, active_node: 'A', mode: 'Normal' },
  dispatch: { config: { worker_slots: 2, compiler_slots: 1 }, tasks: {}, verifiers: { jobs: {} } } };
const privateRepo = path.join(runtime, 'dispatch/1/repo');
const privateRuntime = path.join(runtime, 'dispatch/1/runtime');
fs.mkdirSync(privateRepo, { recursive: true });
write(path.join(privateRuntime, 'runtime_metadata.json'), { repo_path: privateRepo });
write(path.join(privateRuntime, 'protocol_state.json'), { cycle: 1 });
const attempt = { cycle: 1, in_flight_request: { id: 3, cycle: 1, kind: 'Worker', active_node: 'B', mode: 'Normal' } };
const originalExec = cp.execFile;
const originalRead = fs.readFileSync;
const pending = [];
let active = 0, maxActive = 0;
cp.execFile = (command, args, options, callback) => {
  if (path.basename(args[0] || '') !== 'parallel_state.py') return originalExec(command, args, options, callback);
  active++; maxActive = Math.max(active, maxActive);
  pending.push({ file: args[1], complete(value, error) { active--; callback(error, JSON.stringify(value)); } });
};
fs.readFileSync = function(file, ...args) {
  assert.notStrictEqual(path.basename(String(file)), 'protocol_state.json', 'HTTP may not load full protocol state in Node');
  return originalRead.call(this, file, ...args);
};
const { app, buildChatCalls } = require('./server');
const { readState } = require('./parallel_dispatch');
async function waitFor(predicate) {
  for (let i = 0; i < 1000; i++) {
    if (predicate()) return;
    await new Promise(resolve => setTimeout(resolve, 2));
  }
  throw new Error('expected projection did not start');
}
const server = app.listen(0, '127.0.0.1', async () => {
  function get(url) {
    return new Promise((resolve, reject) => {
      const request = http.get({ host: '127.0.0.1', port: server.address().port, path: url }, res => {
        let body = '';
        res.on('data', chunk => { body += chunk; });
        res.on('end', () => resolve({ status: res.statusCode, payload: JSON.parse(body) }));
      }).on('error', reject);
      request.setTimeout(2000, () => request.destroy(new Error('HTTP request stalled')));
    });
  }
  try {
    const tasks = get('/trellis/example/api/parallel.json');
    const chats = get('/trellis/example/api/chat-calls.json?cycle=live');
    await waitFor(() => pending.length === 1);
    assert.strictEqual((await get('/trellis/api/health.json')).status, 200);
    assert.strictEqual(active, 1);
    pending[0].complete(canonical);
    const [t, c] = await Promise.all([tasks, chats]);
    assert.strictEqual(t.payload.cycle, 3);
    assert.strictEqual(c.status, 200);
    assert.strictEqual(pending.length, 1, 'both endpoints must share one snapshot read');

    const privateCalls = get('/trellis/example/api/parallel.json?attempt=1');
    await waitFor(() => pending.length === 2);
    assert.strictEqual(pending[1].file, path.join(privateRuntime, 'protocol_state.json'));
    pending[1].complete(attempt);
    const calls = (await privateCalls).payload.calls;
    assert(calls.some(call => call.request_id === 3), 'private chat uses exact private request');
    assert(!calls.some(call => call.request_id === 90), 'canonical request cannot leak into private chat');
    // Frozen requests (including paused requests) retain their exact ID.
    const direct = buildChatCalls({ repoPath: privateRepo, stateDir: path.join(privateRepo, '.trellis'),
      repoType: 'trellis', runtimeRoot: privateRuntime, protocolSnapshot: attempt }, 'live');
    assert(direct.calls.some(call => call.request_id === 3));

    write(stateFile, { cycle: 4, marker: 'first replacement' });
    const firstReplacement = readState(stateFile);
    await waitFor(() => pending.length === 3);
    write(stateFile, { cycle: 5, marker: 'newer and larger replacement' });
    const secondReplacement = readState(stateFile);
    await new Promise(resolve => setTimeout(resolve, 20));
    assert.strictEqual(pending.length, 3, 'changed file waits for existing helper');
    pending[2].complete({ cycle: 4 });
    assert.strictEqual((await firstReplacement).cycle, 4);
    await waitFor(() => pending.length === 4);
    pending[3].complete({ cycle: 5 });
    assert.strictEqual((await secondReplacement).cycle, 5);
    assert.strictEqual(maxActive, 1);

    write(stateFile, { cycle: 6 });
    const failed = readState(stateFile);
    await waitFor(() => pending.length === 5);
    pending[4].complete(null, new Error('unreadable snapshot'));
    await assert.rejects(failed, /unreadable snapshot/);
    const retry = readState(stateFile);
    await waitFor(() => pending.length === 6);
    pending[5].complete({ cycle: 6 });
    assert.strictEqual((await retry).cycle, 6, 'failed reads must not poison the cache');
    console.log('protocol projection tests passed');
  } catch (error) {
    console.error(error);
    process.exitCode = 1;
  } finally {
    server.close();
    cp.execFile = originalExec;
    fs.readFileSync = originalRead;
    fs.rmSync(root, { recursive: true, force: true });
  }
});
