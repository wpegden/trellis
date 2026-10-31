'use strict';
const fs = require('fs');
const path = require('path');
const { createHash } = require('crypto');
const { buildTrustSummary, trustSummaryText } = require('./pv_trust_summary');

// Same citation grammar as kernel/src/model_preservation.rs. The kernel owns
// verification; the exporter independently refuses incomplete attachments.
function evidenceReferences(tex) {
  const text = tex.split('\n').map(line => {
    let escaped = false;
    for (let i = 0; i < line.length; i++) {
      if (line[i] === '%' && !escaped) return line.slice(0, i);
      escaped = line[i] === '\\' && !escaped;
    }
    return line;
  }).join('\n');
  const refs = new Set();
  for (const match of text.matchAll(/\\evidenceref(?![A-Za-z])/g)) {
    const argument = text.slice(match.index + match[0].length).match(/^\s*\{([^}]*)\}/);
    if (!argument) throw new Error('evidenceref requires a braced file path');
    validateEvidencePath(argument[1]);
    refs.add(argument[1]);
  }
  return [...refs].sort();
}

function validateEvidencePath(relative) {
  if (!relative.startsWith('reference/model-evidence/') || !/^[A-Za-z0-9_./-]+$/.test(relative)
      || relative.split('/').some(part => !part || part === '.' || part === '..')) {
    throw new Error(`Invalid evidence path: ${relative}; use a normalized file path under reference/model-evidence/`);
  }
}

function evidenceBytes(repoPath, relative) {
  validateEvidencePath(relative);
  let current = repoPath;
  for (const part of relative.split('/')) {
    current = path.join(current, part);
    const stat = fs.lstatSync(current, { throwIfNoEntry: false });
    if (!stat) throw new Error(`Missing evidence file: ${relative}`);
    if (stat.isSymbolicLink()) throw new Error(`Evidence path uses a symlink: ${relative}`);
  }
  if (!fs.statSync(current).isFile()) throw new Error(`Evidence is not a file: ${relative}`);
  return fs.readFileSync(current);
}

function evidenceIndex(repoPath, goals) {
  const tablet = path.join(repoPath, 'Tablet');
  const texFiles = new Set(fs.existsSync(tablet) ? fs.readdirSync(tablet)
    .filter(f => f.endsWith('.tex')).map(f => `Tablet/${f}`) : []);
  for (const goal of goals) {
    for (const relative of [goal.positive_statement, ...(goal.positive_statements || [])]) {
      if (relative?.startsWith('Dormant/')) texFiles.add(relative.replace(/\.lean$/, '.tex'));
    }
  }
  const owners = new Map();
  for (const relative of [...texFiles].sort()) {
    for (const evidence of evidenceReferences(fs.readFileSync(path.join(repoPath, relative), 'utf8'))) {
      if (!owners.has(evidence)) owners.set(evidence, []);
      owners.get(evidence).push(relative);
    }
  }
  // A cited report may depend on sibling probe sources or recorded output.
  // Preserve the authored evidence namespace rather than guessing dependencies
  // from arbitrary prose. Empty cited_by means a supporting attachment, not a
  // new direct citation or an independently verified claim.
  const evidenceRoot = 'reference/model-evidence';
  let component = repoPath;
  for (const part of evidenceRoot.split('/')) {
    component = path.join(component, part);
    const info = fs.lstatSync(component, { throwIfNoEntry: false });
    if (!info) break;
    if (info.isSymbolicLink() || !info.isDirectory()) {
      throw new Error(`Evidence directory is not an independent directory: ${component}`);
    }
  }
  const visit = relative => {
    for (const name of fs.readdirSync(path.join(repoPath, relative)).sort()) {
      const child = `${relative}/${name}`;
      validateEvidencePath(child);
      const info = fs.lstatSync(path.join(repoPath, child));
      if (info.isSymbolicLink()) throw new Error(`Evidence path uses a symlink: ${child}`);
      if (info.isDirectory()) visit(child);
      else if (info.isFile()) {
        if (!owners.has(child)) owners.set(child, []);
      } else throw new Error(`Evidence is not a file: ${child}`);
    }
  };
  if (fs.existsSync(path.join(repoPath, evidenceRoot))) visit(evidenceRoot);
  return [...owners].sort(([a], [b]) => a.localeCompare(b)).map(([relative, cited_by]) => ({
    path: relative, cited_by,
    sha256: createHash('sha256').update(evidenceBytes(repoPath, relative)).digest('hex'),
  }));
}

