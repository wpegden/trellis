// Regression tests for the two event-log walks that used to hold the whole
// log in memory at once.
//
// `/api/usage.json` materialized every record (3.01 GB of JSONL across 971
// cycle files on one live run) to read two scalars per record, and took
// the viewer to a fatal heap OOM at ~4 GB. `attachSoundVerifierFailCounts`
// concatenated every cycle file into one string, crossed node's
// MAX_STRING_LENGTH past ~274 files, and silently walked a truncated log.
//
// Both are now single-line streaming walks. The decisive checks here run them
// under a heap cap smaller than the fixture, so completing at all proves the
// log is never held whole; the pre-change implementation is kept as a golden
// oracle for output equivalence and is asserted to die under the same cap.
//
// `foldEventLogStageWalltime` is additionally INCREMENTAL: each cycle file's
// contribution is memoized on (size, mtime, ino) and only changed files are
// re-read. Section 3 proves that the incremental result is identical to the
// full walk across file boundaries — the adjacency that a naive per-file memo
// would silently drop — and that a rewrite invalidates rather than serves
// stale totals.

const assert = require('assert');
const fs = require('fs');
const os = require('os');
const path = require('path');
const http = require('http');
const { spawnSync } = require('child_process');

const root = fs.mkdtempSync(path.join(os.tmpdir(), 'trellis-viewer-usage-'));
process.on('exit', () => { try { fs.rmSync(root, { recursive: true, force: true }); } catch {} });

process.env.PROJECTS_ROOT = path.join(root, 'projects');
const { app, foldEventLogStageWalltime, _eventLogFoldPartials } = require('./server');

// Drop every memoized per-file partial, so the next fold is a cold full walk.
function coldFold(projectInfo) {
  _eventLogFoldPartials.clear();
  return foldEventLogStageWalltime(projectInfo);
}

// How many cycle files the memo currently holds partials for, for one log dir.
function memoSize(eventLogDir) {
  const m = _eventLogFoldPartials.get(eventLogDir);
  return m ? m.size : 0;
}

const CHUNK = 1024 * 1024;  // forEachFileLine's read size

function projectInfoFor(repoPath) {
  return {
    slug: path.basename(repoPath),
    repoPath,
    repoType: 'trellis',
    stateDir: path.join(repoPath, '.trellis'),
  };
}

function writeCycleFiles(eventLogDir, bodies) {
  fs.mkdirSync(eventLogDir, { recursive: true });
  bodies.forEach((body, i) => {
    fs.writeFileSync(path.join(eventLogDir, `cycle-${String(i + 1).padStart(6, '0')}.jsonl`), body);
  });
}

// The implementation this change replaced: every record materialized in global
// index order, then a walk over adjacent pairs. Golden oracle.
function referenceStageWalltime(eventLogDir) {
  const files = fs.existsSync(eventLogDir)
    ? fs.readdirSync(eventLogDir)
      .filter((n) => /^cycle-\d{6,}\.jsonl$/.test(n)).sort()
      .map((n) => path.join(eventLogDir, n))
    : [];
  const events = [];
  for (const f of files) {
    let rows = [];
    try {
      rows = fs.readFileSync(f, 'utf8').split('\n').filter(Boolean)
        .map((l) => { try { return JSON.parse(l); } catch { return null; } })
        .filter(Boolean);
    } catch { rows = []; }
    events.push(...rows);
  }
  const byStage = {};
  for (let i = 0; i < events.length - 1; i++) {
    const a = events[i], b = events[i + 1];
    const ta = parseInt(a.ts_ms || 0), tb = parseInt(b.ts_ms || 0);
    if (ta <= 0 || tb <= ta) continue;
    const stage = String(a.stage || '?');
    const dt = (tb - ta) / 1000;
    if (!byStage[stage]) byStage[stage] = { intervals: 0, duration_s: 0 };
    byStage[stage].intervals++;
    byStage[stage].duration_s += dt;
  }
  return { byStage, rows: events.length };
}

