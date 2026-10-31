'use strict';
const fs = require('fs');
const path = require('path');
const { createHash } = require('crypto');
const sha = bytes => createHash('sha256').update(bytes).digest('hex');

function reproducerCatalog(repo) {
  const relative = 'reference/rust-witnesses/catalog.json';
  const read = rel => {
    if (!/^reference\/rust-witnesses\/[A-Za-z0-9_./-]+$/.test(rel)
        || rel.split('/').some(part => !part || part === '.' || part === '..')) {
      throw new Error('Invalid reproducer evidence path');
    }
    let file = repo;
    for (const component of rel.split('/')) {
      file = path.join(file, component);
      if (fs.lstatSync(file).isSymbolicLink()) throw new Error('Reproducer evidence uses symlink');
    }
    if (!fs.statSync(file).isFile()) throw new Error('Reproducer evidence is not a file');
    return fs.readFileSync(file);
  };
  if (!fs.existsSync(path.join(repo, relative))) return { cases: [], files: [] };
  const catalog = JSON.parse(read(relative));
  if (catalog.schema !== 'trellis-pv-reproducer-catalog/v1') throw new Error('Unsupported reproducer catalog');
  const files = new Map([[relative, sha(read(relative))]]);
  const ids = new Set();
  for (const item of catalog.cases) {
    if (ids.has(item.id)) throw new Error('Duplicate reproducer case');
    ids.add(item.id);
    if (!['small', 'large', 'symbolic', 'historical'].includes(item.resource_class)) throw new Error('Invalid reproducer resource class');
    const source = item.resource_class === 'small' || item.resource_class === 'large'
      ? `reference/rust-witnesses/target-${sha(item.target_id)}/witness.rs` : null;
    for (const [file, expected] of [[source, item.artifact_sha256],
      [item.supplemental_lock, item.cargo_lock_sha256],
      ...(item.evidence_files || []).map(f => [f.path, f.sha256])]) {
      if (!file) continue;
      const actual = sha(read(file));
      if (!expected || actual !== expected) throw new Error(`Reproducer evidence changed: ${file}`);
      files.set(file, actual);
    }
  }
  return { cases: catalog.cases, files: [...files].map(([path, sha256]) => ({path, sha256})),
    authority: 'An indexed execution is corroboration, not a source-bug verdict or a Lean proof receipt. Historical evidence is labeled separately.' };
}