function readJson(filename, fallback = {}) {
  try { return JSON.parse(fs.readFileSync(filename, 'utf8')); } catch { return fallback; }
}

function assumptionClaim(record) {
  const fields = ['id', 'axiom_name', 'name', 'classification', 'claim_class',
    'lean_statement', 'nl_statement', 'citation_locator', 'rust_justification', 'needed_by'];
  return Object.fromEntries(fields.filter(key => key in record).map(key => [key, record[key]]));
}

function conditionalReviewPackets(protocol) {
  const request = protocol.in_flight_request || {};
  // Mirror the kernel's legacy single-packet read compatibility. The issued
  // request owns the review instrument; never regenerate statements or seals.
  const packets = request.conditional_ratification_packets?.length
    ? request.conditional_ratification_packets
    : request.conditional_ratification_packet ? [request.conditional_ratification_packet] : [];
  const pending = Object.entries(protocol.trust_base?.conditional_candidates || {})
    .filter(([, candidate]) => candidate.stage === 'correspondence_pass' && !candidate.disposition);
  const ratificationGate = protocol.stage === 'HumanGate'
    && ['Advance', 'ProtectedReapproval'].includes(protocol.gate_kind);
  if (!ratificationGate) return [];
  if (pending.length || packets.length) {
    if (request.kind !== 'HumanGate' || request.gate_kind !== protocol.gate_kind
        || request.cycle !== protocol.cycle || request.phase !== protocol.phase
        || !Number.isInteger(request.id)) {
      throw new Error('Conditional review needs the current issued human gate; retry the download');
    }
    const candidates = new Map(pending);
    const seen = new Set();
    for (const packet of packets) {
      const candidate = candidates.get(packet.target_id);
      if (!candidate || seen.has(packet.target_id)
          || packet.generation !== candidate.stamped?.generation
          || !packet.seal_sha256 || packet.seal_sha256 !== candidate.sealed?.seal_sha256
          || !packet.target_lean || !packet.condition_lean || !packet.condition_informal
          || !packet.sealed_statement?.statement_lean
          || !packet.condition_inhabited_obligation?.statement_lean
          || (packet.counterexample_exclusion_obligation
            && !packet.counterexample_exclusion_obligation.statement_lean)) {
        throw new Error('Conditional review packet is missing, stale, or incomplete; retry the download');
      }
      seen.add(packet.target_id);
    }
    if (seen.size !== candidates.size) {
      throw new Error('Conditional review needs every issued ratification packet; retry the download');
    }
  }
  return packets;
}

