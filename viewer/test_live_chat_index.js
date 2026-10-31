'use strict';
const assert = require('assert');
const fs = require('fs');
const os = require('os');
const path = require('path');
const { liveRequestsFromEventLog } = require('./server');

const root = fs.mkdtempSync(path.join(os.tmpdir(), 'trellis-live-chat-index-'));
try {
  const info = { repoPath: path.join(root, 'run') };
  const dir = path.join(info.repoPath, '.trellis-history', 'event-log');
  fs.mkdirSync(dir, { recursive: true });
  const file = n => path.join(dir, `cycle-${String(n).padStart(6, '0')}.jsonl`);
  const record = (cycle, index, id) => JSON.stringify({ cycle, index,
    commands: [{ command: 'issue_request', request: { id, kind: 'Worker' } }] }) + '\n';
  const ids = value => value.bursts.map(burst => burst.request_id);
  function read() {
    const opened = [];
    const original = fs.openSync;
    fs.openSync = function (name, ...args) {
      if (String(name).startsWith(dir + path.sep)) opened.push(path.basename(name));
      return original.call(this, name, ...args);
    };
    try { return { value: liveRequestsFromEventLog(info), opened }; }
    finally { fs.openSync = original; }
  }

  fs.writeFileSync(file(1), record(1, 0, 1));
  fs.writeFileSync(file(2), record(2, 1, 2));
  fs.writeFileSync(file(3), record(3, 2, 3));
  let result = read();
  assert.deepStrictEqual(ids(result.value), [3]);
  assert.strictEqual(result.value.liveCycle, 3);
  assert.deepStrictEqual(result.opened, ['cycle-000003.jsonl'], 'cold live chat must not scan old cycles');
  assert.deepStrictEqual(read().opened, [], 'unchanged live cycle should be cached');
  fs.appendFileSync(file(3), record(3, 3, 4));
  result = read();
  assert.deepStrictEqual(ids(result.value), [3, 4]);
  assert.deepStrictEqual(result.opened, ['cycle-000003.jsonl']);

  // Newly created files and partial final records are not yet history.
  fs.writeFileSync(file(4), record(4, 4, 5).slice(0, -1));
  assert.deepStrictEqual(ids(read().value), [3, 4]);
  fs.appendFileSync(file(4), '\n');
  assert.deepStrictEqual(ids(read().value), [5]);
  // Rewind, including same-size replacement with preserved mtime.
  fs.unlinkSync(file(4));
  assert.deepStrictEqual(ids(read().value), [3, 4]);
  const stat = fs.statSync(file(3));
  fs.writeFileSync(file(3) + '.tmp', record(3, 2, 6) + record(3, 3, 7));
  fs.renameSync(file(3) + '.tmp', file(3));
  fs.utimesSync(file(3), stat.atime, stat.mtime);
  assert.deepStrictEqual(ids(read().value), [6, 7]);
  fs.writeFileSync(file(3), record(3, 2, 6));
  assert.deepStrictEqual(ids(read().value), [6]);

  // Old records without metadata require the original chronological walk:
  // inherit cycle and global fallback index rather than inventing provenance.
  fs.writeFileSync(file(4), JSON.stringify({ commands: [
    { command: 'issue_request', request: { id: 8, kind: 'Review' } },
  ] }) + '\n');
  result = read();
  assert.deepStrictEqual(ids(result.value), [6, 8]);
  assert.strictEqual(result.value.bursts[1].event_index, 3);
  assert(result.opened.includes('cycle-000001.jsonl'));

  // Identical file metadata in another project cannot reuse this project's rows.
  const other = { repoPath: path.join(root, 'other') };
  const otherDir = path.join(other.repoPath, '.trellis-history', 'event-log');
  fs.mkdirSync(otherDir, { recursive: true });
  fs.writeFileSync(path.join(otherDir, 'cycle-000003.jsonl'), record(3, 2, 9));
  assert.deepStrictEqual(ids(liveRequestsFromEventLog(other)), [9]);
  assert.deepStrictEqual(ids(read().value), [6, 8]);
  for (const name of fs.readdirSync(dir)) fs.unlinkSync(path.join(dir, name));
  assert.deepStrictEqual(read().value, { liveCycle: null, bursts: [] });
  fs.writeFileSync(file(1), record(1, 0, 10));
  assert.deepStrictEqual(ids(read().value), [10]);
  console.log('Live chat index tests passed');
} finally {
  fs.rmSync(root, { recursive: true, force: true });
}
