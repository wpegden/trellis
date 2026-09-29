// Chat dropdown: one option per real burst lane, each labelled with the
// burst's real kind.
//
// Both things under test are about a burst's IDENTITY reaching the dropdown
// row — how many rows one burst produces, and what kind they render under —
// and both are decided from names written by two independent producers:
//
//   the bridge     `trellis_<kind>_<id>_<suffix>` chat dirs
//                  (trellis/runtime/bridge.py:_artifact_name)
//   tmux           `trellis-<ns>-<kind>-<id>-<lane>` session names
//                  (trellis/runtime/bridge.py:_single_request_common)
//
// So the fixtures here are the real artefacts: a real event log the supervisor
// would have written, real chat dirs on disk, a real chats git repo with a real
// cycle tag, and real tmux sessions. The tmux sessions live on a PRIVATE socket
// — a live run's viewer owns `-L trellis` and nothing here may see, create or
// kill a session on it.

const assert = require('assert');
const fs = require('fs');
const os = require('os');
const path = require('path');
const { execFileSync } = require('child_process');

const root = fs.mkdtempSync(path.join(os.tmpdir(), 'trellis-viewer-chatcalls-'));
process.env.PROJECTS_ROOT = root;
const SOCKET = `trellis-test-chatcalls-${process.pid}`;
process.env.TRELLIS_TMUX_SOCKET = SOCKET;

const {
  discoverProjects,
  buildChatCalls,
  canonicalLaneKey,
  inferCallKind,
} = require('./server');

// ---- fixture builders -----------------------------------------------------

function writeJson(file, value) {
  fs.mkdirSync(path.dirname(file), { recursive: true });
  fs.writeFileSync(file, JSON.stringify(value, null, 2));
}

// A project is a directory holding trellis.config.json, with its runtime in the
// `<repo>-runtime` sibling.
function makeRun(slug, { inFlightRequestId = null } = {}) {
  const repo = path.join(root, slug);
  writeJson(path.join(repo, 'trellis.config.json'), { project: slug });
  const runtimeRoot = `${repo}-runtime`;
  writeJson(path.join(runtimeRoot, 'runtime_metadata.json'), { repo_path: repo });
  writeJson(path.join(runtimeRoot, 'protocol_state.json'), {
    phase: 'TheoremStating',
    cycle: 1,
    in_flight_request: inFlightRequestId == null ? null : { id: inFlightRequestId },
  });
  return repo;
}

// One `issue_request` command inside one RuntimeStepRecord — the authority for
// what bursts a cycle issued.
function issueRequest(repo, { cycle, index, id, kind, node = 'thm_main' }) {
  const file = path.join(
    repo, '.trellis-history', 'event-log',
    `cycle-${String(cycle).padStart(6, '0')}.jsonl`,
  );
  fs.mkdirSync(path.dirname(file), { recursive: true });
  fs.appendFileSync(file, `${JSON.stringify({
    cycle,
    index,
    commands: [{
      command: 'issue_request',
      request: { id, kind, cycle, active_node: node, mode: 'Normal' },
    }],
  })}\n`);
}

function liveChatDir(repo, name, callJson) {
  const dir = path.join(repo, '.trellis', 'chats', 'live', name);
  fs.mkdirSync(dir, { recursive: true });
  // A codex burst streams its --json events here; its presence is what makes
  // the row report a transcript.
  fs.writeFileSync(path.join(dir, 'output.log'), '{"type":"turn.completed"}\n');
  if (callJson) writeJson(path.join(dir, 'call.json'), callJson);
  return dir;
}

function projectFor(slug) {
  const project = discoverProjects().find(p => p.slug === slug);
  assert.ok(project, `fixture project "${slug}" was not discovered`);
  return project;
}

// ---- tmux on a private socket ---------------------------------------------

function tmux(...args) {
  return execFileSync('tmux', ['-L', SOCKET, ...args], {
    encoding: 'utf8', stdio: ['ignore', 'pipe', 'pipe'],
  });
}

function startBurstSession(name) {
  tmux('new-session', '-d', '-s', name, 'sleep 300');
}

function killBurstSessions() {
  try { tmux('kill-server'); } catch { /* no server was ever started */ }
  // kill-server leaves the socket behind; this host is long-lived, so don't
  // accumulate one dead socket per test run.
  try {
    fs.rmSync(path.join(
      process.env.TMUX_TMPDIR || '/tmp', `tmux-${process.getuid()}`, SOCKET,
    ), { force: true });
  } catch { /* best effort */ }
}

// ---- helpers over the response --------------------------------------------

const forRequest = (calls, id) => calls.filter(c => c.request_id === id);