function buildPvExpertPackage(repoPath, protocol, protectedNodes) {
  const registry = protocol.configured_challenge_targets || {};
  const authored = protocol.authored_decide_goals || {};
  const coverage = protocol.live?.coverage || {};
  const location = (name) => {
    if (!name) return '';
    for (const dir of ['Tablet', 'Dormant']) {
      const relative = `${dir}/${name}.lean`;
      if (fs.existsSync(path.join(repoPath, relative))) return relative;
    }
    return '';
  };
  const goals = [];
  const addPair = (primary, goalId) => {
    const spec = registry[primary];
    if (!spec) throw new Error(`Missing positive goal registration: ${primary}`);
    const direction = protocol.pv_live_polarity?.[primary] || 'prove';
    const liveName = direction === 'disprove' ? `${spec.name}__Refutation` : spec.name;
    goals.push({ goal_id: goalId, primary_id: primary, prose_reference: 'GOAL.md',
      positive_statement: location(spec.name), direction, live_statement: location(liveName),
      coverage: coverage[goalId] || [liveName] });
  };
  for (const [primary, goalId] of Object.entries(authored)) addPair(primary, goalId);
  const covered = new Set(goals.map(g => g.goal_id));
  for (const goalId of protocol.configured_targets || []) {
    if (!covered.has(goalId)) {
      const nodes = coverage[goalId] || [];
      goals.push({ goal_id: goalId, prose_reference: 'GOAL.md', direction: 'prove',
        positive_statements: nodes.map(location), live_statements: nodes.map(location), coverage: nodes });
    }
  }
  if (!goals.length) {
    for (const [id, spec] of Object.entries(registry)) {
      if (String(spec.kind).toLowerCase() !== 'theorem' || id.endsWith('__refutation')) continue;
      addPair(id, id);
    }
  }
  if (protocol.phase === 'Modeling') {
    for (const goal of goals) goal.direction = null;
  }
  const conditionalPackets = conditionalReviewPackets(protocol);
  for (const [index, packet] of conditionalPackets.entries()) {
    const goal = goals.find(goal => goal.primary_id === packet.target_id);
    if (!goal) throw new Error(`Conditional review target has no goal index entry: ${packet.target_id}`);
    goal.original_direction = goal.direction;
    goal.conditional_statement = `CONDITIONAL_REVIEW.md#conditional-${index + 1}`;
    goal.conditional_status = 'awaiting ratification';
    if (!protocol.conditional_coexistence_enabled) {
      goal.direction = 'conditional — awaiting ratification';
      goal.live_statement = goal.conditional_statement;
    }
    goal.conditional_target_id = packet.sealed_statement.target_id;
    goal.original_substantiveness_status = protocol.substantiveness_status?.[registry[packet.target_id]?.name] || null;
  }
  const preparation = readJson(path.join(repoPath, "modeling-input/manifest.json"));
  const supportPath = 'Tablet/model-support.json';
  const support = readJson(path.join(repoPath, supportPath), null);
  const extraction = readJson(path.join(repoPath, 'tcb_manifest.json'), null);
  const assumptions = [
    ...(extraction?.tcb_disclosure || []).filter(a => a.classification === 'rust-undermodel-assumption')
      .map(a => ({ ...assumptionClaim(a), review_status: 'approved' })),
    ...(readJson(path.join(repoPath, 'PROPOSED_ASSUMPTIONS.json')).assumptions || [])
      .filter(a => a.status === 'pending')
      .map(a => ({ ...assumptionClaim(a), review_status: 'pending' })),
  ];
  if (protocol.gate_kind === 'AssumptionReview') {
    const pending = assumptions.filter(a => a.review_status === 'pending').length;
    if (!pending || (typeof protocol.pending_under_model_assumptions === 'number'
        && pending !== protocol.pending_under_model_assumptions)) {
      throw new Error('Assumption-review export needs the complete pending premise batch; retry the download');
    }
  }
  const registeredBindings = (support?.bindings || []).map(binding => ({
    ...binding, lean_path: location(binding.node), tex_path: `Tablet/${binding.node}.tex`,
  }));
  const boundNames = new Set(registeredBindings.map(binding => binding.name));
  const currentModel = {
    binding_manifest: supportPath,
    binding_manifest_present: support !== null,
    composed_aliases: 'Tablet/Preamble.lean',
    under_model_assumptions: 'Tablet/Assumptions.lean',
    registered_bindings: registeredBindings,
    extraction_inventory_present: extraction !== null,
    extraction_boundaries_without_registered_bindings: (extraction?.tcb_disclosure || [])
      .filter(boundary => !boundNames.has(boundary.name)).map(boundary => boundary.name),
  };
  // Index the NL arguments for human review; protection remains kernel-defined.
  const tabletDir = path.join(repoPath, 'Tablet');
  const preservationReviewNodes = fs.existsSync(tabletDir)
    ? fs.readdirSync(tabletDir).filter(filename => filename.endsWith('.tex')
        && fs.readFileSync(path.join(tabletDir, filename), 'utf8').includes('\\begin{preservation}'))
      .sort().map(filename => ({ node: filename.slice(0, -4),
        tex_path: `Tablet/${filename}`, lean_path: location(filename.slice(0, -4)) }))
    : [];
  const bundle = { phase: protocol.phase, stage: protocol.stage, cycle: protocol.cycle,
    request_id: protocol.in_flight_request?.id, gate_kind: protocol.gate_kind,
    assumption_review_mid_phase: protocol.assumption_review_mid_phase === true,
    modeling_human_approved: protocol.modeling_human_approved === true,
    conditional_ratification_packets: conditionalPackets,
    // Match the kernel's mode-B advance boundary: prior modeling approval
    // abbreviates this review only when conditional ratification is the sole
    // remaining human decision. Required trust gates retain the full review.
    conditional_review_only: conditionalPackets.length > 0 && protocol.modeling_human_approved === true
      && protocol.gate_kind === 'Advance' && protocol.phase === 'TheoremStating'
      && (protocol.trust_base?.mode || 'disabled') === 'disabled'
      && (protocol.pending_under_model_assumptions || 0) === 0
      && !assumptions.some(a => a.review_status === 'pending'),
    goals, protected_nodes: protectedNodes, declined_goals: { ...(preparation.declined_goals || {}), ...(protocol.modeling_declines || {}) },
    preservation_review_nodes: preservationReviewNodes,
    evidence_files: evidenceIndex(repoPath, goals),
    current_model: currentModel, assumptions };
  bundle.trust_summary = buildTrustSummary(repoPath, protocol, bundle);
  return bundle;
}

