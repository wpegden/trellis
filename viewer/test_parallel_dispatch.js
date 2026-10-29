const assert = require('assert');
const fs = require('fs');
const os = require('os');
const path = require('path');
const execFile = require('util').promisify(require('child_process').execFile);
const { parallelState, attemptProject, readState } = require('./parallel_dispatch');
const root = fs.mkdtempSync(path.join(os.tmpdir(), 'trellis-parallel-viewer-'));
function write(name, value) {
  const file = path.join(root, name);
  fs.mkdirSync(path.dirname(file), { recursive: true });
  fs.writeFileSync(file, JSON.stringify(value));
}
(async () => { try {
  assert.deepStrictEqual(await parallelState(root), { enabled: false });
  write('protocol_state.json', { phase: 'TheoremStating', stage: 'Reviewer', cycle: 4, dispatch: {
    config: { worker_slots: 4, compiler_slots: 2, verifier_slots: 8 }, tasks: {
      1: { id: 1, launched_at_ms: 100, status: { status: 'checking', candidate_sha256: 'a' },
        binding: { model: 'test', timeout_seconds: 14400 }, assignment: { profile: 'strong',
          compiler: 'managed', task_kind: 'ordinary', scope: { nodes: ['A'] } } },
    }, verifiers: { jobs: {
      7: { id: 7, identity: { lane: 'nl_statement', compiler: 'none' },
        status: { status: 'running', pid: 123 }, required: true, reviewer_requested: false },
    } },
  }});
  write('dispatch/operational-stop.json', { process: 'attempt', returncode: 3, response: { secret: 'do not expose' }});
  const result = await parallelState(root, 1100);
  assert.strictEqual(result.available_slots, 3, 'checking still occupies a worker slot');
  assert.strictEqual(result.tasks[0].elapsed_ms, 1000);
  assert.strictEqual(result.tasks[0].checker, null, 'absence of measurements must not become zero wait');
  assert.deepStrictEqual(result.verifiers, [{ id: 7, lane: 'nl_statement', compiler: 'none',
    status: { status: 'running', pid: 123 }, required: true, reviewer_requested: false, attempt: 'verifier-7' }]);
  assert(!JSON.stringify(result).includes('secret'));
  assert.deepStrictEqual(result.dispatch_receipts, []);
  assert.strictEqual(result.storage_wait, null);
  const wait = { waiting: true, retry_seconds: 30, private: 'storage-secret', filesystems: [{
    path: root, free_bytes: 19 * 1024 ** 3, required_bytes: 22 * 1024 ** 3,
    free_inodes: 200000, required_inodes: 100000,
  }] };
  write('dispatch/preparation-capacity.json', wait);
  const blocked = await parallelState(root, 1100);
  assert.strictEqual(blocked.storage_wait.filesystems[0].shortfall_bytes, 3 * 1024 ** 3);
  assert(!JSON.stringify(blocked).includes('storage-secret'));
  // Capacity changes independently of the cached native protocol snapshot.
  write('dispatch/preparation-capacity.json', { ...wait, waiting: false });
  assert.strictEqual((await parallelState(root, 1100)).storage_wait, null);
  write('dispatch/preparation-capacity.json', { ...wait, filesystems: [{ ...wait.filesystems[0],
    free_bytes: 30 * 1024 ** 3, free_inodes: 100, required_inodes: 1000 }] });
  const inodeWait = (await parallelState(root, 1100)).storage_wait;
  assert.strictEqual(inodeWait.label, 'WAITING FOR FREE INODES');
  assert(inodeWait.reason.includes('900 short'));
  assert(!inodeWait.reason.includes('GiB'));
  for (const bad of [{ waiting: 'true' }, { waiting: true, filesystems: [null] },
    { ...wait, filesystems: [{ ...wait.filesystems[0], free_bytes: -1 }] }]) {
    write('dispatch/preparation-capacity.json', bad);
    assert.strictEqual((await parallelState(root, 1100)).storage_wait, null);
  }
  fs.writeFileSync(path.join(root, 'dispatch/preparation-capacity.json'), '{broken');
  assert.strictEqual((await parallelState(root, 1100)).storage_wait, null);
  fs.unlinkSync(path.join(root, 'dispatch/preparation-capacity.json'));
  // Large requests/seeds are irrelevant to display and must stay out of Node.
  const savedState = fs.readFileSync(path.join(root, 'protocol_state.json'), 'utf8');
  const expanded = JSON.parse(savedState);
  expanded.dispatch.tasks['1'].assignment.request = { private: 'omitted'.repeat(100000) };
  expanded.dispatch.exclusive_workers = { private: 'omitted'.repeat(100000) };
  // A private verifier request alone exceeds the Node helper's output limit.
  // Projection must happen before stdout, not after Node parses the result.
  expanded.dispatch.verifiers.jobs['7'].request = { private: 'verifier-private'.repeat(1200000) };
  expanded.dispatch.verifiers.jobs['7'].identity.input_hashes = { private: 'private-input-hash' };
  expanded.dispatch.verifiers.jobs['7'].identity.panel_sha256 = 'private-panel-hash';
  expanded.in_flight_request = { id: 12, kind: 'Worker', cycle: 4, active_node: 'A',
    mode: 'Normal', payload: { private: 'request-private'.repeat(100000) } };
  expanded.sidecar_queue = [{ node: 'A', entry_seq: 1 }];
  expanded.closure_provenance = { B: { closed_by: 'sidecar', cycle: 3 } };
  expanded.dispatch.verifiers.next_id = 8;
  // Numeric review order matters when a run crosses a digit boundary.
  expanded.dispatch.receipts = Object.fromEntries(Array.from({ length: 12 }, (_, i) => {
    const review_id = 990 + i;
    return [String(review_id), { review_id, response_sha256: 'receipt-private-hash', decisions: [
      { assignment_id: 'rejected<assignment>', task_id: null, rejection: 'cannot include dismissed_tasks' },
      { assignment_id: 'accepted', task_id: 1, rejection: null },
    ] }];
  }));
  write('protocol_state.json', expanded);
  const { stdout: projectedText } = await execFile('python3', [path.join(__dirname, 'parallel_state.py'),
    path.join(root, 'protocol_state.json')], { encoding: 'utf8', maxBuffer: 64 * 1024 });
  assert(!projectedText.includes('verifier-private'));
  assert(!projectedText.includes('private-input-hash'));
  assert(!projectedText.includes('private-panel-hash'));
  assert(!projectedText.includes('request-private'));
  assert(!projectedText.includes('receipt-private-hash'));
  const projected = JSON.parse(projectedText);
  assert.deepStrictEqual(projected.in_flight_request, { id: 12, kind: 'Worker', cycle: 4,
    active_node: 'A', mode: 'Normal' });
  assert.deepStrictEqual(projected.sidecar_queue, expanded.sidecar_queue);
  assert.deepStrictEqual(projected.closure_provenance, expanded.closure_provenance);
  assert.deepStrictEqual(projected.dispatch.verifiers, {
    jobs: { 7: JSON.parse(savedState).dispatch.verifiers.jobs['7'] },
  });
  assert.deepStrictEqual(projected.dispatch.receipts.map(r => r.review_id),
    Array.from({ length: 10 }, (_, i) => 992 + i));
  const expected = { ...result, dispatch_receipts: projected.dispatch.receipts.slice().reverse() };
  assert.strictEqual(expected.dispatch_receipts[0].decisions[0].rejection, 'cannot include dismissed_tasks');
  assert.strictEqual(expected.dispatch_receipts[0].decisions[1].task_id, 1);
  const page = fs.readFileSync(path.join(__dirname, 'public/index.html'), 'utf8');
  const renderSource = page.slice(page.indexOf('async function renderParallelDetail()'),
    page.indexOf('async function loadParallelCalls('));
  const elements = Object.fromEntries(['detail-content', 'parallel-summary', 'parallel-tasks']
    .map(id => [id, { innerHTML: '', textContent: '', querySelectorAll: () => [] }]));
  const render = require('vm').runInNewContext(`(${renderSource})`, {
    parallelRefreshTimer: null, clearTimeout() {}, setTimeout() {}, detailMode: 'parallel',
    document: { getElementById: id => elements[id] }, apiPath: value => value,
    fetch: async () => ({ json: async () => expected }),
    escHtml: value => String(value).replace(/</g, '&lt;').replace(/>/g, '&gt;'),
    detailTable: (_columns, rows) => rows.flat().join(' '),
  });
  await render();
  assert(elements['parallel-tasks'].innerHTML.includes('Rejected: cannot include dismissed_tasks'));
  assert(elements['parallel-tasks'].innerHTML.includes('Assigned task #1'));
  assert(elements['parallel-tasks'].innerHTML.includes('rejected&lt;assignment&gt;'));
  assert(elements['parallel-tasks'].innerHTML.includes('data-parallel-attempt="review-1001"'));
  const readSync = fs.readFileSync;
  fs.readFileSync = function(file, ...args) {
    assert.notStrictEqual(String(file), path.join(root, 'protocol_state.json'), 'state read must not block Node');
    return readSync.call(fs, file, ...args);
  };
  try {
    const [first, second] = await Promise.all([parallelState(root, 1100), parallelState(root, 1100)]);
    assert.deepStrictEqual(first, expected);
    assert.deepStrictEqual(second, expected);
  } finally { fs.readFileSync = readSync; }
  fs.writeFileSync(path.join(root, 'protocol_state.json'), '{');
  await assert.rejects(parallelState(root), /JSONDecodeError/);
  expanded.cycle = 5;
  write('protocol_state.json', expanded);
  assert.strictEqual((await parallelState(root)).cycle, 5, 'read errors must not poison the cache');
  const coarseRepo = path.join(root, 'coarse-repo');
  fs.mkdirSync(coarseRepo);
  write('runtime_metadata.json', { repo_path: coarseRepo });
  const report = { version: 1, evidence: null, migrated_from: 'f'.repeat(64),
    context: { goals: ['A'], previous: {}, reason: 'Inspect <A>' },
    plan: { written_at_cycle: 4, written_by_request: 9, report: 'Advice <body>' } };
  const reportBytes = JSON.stringify(report);
  const reference = { sha256: require('crypto').createHash('sha256').update(reportBytes).digest('hex'),
    bytes: Buffer.byteLength(reportBytes) };
  write(`coarse-repo/.trellis-history/coarse-audits/${reference.sha256}.json`, report);
  expanded.coarse_goals = { limit: 2, goals: { A: { status: 'active', started_cycle: 3,
    completed_attempts: 2, attempts_since_progress: 1, history_head: reference, plan: { tasks: [] } } },
    pending_audits: [] };
  expanded.dispatch.verifiers.jobs['7'].request = { scoped_audit: { goals: ['A'] } };
  write('protocol_state.json', expanded);
  const cones = await parallelState(root);
  assert.strictEqual(cones.coarse_goals.goals[0].history[0].report, 'Advice <body>');
  assert.deepStrictEqual(cones.verifiers[0].coarse_nodes, ['A']);
  fs.writeFileSync(path.join(coarseRepo, `.trellis-history/coarse-audits/${reference.sha256}.json`), 'corrupt');
  expanded.cycle += 1; write('protocol_state.json', expanded);
  const corrupt = await parallelState(root);
  assert(corrupt.coarse_goals.goals[0].history_error.includes('size'));
  assert.strictEqual(corrupt.coarse_goals.goals[0].history.length, 0);
  const repo = path.join(root, 'dispatch/1/repo'); fs.mkdirSync(repo, { recursive: true });
  write('dispatch/1/runtime/runtime_metadata.json', { repo_path: repo });
  assert.strictEqual(attemptProject(root, '1').repoPath, repo);
  write('dispatch/1/runtime/protocol_state.json', { cycle: 6, in_flight_request: {
    id: 2, cycle: 6, kind: 'Worker', active_node: 'A', mode: 'Normal',
  }});
  const { buildChatCalls, buildChatEvents, liveRequestsFromEventLog } = require('./server');
  const attempt = attemptProject(root, '1');
  assert.strictEqual(liveRequestsFromEventLog(attempt).bursts[0].request_id, 2);
  const artifact = 'trellis_worker_2_result';
  const chat = path.join(repo, '.trellis/chats/live', artifact);
  fs.mkdirSync(chat, { recursive: true });
  fs.writeFileSync(path.join(chat, 'call.json'), JSON.stringify({ request_id: 2, cycle: 6, kind: 'Worker', model: 'fixture', provider: 'codex' }));
  fs.writeFileSync(path.join(chat, 'output.log'), '{"type":"turn.completed"}\n');
  const calls = buildChatCalls(attempt, 'live', { showStale: true });
  assert(calls.calls.some(c => c.request_id === 2 && c.artifact_id === artifact));
  const events = buildChatEvents(attempt, 'live', artifact, 'transcript');
  assert.strictEqual(events.missing, false);
  assert.strictEqual(events.call.request_id, 2);
  const privateState = path.join(root, 'dispatch/1/runtime/protocol_state.json');
  const originalPrivate = fs.readFileSync(privateState);
  const privateProjection = await readState(privateState);
  fs.writeFileSync(privateState + '.gz', require('zlib').gzipSync(originalPrivate));
  fs.writeFileSync(privateState + '.retained.json', JSON.stringify({
    format: 'trellis-private-evidence-gzip/1', name: 'protocol_state.json',
    bytes: originalPrivate.length, mode: 420,
    sha256: require('crypto').createHash('sha256').update(originalPrivate).digest('hex'),
  }));
  fs.unlinkSync(privateState);
  const archivedProjection = await readState(privateState);
  assert.deepStrictEqual(archivedProjection, privateProjection);
  const archivedCalls = buildChatCalls({ ...attempt, protocolSnapshot: archivedProjection }, 'live', { showStale: true });
  assert(archivedCalls.calls.some(c => c.request_id === 2 && c.artifact_id === artifact));
  fs.writeFileSync(privateState + '.gz', 'corrupt archived snapshot');
  await assert.rejects(readState(privateState), /corrupt/);
  fs.unlinkSync(privateState + '.gz');
  fs.unlinkSync(privateState + '.retained.json');
  const display = { format: 'trellis-private-display-state/1', task_id: 1,
    checkpoint_commit: 'a'.repeat(40), phase: null, stage: null,
    cycle: 6, in_flight_request: JSON.parse(originalPrivate).in_flight_request };
  write('dispatch/1/runtime/display-state.json', display);
  // A sibling call copied with the source workspace is not this task's history.
  const sibling = path.join(repo, '.trellis/chats/live/trellis_worker_999_result');
  fs.mkdirSync(sibling, { recursive: true });
  fs.writeFileSync(path.join(sibling, 'call.json'), JSON.stringify({ request_id: 999, cycle: 5, kind: 'Worker' }));
  const retired = await readState(privateState);
  assert.deepStrictEqual(retired, privateProjection);
  const retiredCalls = buildChatCalls({ ...attempt, protocolSnapshot: retired }, 'live', { showStale: true });
  assert(retiredCalls.calls.some(c => c.request_id === 2 && c.artifact_id === artifact));
  assert(!retiredCalls.calls.some(c => c.request_id === 999), 'retirement must not expose cloned sibling calls');
  for (const bad of [null, { ...display, task_id: 2 }, { ...display, in_flight_request: null }]) {
    write('dispatch/1/runtime/display-state.json', bad);
    await assert.rejects(readState(privateState), /invalid retired/);
  }
  for (const kind of ['review', 'verifier']) {
    const statePath = path.join(root, `dispatch/${kind}-7/runtime/protocol_state.json`);
    const { task_id, ...shared } = display;
    const owned = { ...shared, format: 'trellis-private-display-state/2', owner: { kind, id: 7 } };
    write(`dispatch/${kind}-7/runtime/display-state.json`, owned);
    assert.deepStrictEqual(await readState(statePath), privateProjection);
    write(`dispatch/${kind}-7/runtime/display-state.json`, { ...owned, owner: { kind, id: 8 } });
    await assert.rejects(readState(statePath), /invalid retired/);
  }
  assert.throws(() => attemptProject(root, '../../elsewhere'), /invalid/);
  fs.symlinkSync(root, path.join(root, 'dispatch/2'));
  assert.throws(() => attemptProject(root, '2'), /escapes/);
  console.log('parallel viewer tests passed');
} finally { fs.rmSync(root, { recursive: true, force: true }); }
})().catch(error => { console.error(error); process.exitCode = 1; });
