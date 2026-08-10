// Tests for the Grunts tab's server surface (/api/grunts.json), driven
// through the exported core assembler against fixture runtime roots.
//
// Fixtures live under the worktree (viewer/.test-fixtures-grunts/), never
// /tmp, and are removed on the way out.
//
// Run: node viewer/test_grunts.js
const assert = require('assert');
const fs = require('fs');
const path = require('path');

const { gruntsStateForRuntimeRoot } = require('./server');

const root = fs.mkdtempSync(path.join(__dirname, '.test-fixtures-grunts-'));

function runtimeRoot(name) {
  const p = path.join(root, name);
  fs.mkdirSync(path.join(p, 'sidecar'), { recursive: true });
  return p;
}
function writeJson(p, value) {
  fs.mkdirSync(path.dirname(p), { recursive: true });
  fs.writeFileSync(p, typeof value === 'string' ? value : JSON.stringify(value, null, 1));
}
function spool(rt, dir, n) {
  const d = path.join(rt, 'sidecar', 'spool', dir);
  fs.mkdirSync(d, { recursive: true });
  for (let i = 0; i < n; i++) writeJson(path.join(d, `attempt-${dir}-${i}.json`), { i });
}

const tests = [];
function test(name, fn) { tests.push([name, fn]); }

// ---------------------------------------------------------------------
test('disabled: null runtime root', () => {
  assert.deepStrictEqual(gruntsStateForRuntimeRoot(null), {
    enabled: false, reason: 'no_runtime_root',
  });
});

test('disabled: runtime root without a sidecar dir', () => {
  const rt = path.join(root, 'no-sidecar');
  fs.mkdirSync(rt, { recursive: true });
  writeJson(path.join(rt, 'protocol_state.json'), { sidecar_queue: [] });
  const d = gruntsStateForRuntimeRoot(rt);
  assert.strictEqual(d.enabled, false);
  assert.strictEqual(d.reason, 'no_sidecar_dir');
  // Degradation is total: nothing else is promised (the frontend hides
  // the tab on `enabled:false` alone).
  assert.deepStrictEqual(Object.keys(d).sort(), ['enabled', 'reason']);
});

test('enabled: empty sidecar dir degrades to an empty-but-enabled payload', () => {
  const rt = runtimeRoot('bare');
  const d = gruntsStateForRuntimeRoot(rt);
  assert.strictEqual(d.enabled, true);
  assert.deepStrictEqual(d.in_flight, []);
  assert.deepStrictEqual(d.queue, []);
  assert.deepStrictEqual(d.history, []);
  assert.deepStrictEqual(d.errors, []);
  assert.strictEqual(d.pool.grunts, null);
  assert.strictEqual(d.pool.idle, null);
  assert.strictEqual(d.export.present, false);
  assert.strictEqual(d.queue_source, 'export');
  assert.deepStrictEqual(d.spool, {
    pending: 0, claimed: 0, applied: 0, rejected: 0, abandoned: 0, inflight: 0,
    outcomes: 0, claimed_outcomes: 0, outcomes_consumed: 0,
  });
});