// A file in which a 3-byte '€' spans the 1 MiB chunk boundary, with `phase`
// of its bytes landing in the first chunk. The byte-exact hazard
// forEachFileLineFromOffset's comment documents.
function straddleBody(phase) {
  const rec = `${JSON.stringify({ ts_ms: 2000, stage: 'stra€dle' })}\n`;
  const euroByteOffsetInRec = Buffer.byteLength(rec.slice(0, rec.indexOf('€')), 'utf8');
  const headBytes = CHUNK - phase - euroByteOffsetInRec;
  const emptyHead = `${JSON.stringify({ ts_ms: 1000, stage: 'pad', p: '' })}\n`;
  const head = `${JSON.stringify({ ts_ms: 1000, stage: 'pad', p: 'x'.repeat(headBytes - emptyHead.length) })}\n`;
  assert.strictEqual(Buffer.byteLength(head, 'utf8'), headBytes);
  return `${head}${rec}${JSON.stringify({ ts_ms: 3000, stage: 'tail' })}\n`;
}

// ---------------------------------------------------------------------------
// 1. Golden equivalence of the streaming fold against the materialized walk.
// ---------------------------------------------------------------------------

const goldenRepo = path.join(root, 'golden');
const goldenLog = path.join(goldenRepo, '.trellis-history', 'event-log');

// No event-log dir at all.
assert.deepStrictEqual(coldFold(projectInfoFor(goldenRepo)), { byStage: {}, rows: 0 });

writeCycleFiles(goldenLog, [
  // Ordinary records.
  `${JSON.stringify({ ts_ms: 1000, stage: 'worker' })}\n${JSON.stringify({ ts_ms: 4000, stage: 'reviewer' })}\n`,
  // Empty file.
  '',
  // Blank line, unparseable line, a `null` record, and a trailing partial line
  // (valid JSON, no closing newline).
  `${JSON.stringify({ ts_ms: 9000, stage: 'worker' })}\n`
  + '\n'
  + '{not json\n'
  + 'null\n'
  + JSON.stringify({ ts_ms: 12000, stage: 'verifier' }),
  // Degenerate timestamps and a missing stage: zero, backwards, equal, absent.
  `${JSON.stringify({ ts_ms: 0, stage: 'worker' })}\n`
  + `${JSON.stringify({ ts_ms: 20000 })}\n`
  + `${JSON.stringify({ ts_ms: 19000, stage: 'worker' })}\n`
  + `${JSON.stringify({ ts_ms: 19000, stage: 'worker' })}\n`
  + `${JSON.stringify({ stage: 'worker' })}\n`
  + `${JSON.stringify({ ts_ms: 30000, stage: 'worker' })}\n`,
  straddleBody(1),
  straddleBody(2),
  straddleBody(3),
  // Last line has no trailing newline; the next file must still start a new
  // record rather than fusing with it (the concatenating walk fused them).
  JSON.stringify({ ts_ms: 40000, stage: 'tailless' }),
  `${JSON.stringify({ ts_ms: 44000, stage: 'worker' })}\n`,
  // Trailing partial line that is not valid JSON: dropped, so it does not
  // break adjacency across the file boundary.
  '{truncated',
  `${JSON.stringify({ ts_ms: 50000, stage: 'worker' })}\n`,
]);

const goldenFold = coldFold(projectInfoFor(goldenRepo));
const goldenRef = referenceStageWalltime(goldenLog);
assert.deepStrictEqual(goldenFold, goldenRef);
// The straddled '€' survives as a stage name, so the multi-byte boundary was
// decoded rather than mangled into replacement characters.
assert.ok(goldenFold.byStage['stra€dle'], 'multi-byte record straddling the chunk boundary was mangled');
assert.strictEqual(goldenFold.byStage['stra€dle'].intervals, 3);
assert.strictEqual(goldenFold.byStage.tailless.intervals, 1);
assert.strictEqual(typeof goldenFold.rows, 'number');
assert.strictEqual(goldenFold.rows, goldenRef.rows);

// ---------------------------------------------------------------------------
// 2. Memory + string-limit regression, on a fixture larger than the heap cap.
// ---------------------------------------------------------------------------

const bigRepo = path.join(process.env.PROJECTS_ROOT, 'usagefix');
const bigLog = path.join(bigRepo, '.trellis-history', 'event-log');
// The cap is below the fixture's byte size, so neither an array of records nor
// a single concatenated string of the log can fit — the walks complete only by
// never holding more than a line.
const HEAP_CAP_MB = 40;
const FIXTURE_BYTES = 48 * 1000 * 1000;
const CP_SHA = '0'.repeat(40);

