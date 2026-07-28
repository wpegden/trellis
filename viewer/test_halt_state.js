const assert = require('assert');
const fs = require('fs');
const os = require('os');
const path = require('path');

const {
  haltStateForRuntimeRoot,
  recentSystemFeedbackForRuntimeRoot,
} = require('./server');

const root = fs.mkdtempSync(path.join(os.tmpdir(), 'trellis-viewer-halt-'));
try {
  // Unified system-feedback log tail (opt-in-halt feature: with the
  // knob off, log-and-continue emissions surface only here).
  assert.deepStrictEqual(recentSystemFeedbackForRuntimeRoot(null), {
    entries: [],
    reason: 'no_runtime_root',
  });
  let feed = recentSystemFeedbackForRuntimeRoot(root);
  assert.deepStrictEqual(feed.entries, []);
  const logPath = path.join(root, 'system_feedback_log.jsonl');
  fs.writeFileSync(
    logPath,
    JSON.stringify({ kind: 'system_feedback', cycle: 1, halted: false, acked: false }) + '\n' +
    JSON.stringify({ kind: 'system_feedback', cycle: 2, halted: true, acked: false }) + '\n' +
    '{broken row\n',
  );
  feed = recentSystemFeedbackForRuntimeRoot(root);
  assert.strictEqual(feed.total, 3);
  assert.strictEqual(feed.entries.length, 3);
  assert.strictEqual(feed.entries[0].cycle, 1);
  assert.strictEqual(feed.entries[1].halted, true);
  assert.strictEqual(typeof feed.entries[2].parse_error, 'string');
  feed = recentSystemFeedbackForRuntimeRoot(root, 2);
  assert.strictEqual(feed.entries.length, 2);
  assert.strictEqual(feed.entries[0].cycle, 2);

  // Halt-marker surface. The `.jsonl` log written above is a different
  // file from the halt markers, so the two surfaces stay independent.
  assert.deepStrictEqual(haltStateForRuntimeRoot(null), {
    halted: false,
    reason: 'no_runtime_root',
  });
  assert.deepStrictEqual(haltStateForRuntimeRoot(root), { halted: false });

  const systemPath = path.join(root, 'system_feedback_halt.json');
  fs.writeFileSync(systemPath, JSON.stringify({ fingerprint: 'system-fp' }));
  let state = haltStateForRuntimeRoot(root);
  assert.strictEqual(state.halted, true);
  assert.strictEqual(state.marker_kind, 'system_feedback');
  assert.strictEqual(state.marker.fingerprint, 'system-fp');
  assert.strictEqual(state.markers.length, 1);

  const checkerPath = path.join(root, 'checker_disagreement_halt.json');
  fs.writeFileSync(checkerPath, JSON.stringify({ node: 'Example' }));
  state = haltStateForRuntimeRoot(root);
  assert.strictEqual(state.marker_kind, 'checker_disagreement');
  assert.strictEqual(state.marker.node, 'Example');
  assert.deepStrictEqual(
    state.markers.map(entry => entry.marker_kind),
    ['checker_disagreement', 'system_feedback'],
  );

  // A malformed marker must be visible even when another valid marker exists.
  fs.writeFileSync(systemPath, '{not valid json');
  state = haltStateForRuntimeRoot(root);
  assert.strictEqual(state.halted, true);
  assert.strictEqual(state.marker_kind, 'system_feedback');
  assert.strictEqual(typeof state.parse_error, 'string');
  assert.strictEqual(state.markers.length, 2);

  // With both malformed, the fixed checker-then-system ordering wins.
  fs.writeFileSync(checkerPath, '{also invalid');
  state = haltStateForRuntimeRoot(root);
  assert.strictEqual(state.marker_kind, 'checker_disagreement');
  assert.strictEqual(typeof state.parse_error, 'string');
} finally {
  fs.rmSync(root, { recursive: true, force: true });
}

console.log('viewer halt-state + system-feedback log tests passed');
