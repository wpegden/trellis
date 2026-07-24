const assert = require('assert');
const fs = require('fs');
const os = require('os');
const path = require('path');

const { recentSystemFeedbackForRuntimeRoot } = require('./server');

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
} finally {
  fs.rmSync(root, { recursive: true, force: true });
}

console.log('viewer system-feedback log tests passed');