fs.mkdirSync(bigLog, { recursive: true });
fs.mkdirSync(path.join(bigRepo, '.trellis', 'logs'), { recursive: true });
fs.writeFileSync(path.join(bigRepo, 'trellis.config.json'), '{}\n');

// A Sound request in the FIRST file and its response in the SECOND, with the
// checkpoint's event_count only in the LAST — so the counts attach only if the
// walk reaches the end of a multi-file log.
const soundIssue = {
  index: 0,
  ts_ms: 1700000000000,
  stage: 'proof_formalization',
  commands: [{
    command: 'issue_request',
    request: {
      id: 7,
      kind: 'Sound',
      blockers: [{ kind: 'Soundness', object: { node: 'NodeA' }, fingerprint: 'fp-a' }],
    },
  }],
};
const soundResponse = {
  index: 1,
  ts_ms: 1700000001000,
  stage: 'proof_formalization',
  event: { event: 'wrapper_response', response: { kind: 'sound', request_id: 7, lane_updates: { soundness: { NodeA: { Set: 'Fail' } } } } },
};

let recordIndex = 0;
let fixtureBytes = 0;
let cycle = 0;
let checkpointIndex = 0;
while (fixtureBytes < FIXTURE_BYTES) {
  cycle++;
  const lines = [];
  let fileBytes = 0;
  if (cycle === 1) lines.push(`${JSON.stringify(soundIssue)}\n`);
  if (cycle === 2) lines.push(`${JSON.stringify(soundResponse)}\n`);
  while (fileBytes < FIXTURE_BYTES / 40 && fixtureBytes + fileBytes < FIXTURE_BYTES) {
    recordIndex += 1;
    const line = `${JSON.stringify({
      index: recordIndex + 1,
      ts_ms: 1700000000000 + recordIndex * 1000,
      stage: recordIndex % 3 === 0 ? 'proof_formalization' : 'theorem_stating',
      cycle,
      commands: [],
      event: { event: 'step' },
    })}\n`;
    lines.push(line);
    fileBytes += Buffer.byteLength(line);
  }
  checkpointIndex = recordIndex + 1;
  fs.writeFileSync(path.join(bigLog, `cycle-${String(cycle).padStart(6, '0')}.jsonl`), lines.join(''));
  fixtureBytes += fileBytes;
}
assert.ok(fixtureBytes > HEAP_CAP_MB * 1024 * 1024,
  'fixture must exceed the heap cap or the memory assertions prove nothing');

// Seed the sound-cpinfo sidecar so attachSoundVerifierFailCounts resolves its
// checkpoint projection from cache; the walk under test is the event-log one,
// not the git blob reads.
fs.mkdirSync(path.join(bigRepo, '.trellis', 'viewer'), { recursive: true });
fs.writeFileSync(path.join(bigRepo, '.trellis', 'viewer', 'sound-cpinfo-cache-v1.json'), JSON.stringify({
  version: 1,
  entries: {
    [CP_SHA]: {
      event_count: checkpointIndex,
      currentFps: { NodeA: 'fp-a' },
      nodeSets: {
        all: ['NodeA', 'NodeB'],
        all_proofs_only: ['NodeA'],
        coarse_shallow: [],
        coarse_shallow_proofs_only: [],
      },
    },
  },
}));

const runnerPath = path.join(root, 'walk-runner.js');
fs.writeFileSync(runnerPath, `
const { attachSoundVerifierFailCounts, foldEventLogStageWalltime } = require(${JSON.stringify(path.join(__dirname, 'server.js'))});
const repoPath = process.argv[2];
const projectInfo = { slug: 'usagefix', repoPath, repoType: 'trellis', stateDir: repoPath + '/.trellis' };
const heapMB = () => { global.gc(); return process.memoryUsage().heapUsed / 1048576; };
const fold = foldEventLogStageWalltime(projectInfo);
const foldHeapMB = heapMB();
const checkpoints = [{
  sha: ${JSON.stringify(CP_SHA)},
  sketch_nodes: ['NodeB'],
  all: {}, all_proofs_only: {}, coarse_shallow: {}, coarse_shallow_proofs_only: {},
}];
attachSoundVerifierFailCounts(projectInfo, checkpoints);
const attachHeapMB = heapMB();
process.stdout.write(JSON.stringify({ rows: fold.rows, byStage: fold.byStage, foldHeapMB, attachHeapMB, checkpoint: checkpoints[0] }));
`);

