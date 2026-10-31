'use strict';
const assert = require('assert');
const fs = require('fs');
const os = require('os');
const path = require('path');
const express = require('express');
const pv = require('./pv_revision');

const root = fs.mkdtempSync(path.join(os.tmpdir(), 'pv-revision-viewer-'));
const runtime = path.join(root, 'known-runtime');
const digest = 'a'.repeat(64);
const saved = { plan: { summary: 'repair the requirement', dispositions: [{ goal_id: 'g', remedy: 'repair', reason: 'refuted' }] },
  tasks: { fix: { authorization: { title: 'Fix bounds', authorization: 'bounds only' }, status: 'pending', attempts: [] } }, attempts: [], round: 1 };
let exportPath = path.join(runtime, 'pv-revision', 'reviews', digest, 'review.zip');
let campaignExportPath = path.join(runtime, 'campaign-archives', digest + '.zip');
let campaign = { can_export_final: true, terminal_outcome: 'complete', generation_count: 2 };
let revisionPresent = true;
const pkg = { digest, aggregate_diff: { patch: '--- old\n+++ new\n' } };
fs.mkdirSync(path.join(runtime, 'pv-revision', 'reviews', digest), { recursive: true });
fs.writeFileSync(path.join(runtime, 'pv-revision', 'state.json'), JSON.stringify(saved));
fs.writeFileSync(path.join(runtime, 'pv-revision', 'reviews', digest, 'README.md'), 'Review immutable candidate\n');
fs.writeFileSync(exportPath, Buffer.from('PK archive'));
fs.mkdirSync(path.dirname(campaignExportPath), { recursive: true });
fs.writeFileSync(campaignExportPath, Buffer.from('PK campaign'));
const reads = new Map();
const controls = new Map();
const calls = [];
let transform;
let stale = false;
let deferredSchedule;
pv.install({ TRELLIS_ROOT: root, express,
  registerReadRoute: (name, handler) => reads.set(name, handler),
  registerControlRoute: (name, middleware, handler) => controls.set(name, { middleware, handler }),
  registerProjectHtmlTransform: callback => { transform = callback; },
  resolveProject: req => { if (req.params?.project === 'unknown') throw new Error('Unknown project'); return { repoPath: '/known/repo' }; },
  runtimeRootForProject: () => runtime,
  execFile: (command, args, options, callback) => {
    calls.push({ command, args, options });
    const action = args[1];
    if (action === 'decide-json') return { stdin: { on() {}, end(input) {
      calls.at(-1).input = JSON.parse(input);
      if (stale) callback(new Error('command failed'), JSON.stringify({ error: 'Human decision refers to a stale candidate' }), '');
      else callback(null, JSON.stringify({ state: 'active' }), '');
    } } };
    if (action === 'schedule') { deferredSchedule = callback; return; }
    if (action === 'decide' && stale) return callback(new Error('command failed'), JSON.stringify({ error: 'Human decision refers to a stale candidate' }), '');
    const result = action === 'campaign-status' ? campaign
      : action === 'campaign-export' ? { path: campaignExportPath, sha256: digest }
      : action === 'status' ? (revisionPresent ? { controller: 'pv_revision', state: 'awaiting_human', digest, phase: 'human_gate' } : null)
      : action === 'review' ? pkg : action === 'review-file' ? { digest, path: 'code.rs', rows: [] } : action === 'package' ? pkg : action === 'export' ? { path: exportPath, digest } : { state: args.includes('deny') ? 'denied' : 'accepted_for_modeling' };
    callback(null, JSON.stringify(result), '');
  },
});

async function invoke(handler, req = {}) {
  const result = { status: 200, headers: {} };
  const res = { status(value) { result.status = value; return this; }, set(key, value) { result.headers[key] = value; return this; },
    type(value) { result.type = value; return this; }, download(file, name) { result.body = fs.readFileSync(file); result.downloadName = name; return this; },
    json(value) { result.body = value; return this; }, send(value) { result.body = value; return this; } };
  await handler({ query: {}, params: {}, ...req }, res);
  return result;
}