// ---------------------------------------------------------------------
// A fully-populated run, shaped after the live perfect-runtime sidecar.
function populated(name, extra) {
  const rt = runtimeRoot(name);
  const sc = path.join(rt, 'sidecar');
  writeJson(path.join(sc, 'candidates.json'), Object.assign({
    schema: 2,
    snapshot_sha: 'deadbeef',
    generated_at_ms: 1_000_000,
    cycle: 309,
    phase: 'TheoremStating',
    sidecar_window_open: true,
    queue: [
      { node: 'Alpha', entry_seq: 1, queued_at_cycle: 309, status: 'ready',
        node_file_sha256: 'aa', statement_prefix_sha256: 'bb' },
      { node: 'Beta', entry_seq: 2, queued_at_cycle: 309, status: 'blocked:active_node',
        node_file_sha256: 'cc', statement_prefix_sha256: 'dd' },
      { node: 'Gamma', entry_seq: 3, queued_at_cycle: 310, status: 'ready',
        node_file_sha256: 'ee', statement_prefix_sha256: 'ff' },
    ],
    eligible_now: [{ node: 'Delta', tier: 1 }, { node: 'Epsilon', tier: 2 }],
    pruned_recent: [
      { node: 'Old1', entry_seq: 90, cycle: 300, reason: 'closed_elsewhere' },
      { node: 'Old2', entry_seq: 91, cycle: 305, reason: 'ineligible' },
    ],
    recent_closures: [{ node: 'Zeta', cycle: 308, model: 'export-model' }],
  }, (extra || {}).candidates || {}));
  writeJson(path.join(sc, 'status.json'), Object.assign({
    generated_at_ms: 1_050_000,
    grunts: 2,
    in_flight: [
      { node: 'Alpha', entry_seq: 1, grunt: 0, attempt_id: 'sc-a', started_at_ms: 1_040_000 },
    ],
    attempts: {
      Zeta: [{ entry_seq: 7, attempt_id: 'sc-z', status: 'success', detail: '',
        iterations: 35, prompt_tokens: 100, completion_tokens: 10, wall_secs: 164.5, ts: 999 }],
      Beta: [{ entry_seq: 2, attempt_id: 'sc-b', status: 'failed', detail: 'sorry remained',
        iterations: 60, prompt_tokens: 200, completion_tokens: 20, wall_secs: 400, ts: 998 }],
    },
  }, (extra || {}).status || {}));
  writeJson(path.join(sc, 'manager_cursor.json'), { last_seen_cycle: 309 });
  fs.writeFileSync(path.join(sc, 'daemon.pid'), '4242\n');
  writeJson(path.join(rt, 'protocol_state.json'), Object.assign({
    sidecar_queue: [
      { node: 'Alpha', entry_seq: 1, queued_at_cycle: 309 },
      { node: 'Beta', entry_seq: 2, queued_at_cycle: 309 },
      { node: 'Gamma', entry_seq: 3, queued_at_cycle: 310 },
      { node: 'Newborn', entry_seq: 4, queued_at_cycle: 311 },
    ],
    sidecar_queue_seq: 4,
    sidecar_closures: {
      Zeta: { attempt_id: 'sc-z', provider: 'mistral', model: 'leanstral', cycle: 308, wall_ms: 164500 },
      Eta: { attempt_id: 'sc-e', provider: 'mistral', model: 'leanstral', cycle: 301, wall_ms: 90000 },
    },
  }, (extra || {}).state || {}));
  spool(rt, 'pending', 2);
  spool(rt, 'claimed', 1);
  spool(rt, 'applied', 3);
  spool(rt, 'rejected', 0);
  spool(rt, 'outcomes', 1);
  spool(rt, 'outcomes_consumed', 2);
  return rt;
}

function ledgerRow(i, status) {
  return JSON.stringify({
    attempt_id: `sc-${i}`, node: `Node${i}`, entry_seq: i, grunt: i % 2,
    status: status || (i % 3 === 0 ? 'failed' : 'success'),
    detail: status === 'error' ? 'transport' : '',
    iterations: i, prompt_tokens: i * 10, completion_tokens: i,
    wall_secs: i * 1.5, ts: 1000 + i,
    provenance: { provider: 'mistral', model: 'leanstral' },
    timings: { api_secs: 1.0 },
  });
}

test('enabled: pool, in-flight elapsed, export header, spool counts', () => {
  const rt = populated('full');
  const d = gruntsStateForRuntimeRoot(rt, { now: 1_060_000 });
  assert.strictEqual(d.enabled, true);
  assert.strictEqual(d.sidecar_dir, path.join(rt, 'sidecar'));
  assert.strictEqual(d.pool.grunts, 2);
  assert.strictEqual(d.pool.busy, 1);
  assert.strictEqual(d.pool.idle, 1);
  assert.strictEqual(d.pool.daemon_pid, 4242);
  assert.strictEqual(d.pool.last_seen_cycle, 309);
  assert.strictEqual(d.pool.status_age_ms, 10_000);
  assert.strictEqual(d.export.present, true);
  assert.strictEqual(d.export.cycle, 309);
  assert.strictEqual(d.export.phase, 'TheoremStating');
  assert.strictEqual(d.export.window_open, true);
  assert.strictEqual(d.export.age_ms, 60_000);
  assert.strictEqual(d.export.eligible_now, 2);
  assert.strictEqual(d.in_flight.length, 1);
  assert.strictEqual(d.in_flight[0].node, 'Alpha');
  assert.strictEqual(d.in_flight[0].grunt, 0);
  assert.strictEqual(d.in_flight[0].elapsed_ms, 20_000);
  assert.deepStrictEqual(d.spool, {
    pending: 2, claimed: 1, applied: 3, rejected: 0, abandoned: 0, inflight: 0,
    // The outcome lanes: daemon -> kernel "this generation is spent".
    outcomes: 1, claimed_outcomes: 0, outcomes_consumed: 2,
  });
});

