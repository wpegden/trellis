const assert = require('assert');
const fs = require('fs');
const os = require('os');
const path = require('path');
const { readTopLevelScalars, createWindowedScalarReader } = require('./protocol_hints');

const root = fs.mkdtempSync(path.join(os.tmpdir(), 'trellis-protocol-hints-'));
process.env.PROJECTS_ROOT = root;
process.env.STATIC_OUT = path.join(root, 'static');
const { readProtocolHeadCheap, readGateKindCheap } = require('./server');
const file = path.join(root, 'protocol_state.json');
const expected = { phase: 'ProofFormalization', stage: 'HumanGate', cycle: 23 };
const historical = { phase: 'TheoremStating', stage: 'Worker', cycle: 17, gate_kind: 'None' };

try {
  for (const indent of [undefined, 2]) {
    fs.writeFileSync(file, JSON.stringify({
      dispatch: { phase: 'TheoremStating', tasks: [{ assignment: { request: historical } }] },
      text: '\\"} ], "phase":"Complete", "gate_kind":"Advance"',
      ...expected, gate_kind: 'NeedInput',
    }, null, indent));
    assert.deepStrictEqual(readProtocolHeadCheap(root), expected);
    assert.strictEqual(readGateKindCheap(root), 'NeedInput');
  }

  // Quotes, escaped backslashes, Unicode, and keys can cross chunk boundaries.
  const prefix = '{"dispatch":{"padding":"';
  const padding = 'x'.repeat(65535 - prefix.length);
  fs.writeFileSync(file, prefix + padding + '\\"escaped\\\\text雪","gate_kind":"None"},' +
    '"gate_ki\\u006ed":"NeedInput","phase":"ProofFormalization","stage":"HumanGate","cycle":23}');
  assert.strictEqual(readGateKindCheap(root), 'NeedInput');
  assert.deepStrictEqual({ ...readTopLevelScalars(file, ['phase', 'stage', 'cycle'], 128 * 1024) }, expected);
  assert.strictEqual(readProtocolHeadCheap(root), null, 'unseen canonical hints must stay unknown');

  for (const indent of [undefined, 2]) {
    fs.writeFileSync(file, JSON.stringify({
      in_flight_request: {id: 363, contract: {text: 'x'.repeat(500000),
        nested: {id: 99, cycle: 0, kind: 'Worker'}}, kind: 'HumanGate', cycle: 72},
      cycle: 72,
    }, null, indent));
    const got = readTopLevelScalars(file, ['cycle', 'in_flight_request'], 1024 * 1024,
      {in_flight_request: ['id', 'kind', 'cycle']});
    assert.deepStrictEqual(JSON.parse(JSON.stringify(got)),
      {in_flight_request: {id: 363, kind: 'HumanGate', cycle: 72}, cycle: 72});
    assert.strictEqual(readTopLevelScalars(file, ['in_flight_request'], 65536,
      {in_flight_request: ['id', 'kind', 'cycle']}), null);
  }

  // No partial string or number can become a hint at the byte boundary.
  fs.writeFileSync(file, '{"cycle":1234}');
  assert.strictEqual(readTopLevelScalars(file, ['cycle'], 12), null);
  fs.writeFileSync(file, '{"stage":"HumanGate"}');
  assert.strictEqual(readTopLevelScalars(file, ['stage'], 17), null);

  // Large discarded values use a fixed read buffer and a bounded token buffer.
  // Count actual reads so this regression cannot quietly become a full parse.
  const fd = fs.openSync(file, 'w');
  fs.writeSync(fd, '{"dispatch":{"request":' + JSON.stringify(historical) + ',"padding":"');
  const block = Buffer.alloc(1024 * 1024, 120);
  for (let i = 0; i < 13; i++) fs.writeSync(fd, block);
  fs.writeSync(fd, '"},"phase":"ProofFormalization","stage":"HumanGate","cycle":23,"gate_kind":"NeedInput"}');
  fs.closeSync(fd);
  const originalRead = fs.readSync;
  let bytes = 0, biggest = 0;
  fs.readSync = function (...args) {
    biggest = Math.max(biggest, args[1].length);
    const n = originalRead.apply(this, args); bytes += n; return n;
  };
  try {
    assert.strictEqual(readProtocolHeadCheap(root), null);
    assert.ok(bytes <= 64 * 1024);
    bytes = 0;
    assert.strictEqual(readGateKindCheap(root), null);
    assert.ok(bytes <= 12 * 1024 * 1024);
    assert.ok(biggest <= 64 * 1024);
  } finally { fs.readSync = originalRead; }

  // Actual production puts the request after 75 MB. Scan bounded windows
  // across ticks, then reuse the exact unchanged-file projection without I/O.
  const long = fs.openSync(file, 'w');
  fs.writeSync(long, '{"cycle":72,"stage":"HumanGate","gate_kind":"AssumptionReview","history":"');
  for (let i = 0; i < 75; i++) fs.writeSync(long, block);
  fs.writeSync(long, '","in_flight_request":{"id":363,"kind":"HumanGate","cycle":72}}');
  fs.closeSync(long);
  const windowed = createWindowedScalarReader(['cycle', 'stage', 'gate_kind', 'in_flight_request'],
    12 * 1024 * 1024, {in_flight_request: ['id', 'kind', 'cycle']});
  const savedRead = fs.readSync; let totalRead = 0;
  fs.readSync = function(...args) { const n = savedRead.apply(this,args); totalRead += n; return n; };
  try {
    for(let i=0;i<6;i++) {
      const before=totalRead;assert.strictEqual(windowed(file),null);
      assert.ok(totalRead-before<=12*1024*1024);
    }
    assert.strictEqual(windowed(file).in_flight_request.id,363);
    const before=totalRead;assert.strictEqual(windowed(file).in_flight_request.id,363);
    assert.strictEqual(totalRead,before);
    fs.writeFileSync(file, '{"cycle":73,"in_flight_request":{"id":364,"cycle":73,"kind":"HumanGate"},"stage":"HumanGate","gate_kind":"AssumptionReview"}');
    assert.strictEqual(windowed(file).in_flight_request.id,364);
  } finally {fs.readSync=savedRead;}

  fs.writeFileSync(file, '{"phase":"Complete","dispatch":');
  assert.deepStrictEqual(readProtocolHeadCheap(root), { phase: 'Complete', stage: null, cycle: null });
  fs.writeFileSync(file, JSON.stringify({ phase: {}, stage: [], cycle: '23', gate_kind: 7 }));
  assert.strictEqual(readGateKindCheap(root), null);
  assert.deepStrictEqual(readProtocolHeadCheap(root), { phase: null, stage: null, cycle: null });
  console.log('protocol hints: passed');
} finally {
  fs.rmSync(root, { recursive: true, force: true });
}
