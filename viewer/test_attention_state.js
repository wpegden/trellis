// Attention-state ladder: every "the run has stopped and wants a human"
// state must render DISTINCTLY, and the severity order must be total and
// stable. The defect these tests pin: `NeedInput` (a fail-loud process
// failure) and the advance gate (a routine checkpoint) used to render
// identically, so rubber-stamping the first looked exactly like approving
// the second.
//
// Plain node test file, no framework — matches test_grunts.js /
// test_halt_state.js. Run: node viewer/test_attention_state.js

const assert = require('assert');
const fs = require('fs');
const os = require('os');
const path = require('path');

const {
  ATTENTION_STATES,
  GATE_ROWS,
  HALT_ROWS,
  augmentViewerStateAttention,
  gateAttention,
  haltAttention,
  haltMarkersAreViewerLiftable,
  haltStateForRuntimeRoot,
  liftHaltForResume,
  pauseAttention,
  supervisorContextFromStatus,
} = require('./server');

let passed = 0;
function check(name, fn) {
  fn();
  passed += 1;
  console.log(`  ok   ${name}`);
}

// ---------------------------------------------------------------------------
// Catalogue invariants
// ---------------------------------------------------------------------------

check('every attention state has a unique rank', () => {
  const ranks = Object.values(ATTENTION_STATES).map(s => s.rank);
  assert.strictEqual(new Set(ranks).size, ranks.length);
});

check('every attention state has a unique label', () => {
  const labels = Object.values(ATTENTION_STATES).map(s => s.label);
  assert.strictEqual(new Set(labels).size, labels.length);
});

check('every attention state carries a tier, icon and non-empty summary', () => {
  for (const [id, s] of Object.entries(ATTENTION_STATES)) {
    assert.strictEqual(s.id, id, `${id}: id field must match its key`);
    assert.ok(['fault', 'failure', 'gate', 'pause'].includes(s.tier), `${id}: bad tier ${s.tier}`);
    assert.ok(s.icon && s.icon.length, `${id}: missing icon`);
    assert.ok(s.summary && s.summary.length > 20, `${id}: missing summary`);
  }
});

check('only halt markers reach the fault tier — no gate can look like a halt', () => {
  const faultIds = Object.values(ATTENTION_STATES).filter(s => s.tier === 'fault').map(s => s.id);
  assert.deepStrictEqual(
    faultIds.sort(),
    ['checker_disagreement_halt', 'malformed_halt_marker', 'system_feedback_halt'],
  );
  // The lookup table a gate kind resolves against must contain NO fault row —
  // that is what makes the invariant structural rather than conventional.
  for (const [kind, row] of Object.entries(GATE_ROWS)) {
    assert.notStrictEqual(row.tier, 'fault', `GATE_ROWS.${kind} must not be fault tier`);
  }
  // ...and the halt table must contain ONLY fault rows.
  for (const [kind, row] of Object.entries(HALT_ROWS)) {
    assert.strictEqual(row.tier, 'fault', `HALT_ROWS.${kind} must be fault tier`);
  }
});

check('checker disagreement outranks system feedback (soundness stays dominant)', () => {
  assert.ok(
    ATTENTION_STATES.checker_disagreement_halt.rank > ATTENTION_STATES.system_feedback_halt.rank,
  );
  // ...and both outrank every non-halt state.
  const maxNonFault = Math.max(
    ...Object.values(ATTENTION_STATES).filter(s => s.tier !== 'fault').map(s => s.rank),
  );
  assert.ok(ATTENTION_STATES.system_feedback_halt.rank > maxNonFault);
});

check('need_input and advance differ in tier, icon AND label', () => {
  const a = ATTENTION_STATES.advance;
  const n = ATTENTION_STATES.need_input;
  assert.notStrictEqual(a.tier, n.tier);
  assert.notStrictEqual(a.icon, n.icon);
  assert.notStrictEqual(a.label, n.label);
  assert.ok(n.rank > a.rank, 'a process failure must outrank a routine gate');
  assert.strictEqual(n.tier, 'failure');
  assert.strictEqual(a.tier, 'gate');
});

check('routine gates all share the calm gate tier', () => {
  for (const kind of ['advance', 'protected_reapproval', 'assumption_review']) {
    assert.strictEqual(ATTENTION_STATES[kind].tier, 'gate', kind);
  }
});

// ---------------------------------------------------------------------------
// gateAttention — the kernel gate_kind path
// ---------------------------------------------------------------------------

const stateAt = (kind, extra = {}) => ({
  awaiting_human_input: true,
  gate: { kind, from_invalid_attempt: false, reason: '', reason_source: '', question: '', unblocking_input: '', ruled_out: [], escalated_at_cycle: null, ...extra },
});

