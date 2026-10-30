'use strict';

const assert = require('assert');
const fs = require('fs');
const os = require('os');
const path = require('path');
const root = fs.mkdtempSync(path.join(os.tmpdir(), 'trellis-parallel-quota-'));
process.env.PROJECTS_ROOT = root;
const { app, quotaSummaryForProject, buildUsageRollup, pauseBudgetSummary } = require('./server');
const project = { slug: 'quota', repoPath: path.join(root, 'repo'), runtimeRoot: path.join(root, 'runtime') };
const row = (ts, pct_used) => ({ provider: 'codex', ts, ok: true,
  windows: [{ name: 'weekly', pct_used, pct_used_kind: 'exact' }] });
function write(repo, rows) {
  const file = path.join(repo, '.trellis/logs/quota-snapshots.jsonl');
  fs.mkdirSync(path.dirname(file), { recursive: true });
  fs.writeFileSync(file, rows.map(r => JSON.stringify(r)).join('\n') + '\n');
  return file;
}

try {
  write(project.repoPath, [row(100, 88)]);
  assert.strictEqual(quotaSummaryForProject(project).codex.weekly_budget.pct_left, 12);
  for (const [i, attempt] of ['16', 'review-832', 'verifier-8', 'exclusive-830'].entries()) {
    write(path.join(project.runtimeRoot, 'dispatch', attempt, 'repo'), [row(200 + i, 53 - i)]);
  }
  // Unrelated trees and aliases must never be scanned, even when newer.
  write(path.join(project.runtimeRoot, 'dispatch', 'compiler-view', 'repo'), [row(999, 99)]);
  write(path.join(root, 'other'), [row(1000, 99)]);
  fs.symlinkSync(path.join(root, 'other'), path.join(project.runtimeRoot, 'dispatch', '99'));
  const latest = quotaSummaryForProject(project).codex;
  assert.strictEqual(latest.ts, 203);
  assert.strictEqual(latest.weekly_budget.pct_left, 50);
  assert.deepStrictEqual(buildUsageRollup(project).quota.codex.weekly_budget, latest.weekly_budget);
  assert.deepStrictEqual(pauseBudgetSummary(project).codex, latest.weekly_budget);

  // Updates in a worker ledger are visible without touching the canonical log.
  const file = write(path.join(project.runtimeRoot, 'dispatch', '16', 'repo'), [row(300, 55)]);
  fs.appendFileSync(file, '{"provider":"codex","ts":');
  assert.strictEqual(quotaSummaryForProject(project).codex.weekly_budget.pct_left, 45);
  assert.strictEqual(buildUsageRollup(project).quota.codex.weekly_budget.pct_left, 45);
  fs.writeFileSync(path.join(project.repoPath, 'trellis.config.json'), '{}');
  fs.writeFileSync(path.join(project.runtimeRoot, 'runtime_metadata.json'), JSON.stringify({ repo_path: project.repoPath }));
  const route = app.router.stack.find(layer => Array.isArray(layer.route?.path)
    && layer.route.path.includes('/trellis/:project/api/quota.json')).route;
  for (const params of [{ project: 'repo' }, {}]) {
    let response;
    route.stack[0].handle({ params }, { json: value => { response = value; },
      status: code => { throw Error(`quota HTTP ${code}`); } });
    assert.deepStrictEqual(Object.keys(response), ['quota']);
    assert.strictEqual(response.quota.codex.weekly_budget.pct_left, 45);
  }
  // A failed probe remains the latest status, while the previous weekly
  // reading remains visible with its own measurement time.
  write(path.join(project.runtimeRoot, 'dispatch', '16', 'repo'), [
    row(300, 55), { provider: 'codex', ts: '301', ok: false, error: 'timeout' },
    { provider: 'codex', ts: 'bad', ok: true, windows: [{ name: 'weekly', pct_used: 99 }] },
  ]);
  const failed = quotaSummaryForProject(project).codex;
  assert.strictEqual(failed.ts, '301');
  assert.strictEqual(failed.ok, false);
  assert.strictEqual(failed.error, 'timeout');
  assert.strictEqual(failed.weekly_budget.pct_left, 45);
  assert.strictEqual(failed.weekly_budget.observed_ts, 300);
  assert.strictEqual(failed.weekly_budget.stale, true);
  assert.deepStrictEqual(buildUsageRollup(project).quota.codex.weekly_budget, failed.weekly_budget);
  assert.deepStrictEqual(pauseBudgetSummary(project).codex, failed.weekly_budget);

  // Even a successful probe without a usable weekly window must not erase
  // the last measured budget. Other attempt paths remain eligible too.
  write(path.join(project.runtimeRoot, 'dispatch', '16', 'repo'), [row(300, 55), row(302, null)]);
  const missing = quotaSummaryForProject(project).codex;
  assert.strictEqual(missing.ts, 302);
  assert.strictEqual(missing.weekly_budget.observed_ts, 300);
  assert.strictEqual(missing.weekly_budget.stale, true);
  write(path.join(project.runtimeRoot, 'dispatch', 'verifier-8', 'repo'), [row(303, 25)]);
  assert.strictEqual(quotaSummaryForProject(project).codex.weekly_budget.pct_left, 75);

  // Numeric strings are valid times; booleans, blanks, and malformed values
  // cannot replace a reading. Equal times use the main log first.
  const gemini = (ts, account, used, ok = true) => ({ provider: 'gemini', ts, account, ok,
    windows: used == null ? [] : [{ name: 'weekly', pct_used: used }] });
  write(project.repoPath, [gemini('500', 'alpha', 20), gemini(true, 'alpha', 99),
    gemini(' ', 'alpha', 99), gemini(-1, 'alpha', 99), gemini('oops', 'alpha', 99)]);
  write(path.join(project.runtimeRoot, 'dispatch', '16', 'repo'), [gemini(500, 'alpha', 90)]);
  assert.strictEqual(quotaSummaryForProject(project).gemini.weekly_budget.pct_used, 20);

  // Account switches must not attach the old account's percentage to the new
  // one. If the provider later switches back, its own earlier reading applies.
  write(path.join(project.runtimeRoot, 'dispatch', '16', 'repo'), [
    gemini(501, 'beta', null, false), gemini(502, 'alpha', null, false),
  ]);
  const switchedBack = quotaSummaryForProject(project).gemini;
  assert.strictEqual(switchedBack.weekly_budget.pct_used, 20);
  assert.strictEqual(switchedBack.weekly_budget.source_account, 'alpha');
  assert.strictEqual(switchedBack.weekly_budget.stale, true);
  write(path.join(project.runtimeRoot, 'dispatch', '16', 'repo'), [gemini(503, 'beta', null, false)]);
  assert.strictEqual(quotaSummaryForProject(project).gemini.weekly_budget, null);
  write(path.join(project.runtimeRoot, 'dispatch', '16', 'repo'), [
    gemini(503, 'beta', null, false), { provider: 'gemini', ts: 504, ok: false, error: 'timeout' },
  ]);
  assert.strictEqual(quotaSummaryForProject(project).gemini.ts, 504);
  assert.strictEqual(quotaSummaryForProject(project).gemini.weekly_budget, null);
  write(path.join(project.runtimeRoot, 'dispatch', '16', 'repo'), [
    gemini(503, 'beta', null, false), gemini(505, null, 31),
  ]);
  assert.strictEqual(quotaSummaryForProject(project).gemini.weekly_budget.pct_used, 31);

  // A provider with no usable weekly measurement has no budget headline.
  write(project.repoPath, [{ provider: 'claude', ts: 304, ok: false, error: 'offline' }]);
  assert.strictEqual(quotaSummaryForProject(project).claude.weekly_budget, null);
  const reset = Math.floor(Date.now() / 1000) - 10;
  write(project.repoPath, [
    { provider: 'claude', ts: reset - 100, ok: true, windows: [
      { name: 'weekly', pct_used: 81, resets_at: reset, resets_at_repr: 'past reset' }] },
    { provider: 'claude', ts: reset + 1, ok: false, error: 'offline' },
  ]);
  const expired = quotaSummaryForProject(project).claude.weekly_budget;
  assert.strictEqual(expired.pct_left, 19);
  assert.strictEqual(expired.expired, true);
  assert.strictEqual(expired.stale, true);
  console.log('parallel quota snapshots: OK');
} finally {
  fs.rmSync(root, { recursive: true, force: true });
}