const capped = spawnSync(process.execPath,
  [`--max-old-space-size=${HEAP_CAP_MB}`, '--expose-gc', runnerPath, bigRepo],
  { encoding: 'utf8', maxBuffer: 16 * 1024 * 1024 });
assert.strictEqual(capped.status, 0,
  `streaming walks died under a ${HEAP_CAP_MB} MB heap cap:\n${capped.stderr}`);
const walk = JSON.parse(capped.stdout);

const heapBoundMB = fixtureBytes / 1048576 / 4;
assert.ok(walk.foldHeapMB < heapBoundMB,
  `fold retained ${walk.foldHeapMB.toFixed(1)} MB over a ${(fixtureBytes / 1048576).toFixed(0)} MB log`);
assert.ok(walk.attachHeapMB < heapBoundMB,
  `sound-verifier walk retained ${walk.attachHeapMB.toFixed(1)} MB over a ${(fixtureBytes / 1048576).toFixed(0)} MB log`);
assert.strictEqual(walk.rows, recordIndex + 2);
assert.deepStrictEqual(Object.keys(walk.byStage).sort(), ['proof_formalization', 'theorem_stating']);

// The counts attached, from evidence spread across the first, second and last
// cycle files. NodeA fails the verifier at the current fingerprint; NodeB is
// only a sketch node, so it lifts the definitive count alone.
assert.strictEqual(walk.checkpoint.all.sound_verifier_fail, 1);
assert.strictEqual(walk.checkpoint.all.sound_definitive_fail, 2);
assert.strictEqual(walk.checkpoint.all_proofs_only.sound_verifier_fail, 1);
assert.strictEqual(walk.checkpoint.all_proofs_only.sound_definitive_fail, 1);
assert.strictEqual(walk.checkpoint.coarse_shallow.sound_verifier_fail, 0);

// The same fixture under the same cap kills the materializing walk, so the
// bounds above are a real constraint rather than a fixture that always fit.
const oraclePath = path.join(root, 'oracle-runner.js');
fs.writeFileSync(oraclePath, `
const fs = require('fs'), path = require('path');
const dir = process.argv[2];
const out = [];
for (const n of fs.readdirSync(dir).sort()) {
  out.push(...fs.readFileSync(path.join(dir, n), 'utf8').split('\\n').filter(Boolean)
    .map((l) => { try { return JSON.parse(l); } catch { return null; } }).filter(Boolean));
}
process.stdout.write(String(out.length));
`);
const oracle = spawnSync(process.execPath,
  [`--max-old-space-size=${HEAP_CAP_MB}`, oraclePath, bigLog],
  { encoding: 'utf8', maxBuffer: 16 * 1024 * 1024, stdio: ['ignore', 'pipe', 'ignore'] });
assert.notStrictEqual(oracle.status, 0,
  'materializing the fixture survived the heap cap; raise FIXTURE_BYTES to keep this test decisive');

// ---------------------------------------------------------------------------
// 3. Incremental fold == full walk, across file boundaries.
//
// The fold memoizes each cycle file's contribution on (size, mtime, ino). The
// hazard a naive per-file memo introduces is that per-stage walltime comes
// from ADJACENT record pairs, and adjacency crosses file boundaries: the pair
// (last record of file N, first record of file N+1) belongs to no single file.
// Every fixture boundary below is a stage CHANGE, so dropping or misattributing
// a boundary pair moves a duration between stages and the equality fails.
// ---------------------------------------------------------------------------

const incRepo = path.join(root, 'incremental');
const incLog = path.join(incRepo, '.trellis-history', 'event-log');
const incInfo = projectInfoFor(incRepo);

function ev(ts, stage) { return `${JSON.stringify({ ts_ms: ts, stage })}\n`; }