check('no gate open => null (mutually exclusive with every gate row)', () => {
  assert.strictEqual(gateAttention(stateAt('none')), null);
  assert.strictEqual(gateAttention({ gate: { kind: 'none' }, awaiting_human_input: true }), null);
  assert.strictEqual(gateAttention(null), null);
  assert.strictEqual(gateAttention({}), null);
});

check('each gate_kind maps to its own distinct row', () => {
  const kinds = ['advance', 'need_input', 'protected_reapproval', 'assumption_review'];
  const rows = kinds.map(k => gateAttention(stateAt(k)));
  rows.forEach((row, i) => {
    assert.ok(row, `${kinds[i]} produced no row`);
    assert.strictEqual(row.gate_kind, kinds[i]);
    assert.strictEqual(row.id, kinds[i]);
    assert.strictEqual(row.degraded, false);
  });
  assert.strictEqual(new Set(rows.map(r => r.label)).size, kinds.length);
  assert.strictEqual(new Set(rows.map(r => r.icon)).size, kinds.length);
  assert.strictEqual(new Set(rows.map(r => r.rank)).size, kinds.length);
});

check('exactly one gate row is produced at a time', () => {
  const row = gateAttention(stateAt('advance'));
  assert.ok(row && !Array.isArray(row));
  assert.strictEqual(row.tier, 'gate');
});

check('need_input carries the reason, its source, and inspect pointers', () => {
  const row = gateAttention(stateAt('need_input', {
    reason: 'NeedInputAuditor failed twice in a row; routing to HumanGate without a new plan',
    reason_source: 'latest_stuck_math_audit_rejection_reason',
  }));
  assert.strictEqual(row.tier, 'failure');
  assert.match(row.reason, /NeedInputAuditor failed twice/);
  assert.strictEqual(row.reason_source, 'latest_stuck_math_audit_rejection_reason');
  assert.ok(row.inspect.length >= 3, 'a process failure must point at what to inspect');
  assert.ok(row.inspect.some(p => p.includes('latest_stuck_math_audit_rejection_reason')));
  assert.ok(row.inspect.some(p => p.includes('stuck_math_audit')));
  assert.ok(row.inspect.some(p => p.toLowerCase().includes('chats')));
});

check('need_input surfaces the GapResearch question / ruled-out / unblocking input', () => {
  const row = gateAttention(stateAt('need_input', {
    reason: 'planner_human_required',
    reason_source: 'gap_human_escalation.reason',
    question: 'Is the constant in Lemma 4.2 meant to be 2 or 3?',
    unblocking_input: 'the intended constant',
    ruled_out: ['induction on n', 'direct Cauchy-Schwarz'],
    escalated_at_cycle: 941,
  }));
  assert.strictEqual(row.question, 'Is the constant in Lemma 4.2 meant to be 2 or 3?');
  assert.deepStrictEqual(row.ruled_out, ['induction on n', 'direct Cauchy-Schwarz']);
  assert.strictEqual(row.unblocking_input, 'the intended constant');
  assert.strictEqual(row.escalated_at_cycle, 941);
});

check('a routine gate never carries an escalation diagnosis, whatever the adapter sends', () => {
  // The adapter-side guard is one process and one version away (a viewer can
  // serve a runtime whose adapter predates it, or a hand-edited protocol
  // state). Every diagnosis field must be stripped again at assembly, or a
  // routine checkpoint renders the `gate-reason` block and reads as an
  // incident — the exact failure the design comment warns about.
  for (const kind of ['advance', 'protected_reapproval', 'assumption_review']) {
    const row = gateAttention(stateAt(kind, {
      reason: 'stale incident text',
      reason_source: 'latest_stuck_math_audit_rejection_reason',
      question: 'a leaked question',
      unblocking_input: 'a leaked unblocking input',
      ruled_out: ['a leaked alternative'],
      escalated_at_cycle: 941,
    }));
    assert.strictEqual(row.tier, 'gate', kind);
    assert.strictEqual(row.reason, '', `${kind} leaked reason`);
    assert.strictEqual(row.reason_source, '', `${kind} leaked reason_source`);
    assert.strictEqual(row.question, '', `${kind} leaked question`);
    assert.strictEqual(row.unblocking_input, '', `${kind} leaked unblocking_input`);
    assert.deepStrictEqual(row.ruled_out, [], `${kind} leaked ruled_out`);
    assert.strictEqual(row.escalated_at_cycle, null, `${kind} leaked escalated_at_cycle`);
    assert.deepStrictEqual(row.inspect, [], `${kind} leaked inspect pointers`);
  }
});

check('an unclassified gate never carries an escalation diagnosis either', () => {
  const row = gateAttention(stateAt('some_future_gate', { reason: 'leaked', question: 'leaked' }));
  assert.strictEqual(row.tier, 'gate');
  assert.strictEqual(row.reason, '');
  assert.strictEqual(row.question, '');
  assert.deepStrictEqual(row.inspect, []);
});

