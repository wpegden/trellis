'use strict';
const assert = require('assert');
const fs = require('fs');
const os = require('os');
const path = require('path');
const { buildPvExpertPackage, pvExpertReadme, copyPvExpertContext } = require('./pv_expert_package');

const root = fs.mkdtempSync(path.join(os.tmpdir(), 'trellis-expert-package-'));
const repo = path.join(root, 'repo');
fs.mkdirSync(path.join(repo, 'Tablet'), { recursive: true });
function packet(name, index) {
  const obligation = kind => ({ kind, target_id: `${name}__${kind}`, node: `${name}_${kind}`,
    statement_lean: `theorem ${name}_${kind} (s : List Nat) :\n    s.length ≤ 9007199254740993 → True := by` });
  return { target_id: `authored:${name}`, generation: 1, proposal_sha256: `proposal-${index}`,
    seal_sha256: `seal-${index}`, target_lean: `theorem ${name} (s : List Nat) : True := by`,
    condition_informal: 'List length is at most 9007199254740993.',
    condition_lean: 's.length ≤ 9007199254740993',
    rationale: 'An explicit restriction, with recorded counterexample evidence.',
    sealed_statement: obligation('conditional_proof'),
    condition_inhabited_obligation: obligation('condition_inhabited'),
    counterexample_exclusion_obligation: obligation('counterexample_excluded'),
    correspondence: { verdict: { findings: 'Recorded judgment only.' } },
    assumption_status: [{ id: 'platform', status: 'approved', record: { lean_statement: 'axiom width : True' } }],
  };
}
const packets = ['SequenceBound', 'SequenceParity'].map(packet);
// A second, non-numeric restriction has no counterexample-exclusion obligation.
packets[1].condition_informal = 'List length is even.';
packets[1].condition_lean = 's.length % 2 = 0';
packets[1].counterexample_exclusion_obligation = null;
for (const key of ['sealed_statement', 'condition_inhabited_obligation']) {
  packets[1][key].statement_lean = packets[1][key].statement_lean.replace(
    's.length ≤ 9007199254740993', packets[1].condition_lean);
}
const protocol = {
  phase: 'TheoremStating', stage: 'HumanGate', gate_kind: 'Advance', cycle: 7,
  modeling_human_approved: true,
  authored_decide_goals: {}, configured_challenge_targets: {}, substantiveness_status: {},
  trust_base: { mode: 'disabled', conditional_candidates: {} },
  in_flight_request: { id: 19, kind: 'HumanGate', gate_kind: 'Advance', phase: 'TheoremStating',
    cycle: 7, conditional_ratification_packets: packets },
};
for (const p of packets) {
  const name = p.target_id.slice('authored:'.length);
  protocol.authored_decide_goals[p.target_id] = name.toLowerCase();
  protocol.configured_challenge_targets[p.target_id] = { name, kind: 'theorem' };
  protocol.substantiveness_status[name] = 'FalseAsStated';
  protocol.trust_base.conditional_candidates[p.target_id] = {
    stage: 'correspondence_pass', stamped: { generation: 1 }, sealed: { seal_sha256: p.seal_sha256 },
  };
  fs.writeFileSync(path.join(repo, 'Tablet', `${name}.lean`), p.target_lean + '\n  sorry\n');
  fs.writeFileSync(path.join(repo, 'Tablet', `${name}.tex`), 'Original NL statement.\n');
}
const clone = value => JSON.parse(JSON.stringify(value));
try {
  const before = JSON.stringify(protocol);
  const bundle = buildPvExpertPackage(repo, protocol, []);
  const readme = pvExpertReadme(bundle, '');
  assert.strictEqual(bundle.conditional_review_only, true);
  assert.deepStrictEqual(bundle.conditional_ratification_packets, packets);
  assert.match(readme, /Approval ratifies all 2 conditional packets/);
  assert.match(readme, /Lean condition expresses its NL restriction/);
  assert.match(readme, /Modeling approval is already recorded/);
  assert.match(readme, /conditional theorem and condition-inhabited obligation must still be proved/);
  assert.ok(!readme.includes('Approval covers two tasks'));
  for (const [index, goal] of bundle.goals.entries()) {
    assert.strictEqual(goal.direction, 'conditional — awaiting ratification');
    assert.strictEqual(goal.live_statement, `CONDITIONAL_REVIEW.md#conditional-${index + 1}`);
    assert.strictEqual(goal.original_substantiveness_status, 'FalseAsStated');
  }
  const out = path.join(root, 'export');
  fs.mkdirSync(out);
  fs.cpSync(path.join(repo, 'Tablet'), path.join(out, 'Tablet'), { recursive: true });
  copyPvExpertContext(repo, out, bundle);
  assert.deepStrictEqual(JSON.parse(fs.readFileSync(path.join(out, 'conditional-ratification-packets.json'))), packets);
  assert.deepStrictEqual(JSON.parse(fs.readFileSync(path.join(out, 'expert-review.json'))).conditional_ratification_packets, packets);
  const review = fs.readFileSync(path.join(out, 'CONDITIONAL_REVIEW.md'), 'utf8');
  for (const p of packets) {
    assert.ok(review.includes(p.target_lean));
    assert.ok(review.includes(p.sealed_statement.statement_lean));
    if (p.counterexample_exclusion_obligation) {
      assert.ok(review.includes(p.counterexample_exclusion_obligation.statement_lean));
    }
    assert.ok(readme.includes(p.condition_lean));
    assert.ok(review.includes(p.condition_inhabited_obligation.statement_lean));
    assert.ok(review.includes(p.seal_sha256));
  }
  assert.strictEqual(JSON.stringify(protocol), before, 'export cannot mutate the gate or trust state');

  // Incomplete or stale instruments must not silently become general review.
  for (const corrupt of [
    p => { p.in_flight_request.conditional_ratification_packets = []; },
    p => { p.in_flight_request.conditional_ratification_packets.pop(); },
    p => { p.in_flight_request.conditional_ratification_packets[0].seal_sha256 = 'stale'; },
    p => { p.in_flight_request.conditional_ratification_packets[0].generation++; },
    p => { p.in_flight_request.conditional_ratification_packets[0].condition_lean = ''; },
    p => { p.in_flight_request.cycle--; },
    p => { p.in_flight_request.conditional_ratification_packets[0].counterexample_exclusion_obligation = {}; },
    p => { p.in_flight_request.conditional_ratification_packets[1] = clone(p.in_flight_request.conditional_ratification_packets[0]); },
  ]) {
    const bad = clone(protocol); corrupt(bad);
    assert.throws(() => buildPvExpertPackage(repo, bad, []), /Conditional review/);
  }

  // Legacy single-packet requests retain the exact issued packet too.
  const legacy = clone(protocol);
  legacy.trust_base.conditional_candidates = { [packets[0].target_id]: protocol.trust_base.conditional_candidates[packets[0].target_id] };
  delete legacy.in_flight_request.conditional_ratification_packets;
  legacy.in_flight_request.conditional_ratification_packet = packets[0];
  const single = buildPvExpertPackage(repo, legacy, []);
  assert.deepStrictEqual(single.conditional_ratification_packets, [packets[0]]);
  assert.match(pvExpertReadme(single, ''), /Approval ratifies the conditional packet below/);

  // Earlier modeling approval cannot abbreviate a required trust gate or a
  // protected reapproval; without earlier approval the full review stays.
  for (const change of [
    p => { p.trust_base.mode = 'required_v1'; },
    p => { p.modeling_human_approved = false; },
    p => { p.pending_under_model_assumptions = 1; },
    p => { p.gate_kind = p.in_flight_request.gate_kind = 'ProtectedReapproval'; },
  ]) {
    const full = clone(protocol); change(full);
    const b = buildPvExpertPackage(repo, full, []);
    assert.strictEqual(b.conditional_review_only, false);
    assert.match(pvExpertReadme(b, ''), /Approval covers two tasks/);
    assert.match(pvExpertReadme(b, ''), /Approval ratifies all 2 conditional packets/);
  }

  const coexistence = clone(protocol);
  coexistence.conditional_coexistence_enabled = true;
  coexistence.pv_live_polarity = { [packets[0].target_id]: 'disprove' };
  fs.writeFileSync(path.join(repo, 'Tablet', 'SequenceBound__Refutation.lean'), 'theorem SequenceBound__Refutation : False := by');
  const coexistenceBundle = buildPvExpertPackage(repo, coexistence, []);
  const paired = coexistenceBundle.goals[0];
  assert.strictEqual(paired.direction, 'disprove');
  assert.strictEqual(paired.live_statement, 'Tablet/SequenceBound__Refutation.lean');
  assert.strictEqual(paired.conditional_statement, 'CONDITIONAL_REVIEW.md#conditional-1');
  assert.match(pvExpertReadme(coexistenceBundle, ''), /conditional companion \(awaiting ratification\)/);

  const modeling = clone(protocol);
  modeling.phase = 'Modeling'; modeling.trust_base.conditional_candidates = {};
  modeling.in_flight_request = {};
  const modelingReadme = pvExpertReadme(buildPvExpertPackage(repo, modeling, []), '');
  assert.match(modelingReadme, /Approval starts argument development/);
  assert.match(modelingReadme, /Approval covers two tasks/);
  assert.ok(!modelingReadme.includes('Approval ratifies'));

  // A preservation report's probe sources/output must survive ordinary export
  // even when the TeX cites only the report. Do not infer new direct citations.
  const evidenceDir = path.join(repo, 'reference/model-evidence');
  fs.mkdirSync(path.join(evidenceDir, 'probes'), { recursive: true });
  const report = 'reference/model-evidence/report.txt';
  const probe = 'reference/model-evidence/probes/check.rs';
  const output = 'reference/model-evidence/probes/check.out';
  fs.appendFileSync(path.join(repo, 'Tablet/SequenceBound.tex'), `\\evidenceref{${report}}\n`);
  fs.writeFileSync(path.join(repo, report), 'Command and hash refer to probes/check.rs and check.out.\n');
  fs.writeFileSync(path.join(repo, probe), 'fn main() { println!("exact probe"); }\n');
  fs.writeFileSync(path.join(repo, output), 'exact probe\n');
  fs.writeFileSync(path.join(repo, 'reference/unrelated.txt'), 'not evidence');
  const withEvidence = buildPvExpertPackage(repo, modeling, []);
  assert.strictEqual(withEvidence.evidence_files.length, 3);
  assert.deepStrictEqual(withEvidence.evidence_files.find(f => f.path === report).cited_by, ['Tablet/SequenceBound.tex']);
  assert.deepStrictEqual(withEvidence.evidence_files.find(f => f.path === probe).cited_by, []);
  const evidenceOut = path.join(root, 'evidence-export');
  fs.mkdirSync(evidenceOut);
  fs.cpSync(path.join(repo, 'Tablet'), path.join(evidenceOut, 'Tablet'), { recursive: true });
  copyPvExpertContext(repo, evidenceOut, withEvidence);
  for (const relative of [report, probe, output]) {
    assert.deepStrictEqual(fs.readFileSync(path.join(evidenceOut, relative)), fs.readFileSync(path.join(repo, relative)));
  }
  assert.ok(!fs.existsSync(path.join(evidenceOut, 'reference/unrelated.txt')));
  assert.match(pvExpertReadme(withEvidence, ''), /companion probe sources and outputs/);
  fs.writeFileSync(path.join(repo, probe), 'changed probe');
  assert.throws(() => copyPvExpertContext(repo, evidenceOut, withEvidence), /Evidence changed during export/);
  fs.unlinkSync(path.join(repo, probe));
  fs.symlinkSync(path.join(repo, output), path.join(repo, probe));
  assert.throws(() => buildPvExpertPackage(repo, modeling, []), /symlink/);
  fs.unlinkSync(path.join(repo, probe));
  fs.unlinkSync(path.join(repo, report));
  assert.throws(() => buildPvExpertPackage(repo, modeling, []), /Missing evidence file/);
  fs.writeFileSync(path.join(repo, report), 'restored report');
  fs.renameSync(evidenceDir, evidenceDir + '-original');
  fs.symlinkSync(evidenceDir + '-original', evidenceDir);
  assert.throws(() => buildPvExpertPackage(repo, modeling, []), /independent directory/);
  fs.unlinkSync(evidenceDir);
  fs.renameSync(evidenceDir + '-original', evidenceDir);

  const assumption = clone(modeling);
  assumption.gate_kind = 'AssumptionReview'; assumption.assumption_review_mid_phase = true;
  fs.writeFileSync(path.join(repo, 'PROPOSED_ASSUMPTIONS.json'), JSON.stringify({
    assumptions: [{ status: 'pending', axiom_name: 'width', lean_statement: 'axiom width : True' }],
  }));
  const assumptionReadme = pvExpertReadme(buildPvExpertPackage(repo, assumption, []), '');
  assert.match(assumptionReadme, /Approval admits the pending premises/);
  assert.match(assumptionReadme, /## Premise review/);
  assert.ok(!assumptionReadme.includes('Approval ratifies'));
} finally {
  fs.rmSync(root, { recursive: true, force: true });
}
console.log('PV expert package conditional ratification tests passed');
