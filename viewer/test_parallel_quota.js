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
  // An unknown measurement is not a fully unused budget.
  write(path.join(project.runtimeRoot, 'dispatch', '16', 'repo'), [row(301, null)]);
  assert.strictEqual(quotaSummaryForProject(project).codex.weekly_budget, null);
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
  const render = require('vm').runInNewContext(`(${source})`, {
    usageShellBuilt: true, lastUsageDataKey: null,
    document: { getElementById: id => id === 'usage-status' ? status : content,
      createElement: () => ({ style: {}, innerHTML: '' }) },
    fetch: async () => ({ ok: true, json: async () => ({ counts: { quota_rows: 5 },
      quota: { codex: { ts: 1791293471 + used, weekly_budget: {
        pct_used: used, pct_left: 100 - used, pct_used_kind: 'exact' } } } }) }),
    apiPath: x => { assert.strictEqual(x, 'quota.json'); return x; },
    escHtml: x => String(x), buildBudgetPauseControl: () => ({ innerHTML: '' }),
  });
  await render();
  assert(content.children[0].innerHTML.includes('47.0%'));
  assert(content.children[0].innerHTML.includes('checked '));
  used = 54;
  await render();
  assert(content.children[0].innerHTML.includes('46.0%'));
  assert(!status.textContent.startsWith('Load failed'));
  console.log('parallel quota display refresh: OK');
}
checkRender().catch(error => { console.error(error); process.exitCode = 1; });