check('gate_from_invalid_attempt is carried through', () => {
  assert.strictEqual(gateAttention(stateAt('need_input', { from_invalid_attempt: true })).from_invalid_attempt, true);
  assert.strictEqual(gateAttention(stateAt('advance')).from_invalid_attempt, false);
});

check('an unrecognised gate_kind degrades to the unclassified row, not to routine', () => {
  const row = gateAttention(stateAt('some_future_gate'));
  assert.strictEqual(row.id, 'unclassified_gate');
  assert.strictEqual(row.gate_kind, 'some_future_gate');
  assert.match(row.summary, /does not report which gate kind/);
});

// `gate_kind` is read out of protocol_state.json as raw JSON and never passes
// through the kernel's serde, so these two are reachable from a corrupt,
// hand-edited or future state file — not hypothetical.
check('a gate_kind naming a HALT row cannot borrow the halt treatment', () => {
  for (const kind of ['checker_disagreement_halt', 'system_feedback_halt', 'malformed_halt_marker']) {
    const row = gateAttention(stateAt(kind));
    assert.strictEqual(row.id, 'unclassified_gate', `${kind} resolved to a halt row`);
    assert.strictEqual(row.tier, 'gate', `${kind} reached the fault tier`);
    assert.ok(!/HALTED/.test(row.label), `${kind} wore a halt heading`);
  }
});

check('a gate_kind naming an inherited Object property yields no row fields', () => {
  // `ATTENTION_STATES[kind] || fallback` never fires its fallback for these:
  // every object inherits them and they are truthy.
  for (const kind of ['constructor', 'toString', 'valueOf', 'hasOwnProperty', '__proto__']) {
    const row = gateAttention(stateAt(kind));
    assert.strictEqual(row.id, 'unclassified_gate', `${kind} resolved to a prototype member`);
    assert.strictEqual(row.tier, 'gate', `${kind} produced tier ${row.tier}`);
    assert.strictEqual(typeof row.rank, 'number', `${kind} produced a non-numeric rank`);
    assert.ok(row.label && row.label.length, `${kind} produced a blank label`);
    assert.ok(row.icon && row.icon.length, `${kind} produced a blank icon`);
  }
});

check('a marker_kind naming an inherited Object property degrades to malformed', () => {
  for (const kind of ['constructor', 'toString', 'advance']) {
    const rows = haltAttention({ halted: true, markers: [{ marker_kind: kind, marker_path: '/rt/x.json', marker: {} }] });
    assert.strictEqual(rows[0].id, 'malformed_halt_marker', `${kind} resolved to ${rows[0].id}`);
    assert.strictEqual(rows[0].tier, 'fault');
  }
});

// ---------------------------------------------------------------------------
// gateAttention — graceful degradation on a runtime with no `state.gate`
// ---------------------------------------------------------------------------

check('pre-gate runtime, not awaiting => still null', () => {
  assert.strictEqual(gateAttention({ awaiting_human_input: false, last_review: { decision: 'need_input' } }), null);
});

check('pre-gate runtime falls back to the reviewer decision and flags itself degraded', () => {
  const failure = gateAttention({ awaiting_human_input: true, last_review: { decision: 'NeedInput', reason: 'stuck' } });
  assert.strictEqual(failure.id, 'need_input');
  assert.strictEqual(failure.tier, 'failure');
  assert.strictEqual(failure.degraded, true);
  assert.strictEqual(failure.gate_kind, '');
  assert.match(failure.reason_source, /runtime predates state\.gate/);

  const routine = gateAttention({ human_input_outstanding: true, last_review: { decision: 'advance_phase' } });
  assert.strictEqual(routine.id, 'advance');
  assert.strictEqual(routine.tier, 'gate');
  assert.strictEqual(routine.degraded, true);

  const unknown = gateAttention({ awaiting_human_input: true, last_review: {} });
  assert.strictEqual(unknown.id, 'unclassified_gate');
  assert.strictEqual(unknown.degraded, true);
});

// ---------------------------------------------------------------------------
// haltAttention
// ---------------------------------------------------------------------------

check('no halt => no fault rows', () => {
  assert.deepStrictEqual(haltAttention(null), []);
  assert.deepStrictEqual(haltAttention({ halted: false }), []);
});