// The browser must redraw when only a private attempt's reading changes.
async function checkRender() {
  const page = fs.readFileSync(path.join(__dirname, 'public/index.html'), 'utf8');
  const source = page.slice(page.indexOf('async function renderUsageDetail()'), page.indexOf('function usageTable('));
  const content = { children: [], set innerHTML(_) { this.children = []; }, appendChild(c) { this.children.push(c); } };
  const status = { textContent: '' };
  let used = 53;
  let failedProbe = false;
  const render = require('vm').runInNewContext(`(${source})`, {
    usageShellBuilt: true, lastUsageDataKey: null,
    document: { getElementById: id => id === 'usage-status' ? status : content,
      createElement: () => ({ style: {}, innerHTML: '' }) },
    fetch: async () => ({ ok: true, json: async () => ({ counts: { quota_rows: 5 },
      quota: { codex: { ts: 1791293471 + used, ok: !failedProbe, weekly_budget: {
        pct_used: used, pct_left: 100 - used, pct_used_kind: 'exact',
        observed_ts: 1791293471, stale: failedProbe } } } }) }),
    apiPath: x => { assert.strictEqual(x, 'quota.json'); return x; },
    escHtml: x => String(x), buildBudgetPauseControl: () => ({ innerHTML: '' }),
  });
  await render();
  assert(content.children[0].innerHTML.includes('47.0%'));
  assert(content.children[0].innerHTML.includes('measured '));
  used = 54;
  failedProbe = true;
  await render();
  assert(content.children[0].innerHTML.includes('46.0%'));
  assert(content.children[0].innerHTML.includes('stale reading'));
  assert(content.children[0].innerHTML.includes('latest probe failed'));
  assert(!status.textContent.startsWith('Load failed'));
  console.log('parallel quota display refresh: OK');
}
checkRender().catch(error => { console.error(error); process.exitCode = 1; });
