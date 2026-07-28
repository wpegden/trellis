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
  haltStateForRuntimeRoot,
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
    assert.ok(['fault', 'failure', 'gate'].includes(s.tier), `${id}: bad tier ${s.tier}`);
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