check('checker and system-feedback halts render as distinct fault rows', () => {
  const rows = haltAttention({
    halted: true,
    markers: [
      { marker_kind: 'checker_disagreement', marker_path: '/rt/checker_disagreement_halt.json', marker: { active_node: 'Lemma4', cycle: 12 } },
      { marker_kind: 'system_feedback', marker_path: '/rt/system_feedback_halt.json', marker: { active_node: 'Lemma9', cycle: 13, burst_role: 'worker' } },
    ],
  });
  assert.strictEqual(rows.length, 2);
  assert.strictEqual(rows[0].id, 'checker_disagreement_halt');
  assert.strictEqual(rows[1].id, 'system_feedback_halt');
  assert.notStrictEqual(rows[0].icon, rows[1].icon);
  assert.notStrictEqual(rows[0].label, rows[1].label);
  assert.ok(rows[0].rank > rows[1].rank);
  assert.strictEqual(rows[0].node, 'Lemma4');
  assert.strictEqual(rows[0].cycle, '12');
  assert.strictEqual(rows[1].burst_role, 'worker');
  for (const row of rows) {
    assert.strictEqual(row.tier, 'fault');
    assert.ok(row.inspect[0].includes(row.marker_path));
  }
});

check('a malformed marker becomes the top-ranked fault row', () => {
  const rows = haltAttention({
    halted: true,
    markers: [{ marker_kind: 'system_feedback', marker_path: '/rt/system_feedback_halt.json', parse_error: 'Unexpected token' }],
  });
  assert.strictEqual(rows[0].id, 'malformed_halt_marker');
  assert.strictEqual(rows[0].parse_error, 'Unexpected token');
  assert.ok(rows[0].rank > ATTENTION_STATES.checker_disagreement_halt.rank);
});

check('halt rows come out sorted by rank, not in marker-discovery order', () => {
  // Markers are discovered by a fixed filename scan (checker, then system
  // feedback), and the frontend renders the array in order. A MALFORMED
  // system-feedback marker outranks a well-formed checker disagreement, so
  // discovery order and severity order come apart exactly here.
  const rows = haltAttention({
    halted: true,
    markers: [
      { marker_kind: 'checker_disagreement', marker_path: '/rt/c.json', marker: {} },
      { marker_kind: 'system_feedback', marker_path: '/rt/s.json', parse_error: 'bad json' },
    ],
  });
  assert.deepStrictEqual(rows.map(r => r.id), ['malformed_halt_marker', 'checker_disagreement_halt']);
  for (let i = 1; i < rows.length; i += 1) {
    assert.ok(rows[i - 1].rank > rows[i].rank, `rank not descending at ${i}`);
  }
});

check('the DOM stacking order is also the severity order', () => {
  // The halt banner is a separate element that precedes the gate banner in
  // index.html, so "halts render above gates" holds only while EVERY fault
  // rank exceeds every non-fault rank.
  const faults = Object.values(ATTENTION_STATES).filter(s => s.tier === 'fault').map(s => s.rank);
  const others = Object.values(ATTENTION_STATES).filter(s => s.tier !== 'fault').map(s => s.rank);
  assert.ok(Math.min(...faults) > Math.max(...others));
});

// ---------------------------------------------------------------------------
// Liveness. A halt marker records WHY the kernel stopped dispatching and
// nothing about whether the supervisor process survived. The defect these
// tests pin: the halt banner asserted a parked supervisor from marker
// existence alone while the pause banner, looking at the same exit, called it
// unexplained — two dialogs, each holding half the truth.
// ---------------------------------------------------------------------------

const HALT_MARKERS = [{
  marker_kind: 'system_feedback',
  marker_path: '/rt/system_feedback_halt.json',
  marker: { cycle: 847, burst_role: 'stuck_math_audit' },
}];

check('every fault row states its liveness in a separate sentence', () => {
  for (const row of Object.values(ATTENTION_STATES).filter(s => s.tier === 'fault')) {
    assert.ok(row.cause && row.cause.length > 20, `${row.id}: missing cause`);
    assert.ok(row.summary.startsWith(row.cause), `${row.id}: summary must open with its cause`);
    assert.ok(row.summary.length > row.cause.length, `${row.id}: summary must close with a liveness sentence`);
  }
});

check('a halt marker with the supervisor still up reads as parked', () => {
  const supervisor = supervisorContextFromStatus({
    state: 'running', wrapper_pid: 4242, resumable: false, launch_env: { present: true },
  });
  const [row] = haltAttention({ halted: true, markers: HALT_MARKERS }, supervisor);
  assert.match(row.summary, /will not dispatch new bursts/);
  assert.ok(!/exited/.test(row.summary), 'a live supervisor must not be reported as gone');
  assert.strictEqual(row.supervisor_state, 'running');
  assert.strictEqual(row.resumable, false);
  assert.strictEqual(row.launch_env, null);
  assert.match(row.inspect[0], /rename the marker to a \.lifted-<unix_ts>\.json sibling/);
});