function pvExpertReadme(bundle, protectedText) {
  const cell = value => String(value || '').replace(/\|/g, '\\|').replace(/\n/g, ' ');
  const modeling = bundle.phase === 'Modeling';
  const assumptionReview = bundle.gate_kind === 'AssumptionReview';
  const conditionalPackets = bundle.conditional_ratification_packets || [];
  const conditionalReview = conditionalPackets.length > 0;
  const phaseName = ({ Modeling: 'modeling and goal stating', TheoremStating: 'argument development',
    RevisionStating: 'revision stating', ProofFormalization: 'proof formalization' })[bundle.phase]
    || cell(bundle.phase);
  const rows = bundle.goals.map(g => {
    const positive = cell(g.positive_statement || (g.positive_statements || []).join(', '));
    const live = g.conditional_statement && g.direction !== 'conditional — awaiting ratification'
      ? `${cell(g.live_statement)}; [conditional companion (${cell(g.conditional_status)})](${g.conditional_statement})`
      : g.conditional_target_id
      ? `[Sealed proposal](${g.live_statement})`
      : cell(g.live_statement || (g.live_statements || []).join(', '));
    return modeling ? `| ${cell(g.goal_id)} | ${positive} |`
      : `| ${cell(g.goal_id)} | ${positive} | ${g.direction} | ${live} |`;
  }).join('\n');
  const goalTable = modeling ? '| Supplied goal | Positive formalization |\n| --- | --- |'
    : `| Supplied goal | Positive formalization | Direction | ${conditionalReview ? 'Obligation or pending proposal' : 'Live obligation'} |\n| --- | --- | --- | --- |`;
  const boundary = assumptionReview
    ? (bundle.assumption_review_mid_phase
      ? `Approval admits the pending premises into the trusted basis and resumes ${phaseName}.`
      : 'Approval admits the pending premises into the trusted basis, then opens the separate phase-advance review.')
    : bundle.gate_kind === 'Advance'
      ? (modeling ? 'Approval starts argument development.' : 'Approval starts Lean proof formalization.')
      : 'This snapshot supplies the artifacts for the current review.';
  const proofStatus = modeling
    ? 'GOAL proof, disproof, and truth classification are developed afterward.'
    : 'NL arguments and any completed Lean proofs are included. A disproof targets the exact negation of the supplied positive statement.';
  const preservationIndex = (bundle.preservation_review_nodes || []).map(subject =>
    `- [${cell(subject.node)}](${subject.tex_path})${subject.lean_path ? ` ([Lean](${subject.lean_path}))` : ''}`
  ).join('\n') || 'No node preservation sections were found in this snapshot; consult the source correspondence material below.';
  const model = bundle.current_model;
  const bindingRows = (model?.registered_bindings || []).map(binding =>
    `| ${cell(binding.name)} | ${cell(binding.declaration)} | ${binding.lean_path ? `[${cell(binding.node)}](${binding.lean_path})` : 'Lean file missing'} ([NL](${binding.tex_path})) |`
  ).join('\n');
  const unbound = !model?.extraction_inventory_present ? 'Extraction inventory unavailable.'
    : model.extraction_boundaries_without_registered_bindings.length
      ? model.extraction_boundaries_without_registered_bindings.map(cell).join(', ')
      : 'None.';
  const currentBindings = model?.binding_manifest_present ? `### Current model bindings

These registrations come from [Tablet/model-support.json](Tablet/model-support.json). Inspect the composed aliases in [Preamble](Tablet/Preamble.lean) and the linked definitions and preservation arguments.

| Extraction boundary | Registered declaration | Authored node |
| --- | --- | --- |
${bindingRows || '| No bindings registered | | |'}

Extraction boundaries without a registered binding: ${unbound}

The extraction-era \`tcb_manifest.json\` inventories original boundaries. \`APPROVED_AXIOMS.json\` is an allowance list, not an inventory of axioms still used by the current model. Current under-model assumptions are declared in [Assumptions](Tablet/Assumptions.lean) and listed below when present.

` : '';
  const assumptionList = status => (bundle.assumptions || [])
    .filter(a => a.review_status === status)
    .map(a => `- \`${cell(a.axiom_name || a.name)}\` — ${cell(a.claim_class || 'behavior')}`).join('\n') || 'None.';
  const assumptionsText = (bundle.assumptions || []).length ? `### Pending premises — awaiting ratification

${assumptionList('pending')}

### Previously approved premises — context

${assumptionList('approved')}

Statements, preservation arguments, and source citations are in [ASSUMPTIONS_REVIEW.md](ASSUMPTIONS_REVIEW.md) and [Tablet/Assumptions.tex](Tablet/Assumptions.tex), paired with [Lean](Tablet/Assumptions.lean).

` : '';
  const generalReviewTask = assumptionReview ? `## Premise review

Decide whether to admit each pending premise into the formalization's trusted basis. Check its Lean/NL pairing, primary citations, consistency with the existing model commitments, and whether it is the minimum adequate addition. Review its preservation argument and goal-specific applications to judge whether the resulting formalization remains convincing evidence about the intended Rust behavior.

For an environment premise, check the binding to the already selected environment against the declared scope and pinned source evidence. Executable probes provide corroboration with their stated limits. The same environment governs proofs and disproofs.

${assumptionsText}The remaining goal, protected-node, and model material supplies context for assessing these premises and their effects.
` : `${bundle.stage === 'HumanGate' ? 'Approval covers two tasks: goal meanings and model preservation.' : 'No human approval is requested at this stage. The following sections explain review criteria; they do not add ratification to this snapshot.'} ${proofStatus}

## Goal-meaning review

For each supplied GOAL, check that its covering NL statement preserves the prose in \`GOAL.md\` and that the paired positive Lean statement expresses that meaning. Supplied goals may be true or false. Check retained scope, disclosed limitations, and declined goals against the original GOALs.

Protected nodes are the nodes whose meanings must be correct for the supplied GOALs to have their intended meaning in Lean. Review their actual Lean declarations and paired NL meanings. Semantic changes to approved goal meanings use the existing renewed-review process.

## Model-preservation review

Review the NL proofs that justify the suitability of the Lean model and its correspondence to the original Rust. These arguments concern the connection to source behavior outside the formal model; Lean proofs within that model cannot certify this connection. Judge whether the arguments, selected scope, and disclosed assumptions make subsequent formalization convincing evidence about the program.

Review each preservation claim, its proof, and its applications to the supplied GOALs. Follow cited supporting nodes and source code, including material outside the protected-node list. Preamble carries setup and adaptation arguments; source comments in \`crate/\` and the notes in \`modeling-input/\` supply their supporting evidence. Compare adaptations with \`upstream/\`.

The selected program and configuration are recorded in the setup notes and extraction metadata. The paired preservation claims state the current source-correspondence domains and limitations, including restrictions on which model inputs represent source executions.
`;
  const conditionalRows = conditionalPackets.map((packet, index) => {
    const goal = bundle.goals.find(g => g.primary_id === packet.target_id);
    return `| [${cell(goal?.goal_id || packet.target_id)}](CONDITIONAL_REVIEW.md#conditional-${index + 1}) | ${cell(packet.condition_informal)} | \`${cell(packet.condition_lean)}\` |`;
  }).join('\n');
  const conditionalTask = conditionalReview ? `## Conditional ratification — the decision at this gate

Approval ratifies ${conditionalPackets.length === 1 ? 'the conditional packet below' : `all ${conditionalPackets.length} conditional packets below together`}. Review whether each Lean condition expresses its NL restriction, whether the sealed conditional statement retains the original conclusion and premises within that domain, and whether the resulting coverage is acceptable for the intended use. Check the excluded cases and the rationale against the original GOALs and source context.

${bundle.conditional_review_only ? 'Modeling approval is already recorded for the original model, scope, and goal meanings. This gate reviews the conditional changes; the goal and preservation indexes below supply context, including any later supporting material.' : 'These conditional changes are part of this gate’s broader semantic review; the goal and model review duties below still apply.'}

| Goal | Proposed restriction | Exact Lean condition |
| --- | --- | --- |
${conditionalRows}

Read [CONDITIONAL_REVIEW.md](CONDITIONAL_REVIEW.md) for each original statement, exact sealed conditional statement, rationale, and remaining proof obligations. [conditional-ratification-packets.json](conditional-ratification-packets.json) contains the complete kernel-issued packets, including their identities, frozen assumption records, and correspondence assessments. Those assessments support the human meaning review. They do not prove the conditional theorems.

Approval authorizes these restricted claims and subsequent proof work. It does not mark the unrestricted originals as valid, erase their failure evidence, prove the condition for every original input, or turn the condition into a new axiom. The conditional theorem and condition-inhabited obligation must still be proved; a counterexample-exclusion obligation must also be proved wherever the packet includes one. Prior findings remain disclosed. Packet rationales and correspondence assessments are recorded judgments, not current proof-completion certificates.

For conditional proposals, the positive-formalization column retains the original reference statement. Where an unrestricted direction is selected, the conditional companion is displayed separately and approval does not change that direction. Rows marked “conditional — awaiting ratification” link to proposals whose proof obligations become active only after approval.
` : '';
  const reviewTask = conditionalTask + (bundle.conditional_review_only ? '' : generalReviewTask);
  return `# Program-verification ${conditionalReview ? 'conditional' : assumptionReview ? 'assumption' : 'expert'} review

Snapshot: ${cell(bundle.phase)}, cycle ${bundle.cycle}, request ${bundle.request_id}, gate ${cell(bundle.gate_kind)}. ${boundary}

${reviewTask}

## Review index

### ${modeling ? 'Goal statements' : 'Goals and selected directions'}

Paths below refer to Lean declarations; the paired \`.tex\` files are alongside them. ${modeling ? 'No proof or disproof direction is selected in modeling.' : 'Dormant positive statements remain the reference when disproof is selected.'}

${goalTable}
${rows}

\`expert-review.json\` indexes goal coverage, protected nodes, preservation subjects, declined goals, model bindings, assumption claims, and any exact conditional ratification packets. [TRUST_SUMMARY.md](TRUST_SUMMARY.md) separates historical human ratification, current automated checks, formal results, and source execution evidence.

### Protected nodes

${protectedText}

### Preservation review subjects

These nodes contain preservation arguments and their goal-specific applications.

${preservationIndex}

${currentBindings}${assumptionReview ? '' : assumptionsText}## Provenance and files

- \`modeling-input/manifest.json\` contains preparation provenance, declared scope, and limitations. Prior audits, route deliberations, and support-task directives are omitted. Authored citations use original manifest line numbers; locate the named fields in this reformatted export. For native Modeling runs, \`source_sha256\` hashes the canonical complete-crate tree manifest; \`source_file\` identifies a declaration's source location. Current preservation obligations are stated in the paired Tablet files.
- \`extraction-evidence/\` and \`generated.lean\` preserve the machine-derivation evidence when available. Read \`tcb_manifest.json\` and \`APPROVED_AXIOMS.json\` together with the current bindings and assumption declarations; missing records remain missing evidence.
- The authored \`reference/model-evidence/\` tree is included verbatim, including companion probe sources and outputs used by its reports. \`expert-review.json\` records each file's hash and direct citing arguments; an empty \`cited_by\` list marks a supporting attachment, not an additional verified claim. Check what each record establishes within the preservation claim's scope.
- \`Tablet/model-support.json\` identifies authored definitions used to interpret extraction boundaries; their paired nodes and composed aliases are in \`Tablet/\`.
- \`Tablet/\` contains live node pairs. Proof-only support may evolve under the ordinary verifier machinery while approved goal meanings remain fixed.
`;
}