test('queue: kernel order is authoritative, export supplies blocked reasons', () => {
  const d = gruntsStateForRuntimeRoot(populated('queue'), { now: 1_060_000 });
  assert.strictEqual(d.queue_source, 'kernel');
  assert.deepStrictEqual(d.queue.map((r) => r.node), ['Alpha', 'Beta', 'Gamma', 'Newborn']);
  assert.strictEqual(d.queue[0].status, 'ready');
  assert.strictEqual(d.queue[0].in_flight, true);
  assert.strictEqual(d.queue[0].grunt, 0);
  assert.strictEqual(d.queue[1].status, 'blocked');
  assert.strictEqual(d.queue[1].blocked_reason, 'active_node');
  assert.strictEqual(d.queue[1].in_flight, false);
  assert.strictEqual(d.queue[2].queued_at_cycle, 310);
  // Queued after the export was written: present in kernel state only.
  assert.strictEqual(d.queue[3].in_export, false);
  assert.strictEqual(d.queue[3].status, 'unknown');
});

test('queue: falls back to the export when kernel state is unreadable', () => {
  const rt = populated('queue-fallback');
  fs.writeFileSync(path.join(rt, 'protocol_state.json'), '{not json');
  const d = gruntsStateForRuntimeRoot(rt, { now: 1_060_000 });
  assert.strictEqual(d.queue_source, 'export');
  assert.deepStrictEqual(d.queue.map((r) => r.node), ['Alpha', 'Beta', 'Gamma']);
  assert.strictEqual(d.queue[1].blocked_reason, 'active_node');
  assert.deepStrictEqual(d.errors.map((e) => e.file), ['protocol_state.json']);
});

test('closures: durable kernel map preferred, newest cycle first', () => {
  const d = gruntsStateForRuntimeRoot(populated('closures'), { now: 1_060_000 });
  assert.strictEqual(d.recent_closures_total, 2);
  assert.deepStrictEqual(d.recent_closures.map((r) => r.node), ['Zeta', 'Eta']);
  assert.strictEqual(d.recent_closures[0].cycle, 308);
  assert.strictEqual(d.recent_closures[0].model, 'leanstral');
  assert.strictEqual(d.recent_closures[0].provider, 'mistral');
  assert.strictEqual(d.recent_closures[0].wall_ms, 164500);
});

test('closures: export mirror used when kernel state has no closure map', () => {
  const rt = populated('closures-export', { state: { sidecar_closures: {} } });
  const d = gruntsStateForRuntimeRoot(rt, { now: 1_060_000 });
  assert.strictEqual(d.recent_closures.length, 1);
  assert.strictEqual(d.recent_closures[0].node, 'Zeta');
  assert.strictEqual(d.recent_closures[0].model, 'export-model');
  assert.strictEqual(d.recent_closures[0].wall_ms, null);
});

test('prunes: newest first', () => {
  const d = gruntsStateForRuntimeRoot(populated('prunes'), { now: 1_060_000 });
  assert.strictEqual(d.pruned_recent_total, 2);
  assert.deepStrictEqual(d.pruned_recent.map((r) => r.node), ['Old2', 'Old1']);
  assert.strictEqual(d.pruned_recent[0].reason, 'ineligible');
});

test('attempts: per-node digests carried through from status.json', () => {
  const d = gruntsStateForRuntimeRoot(populated('attempts'), { now: 1_060_000 });
  assert.strictEqual(d.attempts_total, 2);
  assert.strictEqual(d.attempts.Beta[0].status, 'failed');
  assert.strictEqual(d.attempts.Beta[0].detail, 'sorry remained');
  assert.strictEqual(d.attempts.Beta[0].wall_secs, 400);
  assert.strictEqual(d.attempts.Zeta[0].iterations, 35);
});

// ---------------------------------------------------------------------
test('history: ledger tail newest-first, capped at 200 by default', () => {
  const rt = populated('history');
  const rows = [];
  for (let i = 1; i <= 250; i++) rows.push(ledgerRow(i));
  fs.writeFileSync(path.join(rt, 'sidecar', 'ledger.jsonl'), rows.join('\n') + '\n');
  const d = gruntsStateForRuntimeRoot(rt, { now: 1_060_000 });
  assert.strictEqual(d.history_total, 250);
  assert.strictEqual(d.history.length, 200);
  assert.strictEqual(d.history_limit, 200);
  assert.strictEqual(d.history[0].node, 'Node250');
  assert.strictEqual(d.history[199].node, 'Node51');
  assert.strictEqual(d.history[0].model, 'leanstral');
  assert.strictEqual(d.history[0].provider, 'mistral');
});