(async () => {
  try {
    assert.throws(() => pv.decision({ action: 'accept', digest: '../elsewhere', reason: 'yes' }), /digest/);
    assert.throws(() => pv.decision({ action: 'amend', digest, reason: 'change' }), /amendment/);
    const status = await invoke(reads.get('pv-revision.json'));
    assert.strictEqual(status.body.revision.tasks[0].title, 'Fix bounds');
    assert.strictEqual(status.body.revision.audit_summary, saved.plan.summary);
    assert.strictEqual(pv.attention(status.body.revision).tier, 'gate');
    assert.strictEqual(pv.attention({ controller: 'preparation' }), null);
    const index = await invoke(reads.get('pv-revision/review'), { query: { digest } });
    assert.strictEqual(index.body.digest, digest);
    assert.strictEqual(index.headers['Cache-Control'], 'no-store');
    assert.strictEqual((await invoke(reads.get('pv-revision/review'), { query: { digest: 'b'.repeat(64) } })).status, 409);
    assert.strictEqual((await invoke(reads.get('pv-revision/review'))).status, 409);
    const file = await invoke(reads.get('pv-revision/file'), { query: { digest, path: 'code.rs', mode: 'after', context: '13', offset: '300' } });
    assert.strictEqual(file.body.path, 'code.rs');
    assert.deepStrictEqual(calls.at(-1).args.slice(3), ['--digest', digest, '--path', 'code.rs', '--mode', 'after', '--context', '13', '--offset', '300']);
    for (const query of [{ digest, path: ['code.rs'] }, { digest, path: 'code.rs', mode: 'worktree' }, { digest, path: 'code.rs', offset: '-1' }]) {
      assert.strictEqual((await invoke(reads.get('pv-revision/file'), { query })).status, 409);
    }
    const patch = await invoke(reads.get('pv-revision/package'), { query: { format: 'patch', digest } });
    assert.strictEqual(patch.body, pkg.aggregate_diff.patch);
    const readme = await invoke(reads.get('pv-revision/package'), { query: { format: 'readme', digest } });
    assert.match(readme.body, /immutable candidate/);
    const archive = await invoke(reads.get('pv-revision/package'), { query: { format: 'zip', digest } });
    assert.strictEqual(archive.body.toString(), 'PK archive');
    assert.strictEqual(archive.downloadName, `pv-revision-${digest}.zip`);
    assert.strictEqual((await invoke(reads.get('pv-revision/package'), { query: { format: 'zip', digest: 'b'.repeat(64) } })).status, 409);
    const goodExportPath = exportPath;
    exportPath = path.join(root, 'unrelated.zip');
    fs.writeFileSync(exportPath, 'secret');
    assert.strictEqual((await invoke(reads.get('pv-revision/package'), { query: { format: 'zip', digest } })).status, 409);
    exportPath = goodExportPath;

    revisionPresent = false;
    assert.strictEqual((await invoke(reads.get('pv-revision.json'))).body.revision, null);
    assert.strictEqual((await invoke(reads.get('pv-revision/campaign.json'))).body.campaign.generation_count, 2);
    const finalArchive = await invoke(reads.get('pv-revision/campaign.zip'));
    assert.strictEqual(finalArchive.downloadName, `pv-campaign-final-${digest}.zip`);
    assert.strictEqual(finalArchive.body.toString(), 'PK campaign');
    const preview = await invoke(reads.get('pv-revision/campaign.zip'), { query: { snapshot: '1' } });
    assert.match(preview.downloadName, /snapshot/);
    assert.ok(calls.some(call => call.args[1] === 'campaign-export' && call.args.includes('--allow-incomplete')));
    assert.strictEqual((await invoke(reads.get('pv-revision/campaign.zip'), { query: { snapshot: 'other' } })).status, 400);
    const safeCampaignPath = campaignExportPath;
    campaignExportPath = exportPath;
    assert.strictEqual((await invoke(reads.get('pv-revision/campaign.zip'))).status, 409);
    campaignExportPath = safeCampaignPath;
    revisionPresent = true;

    assert.strictEqual((await invoke(reads.get('pv-revision/package'), { query: { digest: 'b'.repeat(64) } })).status, 409);
    assert.strictEqual((await invoke(reads.get('pv-revision/package'), { query: { format: '../state.json' } })).status, 400);
    const handler = controls.get('pv-revision/decide').handler;
    assert.ok(controls.get('pv-revision/decide').middleware.length);
    for (const action of ['accept', 'deny', 'amend']) {
      for (const reason of [null, false, 3, [], {}, 'x'.repeat(20001), ...(action === 'accept' ? [] : [undefined, '', '  '])]) {
        assert.strictEqual((await invoke(handler, { body: { action, digest, reason, amendment: 'Change it' } })).status, 400);
      }
      assert.strictEqual(pv.decision({ action, digest, reason: 'x'.repeat(20000), amendment: 'Change it' }).reason.length, 20000);
    }
    for (const fields of [{}, { reason: '' }, { reason: '  ' }]) {
      assert.strictEqual(pv.decision({ action: 'accept', digest, ...fields }).reason, fields.reason || '');
    }
    stale = true;
    const refused = await invoke(handler, { body: { action: 'accept', digest } });
    assert.strictEqual(refused.status, 409);
    assert.match(refused.body.error, /stale/);
    assert.strictEqual(calls.filter(call => call.args[1] === 'schedule').length, 0);
    stale = false;
    const accepted = await invoke(handler, { body: { action: 'accept', digest, runtime: '/arbitrary' } });
    assert.strictEqual(accepted.status, 202);
    const approvalArgs = calls.filter(call => call.args[1] === 'decide').at(-1).args;
    assert.strictEqual(approvalArgs[approvalArgs.indexOf('--reason') + 1], '');
    assert.strictEqual(accepted.body.scheduled, true);
    assert.ok(deferredSchedule, 'HTTP returns while schedule callback is pending');
    assert.ok(calls.every(call => call.args[2] === runtime), 'All controller paths come from project runtime resolution');
    deferredSchedule(new Error('launch unavailable'), '', 'launch unavailable');
    assert.match((await invoke(reads.get('pv-revision.json'))).body.revision.launch.error, /unavailable/);
    const denied = await invoke(handler, { body: { action: 'deny', digest, reason: 'no' } });
    assert.strictEqual(denied.status, 200);
    assert.strictEqual(calls.filter(call => call.args[1] === 'schedule').length, 1);
    const amended = await invoke(handler, { body: { action: 'amend', digest, reason: 'tighten', amendment: 'Keep the conditional obligation' } });
    assert.strictEqual(amended.status, 202);
    assert.ok(calls.find(call => call.args.includes('--amendment')));
    const code = { path: 'code.rs', base_sha256: digest, text: 'fn main() { /* `$()` & <script> */ }\r\n' };
    const edited = await invoke(handler, { body: { action: 'amend', digest, reason: 'Use this implementation', code_edits: [code] } });
    assert.strictEqual(edited.status, 202);
    const submitted = calls.find(call => call.args[1] === 'decide-json');
    assert.deepStrictEqual(submitted.args.slice(1), ['decide-json', runtime]);
    assert.deepStrictEqual(submitted.input.code_edits, [code]);
    assert.strictEqual(submitted.input.amendment, '');
    const commentEdit = await invoke(handler, { body: { action: 'amend', digest, reason: 'Wording only', code_edits: [code], comment_only: true } });
    assert.strictEqual(commentEdit.status, 202);
    assert.strictEqual(calls.filter(call => call.args[1] === 'decide-json').at(-1).input.comment_only, true);
    for (const body of [
      { action: 'amend', comment_only: true, amendment: 'No exact edits' },
      { action: 'amend', comment_only: 'true', code_edits: [code] },
      { action: 'accept', comment_only: true },
      { action: 'accept', code_edits: [code] },
      { action: 'amend', code_edits: {} },
      { action: 'amend', code_edits: [code, code] },
      { action: 'amend', code_edits: [{ ...code, path: '../code.rs' }] },
      { action: 'amend', code_edits: [{ ...code, base_sha256: 'wrong' }] },
      { action: 'amend', code_edits: [{ ...code, text: 'x'.repeat(1024 * 1024 + 1) }] },
    ]) assert.strictEqual((await invoke(handler, { body: { digest, reason: 'requested', ...body } })).status, 400);
    const schedulesBefore = calls.filter(call => call.args[1] === 'schedule').length;
    stale = true;
    const staleEdit = await invoke(handler, { body: { action: 'amend', digest, reason: 'requested', code_edits: [code] } });
    assert.strictEqual(staleEdit.status, 409);
    assert.strictEqual(calls.filter(call => call.args[1] === 'schedule').length, schedulesBefore);
    assert.strictEqual((await invoke(reads.get('pv-revision/file'), { query: { digest, path: 'code.rs', mode: 'edit' } })).status, 200);
    assert.match(transform('<body></body>'), /pv-revision-panel/);
    assert.match(transform('<body></body>'), /controlHeaders/);
    assert.match(transform('<body></body>'), /Download final verification archive/);
    assert.match(transform('<body></body>'), /Retry final archive export/);
    const server = require('./server');
    assert.ok(server.CONTROL_ROUTE_TAILS.includes('pv-revision/decide'));
    assert.strictEqual(server.pauseAttention({}, { controller: 'pv_revision', state: 'awaiting_human' }, { halted: false }).tier, 'gate');
    assert.strictEqual(server.pauseAttention({}, { controller: 'pv_revision', state: 'awaiting_human' }, { halted: true }), null);
    assert.strictEqual(server.classifyLifecycle({ has_runtime: true, pv_revision: true, pause_state: 'awaiting_human' }).lifecycle, 'paused');
    console.log('PV revision viewer controls passed');
  } finally { fs.rmSync(root, { recursive: true, force: true }); }
})().catch(error => { console.error(error); process.exitCode = 1; });