function conditionalReviewText(bundle) {
  const code = text => {
    const runs = String(text).match(/`+/g) || [];
    const fence = '`'.repeat(Math.max(3, ...runs.map(run => run.length + 1)));
    return `${fence}lean\n${text}\n${fence}`;
  };
  const sections = bundle.conditional_ratification_packets.map((packet, index) => {
    const goal = bundle.goals.find(g => g.primary_id === packet.target_id);
    const obligations = [packet.sealed_statement, packet.condition_inhabited_obligation,
      packet.counterexample_exclusion_obligation].filter(Boolean);
    return `<a id="conditional-${index + 1}"></a>
## ${goal?.goal_id || packet.target_id}

Original paired files: ${goal?.positive_statement || 'see original declaration below'} and its adjacent .tex file. Original substantiveness finding: ${goal?.original_substantiveness_status || 'not recorded'}.

### Original registered statement

${code(packet.target_lean)}

### Proposed condition and rationale

${packet.condition_informal}

${code(packet.condition_lean)}

Recorded proposal rationale: ${packet.rationale || 'Not recorded.'}

### Sealed proof obligations — approval does not discharge these

${obligations.map(obligation => `**${obligation.kind}** (\`${obligation.target_id}\`)

${code(obligation.statement_lean)}`).join('\n\n')}

### Exact packet identity and evidence

Target: \`${packet.target_id}\`; generation: ${packet.generation}.
Proposal: \`${packet.proposal_sha256}\`.
Seal: \`${packet.seal_sha256}\`.
The complete packet, evidence digests, correspondence assessment, and frozen assumption records are entry ${index + 1} of [conditional-ratification-packets.json](conditional-ratification-packets.json).
`;
  });
  return `# Conditional ratification packets\n\nRequest ${bundle.request_id}, cycle ${bundle.cycle}, gate ${bundle.gate_kind}. Approval ratifies this complete batch. Statements below are copied from the issued packets without rewriting Lean syntax. See README.md for the human decision and remaining verification.\n\n${sections.join('\n')}`;
}