test('history: explicit limit honoured and clamped to the cap', () => {
  const rt = populated('history-limit');
  const rows = [];
  for (let i = 1; i <= 250; i++) rows.push(ledgerRow(i));
  fs.writeFileSync(path.join(rt, 'sidecar', 'ledger.jsonl'), rows.join('\n') + '\n');
  assert.strictEqual(gruntsStateForRuntimeRoot(rt, { historyLimit: 5 }).history.length, 5);
  assert.strictEqual(gruntsStateForRuntimeRoot(rt, { historyLimit: 5 }).history[0].node, 'Node250');
  // Over-cap and nonsense limits both land on the 200 cap.
  assert.strictEqual(gruntsStateForRuntimeRoot(rt, { historyLimit: 10_000 }).history.length, 200);
  assert.strictEqual(gruntsStateForRuntimeRoot(rt, { historyLimit: NaN }).history.length, 200);
});

test('history: model/provider backfilled from the closure map by attempt id', () => {
  const rt = populated('history-backfill');
  // Ledger rows carry no provenance (the live daemon's shape); the
  // landed-closure map keys the same attempt id.
  fs.writeFileSync(
    path.join(rt, 'sidecar', 'ledger.jsonl'),
    JSON.stringify({ attempt_id: 'sc-z', node: 'Zeta', entry_seq: 7, status: 'success',
      iterations: 35, prompt_tokens: 100, completion_tokens: 10, wall_secs: 164.5, ts: 999 }) + '\n' +
    JSON.stringify({ attempt_id: 'sc-unknown', node: 'Theta', entry_seq: 8, status: 'failed',
      iterations: 5, prompt_tokens: 1, completion_tokens: 1, wall_secs: 3, ts: 1000 }) + '\n',
  );
  const d = gruntsStateForRuntimeRoot(rt, { now: 1_060_000 });
  const byNode = Object.fromEntries(d.history.map((r) => [r.node, r]));
  assert.strictEqual(byNode.Zeta.model, 'leanstral');
  assert.strictEqual(byNode.Zeta.provider, 'mistral');
  assert.strictEqual(byNode.Zeta.landed_cycle, 308);
  assert.strictEqual(byNode.Theta.model, null);
  assert.strictEqual(byNode.Theta.landed_cycle, null);
});

test('history: malformed ledger rows are dropped, not fatal', () => {
  const rt = populated('history-broken');
  fs.writeFileSync(
    path.join(rt, 'sidecar', 'ledger.jsonl'),
    ledgerRow(1) + '\n{broken row\n' + ledgerRow(2, 'error') + '\n',
  );
  const d = gruntsStateForRuntimeRoot(rt, { now: 1_060_000 });
  assert.strictEqual(d.history.length, 2);
  assert.strictEqual(d.history[0].node, 'Node2');
  assert.strictEqual(d.history[0].status, 'error');
  assert.strictEqual(d.history[0].detail, 'transport');
});

// ---------------------------------------------------------------------
// Lean LOC: the proof body lives only on the spool record, joined onto the
// attempt rows by attempt id.
function spoolRecord(rt, dir, attemptId, body) {
  const rec = { attempt_id: attemptId, node: attemptId, status: 'success', artifact: {} };
  if (body !== undefined) rec.artifact.proof_body = body;
  writeJson(path.join(rt, 'sidecar', 'spool', dir, `attempt-${attemptId}.json`), rec);
}

test('loc: body_lines joined onto history and attempts by attempt id', () => {
  const rt = populated('loc-join');
  spoolRecord(rt, 'rejected', 'sc-z', '  by\n  simp\n  done\n');
  fs.writeFileSync(
    path.join(rt, 'sidecar', 'ledger.jsonl'),
    JSON.stringify({ attempt_id: 'sc-z', node: 'Zeta', entry_seq: 7, status: 'success' }) + '\n',
  );
  const d = gruntsStateForRuntimeRoot(rt, { now: 1_060_000 });
  assert.strictEqual(d.history[0].body_lines, 3);
  assert.strictEqual(d.history[0].body_bytes, 19);
  // The status.json digest for the same attempt id gets the same body.
  assert.strictEqual(d.attempts.Zeta[0].body_lines, 3);
  assert.deepStrictEqual(d.errors, []);
});