check('a halt marker with the supervisor gone says so and carries the resume control', () => {
  const supervisor = supervisorContextFromStatus({
    state: 'down', wrapper_pid: null, resumable: true, launch_env: { present: true, tmux_session: 'run' },
  });
  const rows = haltAttention({ halted: true, markers: HALT_MARKERS }, supervisor);
  assert.strictEqual(rows.length, 1, 'an explained exit is one dialog');
  const [row] = rows;
  assert.match(row.summary, /process has exited/);
  assert.ok(!/will not dispatch/.test(row.summary));
  assert.strictEqual(row.supervisor_state, 'down');
  assert.strictEqual(row.resumable, true);
  assert.strictEqual(row.launch_env.present, true);
  assert.strictEqual(row.resume_withheld, '');
  // Lifting the marker resumes nothing once the process is gone, so the
  // control that does both is what the remediation names.
  assert.match(row.inspect[0], /the resume control lifts this marker/);
});

check('an unreadable pause status leaves the parked wording in place', () => {
  const supervisor = supervisorContextFromStatus({ state: 'unknown', error: 'status failed' });
  assert.strictEqual(supervisor.known, false);
  const [row] = haltAttention({ halted: true, markers: HALT_MARKERS }, supervisor);
  assert.match(row.summary, /will not dispatch new bursts/);
  assert.strictEqual(row.resumable, false);
});

check('no liveness context yields the halt row unchanged', () => {
  const [row] = haltAttention({ halted: true, markers: HALT_MARKERS });
  assert.match(row.summary, /will not dispatch new bursts/);
  assert.strictEqual(row.supervisor_state, '');
  assert.strictEqual(row.resumable, false);
});

// ---------------------------------------------------------------------------
// Which halts the resume control may lift. Resuming lifts every marker on
// disk at once, so the strictest one on the run decides for all of them.
// ---------------------------------------------------------------------------

const CHECKER_MARKER = {
  marker_kind: 'checker_disagreement',
  marker_path: '/rt/checker_disagreement_halt.json',
  marker: { active_node: 'Example', cycle: 12 },
};
const UNREADABLE_MARKER = {
  marker_kind: 'system_feedback',
  marker_path: '/rt/system_feedback_halt.json',
  parse_error: 'Unexpected token',
};
const EXITED = supervisorContextFromStatus({
  state: 'down', wrapper_pid: null, resumable: true, launch_env: { present: true },
});

check('a system-feedback marker is the one kind the viewer lifts', () => {
  assert.strictEqual(haltMarkersAreViewerLiftable(HALT_MARKERS), true);
  assert.strictEqual(haltMarkersAreViewerLiftable([CHECKER_MARKER]), false);
  assert.strictEqual(haltMarkersAreViewerLiftable([UNREADABLE_MARKER]), false);
  assert.strictEqual(haltMarkersAreViewerLiftable([...HALT_MARKERS, CHECKER_MARKER]), false);
  assert.strictEqual(haltMarkersAreViewerLiftable([]), false);
});

check('a soundness halt withholds the resume control and says why', () => {
  const [row] = haltAttention({ halted: true, markers: [CHECKER_MARKER] }, EXITED);
  assert.match(row.summary, /process has exited/);
  assert.strictEqual(row.resumable, false);
  assert.strictEqual(row.liftable, false);
  assert.ok(row.resume_withheld.length > 20, 'the absent control needs an explanation');
  assert.match(row.inspect[0], /rename the marker to a \.lifted-<unix_ts>\.json sibling/);
});

check('a soundness halt alongside a system-feedback one governs both rows', () => {
  const rows = haltAttention({ halted: true, markers: [CHECKER_MARKER, ...HALT_MARKERS] }, EXITED);
  assert.strictEqual(rows.length, 2);
  assert.deepStrictEqual(rows.map(r => r.resumable), [false, false]);
  // The explanation sits on the row that causes it, once.
  assert.deepStrictEqual(
    rows.filter(r => r.resume_withheld).map(r => r.marker_kind),
    ['checker_disagreement'],
  );
});

check('a marker nobody can parse withholds the control too', () => {
  const [row] = haltAttention({ halted: true, markers: [UNREADABLE_MARKER] }, EXITED);
  assert.strictEqual(row.id, 'malformed_halt_marker');
  assert.strictEqual(row.resumable, false);
  assert.ok(row.resume_withheld.length > 20);
});

check('a run with no recorded launch env stays non-resumable and silent about it', () => {
  // The three dead runs: a June marker, no `launch_env.json`. A resume
  // control there would relaunch a run nobody is running.
  const dead = supervisorContextFromStatus({
    state: 'down', wrapper_pid: null, resumable: false, launch_env: { present: false },
  });
  const [row] = haltAttention({ halted: true, markers: HALT_MARKERS }, dead);
  assert.strictEqual(row.resumable, false);
  assert.strictEqual(row.launch_env.present, false);
  assert.strictEqual(row.resume_withheld, '',
    'a run that was never resumable needs no note about a withheld control');
});

// ---------------------------------------------------------------------------
// pauseAttention — one dialog per stopped run
// ---------------------------------------------------------------------------