function copyPvExpertContext(repoPath, destination, bundle) {
  const ignored = new Set(['.git', '.trellis', '.lake', 'target', '__pycache__']);
  const copy = (relative) => {
    const source = path.join(repoPath, relative);
    if (fs.existsSync(source)) fs.cpSync(source, path.join(destination, relative), {
      recursive: true, filter: p => !ignored.has(path.basename(p)),
    });
  };
  for (const relative of ['upstream', 'crate',
    'extraction-evidence', 'generated.lean', 'pv_tablet.json', 'Tablet/model-support.json']) copy(relative);
  // Preparation notes and source evidence are review artifacts. The mixed
  // preparation manifest also contains prior agent judgments, so project it.
  for (const filename of ['SETUP.md', 'extraction.json', 'goals.json', 'tool-identities.json']) {
    copy(`modeling-input/${filename}`);
  }
  const writeJson = (relative, value) => {
    const filename = path.join(destination, relative);
    fs.mkdirSync(path.dirname(filename), { recursive: true });
    fs.writeFileSync(filename, JSON.stringify(value, null, 2) + '\n');
  };
  if (bundle.conditional_ratification_packets?.length) {
    writeJson('conditional-ratification-packets.json', bundle.conditional_ratification_packets);
    fs.writeFileSync(path.join(destination, 'CONDITIONAL_REVIEW.md'), conditionalReviewText(bundle));
  }
  const preparation = readJson(path.join(repoPath, 'modeling-input/manifest.json'), null);
  if (preparation) {
    const { audits, route_decisions, support_obligations, ...provenance } = preparation;
    writeJson('modeling-input/manifest.json', { ...provenance,
      review_export: 'Preparation provenance and declared scope; prior audits, route deliberations, and support-task directives omitted.' });
  }
  const extraction = readJson(path.join(repoPath, 'tcb_manifest.json'), null);
  if (extraction) {
    writeJson('tcb_manifest.json', { ...extraction,
      tcb_disclosure: (extraction.tcb_disclosure || []).map(record =>
        record.classification === 'rust-undermodel-assumption' ? assumptionClaim(record) : record) });
  }
  if (bundle.assumptions.length) {
    const claims = bundle.assumptions.map(a => `## ${a.axiom_name || a.name}

Status: ${a.review_status === 'pending' ? 'pending — awaiting human ratification' : 'previously approved — context'}.
Claim class: \`${a.claim_class || 'behavior'}\`.

### NL statement

\`\`\`tex
${a.nl_statement || ''}
\`\`\`

### Lean statement

\`\`\`lean
${a.lean_statement || ''}
\`\`\`

### Source citation and justification

${a.citation_locator || ''}

${a.rust_justification || ''}

Needed by: ${(a.needed_by || []).join(', ') || 'not recorded'}.
`).join('\n');
    fs.writeFileSync(path.join(destination, 'ASSUMPTIONS_REVIEW.md'),
      '# Assumption claims\n\nThe pending premises are the proposed additions to the trusted basis. Previously approved premises provide context. See README.md for this gate\'s decision and review criteria.\n\n' + claims);
  }
  for (const goal of bundle.goals) {
    for (const relative of [goal.positive_statement, ...(goal.positive_statements || [])]) {
      if (relative?.startsWith('Dormant/')) {
        fs.mkdirSync(path.join(destination, 'Dormant'), { recursive: true });
        copy(relative); copy(relative.replace(/\.lean$/, '.tex'));
      }
    }
  }
  for (const evidence of bundle.evidence_files || []) {
    const bytes = evidenceBytes(repoPath, evidence.path);
    if (createHash('sha256').update(bytes).digest('hex') !== evidence.sha256) {
      throw new Error(`Evidence changed during export: ${evidence.path}; retry the download`);
    }
    const target = path.join(destination, evidence.path);
    fs.mkdirSync(path.dirname(target), { recursive: true });
    fs.writeFileSync(target, bytes);
  }
  if (JSON.stringify(evidenceIndex(destination, bundle.goals)) !== JSON.stringify(bundle.evidence_files || [])) {
    throw new Error('Evidence citations changed during export; retry the download');
  }
  for (const entry of bundle.trust_summary.reproductions.files) {
    // Recheck after indexing: an export must not substitute changed evidence.
    const source = path.join(repoPath, entry.path);
    let component = repoPath;
    for (const part of entry.path.split('/')) {
      component = path.join(component, part);
      if (fs.lstatSync(component).isSymbolicLink()) throw new Error('Reproducer evidence uses symlink');
    }
    const bytes = fs.readFileSync(source);
    if (createHash('sha256').update(bytes).digest('hex') !== entry.sha256) throw new Error('Reproducer evidence changed during export');
    const dest = path.join(destination, entry.path);
    fs.mkdirSync(path.dirname(dest), { recursive: true });
    fs.writeFileSync(dest, bytes);
  }
  writeJson('trust-summary.json', bundle.trust_summary);
  fs.writeFileSync(path.join(destination, 'TRUST_SUMMARY.md'), trustSummaryText(bundle.trust_summary));
  fs.writeFileSync(path.join(destination, 'expert-review.json'), JSON.stringify(bundle, null, 2) + '\n');
}
module.exports = { buildPvExpertPackage, pvExpertReadme, copyPvExpertContext, evidenceReferences };