test('loc: attempts with no spool record report null', () => {
  const rt = populated('loc-missing');
  fs.writeFileSync(
    path.join(rt, 'sidecar', 'ledger.jsonl'),
    JSON.stringify({ attempt_id: 'sc-nowhere', node: 'Theta', entry_seq: 8, status: 'failed' }) + '\n',
  );
  const d = gruntsStateForRuntimeRoot(rt, { now: 1_060_000 });
  assert.strictEqual(d.history[0].body_lines, null);
  assert.strictEqual(d.history[0].body_bytes, null);
  // status.json digests with no record too (populated() spools no bodies).
  assert.strictEqual(d.attempts.Beta[0].body_lines, null);
});

test('loc: empty / whitespace / missing body is null, never 0', () => {
  const rt = populated('loc-empty');
  spoolRecord(rt, 'rejected', 'sc-1', '');
  spoolRecord(rt, 'rejected', 'sc-2', '   \n\n  ');
  spoolRecord(rt, 'rejected', 'sc-3');
  spoolRecord(rt, 'rejected', 'sc-4', 42);
  fs.writeFileSync(
    path.join(rt, 'sidecar', 'ledger.jsonl'),
    [1, 2, 3, 4].map((i) =>
      JSON.stringify({ attempt_id: `sc-${i}`, node: `N${i}`, entry_seq: i, status: 'failed' })).join('\n') + '\n',
  );
  const d = gruntsStateForRuntimeRoot(rt, { now: 1_060_000 });
  for (const row of d.history) {
    assert.strictEqual(row.body_lines, null, `${row.attempt_id} body_lines`);
    assert.strictEqual(row.body_bytes, null, `${row.attempt_id} body_bytes`);
  }
});

test('loc: a malformed spool record lands in errors[] and the rest assembles', () => {
  const rt = populated('loc-broken');
  spoolRecord(rt, 'rejected', 'sc-good', 'one\ntwo\n');
  writeJson(path.join(rt, 'sidecar', 'spool', 'rejected', 'attempt-sc-bad.json'), '{ truncated');
  fs.writeFileSync(
    path.join(rt, 'sidecar', 'ledger.jsonl'),
    JSON.stringify({ attempt_id: 'sc-good', node: 'Good', entry_seq: 1, status: 'success' }) + '\n',
  );
  const d = gruntsStateForRuntimeRoot(rt, { now: 1_060_000 });
  assert.strictEqual(d.enabled, true);
  assert.deepStrictEqual(d.errors.map((e) => e.file), ['attempt-sc-bad.json']);
  assert.strictEqual(d.history[0].body_lines, 2);
  assert.strictEqual(d.queue.length, 4);
});

test('loc: a record past the per-file byte cap is skipped, not read', () => {
  const rt = populated('loc-oversize');
  spoolRecord(rt, 'rejected', 'sc-huge', 'x\n'.repeat(2_200_000)); // > 4MB on disk
  spoolRecord(rt, 'rejected', 'sc-small', 'a\nb\n');
  fs.writeFileSync(
    path.join(rt, 'sidecar', 'ledger.jsonl'),
    [JSON.stringify({ attempt_id: 'sc-huge', node: 'Huge', entry_seq: 1, status: 'success' }),
      JSON.stringify({ attempt_id: 'sc-small', node: 'Small', entry_seq: 2, status: 'success' })].join('\n') + '\n',
  );
  const d = gruntsStateForRuntimeRoot(rt, { now: 1_060_000 });
  const byAttempt = Object.fromEntries(d.history.map((r) => [r.attempt_id, r]));
  assert.strictEqual(byAttempt['sc-huge'].body_lines, null);
  assert.strictEqual(byAttempt['sc-small'].body_lines, 2);
  assert.deepStrictEqual(d.errors, []);
});