const pauseRoot = fs.mkdtempSync(path.join(os.tmpdir(), 'trellis-viewer-pause-'));
try {
  const repoPath = path.join(pauseRoot, 'repo');
  const runtimeRoot = `${repoPath}-runtime`;
  fs.mkdirSync(repoPath, { recursive: true });
  fs.mkdirSync(runtimeRoot, { recursive: true });
  fs.writeFileSync(path.join(runtimeRoot, 'runtime_metadata.json'), '{}');
  const projectInfo = { slug: 'pause-attention-fixture', repoPath, stateDir: repoPath };
  const down = { state: 'down', wrapper_pid: null, request: null, resumable: true, launch_env: { present: true } };
  const halted = { halted: true, markers: HALT_MARKERS };

  const request = { kind: 'operator', reason: 'stopping for the night' };
  const paused = {
    state: 'paused', request, resumable: true, launch_env: { present: true },
  };

  check('down with a halt marker yields no pause row', () => {
    assert.strictEqual(pauseAttention(projectInfo, down, halted), null);
  });

  check('paused with a halt marker yields no pause row', () => {
    // The combination that produced two banners and two Resume buttons,
    // each explaining the same stop a different way.
    assert.strictEqual(pauseAttention(projectInfo, paused, halted), null);
  });

  check('a halt marker leaves exactly one row and one resume control', () => {
    // The pair the operator actually sees, asserted together: whichever way
    // the supervisor went down, the halt banner is the dialog and it holds
    // the only control.
    for (const status of [down, paused]) {
      const rows = haltAttention(halted, supervisorContextFromStatus(status));
      assert.strictEqual(rows.length, 1, `${status.state}: one dialog`);
      assert.strictEqual(rows.filter(r => r.resumable).length, 1, `${status.state}: one control`);
      assert.strictEqual(pauseAttention(projectInfo, status, halted), null, `${status.state}: no second dialog`);
    }
  });

  check('down with nothing on disk still raises the unexplained-exit alarm', () => {
    const row = pauseAttention(projectInfo, down, { halted: false });
    assert.strictEqual(row.id, 'supervisor_down_unexplained');
    assert.strictEqual(row.tier, 'failure');
    assert.strictEqual(row.resumable, true);
    assert.strictEqual(row.launch_env.present, true);
    assert.deepStrictEqual(haltAttention({ halted: false }, supervisorContextFromStatus(down)), []);
  });

  check('paused with nothing on disk keeps its own row', () => {
    const row = pauseAttention(projectInfo, paused, { halted: false });
    assert.strictEqual(row.id, 'operator_pause');
    assert.strictEqual(row.resumable, true);
  });

  check('the halt read defaults to the project runtime root', () => {
    const markerPath = path.join(runtimeRoot, 'system_feedback_halt.json');
    assert.strictEqual(pauseAttention(projectInfo, down).id, 'supervisor_down_unexplained');
    assert.strictEqual(pauseAttention(projectInfo, paused).id, 'operator_pause');
    fs.writeFileSync(markerPath, JSON.stringify({ cycle: 847 }));
    assert.strictEqual(pauseAttention(projectInfo, down), null);
    assert.strictEqual(pauseAttention(projectInfo, paused), null);
    fs.unlinkSync(markerPath);
  });

  check('the resume gate refuses rather than lifting what it should not', () => {
    // The server side of the same policy the banner renders. Every refusal
    // has to leave the marker exactly where it was.
    const systemPath = path.join(runtimeRoot, 'system_feedback_halt.json');
    const checkerPath = path.join(runtimeRoot, 'checker_disagreement_halt.json');
    const payload = JSON.stringify({ fingerprint: 'fp-1', cycle: 847 });
    const stillThere = (p) => assert.strictEqual(fs.existsSync(p), true, `${p} must survive a refusal`);

    assert.deepStrictEqual(liftHaltForResume(projectInfo, { clear_halt: true }), [],
      'a run with no marker resumes untouched');

    fs.writeFileSync(systemPath, payload);
    assert.throws(() => liftHaltForResume(projectInfo, {}), (e) => {
      assert.strictEqual(e.statusCode, 409);
      assert.match(e.message, /Resume from the halt banner/);
      return true;
    }, 'an unauthorized resume must be refused');
    stillThere(systemPath);

    fs.writeFileSync(checkerPath, JSON.stringify({ node: 'Example' }));
    assert.throws(() => liftHaltForResume(projectInfo, { clear_halt: true }), (e) => {
      assert.strictEqual(e.statusCode, 409);
      assert.match(e.message, /checker_disagreement/);
      return true;
    }, 'a soundness halt must be refused even when the lift is authorized');
    stillThere(systemPath);
    stillThere(checkerPath);
    fs.unlinkSync(checkerPath);

    // This fixture has no `launch_env.json`, which is the dead-run shape:
    // `trellis_pause.sh` would refuse the relaunch, so the marker stays.
    assert.throws(() => liftHaltForResume(projectInfo, { clear_halt: true }), (e) => {
      assert.strictEqual(e.statusCode, 409);
      assert.match(e.message, /cannot be relaunched/);
      return true;
    });
    stillThere(systemPath);
    fs.unlinkSync(systemPath);
  });

  check('a live supervisor keeps its own row while a marker sits on disk', () => {
    // An armed pause offers the disarm, which resolves the pause on its own,
    // and the halt row carries no control while the process is up.
    const arming = pauseAttention(projectInfo, { state: 'arming', request }, halted);
    assert.strictEqual(arming.id, 'pause_arming');
    assert.strictEqual(pauseAttention(projectInfo, { state: 'running', request: null }, halted), null);
    const [row] = haltAttention(halted, supervisorContextFromStatus({ state: 'arming', wrapper_pid: 7 }));
    assert.strictEqual(row.resumable, false);
  });
} finally {
  fs.rmSync(pauseRoot, { recursive: true, force: true });
}