// This is a disclosure projection, never an approval or completion authority.
// Retain distinctions even when a historical run lacks modern trust receipts.
function buildTrustSummary(repo, protocol, bundle) {
  const records = protocol.local_closure_records || {};
  const unverified = new Set(protocol.local_closure_unverified_nodes || []);
  const open = new Set(protocol.live?.open_nodes || []);
  const status = node => ({ node, certificate_recorded: !!records[node],
    awaiting_revalidation: unverified.has(node), open: open.has(node),
    kernel_axioms: records[node]?.kernel_axioms || [],
    boundary_theorems: Object.keys(records[node]?.boundary_theorems || {}),
    source_correspondence: protocol.corr_status?.[node] || 'unrecorded',
    // A retained verdict is historical until the native current fingerprint
    // matches its approved baseline. This disclosure does not infer exemptions.
    source_correspondence_current: !!protocol.live?.corr_current_fingerprints?.[node]
      && protocol.live.corr_current_fingerprints[node] === protocol.corr_approved_fingerprints?.[node],
    substantiveness: protocol.substantiveness_status?.[node] || 'unrecorded',
    soundness: protocol.sound_status?.[node] || 'unrecorded' });
  const goals = bundle.goals.map(goal => {
    const candidate = protocol.trust_base?.conditional_candidates?.[goal.primary_id];
    const spec = protocol.configured_challenge_targets?.[goal.primary_id];
    const direction = protocol.pv_live_polarity?.[goal.primary_id] || goal.direction;
    const originalNodes = spec ? [spec.name + (direction === 'disprove' ? '__Refutation' : '')] : (goal.coverage || []);
    return { goal_id: goal.goal_id, primary_id: goal.primary_id || null,
      positive_statement: goal.positive_statement || goal.positive_statements || [],
      selected_direction: protocol.pv_live_polarity?.[goal.primary_id] || goal.direction,
      original_obligations: originalNodes.map(status),
      source_bug_classification: 'Consult the cited audit and source evidence; proof direction alone supplies no classification.',
      conditional: candidate ? { stage: candidate.stage, generation: candidate.stamped?.generation,
        disposition: candidate.disposition || null,
        ratification: candidate.ratification || null,
        seal_sha256: candidate.sealed?.seal_sha256 || null } : null };
  });
  const protectedNodes = [...new Set([
    ...Object.values(protocol.approved_targets?.coverage || {}).flat(),
    ...(protocol.approved_targets?.protected_closure_nodes || []),
    ...Object.values(protocol.approved_targets?.challenge_coverage || {}).flat(),
  ])].sort();
  const inputHashes = {};
  for (const relative of ['GOAL.md', 'modeling-input/manifest.json', 'modeling-input/tool-identities.json',
    'Tablet/model-support.json', 'APPROVED_AXIOMS.json', 'tcb_manifest.json']) {
    const file = path.join(repo, relative);
    if (fs.existsSync(file) && fs.lstatSync(file).isFile()) inputHashes[relative] = sha(fs.readFileSync(file));
  }
  return { schema: 'trellis-pv-trust-summary/v1', phase: protocol.phase,
    cycle: protocol.cycle, stage: protocol.stage,
    authority: 'Disclosure only. The runtime owns approval, certificate freshness, and final completion.',
    human_review: {
      trust_mode: protocol.trust_base?.mode || 'disabled',
      modeling_approval_recorded: protocol.modeling_human_approved === true,
      current_approval_event_hash: protocol.trust_base?.current_human_approval_event_hash || null,
      advance_gate_episode_id: protocol.trust_base?.advance_gate_episode_id || null,
      protected_nodes: protectedNodes,
      modeling_approval_protection_enabled: protocol.modeling_approval_protection_enabled === true,
      protection_history: 'The current enforcement flag does not establish when protection was activated in a historical run.',
      scope: 'Only the actual human review instrument and its approved artifact version establish ratified scope. A boolean, a later report, or an automated Pass does not establish additional ratification.',
      historical_limitation: protocol.trust_base?.mode === 'required_v1' ? null
        : 'Legacy approval records remain legacy records. This export does not create a modern trust receipt or certify what historical review text the human saw.',
    },
    claims: {
      goal_faithfulness: 'The positive Lean statement and its NL meaning depend on explicit human review of that pairing and its supporting definitions. Automated correspondence checks are supporting evidence, not human ratification.',
      formal_results: 'Lean checks propositions in the stated model and under the listed axioms. Recorded local certificates do not by themselves establish transitive closure or final completion.',
      source_correspondence: 'The connection from the model to the Rust program is justified by preservation arguments and source evidence within the disclosed configuration and assumptions. Later argument edits are not retroactively human-approved.',
      execution: 'Rust tests corroborate particular executions. They neither prove the Lean theorem nor establish general model faithfulness.',
      counting: 'One source defect may cause multiple goals to fail. Count failed goals separately from distinct underlying defects.',
    },
    reproductions: reproducerCatalog(repo),
    current_inputs_sha256: inputHashes, assumptions: bundle.assumptions,
    model_bindings: bundle.current_model, preservation_arguments: bundle.preservation_review_nodes,
    evidence_files: bundle.evidence_files, goals,
  };
}
function trustSummaryText(summary) {
  const cell = value => String(value ?? '').replace(/\|/g, '\\|').replace(/[\r\n]/g, ' ');
  const rows = summary.goals.map(goal => {
    const nodes = goal.original_obligations;
    const n = nodes.filter(s => s.certificate_recorded && !s.awaiting_revalidation && !s.open).length;
    const conditional = goal.conditional ? `${goal.conditional.stage}; ${goal.conditional.ratification ? 'ratification recorded' : 'not ratified'}` : 'none';
    return `| ${cell(goal.goal_id)} | ${cell(goal.selected_direction)} | ${n}/${nodes.length} | ${cell(conditional)} |`;
  });
  return `# Trust and results\n\nSnapshot: cycle ${summary.cycle}, ${summary.phase}/${summary.stage}.\n\n${summary.authority}\n\n## Human ratification\n\nModeling approval recorded: ${summary.human_review.modeling_approval_recorded ? 'yes' : 'no'}.\n\n${summary.human_review.scope}\n\n${summary.human_review.historical_limitation || 'Consult the bound approval record and its review instrument.'}\n\n## Meaning of the evidence\n\n${Object.values(summary.claims).map(t => `- ${t}`).join('\n')}\n\n## Original and conditional results\n\n“Recorded local closure” counts certificates with no recorded open or revalidation flag. This report does not rerun the kernel freshness and dependency checks. The runtime determines completion. Original proof direction and a conditional companion are independent.\n\n| Goal | Selected original direction | Recorded local closure | Conditional companion |\n| --- | --- | --- | --- |\n${rows.join('\n')}\n\nSee [trust-summary.json](trust-summary.json) for per-obligation axioms, boundary theorems, review statuses, assumptions, model bindings, evidence links, and exact conditional ratification records. Source-bug classification requires its audit and source evidence; it is not inferred from disproof.\n`;
}
module.exports = { buildTrustSummary, trustSummaryText, reproducerCatalog };
