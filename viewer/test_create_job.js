// Create jobs: state classification, target-page interpretation, eligibility
// rules, slug claims, and the landing-page merge.
//
// Everything here is the exported pure/reader surface of server.js's
// run-creation section (design §9): the wizard page renders these answers
// verbatim and decides nothing itself, so THIS is where the decisions are
// held. Fixture job dirs are real mkdtemp trees shaped exactly like
// scripts/trellis_create_run.sh writes them; tmux liveness and the clock are
// injected (`tmuxSessions`, `now`) so no test depends on a real tmux server.

const assert = require('assert');
const fs = require('fs');
const os = require('os');
const path = require('path');

const root = fs.mkdtempSync(path.join(os.tmpdir(), 'trellis-viewer-create-'));
process.env.PROJECTS_ROOT = root;
process.env.STATIC_OUT = path.join(root, 'static-out');

const {
  CREATE_SLUG_RE,
  CREATE_RUNNING_STATES,
  CREATE_FAILED_STATES,
  intakeUpload,
  decodeCp1252,
  classifyCreateJob,
  interpretTargetsResolution,
  createRetryEligible,
  createDeleteEligible,
  createSlugIssue,
  createJobCard,
  createJobsIndex,
  createStatusPayload,
  listCreateTemplates,
  launcherRolesFromConfigTemplate,
  launcherRoleOptions,
  CREATE_PINNED_MODELS,
  CREATE_DEFAULT_MODEL,
  CREATE_DEFAULT_EFFORT,
  createJobsRoot,
  interpretLoogleProbe,
  diffTargetsResolutions,
  parseReferenceSpec,
  referenceSpecsFromBody,
  validatedOverrides,
  interpretCreateOptions,
  createOptionsArgv,
  CREATE_OPTIONS_MODULE,
  createFailureHints,
  createDiskPreflight,
  lanesFromConfigTemplate,
  createAuthPreflight,
  createUploadsRoot,
  projectsIndex,
} = require('./server');

function writeJson(file, value) {
  fs.mkdirSync(path.dirname(file), { recursive: true });
  fs.writeFileSync(file, JSON.stringify(value, null, 2));
}

// A fixture job dir in the exact shape trellis_create_run.sh writes.
function makeJob(slug, { state, stage, phase, updated_ts, started_ts, error,
                         job = {}, resolution, log } = {}) {
  const dir = path.join(createJobsRoot(root), slug);
  fs.mkdirSync(dir, { recursive: true });
  if (state !== undefined) {
    writeJson(path.join(dir, 'status.json'), {
      slug, state, stage: stage || state, phase: phase || null,
      started_ts: started_ts || 1000, updated_ts: updated_ts || 1000,
      ...(error ? { error } : {}),
    });
  }
  writeJson(path.join(dir, 'job.json'), {
    slug,
    paper_name: 'paper.tex',
    loogle: 'off',
    repo: path.join(root, slug),
    runtime_root: path.join(root, `${slug}-runtime`),
    selected: null,
    ...job,
  });
  if (resolution) writeJson(path.join(dir, 'targets_resolution.json'), resolution);
  if (log) fs.writeFileSync(path.join(dir, 'create.log'), log);
  return dir;
}