// ---------------------------------------------------------------------------
// Wiring: halt-state endpoint payload + viewer-state augmentation
// ---------------------------------------------------------------------------

const root = fs.mkdtempSync(path.join(os.tmpdir(), 'trellis-viewer-attention-'));
try {
  check('haltStateForRuntimeRoot stamps attention rows only when halted', () => {
    assert.strictEqual(haltStateForRuntimeRoot(root).attention, undefined);
    fs.writeFileSync(path.join(root, 'checker_disagreement_halt.json'), JSON.stringify({ active_node: 'N', cycle: 4 }));
    const state = haltStateForRuntimeRoot(root);
    assert.strictEqual(state.halted, true);
    assert.strictEqual(state.attention.length, 1);
    assert.strictEqual(state.attention[0].id, 'checker_disagreement_halt');
    assert.strictEqual(state.attention[0].tier, 'fault');
  });
} finally {
  fs.rmSync(root, { recursive: true, force: true });
}

// ---------------------------------------------------------------------------
// Cross-layer: server.js decides the tier, index.html styles it. Nothing here
// asserted anything about the frontend, which is how a tier that server.js
// could emit ended up with no CSS rule at all.
// ---------------------------------------------------------------------------

const indexHtml = fs.readFileSync(path.join(__dirname, 'public', 'index.html'), 'utf-8');

check('every tier a row can carry has both a banner and a panel CSS rule', () => {
  const gateTiers = new Set(
    Object.values(GATE_ROWS).concat([ATTENTION_STATES.unclassified_gate]).map(r => r.tier),
  );
  const haltTiers = new Set(
    Object.values(HALT_ROWS).concat([ATTENTION_STATES.malformed_halt_marker]).map(r => r.tier),
  );
  // Banner rule for every tier that can reach either banner. Anchored to the
  // start of a line: `.feedback-panel.tier-failure {` CONTAINS the substring
  // `.tier-failure {`, so an unanchored check is satisfied by the panel rule
  // and proves nothing about the banner.
  // It must be the rule that actually PAINTS the bar, not one of the
  // descendant rules (`.tier-failure .attention-label { … }`) that also start
  // at column 0 and would satisfy a bare name check.
  for (const tier of new Set([...gateTiers, ...haltTiers])) {
    assert.ok(
      new RegExp(`^\\.tier-${tier}\\s*\\{[^}]*background`, 'm').test(indexHtml),
      `index.html has no top-level .tier-${tier} banner rule setting a background`,
    );
  }
  // Panel rule for every tier that can reach the feedback panel. Halt tiers
  // never do — the panel renders a gate row only.
  for (const tier of gateTiers) {
    assert.ok(
      indexHtml.includes(`.feedback-panel.tier-${tier} {`),
      `index.html has no .feedback-panel.tier-${tier} rule, so that panel renders unstyled`,
    );
  }
});