test('loc: the spool scan stops at the 500-record cap', () => {
  const rt = runtimeRoot('loc-cap');
  // 600 spool records in one dir; the digests come off status.json, which
  // is uncapped, so every attempt id is visible in the response.
  const digests = [];
  for (let i = 0; i < 600; i++) {
    const id = `sc-${String(i).padStart(4, '0')}`;
    spoolRecord(rt, 'applied', id, 'a\nb\n');
    digests.push({ entry_seq: i, attempt_id: id, status: 'success' });
  }
  writeJson(path.join(rt, 'sidecar', 'status.json'), { grunts: 1, attempts: { Node: digests } });
  const d = gruntsStateForRuntimeRoot(rt, { now: 1_060_000 });
  const byAttempt = Object.fromEntries(d.attempts.Node.map((r) => [r.attempt_id, r]));
  assert.strictEqual(d.attempts.Node.length, 600);
  // Name-sorted, so the first 500 are read and the rest are left null.
  assert.strictEqual(byAttempt['sc-0000'].body_lines, 2);
  assert.strictEqual(byAttempt['sc-0499'].body_lines, 2);
  assert.strictEqual(byAttempt['sc-0500'].body_lines, null);
  assert.strictEqual(byAttempt['sc-0599'].body_lines, null);
});

// ---------------------------------------------------------------------
test('malformed: every sidecar JSON file broken -> enabled with errors[]', () => {
  const rt = populated('all-broken');
  const sc = path.join(rt, 'sidecar');
  for (const f of ['candidates.json', 'status.json', 'manager_cursor.json']) {
    fs.writeFileSync(path.join(sc, f), '{ truncated');
  }
  fs.writeFileSync(path.join(rt, 'protocol_state.json'), 'nope');
  const d = gruntsStateForRuntimeRoot(rt, { now: 1_060_000 });
  assert.strictEqual(d.enabled, true);
  assert.deepStrictEqual(d.errors.map((e) => e.file).sort(), [
    'candidates.json', 'manager_cursor.json', 'protocol_state.json', 'status.json',
  ]);
  assert.deepStrictEqual(d.queue, []);
  assert.deepStrictEqual(d.in_flight, []);
  assert.deepStrictEqual(d.recent_closures, []);
  assert.strictEqual(d.pool.grunts, null);
  assert.strictEqual(d.export.present, false);
  // Spool counts come off the filesystem, so they still work.
  assert.strictEqual(d.spool.applied, 3);
});

test('malformed: non-object / wrong-typed fields degrade per-section', () => {
  const rt = runtimeRoot('wrong-types');
  const sc = path.join(rt, 'sidecar');
  writeJson(path.join(sc, 'candidates.json'), {
    schema: 'two', cycle: null, queue: 'not-an-array',
    eligible_now: {}, pruned_recent: null, sidecar_window_open: 0,
  });
  writeJson(path.join(sc, 'status.json'), { grunts: 'many', in_flight: {}, attempts: [] });
  writeJson(path.join(rt, 'protocol_state.json'), { sidecar_queue: 'nope', sidecar_closures: 5 });
  const d = gruntsStateForRuntimeRoot(rt, { now: 1_060_000 });
  assert.strictEqual(d.enabled, true);
  assert.deepStrictEqual(d.errors, []);
  assert.deepStrictEqual(d.queue, []);
  assert.strictEqual(d.queue_source, 'export');
  assert.deepStrictEqual(d.in_flight, []);
  assert.deepStrictEqual(d.attempts, {});
  assert.strictEqual(d.pool.grunts, null);
  assert.strictEqual(d.export.schema, null);
  assert.strictEqual(d.export.window_open, false);
  assert.strictEqual(d.export.eligible_now, 0);
});

test('in-flight rows missing started_at_ms report a null elapsed', () => {
  const rt = populated('no-start', {
    status: {
      grunts: 1,
      in_flight: [{ node: 'Alpha', entry_seq: 1, grunt: 0, attempt_id: 'sc-a' }],
      generated_at_ms: 1_050_000,
      attempts: {},
    },
  });
  const d = gruntsStateForRuntimeRoot(rt, { now: 1_060_000 });
  assert.strictEqual(d.in_flight[0].started_at_ms, null);
  assert.strictEqual(d.in_flight[0].elapsed_ms, null);
  assert.strictEqual(d.pool.idle, 0);
});

// ---------------------------------------------------------------------
let failures = 0;
try {
  for (const [name, fn] of tests) {
    try {
      fn();
      console.log(`  ok   ${name}`);
    } catch (e) {
      failures += 1;
      console.log(`  FAIL ${name}\n       ${e.message}`);
    }
  }
} finally {
  fs.rmSync(root, { recursive: true, force: true });
}
if (failures) {
  console.error(`viewer grunts tests: ${failures} failure(s)`);
  process.exit(1);
}
console.log(`viewer grunts tests passed (${tests.length})`);