function main() {
  // -----------------------------------------------------------------
  // 1. classifyCreateJob — the heartbeat rule (design §4, rows 22-24).
  // -----------------------------------------------------------------
  const NOW = 10_000;

  // A running phase with a live session is simply creating.
  for (const state of CREATE_RUNNING_STATES) {
    const c = classifyCreateJob({ state, updated_ts: NOW - 5, now: NOW, tmux_alive: true });
    assert.strictEqual(c.effective_state, state, state);
    assert.strictEqual(c.bucket, 'creating');
  }

  // Stale heartbeat + session absent => interrupted (rows 22-23).
  for (const state of CREATE_RUNNING_STATES) {
    const c = classifyCreateJob({ state, updated_ts: NOW - 120, now: NOW, tmux_alive: false });
    assert.strictEqual(c.effective_state, 'interrupted', `${state} must decay when stale+dead`);
    assert.strictEqual(c.bucket, 'create_failed');
    assert.match(c.reason, /stale/);
  }

  // A FRESH heartbeat with the session momentarily unseen is NOT interrupted:
  // both conditions are required, or a poll racing session startup flaps.
  const fresh = classifyCreateJob({ state: 'building', updated_ts: NOW - 5, now: NOW, tmux_alive: false });
  assert.strictEqual(fresh.effective_state, 'building');

  // A stale heartbeat with the session STILL ALIVE is not interrupted either
  // (a long stage between heartbeats; the session is the liveness authority).
  const slow = classifyCreateJob({ state: 'building', updated_ts: NOW - 120, now: NOW, tmux_alive: true });
  assert.strictEqual(slow.effective_state, 'building');

  // No heartbeat ever written + dead session: interrupted, not a crash loop.
  const never = classifyCreateJob({ state: 'resolving', updated_ts: null, now: NOW, tmux_alive: false });
  assert.strictEqual(never.effective_state, 'interrupted');

  // awaiting_targets NEVER decays: the park is process-down by design
  // (row 24 — host reboot during the wait needs no recovery at all).
  const parked = classifyCreateJob({
    state: 'awaiting_targets', updated_ts: NOW - 1_000_000, now: NOW, tmux_alive: false,
  });
  assert.strictEqual(parked.effective_state, 'awaiting_targets');
  assert.strictEqual(parked.bucket, 'creating');

  // Failed states carry their own bucket; done is done.
  for (const state of CREATE_FAILED_STATES) {
    const c = classifyCreateJob({ state, updated_ts: NOW - 5, now: NOW, tmux_alive: false });
    assert.strictEqual(c.effective_state, state);
    assert.strictEqual(c.bucket, 'create_failed');
  }
  assert.strictEqual(
    classifyCreateJob({ state: 'done', updated_ts: NOW, now: NOW, tmux_alive: false }).bucket,
    'done');

  // The threshold is honored, not hardcoded.
  const custom = classifyCreateJob({
    state: 'building', updated_ts: NOW - 30, now: NOW, tmux_alive: false, stale_secs: 10,
  });
  assert.strictEqual(custom.effective_state, 'interrupted');

  // -----------------------------------------------------------------
  // 2. interpretTargetsResolution — the target page's decisions (§5).
  // -----------------------------------------------------------------
  const resolution = {
    paper_sha256: 'abc', paper_name: 'paper.tex',
    main_result_envs: ['theorem', 'corollary'],
    candidates: [
      { key: 'thm:main', tex_label: 'thm:main', env: 'theorem', start_line: 5, end_line: 9,
        text: 'x', preselected: true, rank: 1, rank_reasons: ['r'], first_class: true },
      { key: 'lines:20-24', tex_label: null, env: 'corollary', start_line: 20, end_line: 24,
        text: 'y', preselected: false, rank: 2, rank_reasons: [], first_class: false,
        note: 'unlabeled: cannot be extended later via add-targets' },
    ],
    available_labels: ['thm:main'],
    envs_present: [{ env: 'theorem', count: 1, canonical: 'theorem', main_result: true }],
    rejected_blocks: [{ env: 'lemma', label: 'lem:k', start_line: 1, end_line: 2,
      reason_code: 'env_not_main_result', message: 'm' }],
    file_warnings: [],
    scan_truncated: null,
  };
  const view = interpretTargetsResolution(resolution);
  // Auto-selection: exactly the resolver's preselected set arrives checked.
  assert.deepStrictEqual(view.candidates.map(c => c.checked), [true, false]);
  // first_class rides through untouched — the page renders the badge from it.
  assert.deepStrictEqual(view.candidates.map(c => c.first_class), [true, false]);
  assert.strictEqual(view.zero_candidates, false);
  assert.strictEqual(view.confirm_allowed, true);
  assert.strictEqual(view.show_general_rules, false);
  assert.strictEqual(view.rejected_blocks.length, 1);

  // Zero-candidate diagnostic mode (row 7): confirm disabled; the general
  // R1-R9 guidance shows only when no rejected block explains the emptiness
  // (the §1.3 honesty boundary).
  const zeroExplained = interpretTargetsResolution({ ...resolution, candidates: [] });
  assert.strictEqual(zeroExplained.zero_candidates, true);
  assert.strictEqual(zeroExplained.confirm_allowed, false);
  assert.strictEqual(zeroExplained.show_general_rules, false, 'a rejected block explains it');
  const zeroBare = interpretTargetsResolution({ ...resolution, candidates: [], rejected_blocks: [] });
  assert.strictEqual(zeroBare.show_general_rules, true);

  // Absent env list renders as the kernel default, and junk input is null.
  assert.deepStrictEqual(
    interpretTargetsResolution({ candidates: [] }).main_result_envs, ['theorem', 'corollary']);
  assert.strictEqual(interpretTargetsResolution(null), null);
  assert.strictEqual(interpretTargetsResolution('nope'), null);

  // -----------------------------------------------------------------
  // 3. Retry / delete eligibility.
  // -----------------------------------------------------------------
  for (const s of [...CREATE_FAILED_STATES, 'interrupted']) {
    assert.strictEqual(createRetryEligible(s), true, s);
  }
  for (const s of ['awaiting_targets', 'building', 'done', 'unknown']) {
    assert.strictEqual(createRetryEligible(s), false, s);
  }
  // Delete: never while a phase runs, never after done (the repo is a run).
  assert.strictEqual(createDeleteEligible({ effective_state: 'build_failed', tmux_alive: false }), true);
  assert.strictEqual(createDeleteEligible({ effective_state: 'awaiting_targets', tmux_alive: false }), true);
  assert.strictEqual(createDeleteEligible({ effective_state: 'interrupted', tmux_alive: false }), true);
  assert.strictEqual(createDeleteEligible({ effective_state: 'building', tmux_alive: true }), false);
  assert.strictEqual(createDeleteEligible({ effective_state: 'done', tmux_alive: false }), false);

  // -----------------------------------------------------------------
  // 4. Slug claims (rows 4-5, plus the reserved-token collision).
  // -----------------------------------------------------------------
  assert.ok(CREATE_SLUG_RE.test('designs2'));
  for (const bad of ['', '..', 'a.b', '1abc', 'a b', '-x', 'x'.repeat(70), '../etc']) {
    assert.strictEqual(createSlugIssue(bad, { root, token: null }).code, 'invalid_slug', bad);
  }
  fs.mkdirSync(path.join(root, 'existingrun'), { recursive: true });
  assert.strictEqual(createSlugIssue('existingrun', { root, token: null }).code, 'slug_taken');
  makeJob('claimedjob', { state: 'awaiting_targets' });
  assert.strictEqual(createSlugIssue('claimedjob', { root, token: null }).code, 'job_exists');
  assert.strictEqual(createSlugIssue('AbCd1234', { root, token: 'AbCd1234' }).code, 'slug_reserved');
  assert.strictEqual(createSlugIssue('freshname', { root, token: 'AbCd1234' }), null);

  // -----------------------------------------------------------------
  // 5. Cards and the landing merge, from fixture trees.
  // -----------------------------------------------------------------
  const now = Math.floor(Date.now() / 1000);

  makeJob('parkjob', {
    state: 'awaiting_targets', updated_ts: now - 90_000,
    resolution: { candidates: [{ key: 'thm:a', tex_label: 'thm:a', env: 'theorem',
      start_line: 1, end_line: 2, text: 't', preselected: true, rank: 1,
      rank_reasons: [], first_class: true }] },
    log: 'phase A ran\nawaiting targets\n',
  });
  const parkCard = createJobCard('parkjob', { root, now, tmuxSessions: [] });
  assert.strictEqual(parkCard.effective_state, 'awaiting_targets');
  assert.strictEqual(parkCard.bucket, 'creating');
  assert.strictEqual(parkCard.has_resolution, true);
  assert.strictEqual(parkCard.retry_eligible, false);
  assert.strictEqual(parkCard.delete_eligible, true);

  // A building job whose session died an hour ago: interrupted card.
  makeJob('deadjob', { state: 'building', stage: 'setup', phase: 'b', updated_ts: now - 3600 });
  fs.mkdirSync(path.join(root, 'deadjob'), { recursive: true });
  fs.writeFileSync(path.join(root, 'deadjob', '.trellis-creating'), '');
  writeJson(path.join(root, 'deadjob', 'trellis.config.json'), { project: 'deadjob' });
  const deadCard = createJobCard('deadjob', { root, now, tmuxSessions: [] });
  assert.strictEqual(deadCard.effective_state, 'interrupted');
  assert.strictEqual(deadCard.retry_eligible, true);
  assert.strictEqual(deadCard.marker_present, true);

  // ...but with the session present it is simply creating.
  const liveCard = createJobCard('deadjob', {
    root, now, tmuxSessions: ['trellis-create-deadjob'],
  });
  assert.strictEqual(liveCard.effective_state, 'building');
  assert.strictEqual(liveCard.delete_eligible, false, 'no delete while a phase runs');

  // A marker-bearing repo with NO job record surfaces rather than vanishing.
  fs.mkdirSync(path.join(root, 'strayrepo'), { recursive: true });
  fs.writeFileSync(path.join(root, 'strayrepo', '.trellis-creating'), '');
  writeJson(path.join(root, 'strayrepo', 'trellis.config.json'), { project: 'strayrepo' });

  // A finished job whose marker is gone has graduated and is suppressed.
  makeJob('gradjob', { state: 'done', updated_ts: now });
  fs.mkdirSync(path.join(root, 'gradjob'), { recursive: true });
  writeJson(path.join(root, 'gradjob', 'trellis.config.json'), { project: 'gradjob' });

  const jobs = createJobsIndex({ root, now, tmuxSessions: [] });
  const bySlug = Object.fromEntries(jobs.map(j => [j.slug, j]));
  assert.ok(bySlug.parkjob, 'parked job renders');
  assert.ok(bySlug.deadjob, 'interrupted job renders');
  assert.ok(bySlug.claimedjob, 'claimed job renders');
  assert.strictEqual(bySlug.strayrepo.effective_state, 'interrupted');
  assert.strictEqual(bySlug.strayrepo.delete_eligible, true);
  assert.strictEqual(bySlug.strayrepo.retry_eligible, false, 'no job record: nothing to relaunch');
  assert.ok(!bySlug.gradjob, 'a graduated job leaves the creating list');

  // The landing index: marker-bearing repos are NOT project cards (they are
  // create_jobs), and graduated repos are ordinary projects again.
  const idx = projectsIndex({ fresh: true });
  const projectSlugs = idx.projects.map(p => p.slug);
  assert.ok(!projectSlugs.includes('deadjob'), 'a creating repo must not masquerade as a run');
  assert.ok(!projectSlugs.includes('strayrepo'));
  assert.ok(projectSlugs.includes('gradjob'), 'a graduated repo is a project');
  const idxJobs = Object.fromEntries((idx.create_jobs || []).map(j => [j.slug, j]));
  assert.ok(idxJobs.deadjob && idxJobs.parkjob && idxJobs.strayrepo);

  // -----------------------------------------------------------------
  // 6. The status payload (the wizard's poll target).
  // -----------------------------------------------------------------
  const payload = createStatusPayload('parkjob', { root, now, tmuxSessions: [] });
  assert.strictEqual(payload.exists, true);
  assert.strictEqual(payload.resolution.confirm_allowed, true);
  assert.deepStrictEqual(payload.resolution.candidates.map(c => c.checked), [true]);
  assert.ok(payload.log.lines.includes('awaiting targets'));
  assert.ok(payload.log.bytes > 0);
  assert.deepStrictEqual(createStatusPayload('nosuchjob', { root }), { slug: 'nosuchjob', exists: false });

  // -----------------------------------------------------------------
  // 7. Upload intake (rows 1-3): transcode-or-refuse, warn-don't-block.
  // -----------------------------------------------------------------
  const utf8 = intakeUpload(Buffer.from('\\begin{theorem}é\\end{theorem}', 'utf-8'));
  assert.strictEqual(utf8.ok, true);
  assert.strictEqual(utf8.transcoded_from, null);
  assert.deepStrictEqual(utf8.warnings, []);

  // cp1252 smart quote (0x93) transcodes; the é in latin-1 (0xE9) too.
  const cp1252 = intakeUpload(Buffer.from([0x5C, 0x62, 0x65, 0x67, 0x69, 0x6E, 0x93]));
  assert.strictEqual(cp1252.ok, true);
  assert.strictEqual(cp1252.transcoded_from, 'windows-1252');
  assert.ok(cp1252.text.endsWith('“'));
  assert.ok(cp1252.warnings.some(w => w.code === 'not_utf8'));

  // A cp1252-undefined slot (0x81) falls back to latin-1 — the same decode
  // order add_reference_paper.sh uses.
  const latin1 = intakeUpload(Buffer.from([0x5C, 0x62, 0x65, 0x67, 0x69, 0x6E, 0x81]));
  assert.strictEqual(latin1.ok, true);
  assert.strictEqual(latin1.transcoded_from, 'latin-1');
  assert.strictEqual(decodeCp1252(Buffer.from([0x81])), null);

  // Plausibility warns, never blocks (row 2).
  const prose = intakeUpload(Buffer.from('just some prose'));
  assert.strictEqual(prose.ok, true);
  assert.ok(prose.warnings.some(w => w.code === 'no_begin'));

  // Binary and empty are refused at the door (row 1).
  assert.strictEqual(intakeUpload(Buffer.from([0x50, 0x4B, 0x00, 0x01])).ok, false);
  assert.strictEqual(intakeUpload(Buffer.alloc(0)).ok, false);
  assert.strictEqual(intakeUpload('not a buffer').ok, false);

  // -----------------------------------------------------------------
  // 8. The loogle probe verdict, from loogle_json.sh's exit taxonomy.
  //
  // The script is the single source of truth: exit 7 is its "nothing
  // listening" branch (connection refused), any other non-zero exit its
  // "query failed or timed out" branch — the probe maps those, plus what a
  // successful response BODY says, onto the three server-side states. The
  // fourth state the design requires (not-checked-yet) is the page's own
  // initial state, so it has no server mapping to test.
  // -----------------------------------------------------------------
  // curl exit 7: nothing listens — the branch live on a loogle-less host.
  const down = interpretLoogleProbe({
    code: 7,
    stdout: '',
    stderr: 'Loogle is not reachable at 127.0.0.1:8088 (connection refused).\nNo local Loogle server is running here.',
  });
  assert.strictEqual(down.state, 'unreachable');
  assert.match(down.detail, /not reachable at 127\.0\.0\.1:8088/, 'the script\'s own message rides through');
  // ...and the fallback when the runner produced no stderr.
  assert.strictEqual(interpretLoogleProbe({ code: 7, stdout: '', stderr: '' }).state, 'unreachable');
  assert.match(interpretLoogleProbe({ code: 7, stdout: '', stderr: '' }).detail, /not reachable/);

  // Any other non-zero exit is the script's failed-or-timed-out branch.
  const timedOut = interpretLoogleProbe({
    code: 28, stdout: '', stderr: 'Loogle query failed or timed out after 4s (curl exit 28).',
  });
  assert.strictEqual(timedOut.state, 'erroring');
  assert.match(timedOut.detail, /timed out/);
  assert.strictEqual(interpretLoogleProbe({ code: 1, stdout: '', stderr: '' }).state, 'erroring');

  // Exit 0 with clean JSON: reachable and answering (hit count surfaced).
  const up = interpretLoogleProbe({
    code: 0, stdout: '{"count": 2, "hits": [{"name": "Nat"}, {"name": "Nat.succ"}]}', stderr: '',
  });
  assert.strictEqual(up.state, 'answering');
  assert.match(up.detail, /2 hits/);

  // Exit 0 but the service itself reports an error (e.g. "backend process is
  // starting up") — reachable-but-erroring, NOT answering.
  const starting = interpretLoogleProbe({
    code: 0, stdout: '{"error": "The backend process is starting up, please try again later..."}', stderr: '',
  });
  assert.strictEqual(starting.state, 'erroring');
  assert.match(starting.detail, /starting up/);

  // Exit 0 with a non-JSON body: reachable-but-erroring too.
  assert.strictEqual(
    interpretLoogleProbe({ code: 0, stdout: '<html>proxy error</html>', stderr: '' }).state,
    'erroring');

  // -----------------------------------------------------------------
  // 9. Template enumeration: only paired config+policy files, by path.
  // -----------------------------------------------------------------
  const templates = listCreateTemplates();
  assert.ok(templates.length >= 1, 'the shipped examples templates must enumerate');
  assert.ok(templates.every(t => t.path.endsWith('.config.json')));
  assert.ok(templates.some(t => t.default), 'one template is the default');

  // -----------------------------------------------------------------
  // 10. resolve_stale is a first-class failed state (row 10).
  // -----------------------------------------------------------------
  assert.ok(CREATE_FAILED_STATES.includes('resolve_stale'),
    'resolve_stale must classify as create_failed, never as generic noise');
  assert.strictEqual(createRetryEligible('resolve_stale'), true);
  assert.strictEqual(
    classifyCreateJob({ state: 'resolve_stale', updated_ts: NOW, now: NOW, tmux_alive: false }).bucket,
    'create_failed');

  // -----------------------------------------------------------------
  // 11. Candidate-set diffing across a re-resolve (§5.5 item 3, row 29).
  // -----------------------------------------------------------------
  const prevRes = {
    candidates: [
      { key: 'thm:main', tex_label: 'thm:main', env: 'theorem', start_line: 10, end_line: 14, text: 'A' },
      { key: 'thm:aux', tex_label: 'thm:aux', env: 'theorem', start_line: 20, end_line: 24, text: 'B' },
      { key: 'lines:30-34', tex_label: null, env: 'corollary', start_line: 30, end_line: 34, text: 'C' },
    ],
    rejected_blocks: [],
  };
  const nextRes = {
    candidates: [
      // unchanged
      { key: 'thm:main', tex_label: 'thm:main', env: 'theorem', start_line: 10, end_line: 14, text: 'A' },
      // moved (paper edit shifted it)
      { key: 'lines:32-36', tex_label: null, env: 'corollary', start_line: 32, end_line: 36, text: 'C' },
      // added: the widened set admits the proposition that now encloses thm:aux
      { key: 'prop:big', tex_label: 'prop:big', env: 'proposition', start_line: 18, end_line: 28, text: 'D' },
    ],
    rejected_blocks: [
      { env: 'theorem', label: 'thm:aux', start_line: 20, end_line: 24,
        reason_code: 'nested_in_candidate',
        message: 'this \\begin{theorem} sits inside the proposition block at lines 18-28; the scan jumps past a matched block, so nested statements are never seen. Move it out of that block.' },
    ],
  };
  const diff = diffTargetsResolutions(prevRes, nextRes);
  assert.strictEqual(diff.changed, true);
  // The unlabeled candidate's key changed with its lines (`lines:` keys ARE
  // their line range) — that is a remove+add pair, honestly reported as such.
  assert.deepStrictEqual(diff.added.map(c => c.key).sort(), ['lines:32-36', 'prop:big']);
  assert.deepStrictEqual(diff.removed.map(c => c.key).sort(), ['lines:30-34', 'thm:aux']);
  assert.strictEqual(diff.unchanged, 1);
  // Row 29: the disappeared candidate that a nesting rejection explains is
  // singled out, and its message names BOTH blocks.
  assert.strictEqual(diff.swallowed.length, 1);
  assert.strictEqual(diff.swallowed[0].key, 'thm:aux');
  assert.match(diff.swallowed[0].message, /thm:aux disappeared/);
  assert.match(diff.swallowed[0].message, /proposition block at lines 18-28/);
  // No prev — no diff (a first resolve is not "everything added").
  assert.strictEqual(diffTargetsResolutions(null, nextRes), null);
  assert.strictEqual(diffTargetsResolutions(prevRes, null), null);
  const noChange = diffTargetsResolutions(prevRes, prevRes);
  assert.strictEqual(noChange.changed, false);
  assert.strictEqual(noChange.swallowed.length, 0);

  // ...and the standing row-29 warning on the resolution itself: a rejected
  // block whose env IS in the main-result set would be a candidate but for
  // nesting. Confirm stays enabled — the outer block may be intended.
  const swallowedView = interpretTargetsResolution({
    main_result_envs: ['theorem', 'corollary', 'proposition'],
    candidates: nextRes.candidates,
    rejected_blocks: nextRes.rejected_blocks,
  });
  assert.strictEqual(swallowedView.nesting_warnings.length, 1);
  assert.match(swallowedView.nesting_warnings[0].message, /lines 20-24/);
  assert.match(swallowedView.nesting_warnings[0].message, /proposition block at lines 18-28/);
  assert.strictEqual(swallowedView.confirm_allowed, true, 'row 29: confirm stays enabled');
  // A lemma nested in a theorem is ordinary (outside the set): no warning.
  const benignView = interpretTargetsResolution({
    candidates: nextRes.candidates,
    rejected_blocks: [{ env: 'lemma', start_line: 5, end_line: 7,
      reason_code: 'nested_in_candidate', message: 'm' }],
  });
  assert.deepStrictEqual(benignView.nesting_warnings, []);

  // -----------------------------------------------------------------
  // 12. Reference specs: parsing, and the failure hints (rows 25/26/14).
  // -----------------------------------------------------------------
  assert.deepStrictEqual(parseReferenceSpec('refa=/x/refs/refa.tex:src-a'),
    { id: 'refa', file: '/x/refs/refa.tex', source_id: 'src-a' });
  assert.deepStrictEqual(parseReferenceSpec('refa=/x/refs/refa.tex'),
    { id: 'refa', file: '/x/refs/refa.tex', source_id: 'refa' });
  assert.strictEqual(parseReferenceSpec('no-equals'), null);
  assert.strictEqual(parseReferenceSpec(''), null);

  // The wizard's references array -> script specs (row 25 at request time).
  // Uploads live under the process-global uploads root (PROJECTS_ROOT was
  // pinned to this fixture root before the import).
  const upId = 'ab12cd34ef567890';
  const upDir = path.join(createUploadsRoot(root), upId);
  fs.mkdirSync(upDir, { recursive: true });
  fs.writeFileSync(path.join(upDir, 'paper.tex'), '\\begin{theorem}x\\end{theorem}');
  const okRefs = referenceSpecsFromBody([{ id: 'refa', upload_id: upId, source_id: 'src a' }]);
  assert.ok(!okRefs.error);
  assert.strictEqual(okRefs.specs.length, 1);
  assert.match(okRefs.specs[0], /^refa=.*paper\.tex:src a$/);
  assert.strictEqual(
    referenceSpecsFromBody([{ id: '..bad', upload_id: upId }]).error.body.error,
    'bad_reference_id');
  assert.strictEqual(
    referenceSpecsFromBody([
      { id: 'refa', upload_id: upId }, { id: 'refa', upload_id: upId },
    ]).error.body.error, 'duplicate_reference_id');
  assert.strictEqual(
    referenceSpecsFromBody([{ id: 'refa', upload_id: 'ffffffffffffffff' }]).error.body.error,
    'upload_missing');
  // keep-entries re-point at the job's already-ingested file, so editing the
  // set never re-uploads what the browser no longer holds…
  const keepJobDir = path.join(root, 'keepjob-dir');
  fs.mkdirSync(path.join(keepJobDir, 'refs'), { recursive: true });
  fs.writeFileSync(path.join(keepJobDir, 'refs', 'refa.tex'), 'kept');
  const kept = referenceSpecsFromBody(
    [{ id: 'refa', keep: true, source_id: 'src-a' }], { jobDir: keepJobDir });
  assert.ok(!kept.error);
  assert.strictEqual(kept.specs[0], `refa=${path.join(keepJobDir, 'refs', 'refa.tex')}:src-a`);
  // …but keeping something that was never ingested is refused, as is a keep
  // with no job context (a brand-new create has nothing to keep).
  assert.strictEqual(
    referenceSpecsFromBody([{ id: 'refz', keep: true }], { jobDir: keepJobDir }).error.body.error,
    'reference_not_kept');
  assert.strictEqual(
    referenceSpecsFromBody([{ id: 'refa', keep: true }]).error.body.error,
    'reference_not_kept');

  // Row 26: the immutability failure (add_reference_paper.sh's exact words)
  // is recognized and answered with the choose-a-new-id recovery.
  const immutHints = createFailureHints({
    error: 'setup_repo.sh failed — see create.log',
    log_lines: ['ERROR: /repo/paper/refs/refa.tex already exists with different content; reference papers are immutable — use a new id'],
  });
  assert.strictEqual(immutHints.length, 1);
  assert.strictEqual(immutHints[0].code, 'reference_immutable');
  assert.match(immutHints[0].message, /NEW id/);
  // Row 14: ENOSPC in the log tail names the disk, not a mystery.
  const enospcHints = createFailureHints({
    error: null,
    log_lines: ['tar: mathlib.olean: write failed: No space left on device'],
  });
  assert.strictEqual(enospcHints.length, 1);
  assert.strictEqual(enospcHints[0].code, 'disk_full');
  assert.deepStrictEqual(createFailureHints({ error: 'ordinary failure', log_lines: ['lake exited 1'] }), []);

  // -----------------------------------------------------------------
  // 13. Disk preflight (row 14): warn below the threshold, never block.
  // -----------------------------------------------------------------
  const gb = 1024 ** 3;
  const low = createDiskPreflight({ path: '/x', avail_bytes: 3 * gb }, 10);
  assert.strictEqual(low.warn, true);
  assert.match(low.message, /3\.0 GB free/);
  assert.match(low.message, /does not block/);
  const fine = createDiskPreflight({ path: '/x', avail_bytes: 50 * gb }, 10);
  assert.strictEqual(fine.warn, false);
  assert.strictEqual(fine.message, null);
  assert.strictEqual(createDiskPreflight({ path: '/x', error: 'EACCES' }, 10).known, false);

  // -----------------------------------------------------------------
  // 14. Provider auth preflight (row 16): lanes from the template, presence
  // from the credential files, exact login command in the refusal.
  // -----------------------------------------------------------------
  const cfgFixture = {
    worker: { provider: 'codex', model: 'm' },
    reviewer: { provider: 'claude' },
    verification: {
      provider: 'codex',
      soundness_agents: [{ provider: 'gemini', label: 's0' }],
    },
    workflow: { default_target: 'lean' },
  };
  const lanes = lanesFromConfigTemplate(cfgFixture);
  assert.deepStrictEqual(
    lanes.map(l => `${l.lane}=${l.provider}`).sort(),
    ['reviewer=claude', 'verification.soundness_agents[0]=gemini', 'verification=codex', 'worker=codex']);

  // The launcher role surface is derived from the model-bearing config
  // blocks, not a guessed role list. Array indices collapse so a verifier
  // pool with two agents still produces one role control.
  assert.deepStrictEqual(launcherRolesFromConfigTemplate({
    worker: { provider: 'codex', model: 'worker-model' },
    verification: {
      correspondence_agents: [
        { provider: 'codex', model: 'corr-a' },
        { provider: 'claude', model: 'corr-b' },
      ],
    },
    sidecar: { model: { provider: 'mistral', name: 'not-the-agent-model-field' } },
  }), ['worker', 'verification.correspondence_agents']);
  assert.deepStrictEqual(launcherRoleOptions().map(role => role.key), [
    'worker',
    'easy_worker',
    'hard_worker',
    'reviewer',
    'stuck_math_audit',
    'verification.correspondence_agents',
    'verification.soundness_agents',
    'verification.substantiveness_agents',
  ], 'every model-bearing role in every launchable shipped template needs a control');
  assert.deepStrictEqual(CREATE_PINNED_MODELS, [
    'gpt-5.6-luna', 'gpt-5.6-terra', 'gpt-5.6-sol',
  ]);
  assert.strictEqual(CREATE_DEFAULT_MODEL, 'gpt-5.6-sol');
  assert.strictEqual(CREATE_DEFAULT_EFFORT, 'xhigh');
  assert.ok(!CREATE_PINNED_MODELS.some(model => /astra/i.test(model)),
    'Astra is not a served pinned option');

  const templateFile = path.join(root, 'fixture-template.config.json');
  fs.writeFileSync(templateFile, JSON.stringify(cfgFixture));
  const bareHome = fs.mkdtempSync(path.join(os.tmpdir(), 'trellis-home-'));
  const noAuth = createAuthPreflight({ template: templateFile, home: bareHome });
  assert.strictEqual(noAuth.ok, false);
  assert.deepStrictEqual(noAuth.missing.map(m => m.provider).sort(), ['claude', 'codex', 'gemini']);
  const codexRow = noAuth.missing.find(m => m.provider === 'codex');
  assert.strictEqual(codexRow.login_command, 'codex', 'INSTALLATION.md: run the CLI once');
  assert.match(codexRow.message, /codex/);
  assert.match(codexRow.message, /terminal/);
  assert.ok(codexRow.lanes.includes('worker'), 'the refusal names the lanes that need it');

  // Log in (create the credential files) and re-check: the gate opens.
  fs.mkdirSync(path.join(bareHome, '.codex'), { recursive: true });
  fs.writeFileSync(path.join(bareHome, '.codex', 'auth.json'), '{}');
  fs.mkdirSync(path.join(bareHome, '.claude'), { recursive: true });
  fs.writeFileSync(path.join(bareHome, '.claude', '.credentials.json'), '{}');
  fs.mkdirSync(path.join(bareHome, '.gemini'), { recursive: true });
  fs.writeFileSync(path.join(bareHome, '.gemini', 'oauth_creds.json'), '{}');
  const withAuth = createAuthPreflight({ template: templateFile, home: bareHome });
  assert.strictEqual(withAuth.ok, true);
  assert.deepStrictEqual(withAuth.missing, []);

  // Fail-open cases: an unreadable template, and a provider the preflight
  // does not know — the gate must never block what it cannot understand.
  const unreadable = createAuthPreflight({ template: path.join(root, 'nope.json'), home: bareHome });
  assert.strictEqual(unreadable.ok, true);
  assert.match(unreadable.note, /unreadable/);
  const strangeFile = path.join(root, 'strange-template.config.json');
  fs.writeFileSync(strangeFile, JSON.stringify({ worker: { provider: 'stubprov' } }));
  const strange = createAuthPreflight({ template: strangeFile, home: bareHome });
  assert.strictEqual(strange.ok, true);
  assert.strictEqual(strange.providers[0].present, null);
  fs.rmSync(bareHome, { recursive: true, force: true });

  // -----------------------------------------------------------------
  // 15. The status payload carries the M3c surface: references, the
  // resolution diff, the rotation notice, hints, and the parked age.
  // -----------------------------------------------------------------
  makeJob('m3cjob', {
    state: 'awaiting_targets', updated_ts: now - 86_400,
    job: {
      template: templateFile,
      references: ['refa=/jobs/m3cjob/refs/refa.tex:src-a'],
    },
    resolution: nextRes,
    log: 'phase A ran\n',
  });
  const jobDir = path.join(createJobsRoot(root), 'm3cjob');
  writeJson(path.join(jobDir, 'targets_resolution.prev.json'), prevRes);
  fs.writeFileSync(path.join(jobDir, 'create.log.1'), 'old rotated output\n');
  const m3cPayload = createStatusPayload('m3cjob', { root, now, tmuxSessions: [] });
  assert.deepStrictEqual(m3cPayload.references, [{ id: 'refa', source_id: 'src-a' }]);
  assert.strictEqual(m3cPayload.resolution_diff.changed, true);
  assert.strictEqual(m3cPayload.resolution_diff.swallowed[0].key, 'thm:aux');
  assert.strictEqual(m3cPayload.log.rotated, true);
  assert.match(m3cPayload.log.notice, /create\.log\.1/);
  assert.strictEqual(m3cPayload.parked_secs, 86_400, 'row 9: the card says how long the park has lasted');
  assert.ok(m3cPayload.auth_preflight, 'the target page needs the auth verdict');
  assert.ok(m3cPayload.disk_preflight.known, 'the target page needs the disk verdict');
  assert.ok(Array.isArray(m3cPayload.hints));

  // A failed build whose log names the immutability rule surfaces the hint.
  makeJob('immutjob', {
    state: 'build_failed', stage: 'setup', phase: 'b', updated_ts: now,
    error: 'setup_repo.sh failed — see create.log',
    log: 'ERROR: refs/refa.tex already exists with different content; reference papers are immutable — use a new id\n',
  });
  const immutPayload = createStatusPayload('immutjob', { root, now, tmuxSessions: [] });
  assert.ok(immutPayload.hints.some(h => h.code === 'reference_immutable'));
  assert.strictEqual(immutPayload.parked_secs, null, 'only the park has a parked age');

  // --- template overrides (per-role model/effort, grunts, remote_url) ----
  // Blank/absent API fields still mean "inherit" for compatibility. The
  // browser sends an explicit model+effort object for every derived role.
  assert.deepStrictEqual(validatedOverrides({}), {}, 'empty body inherits the template');
  assert.deepStrictEqual(
    validatedOverrides({ model: '', effort: '', grunts: '', remote_url: '' }), {},
    'blank fields inherit the template');
  assert.deepStrictEqual(
    validatedOverrides({ model: 'gpt-5.6-sol', effort: 'xhigh' }),
    { model: 'gpt-5.6-sol', effort: 'xhigh' });
  const perRole = {
    worker: { model: 'gpt-5.6-luna', effort: 'high' },
    'verification.soundness_agents': { model: 'gpt-99-unpriced-preview', effort: 'ultra_v2' },
  };
  assert.deepStrictEqual(validatedOverrides({ role_overrides: perRole }), {
    role_overrides: perRole,
  }, 'well-formed unlisted model/effort values survive per-role validation');
  assert.throws(
    () => validatedOverrides({ role_overrides: { invented_role: { model: 'gpt-5.6-sol' } } }),
    /unknown launcher role/);
  assert.throws(
    () => validatedOverrides({ role_overrides: { worker: { model: 'gpt\/5.6' } } }),
    /role_overrides\.worker\.model must match/);
  assert.throws(
    () => validatedOverrides({ role_overrides: { worker: { effort: 'XHIGH' } } }),
    /role_overrides\.worker\.effort must match/);
  // grunts: false / 'off' / 0 all disable; a positive int enables the pool.
  for (const off of [false, 'off', 0, '0']) {
    assert.deepStrictEqual(validatedOverrides({ grunts: off }), { grunts: 'off' },
      `grunts ${JSON.stringify(off)} disables the sidecar`);
  }
  assert.deepStrictEqual(validatedOverrides({ grunts: 3 }), { grunts: '3' });
  assert.deepStrictEqual(validatedOverrides({ grunts: '2' }), { grunts: '2' });
  for (const bad of [-1, 100, 1.5, 'two']) {
    assert.throws(() => validatedOverrides({ grunts: bad }), /grunts must be/,
      `grunts ${JSON.stringify(bad)} refused`);
  }
  // remote_url ends up in a config the supervisor hands to git, so shell and
  // newline payloads must never survive validation.
  assert.deepStrictEqual(
    validatedOverrides({ remote_url: 'git@github.com:wpegden/x_trellis.git' }),
    { remote_url: 'git@github.com:wpegden/x_trellis.git' });
  assert.deepStrictEqual(
    validatedOverrides({ remote_url: 'https://github.com/wpegden/x' }),
    { remote_url: 'https://github.com/wpegden/x' });
  for (const bad of ['git@h:o/r.git; rm -rf /', 'https://github.com/a/b\nevil', 'not a url', '$(whoami)']) {
    assert.throws(() => validatedOverrides({ remote_url: bad }), /remote_url must be/,
      `remote_url ${JSON.stringify(bad)} refused`);
  }
  assert.throws(() => validatedOverrides({ model: 'gpt/5.6' }), /model must match/);
  assert.throws(() => validatedOverrides({ effort: 'XHIGH' }), /effort must match/);

  // -----------------------------------------------------------------
  // 16. create-options: the derived model/effort suggestion lists.
  // -----------------------------------------------------------------
  // The default derivation is the python module — the endpoint's single hop
  // to the pricing tables and the effort registry (which the pytest drift
  // guard, tests/test_create_options.py, pins to those sources). Repointing
  // it anywhere else must be a deliberate, test-visible change.
  delete process.env.TRELLIS_CREATE_OPTIONS_CMD;
  assert.deepStrictEqual(createOptionsArgv(), ['python3', '-m', 'trellis.create_options']);
  assert.strictEqual(CREATE_OPTIONS_MODULE, 'trellis.create_options');
  process.env.TRELLIS_CREATE_OPTIONS_CMD = 'bash /x/stub.sh --flag';
  assert.deepStrictEqual(createOptionsArgv(), ['bash', '/x/stub.sh', '--flag']);
  delete process.env.TRELLIS_CREATE_OPTIONS_CMD;

  // Shaping is structural only: rows ride through verbatim, a single
  // `provider` folds into `providers`, valueless rows are dropped, and an
  // unparseable document throws (the route answers 502). No value-level
  // filtering — the viewer relays the source faithfully.
  const shaped = interpretCreateOptions(JSON.stringify({
    options_version: 1,
    models: [
      { value: 'gpt-5.6-sol', provider: 'codex', label: 'codex · $5/$30 per Mtok in/out' },
      { value: '', provider: 'codex' },
      { provider: 'codex' },
      'junk',
    ],
    efforts: [{ value: 'high', providers: ['codex', 'claude'], label: 'codex, claude' }],
    sources: { models: 'pricing tables', efforts: 'KNOWN_EFFORTS' },
  }));
  assert.deepStrictEqual(shaped.models, [
    { value: 'gpt-5.6-sol', providers: ['codex'], label: 'codex · $5/$30 per Mtok in/out' },
  ]);
  assert.deepStrictEqual(shaped.efforts, [
    { value: 'high', providers: ['codex', 'claude'], label: 'codex, claude' },
  ]);
  assert.strictEqual(shaped.sources.models, 'pricing tables');
  assert.throws(() => interpretCreateOptions('not json'));
  assert.throws(() => interpretCreateOptions('[]'));
  assert.deepStrictEqual(interpretCreateOptions('{}').models, [],
    'absent lists shape to empty — the page then simply offers nothing');

  // THE LOAD-BEARING PROPERTY: the lists inform, they never gate. Run the
  // REAL derivation (the exact argv the endpoint uses) and pick values in
  // neither list — a usable-but-unpriced model, a brand-new provider tier.
  // Both must pass override validation unchanged; otherwise a stale
  // suggestion list becomes "I cannot start the run I want", the exact
  // drift the derived lists exist to remove.
  const realArgv = createOptionsArgv();
  const derived = require('child_process').spawnSync(realArgv[0], realArgv.slice(1),
    { cwd: path.resolve(__dirname, '..'), encoding: 'utf-8', timeout: 60000 });
  assert.strictEqual(derived.status, 0,
    `create-options derivation failed: ${derived.stderr || (derived.error && derived.error.message)}`);
  const real = interpretCreateOptions(derived.stdout);
  assert.ok(real.models.length >= 1 && real.efforts.length >= 1,
    'the real derivation offers something');
  const unlisted = { model: 'gpt-99-unpriced-preview', effort: 'ultra_v2' };
  assert.ok(!real.models.some(m => m.value === unlisted.model));
  assert.ok(!real.efforts.some(e => e.value === unlisted.effort));
  assert.deepStrictEqual(validatedOverrides(unlisted), unlisted,
    'an unlisted but well-formed value must survive validation untouched');

  // Browser contract: pinned models lead, the live derived list follows,
  // duplicates do not recur, and every model/effort select gets "other…".
  // Lift the pure merge helper out of the inline client without a DOM.
  const landing = fs.readFileSync(path.join(__dirname, 'public', 'landing.html'), 'utf-8');
  const choiceStart = landing.indexOf('function createChoiceRows(');
  const choiceEnd = landing.indexOf('\nfunction createChoiceControl(', choiceStart);
  assert.ok(choiceStart >= 0 && choiceEnd > choiceStart, 'landing page defines createChoiceRows');
  // eslint-disable-next-line no-new-func
  const createChoiceRows = new Function(
    `${landing.slice(choiceStart, choiceEnd)}\nreturn createChoiceRows;`,
  )();
  const mergedModels = createChoiceRows(CREATE_PINNED_MODELS, real.models);
  assert.deepStrictEqual(
    mergedModels.slice(0, CREATE_PINNED_MODELS.length).map(row => row.value),
    CREATE_PINNED_MODELS,
    'the three operator-pinned models lead every model dropdown');
  assert.strictEqual(new Set(mergedModels.map(row => row.value)).size, mergedModels.length,
    'pricing rows already pinned at the top are deduplicated');
  for (const row of real.models) {
    assert.ok(mergedModels.some(option => option.value === row.value),
      `derived model ${row.value} must reach the dropdown`);
  }
  assert.ok(!landing.includes('<datalist'), 'the launcher no longer uses free-text datalist combos');
  assert.ok(landing.includes("el('option', null, 'other…')"),
    'model and effort selects share the required other/freeform escape hatch');
  assert.ok(landing.includes('function createRunSettingsComponent('),
    'the landing page defines the shared run-settings component');
  assert.ok(landing.includes('const settings = createRunSettingsComponent('),
    'the math new-run wizard renders the shared run-settings component');
  assert.ok(landing.includes('...selectedSettings,'),
    'the new-run POST carries the settings component wire shape');

  fs.rmSync(root, { recursive: true, force: true });
  console.log('create job tests passed');
}

main();