// Deliberately non-round millisecond gaps: at 7/17/11/9/21/29 ms a per-file
// float accumulator would drift from a global one in the last ulp, so the
// exact equalities below are a real constraint on how durations are summed.
const incBodies = [
  ev(1000, 'A') + ev(1007, 'A'),   // within: A +7ms
  ev(1013, 'B') + ev(1030, 'B'),   // boundary A +6ms; within B +17ms
  '',                              // empty file — adjacency must step over it
  ev(1041, 'C'),                   // boundary B +11ms; no within-file pair
  ev(1050, 'A') + ev(1071, 'D'),   // boundary C +9ms; within A +21ms
  ev(1100, 'D'),                   // boundary D +29ms
];
writeCycleFiles(incLog, incBodies);
const incFiles = fs.readdirSync(incLog).sort().map((n) => path.join(incLog, n));

// Every cycle file `forEachFileLine` actually opens during `fn`.
function cycleFilesRead(fn) {
  const realOpenSync = fs.openSync;
  const opened = [];
  fs.openSync = function (p, ...rest) {
    if (typeof p === 'string' && /cycle-\d+\.jsonl$/.test(p)) opened.push(path.basename(p));
    return realOpenSync.call(fs, p, ...rest);
  };
  try { return { value: fn(), opened }; } finally { fs.openSync = realOpenSync; }
}

// The materialized oracle sums floats; the fold sums integer milliseconds and
// divides once. Compare interval counts exactly and durations to within a ulp.
function assertMatchesOracle(fold, dir, what) {
  const ref = referenceStageWalltime(dir);
  assert.strictEqual(fold.rows, ref.rows, `${what}: row count`);
  assert.deepStrictEqual(Object.keys(fold.byStage).sort(), Object.keys(ref.byStage).sort(),
    `${what}: stage set`);
  for (const s of Object.keys(ref.byStage)) {
    assert.strictEqual(fold.byStage[s].intervals, ref.byStage[s].intervals, `${what}: ${s} intervals`);
    assert.ok(Math.abs(fold.byStage[s].duration_s - ref.byStage[s].duration_s) < 1e-9,
      `${what}: ${s} duration ${fold.byStage[s].duration_s} vs oracle ${ref.byStage[s].duration_s}`);
  }
}

const incCold = coldFold(incInfo);
assertMatchesOracle(incCold, incLog, 'cold fold');
// The boundary pairs landed on the earlier record's stage, not the later one.
assert.strictEqual(incCold.byStage.A.duration_s, (7 + 6 + 21) / 1000);
assert.strictEqual(incCold.byStage.B.duration_s, (17 + 11) / 1000);
assert.strictEqual(incCold.byStage.C.duration_s, 9 / 1000);
assert.strictEqual(incCold.byStage.D.duration_s, 29 / 1000);
assert.strictEqual(incCold.rows, 8);

// Warm: identical result, and nothing re-read.
{
  const { value: warm, opened } = cycleFilesRead(() => foldEventLogStageWalltime(incInfo));
  assert.deepStrictEqual(warm, incCold, 'warm fold diverged from the cold full walk');
  assert.deepStrictEqual(opened, [], `warm fold re-read ${opened.join(', ')}`);
  assert.strictEqual(memoSize(incLog), incFiles.length);
}

// Append to the LAST file, as the live cycle file does. Only that file is
// re-read, and the result still equals a cold full walk byte for byte.
{
  fs.appendFileSync(incFiles[5], ev(1136, 'E'));
  const { value: inc, opened } = cycleFilesRead(() => foldEventLogStageWalltime(incInfo));
  assert.deepStrictEqual(opened, ['cycle-000006.jsonl'], `re-read ${opened.join(', ')}`);
  assert.deepStrictEqual(inc, coldFold(incInfo), 'append to last file: incremental != full walk');
  assert.strictEqual(inc.byStage.D.duration_s, (29 + 36) / 1000);
}

// Append to a MIDDLE file. Its last record changes, so the pair straddling the
// boundary into the next file changes too — the case a per-file memo that did
// not retain boundary records would get wrong while still looking plausible.
{
  fs.appendFileSync(incFiles[1], ev(1035, 'F'));
  const { value: inc, opened } = cycleFilesRead(() => foldEventLogStageWalltime(incInfo));
  assert.deepStrictEqual(opened, ['cycle-000002.jsonl'], `re-read ${opened.join(', ')}`);
  const full = coldFold(incInfo);
  assert.deepStrictEqual(inc, full, 'append to middle file: incremental != full walk');
  // Boundary into cycle-000004 now leaves stage F, not stage B.
  assert.strictEqual(inc.byStage.F.duration_s, 6 / 1000);
  assert.strictEqual(inc.byStage.B.duration_s, (17 + 5) / 1000);
  assertMatchesOracle(inc, incLog, 'after middle append');
}