function main() {
  assert.doesNotThrow(
    () => execFileSync('tmux', ['-V'], { stdio: 'ignore' }),
    'these tests drive real tmux sessions on a private socket; tmux must be installed',
  );

  // -----------------------------------------------------------------
  // 1. One burst, one chat dir, one row — even when the burst's tmux
  //    session labels its lane differently from its chat dir.
  //
  // An audit is the case where the two producers disagree. Its chat dir
  // (`trellis_stuck_math_audit_1_result`) carries no lane at all, while its
  // tmux session is suffixed `-audit`. Both name the SAME single lane. When
  // the leftover-lane backfill compared those labels literally it concluded
  // that lane "audit" had not written a dir yet and pushed a second,
  // artifact_id-less row for a burst whose transcript was already on disk —
  // the dropdown's "This burst was issued ... but no chat directory was
  // located" twin, which vanished on its own once the cycle went historical
  // because the backfill only runs for the live cycle.
  //
  // Verifier lanes (v1/v2) are spelled identically by both producers, which
  // is why only audits ever duplicated.
  // -----------------------------------------------------------------
  const auditRepo = makeRun('auditrun', { inFlightRequestId: 3 });
  issueRequest(auditRepo, { cycle: 1, index: 0, id: 1, kind: 'StuckMathAudit' });
  liveChatDir(auditRepo, 'trellis_stuck_math_audit_1_result', {
    provider: 'codex',
    model: 'gpt-5.6-sol',
    role: 'initial_planner',
    request_id: null,
    artifact_id: 'trellis_stuck_math_audit_1_result',
    scope: 'theorem_stating:initial_planner:stuck_math_audit:1:codex:gpt-5.6-sol:xhigh',
  });

  // A verifier panel in the same cycle: two lanes issued, only v1's dir
  // written so far. This is the case the backfill exists for and must keep
  // working.
  issueRequest(auditRepo, { cycle: 1, index: 1, id: 2, kind: 'Corr' });
  liveChatDir(auditRepo, 'trellis_corr_2_v1', { provider: 'codex', role: 'reviewer' });

  // And a burst that has genuinely written nothing yet: the request the
  // supervisor is waiting on right now.
  issueRequest(auditRepo, { cycle: 1, index: 2, id: 3, kind: 'Worker' });

  startBurstSession('trellis-auditrun-stuck_math_audit-1-audit');
  startBurstSession('trellis-auditrun-corr-2-v1');
  startBurstSession('trellis-auditrun-corr-2-v2');
  startBurstSession('trellis-auditrun-worker-3-worker');

  try {
    const live = buildChatCalls(projectFor('auditrun'), 'live');
    assert.strictEqual(live.source, 'live');
    assert.strictEqual(live.cycle, 1);

    const audit = forRequest(live.calls, 1);
    assert.strictEqual(audit.length, 1,
      `the audit burst must produce exactly one dropdown row, got ${audit.length}: `
      + JSON.stringify(audit.map(c => c.artifact_id)));
    assert.strictEqual(audit[0].artifact_id, 'trellis_stuck_math_audit_1_result');
    assert.ok(audit[0].has_transcript, 'the audit row must open its transcript');
    assert.strictEqual(audit[0].tmux_session, 'trellis-auditrun-stuck_math_audit-1-audit',
      'the single row keeps the live tmux session; nothing is left for a twin to carry');

    // 1b. The verifier panel still lists both lanes, and the lane with no dir
    //     yet is still announced as in-flight rather than dropped.
    const corr = forRequest(live.calls, 2);
    assert.strictEqual(corr.length, 2,
      'a verifier request whose second lane has not written its dir must still list both lanes');
    const v1 = corr.find(c => c.lane === 'v1');
    const v2 = corr.find(c => c.lane === 'v2');
    assert.ok(v1 && v2, `expected lanes v1 and v2, got ${JSON.stringify(corr.map(c => c.lane))}`);
    assert.strictEqual(v1.artifact_id, 'trellis_corr_2_v1');
    assert.strictEqual(v2.artifact_id, null, 'the lane with no chat dir is the placeholder');
    assert.strictEqual(v2.tmux_session, 'trellis-auditrun-corr-2-v2');

    // 1c. A burst with no chat dir at all is still surfaced when it is the
    //     request the supervisor is waiting on — suppressing phantoms must not
    //     suppress the real "issued, nothing written yet" row.
    const worker = forRequest(live.calls, 3);
    assert.strictEqual(worker.length, 1);
    assert.strictEqual(worker[0].artifact_id, null);
    assert.strictEqual(worker[0].tmux_session, 'trellis-auditrun-worker-3-worker');

    assert.strictEqual(live.calls.length, 4,
      `three bursts must yield four rows (audit, corr v1, corr v2, worker), got `
      + JSON.stringify(live.calls.map(c => [c.request_id, c.artifact_id, c.lane])));

    // -----------------------------------------------------------------
    // 2. The row carries the burst's real kind.
    //
    // The dropdown label and the "this burst was issued (request #N, <kind>)"
    // notice are built from it, so an audit — the burst that plans the whole
    // run — must not render as "other".
    // -----------------------------------------------------------------
    assert.strictEqual(audit[0].kind, 'stuck_math_audit');
    assert.strictEqual(v1.kind, 'corr');
    assert.strictEqual(v2.kind, 'corr');
    assert.strictEqual(worker[0].kind, 'worker');
  } finally {
    killBurstSessions();
  }

  // -----------------------------------------------------------------
  // 3. Same burst, browsed after its cycle is archived.
  //
  // Historical cycles are artifact-driven — there is no burst record in play,
  // only the committed chat dir — so the kind has to come out of the dir name.
  // `stuck_math_audit` contains none of worker/review/paper/corr/sound, so the
  // substring inference that served the other kinds returned "other" here and
  // the operator's most important burst was mislabelled in the archive.
  // -----------------------------------------------------------------
  const archiveRepo = makeRun('archiverun');
  const chatsRepo = path.join(archiveRepo, '.trellis', 'chats');
  const cycleDir = path.join(chatsRepo, 'cycle-0001');
  for (const [name, call] of [
    ['trellis_stuck_math_audit_1_result', { provider: 'codex', role: 'initial_planner', request_id: null }],
    ['trellis_review_2_decision', { provider: 'codex', role: 'reviewer', request_id: 2 }],
  ]) {
    fs.mkdirSync(path.join(cycleDir, name), { recursive: true });
    fs.writeFileSync(path.join(cycleDir, name, 'output.log'), '{"type":"turn.completed"}\n');
    writeJson(path.join(cycleDir, name, 'call.json'), call);
  }
  execFileSync('git', ['-C', chatsRepo, 'init', '-q'], { stdio: 'ignore' });
  execFileSync('git', ['-C', chatsRepo, 'config', 'user.email', 'test@example.com'], { stdio: 'ignore' });
  execFileSync('git', ['-C', chatsRepo, 'config', 'user.name', 'test'], { stdio: 'ignore' });
  execFileSync('git', ['-C', chatsRepo, 'add', '-A'], { stdio: 'ignore' });
  execFileSync('git', ['-C', chatsRepo, 'commit', '-q', '-m', 'cycle 1 chats'], { stdio: 'ignore' });
  execFileSync('git', ['-C', chatsRepo, 'tag', 'cycle-1'], { stdio: 'ignore' });

  const archived = buildChatCalls(projectFor('archiverun'), '1');
  assert.strictEqual(archived.source, 'cycle-1');
  const byId = Object.fromEntries(archived.calls.map(c => [c.artifact_id, c]));
  assert.ok(byId.trellis_stuck_math_audit_1_result, 'the archived audit dir must be listed');
  assert.strictEqual(byId.trellis_stuck_math_audit_1_result.kind, 'stuck_math_audit');
  assert.strictEqual(byId.trellis_stuck_math_audit_1_result.request_id, 1);
  // The archived listing now speaks the same kind vocabulary as the live one
  // (`kindTag`'s), rather than its own near-miss synonyms.
  assert.strictEqual(byId.trellis_review_2_decision.kind, 'review');

  // -----------------------------------------------------------------
  // 4. Supporting checks on the two derivations the rows above depend on.
  // -----------------------------------------------------------------
  // Only verifier panels are genuinely multi-lane; every other kind's lane
  // label — whichever producer wrote it — means "the one lane of this burst".
  assert.strictEqual(canonicalLaneKey('v1'), 'v1');
  assert.strictEqual(canonicalLaneKey('v12'), 'v12');
  for (const singleLane of ['', null, undefined, 'audit', 'worker', 'reviewer']) {
    assert.strictEqual(canonicalLaneKey(singleLane), '',
      `"${singleLane}" is a single-lane label and must collapse to the same key`);
  }
  // A canonical artifact dir names its kind exactly; scope dirs keep the older
  // substring inference.
  assert.strictEqual(inferCallKind('trellis_stuck_math_audit_1_result'), 'stuck_math_audit');
  assert.strictEqual(inferCallKind('trellis_audit_9_result'), 'audit');
  assert.strictEqual(inferCallKind('trellis_corr_38_v2'), 'corr');
  assert.strictEqual(inferCallKind('worker_proof_formalization:worker:codex:gpt-5:default'), 'worker');
  assert.strictEqual(inferCallKind('reviewer_theorem_stating:reviewer:sound:60:v2:gemini:x'), 'reviewer');
  assert.strictEqual(inferCallKind('something_unrecognised'), 'other');

  console.log('chat call tests passed');
}

try {
  main();
} finally {
  killBurstSessions();
  fs.rmSync(root, { recursive: true, force: true });
}
