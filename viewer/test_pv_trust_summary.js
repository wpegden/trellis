'use strict';
const assert = require('assert');
const fs = require('fs');
const os = require('os');
const path = require('path');
const { buildTrustSummary, trustSummaryText } = require('./pv_trust_summary');
const repo = fs.mkdtempSync(path.join(os.tmpdir(), 'trellis-trust-summary-'));
try {
  const protocol = {phase: 'ProofFormalization', cycle: 10, modeling_human_approved: true,
    trust_base: {mode: 'disabled', conditional_candidates: {g: {stage: 'closed', ratification: {approval_record_sha256: 'receipt'}}}},
    configured_challenge_targets: {g: {name: 'G'}}, pv_live_polarity: {g:'disprove'},
    local_closure_records: {G__Refutation: {kernel_axioms: ['PlatformWidth'], boundary_theorems: {Dependency: 'hash'}}},
    local_closure_unverified_nodes: ['G__Refutation'], live: {open_nodes: []}};
  const bundle = {goals: [{goal_id: 'goal', primary_id: 'g', coverage: ['ConditionalG']}], assumptions: []};
  const before = JSON.stringify(protocol);
  const summary = buildTrustSummary(repo, protocol, bundle);
  assert.strictEqual(JSON.stringify(protocol), before);
  assert.strictEqual(summary.human_review.current_approval_event_hash, null);
  assert.match(summary.human_review.historical_limitation, /does not create/);
  assert.strictEqual(summary.goals[0].original_obligations[0].node, 'G__Refutation');
  assert.strictEqual(summary.goals[0].conditional.stage, 'closed');
  assert.deepStrictEqual(summary.goals[0].original_obligations[0].kernel_axioms, ['PlatformWidth']);
  protocol.corr_status = {G__Refutation: 'Pass'};
  protocol.corr_approved_fingerprints = {G__Refutation: 'old'};
  const corr = () => buildTrustSummary(repo, protocol, bundle).goals[0].original_obligations[0];
  assert.strictEqual(corr().source_correspondence, 'Pass');
  assert.strictEqual(corr().source_correspondence_current, false);
  protocol.live.corr_current_fingerprints = {G__Refutation: 'changed'};
  assert.strictEqual(corr().source_correspondence_current, false);
  protocol.live.corr_current_fingerprints.G__Refutation = 'old';
  assert.strictEqual(corr().source_correspondence_current, true);
  const text = trustSummaryText(summary);
  assert.match(text, /\| goal \| disprove \| 0\/1 \| closed; ratification recorded/);
  assert.match(text, /One source defect may cause multiple goals/);
  assert.match(text, /does not rerun the kernel freshness/);
  protocol.trust_base.mode = 'required_v1';
  assert.strictEqual(buildTrustSummary(repo, protocol, bundle).human_review.historical_limitation, null);
} finally {fs.rmSync(repo, {recursive: true, force: true});}
console.log('PV trust summary tests passed');

const { reproducerCatalog } = require('./pv_trust_summary');
const { createHash } = require('crypto');
const sha = bytes => createHash('sha256').update(bytes).digest('hex');
const fixture = fs.mkdtempSync(path.join(os.tmpdir(), 'trellis-repro-export-'));
try {
  const folder = path.join(fixture, 'reference/rust-witnesses');
  const relative = `target-${sha('g')}/witness.rs`;
  fs.mkdirSync(path.join(folder, path.dirname(relative)), {recursive:true});
  const source = '#[test] fn witness() {}';
  fs.writeFileSync(path.join(folder, relative), source);
  fs.writeFileSync(path.join(folder, 'catalog.json'), JSON.stringify({schema:'trellis-pv-reproducer-catalog/v1',cases:[
    {id:'shared',target_id:'g',goals:['g','g2'],resource_class:'small',artifact_sha256:sha(source)},
    {id:'symbolic',resource_class:'symbolic'}]}));
  assert.strictEqual(reproducerCatalog(fixture).cases[0].goals.length, 2);
  fs.writeFileSync(path.join(folder, relative), 'changed');
  assert.throws(() => reproducerCatalog(fixture), /evidence changed/);
  fs.unlinkSync(path.join(folder, relative));
  fs.symlinkSync('/etc/passwd', path.join(folder, relative));
  assert.throws(() => reproducerCatalog(fixture), /symlink/);
} finally { fs.rmSync(fixture, {recursive:true,force:true}); }