// Same-size rewrite: only mtime moves. Keying on the path alone would serve
// the pre-rewrite totals forever.
{
  const before = fs.statSync(incFiles[0]).size;
  fs.writeFileSync(incFiles[0], ev(1000, 'A') + ev(1007, 'Z'));
  assert.strictEqual(fs.statSync(incFiles[0]).size, before, 'rewrite must keep the size identical');
  const t = new Date(Date.now() + 5000);
  fs.utimesSync(incFiles[0], t, t);
  const inc = foldEventLogStageWalltime(incInfo);
  assert.ok(inc.byStage.Z, 'same-size rewrite was served from a stale memo');
  assert.deepStrictEqual(inc, coldFold(incInfo), 'same-size rewrite: incremental != full walk');
}

// Replace-by-rename holding both size and mtime fixed — what `git checkout`
// does when a rewind restores an older revision of a cycle file. Only the
// inode distinguishes it.
{
  const victim = incFiles[0];
  const st = fs.statSync(victim);
  const tmp = `${victim}.tmp`;
  fs.writeFileSync(tmp, ev(1000, 'A') + ev(1007, 'Y'));
  assert.strictEqual(fs.statSync(tmp).size, st.size);
  fs.renameSync(tmp, victim);
  fs.utimesSync(victim, st.atime, st.mtime);
  assert.notStrictEqual(fs.statSync(victim).ino, st.ino, 'rename must land a new inode');
  assert.strictEqual(fs.statSync(victim).mtimeMs, st.mtimeMs);
  const inc = foldEventLogStageWalltime(incInfo);
  assert.ok(inc.byStage.Y, 'replace-by-rename at identical size+mtime was served from a stale memo');
  assert.deepStrictEqual(inc, coldFold(incInfo), 'replace-by-rename: incremental != full walk');
}

// Rewind: history truncated to the first three cycles, the last file shortened.
// Vanished paths must leave the memo rather than linger.
{
  for (const f of incFiles.slice(3)) fs.unlinkSync(f);
  fs.writeFileSync(incFiles[2], ev(1500, 'R'));
  const inc = foldEventLogStageWalltime(incInfo);
  assert.deepStrictEqual(inc, coldFold(incInfo), 'rewind: incremental != full walk');
  assertMatchesOracle(inc, incLog, 'after rewind');
  assert.strictEqual(memoSize(incLog), 3, 'memo kept entries for deleted cycle files');
}

// ---------------------------------------------------------------------------
// 4. /api/usage.json response shape. renderUsageDetail keys its re-render on
//    counts.event_rows, so that field must stay an exact record count.
// ---------------------------------------------------------------------------

fs.writeFileSync(path.join(bigRepo, '.trellis', 'logs', 'cost-ledger.jsonl'),
  `${JSON.stringify({ provider: 'codex', role: 'worker', scope: 'proof_formalization:worker:codex', ok: true, duration_seconds: 3, usage: { input_tokens: 11 } })}\n`);

const server = app.listen(0, () => {
  http.get({ port: server.address().port, path: '/trellis/usagefix/api/usage.json' }, (res) => {
    let body = '';
    res.on('data', (c) => { body += c; });
    res.on('end', () => {
      server.close();
      assert.strictEqual(res.statusCode, 200);
      const payload = JSON.parse(body);
      assert.deepStrictEqual(Object.keys(payload), [
        'runtime_root', 'counts', 'by_provider', 'by_provider_category',
        'wall_clock', 'codex_timing_by_provider', 'quota',
      ]);
      assert.deepStrictEqual(Object.keys(payload.counts),
        ['cost_rows', 'check_rows', 'quota_rows', 'event_rows']);
      assert.strictEqual(payload.counts.event_rows, walk.rows);
      assert.strictEqual(payload.counts.cost_rows, 1);
      const stages = payload.wall_clock.filter((r) => r.kind === 'stage').map((r) => r.name).sort();
      assert.deepStrictEqual(stages, ['proof_formalization', 'theorem_stating']);
      console.log('usage rollup tests passed');
    });
  });
});