check('the halt banner renders the resume control the halt row now carries', () => {
  // `resumable` on the halt row only reaches the operator if the banner
  // appends the button and the fault tier styles it.
  assert.ok(/^\.tier-fault \.attention-actions/m.test(indexHtml),
    'index.html has no .tier-fault .attention-actions rule, so the halt action renders unstyled');
  assert.ok(/^\.tier-fault button \{/m.test(indexHtml),
    'index.html has no .tier-fault button rule, so the halt action renders unstyled');
  const haltBanner = indexHtml.slice(
    indexHtml.indexOf('async function fetchHaltState'),
    indexHtml.indexOf('const UPDATE_DISMISS_KEY'),
  );
  assert.ok(haltBanner.length > 0, 'could not locate the halt banner in index.html');
  assert.ok(/row\.resumable/.test(haltBanner) && /appendAttentionAction\(el, 'Resume run', 'resume'/.test(haltBanner),
    'the halt banner must offer the resume control when the row is resumable');
  assert.ok(/const body = \[\];/.test(haltBanner),
    'liveness is server-computed; the banner body must not open with a hard-coded parked supervisor');
  assert.ok(/row\.resume_withheld/.test(haltBanner),
    'the halt banner must render the server-computed reason a control is absent');
});

check('the halt resume asks first and authorizes the lift explicitly', () => {
  // The lift is a side effect on a fault diagnostic, so the operator answers
  // for it and the server sees an explicit request rather than inferring one.
  const haltBanner = indexHtml.slice(
    indexHtml.indexOf('const HALT_RESUME_CONFIRM'),
    indexHtml.indexOf('const UPDATE_DISMISS_KEY'),
  );
  assert.ok(haltBanner.length > 0, 'could not locate the halt banner in index.html');
  assert.ok(/confirm: HALT_RESUME_CONFIRM/.test(haltBanner),
    'the halt resume must put its confirmation in front of the action');
  assert.ok(/body: \{ clear_halt: true \}/.test(haltBanner),
    'the halt resume must authorize the lift in the request body');
  assert.ok(/\.lifted-<unix_ts>\.json/.test(haltBanner) && /unacknowledged/.test(haltBanner),
    'the confirmation must say where the marker goes and that the fingerprint is untouched');

  const action = indexHtml.slice(
    indexHtml.indexOf('function appendAttentionAction'),
    indexHtml.indexOf('// Fail-loudly halt banner'),
  );
  assert.ok(/options\.confirm && !confirm\(options\.confirm\)/.test(action),
    'a declined confirmation must abandon the action');

  const pauseAction = indexHtml.slice(
    indexHtml.indexOf('async function doPauseAction'),
    indexHtml.indexOf('function renderPauseBanner'),
  );
  assert.ok(/JSON\.stringify\(body \|\| \{\}\)/.test(pauseAction),
    'the pause action must post a JSON body the server can read clear_halt from');
  assert.ok(/fetchHaltState\(\)/.test(pauseAction),
    'a resume moves the halt marker, so the halt banner must be refetched');
});

check('the pause banner keeps its own controls unconfirmed', () => {
  // Arming and disarming a pause move no diagnostics, so they stay one click.
  const pauseBanner = indexHtml.slice(
    indexHtml.indexOf('function renderPauseBanner'),
    indexHtml.indexOf('async function fetchPauseState'),
  );
  assert.ok(pauseBanner.length > 0, 'could not locate the pause banner in index.html');
  assert.ok(/appendAttentionAction\(el, 'Resume run', 'resume'\)/.test(pauseBanner));
  assert.ok(/appendAttentionAction\(el, 'Cancel pause', 'disarm'\)/.test(pauseBanner));
  assert.ok(!/clear_halt/.test(pauseBanner),
    'the pause banner renders only for a run with no marker, so it never asks for a lift');
});

check('the feedback panel inverts its primary action on the failure tier', () => {
  // The whole point of the failure treatment: the empty approve is the rubber
  // stamp, so it must not be the green primary there.
  const ternary = indexHtml.slice(
    indexHtml.indexOf('const buttonsHtml = failure'),
    indexHtml.indexOf('const placeholder = failure'),
  );
  assert.ok(ternary.length > 0, 'could not locate the button ternary in index.html');
  const [failureBranch, routineBranch] = ternary.split(': `');
  assert.ok(failureBranch.includes('btn-approve') && failureBranch.includes('Send Diagnosis / Input'),
    'the failure branch must make "send input" the primary action');
  assert.ok(failureBranch.includes('btn-secondary') && failureBranch.includes('Resume without input'),
    'the failure branch must demote approve to a muted secondary');
  assert.ok(!failureBranch.includes('Approve &amp; Advance'),
    'the failure branch must not offer "Approve & Advance"');
  assert.ok(routineBranch.includes('btn-approve') && routineBranch.includes('Approve &amp; Advance'),
    'the routine branch must keep approve as the primary action');
  assert.ok(!routineBranch.includes('btn-secondary'),
    'the routine branch must not demote approve');
});

check('augmentViewerStateAttention stamps attention_gate on the payload', () => {
  const payload = augmentViewerStateAttention({ state: stateAt('need_input') });
  assert.strictEqual(payload.attention_gate.id, 'need_input');
  assert.strictEqual(payload.attention_gate.tier, 'failure');

  const quiet = augmentViewerStateAttention({ state: stateAt('none') });
  assert.strictEqual(quiet.attention_gate, null);

  // Malformed payloads must not throw — the viewer degrades, never 500s.
  assert.strictEqual(augmentViewerStateAttention(null), null);
  assert.strictEqual(augmentViewerStateAttention({}).attention_gate, null);
});

console.log(`viewer attention-state tests passed (${passed})`);
