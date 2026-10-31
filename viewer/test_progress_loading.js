const assert = require('assert');
const fs = require('fs');
const path = require('path');
const vm = require('vm');

async function main() {
  const html = fs.readFileSync(path.join(__dirname, 'public/index.html'), 'utf8');
  const source = html.match(/\nasync function renderProgressDetail\([^)]*\) \{[\s\S]*?\n\}/)[0];
  const elements = new Map();
  for (const id of ['detail-content', 'progress-charts', 'progress-status',
    'progress-phase-view-select', 'progress-phase-view-note']) {
    elements.set(id, { style: {}, textContent: '', children: [],
      set innerHTML(value) { this.children = []; },
      appendChild(child) { this.children.push(child); }, querySelector() { return null; } });
  }
  let payload;
  const context = vm.createContext({
    document: { getElementById: id => elements.get(id) },
    progressShellBuilt: false, lastProgressDataKey: null,
    progressPhaseView: 'all', includeDefs: true,
    apiPath: x => x, latestProgressPhase: cps => cps.at(-1)?.phase || '',
    progressPhaseLabel: x => x, buildProgressChartCard: (...args) => args,
    fetch: async () => ({ ok: true, json: async () => payload }),
  });
  vm.runInContext(source, context);
  const render = async data => { payload = data; await context.renderProgressDetail(); };
  const status = elements.get('progress-status');
  const charts = elements.get('progress-charts');

  await render({ checkpoints: [], building: true });
  assert.match(status.textContent, /Loading progress history/);
  assert.doesNotMatch(status.textContent, /0 of 0/);
  assert.strictEqual(charts.children.length, 0);

  const checkpoints = [{ sha: 'checkpoint', cycle: 10, phase: 'Proof' }];
  await render({ checkpoints, building: true, stale: true });
  assert.match(status.textContent, /1 of 1 checkpoints/);
  assert.match(status.textContent, /Refreshing progress history/);
  assert.strictEqual(charts.children.length, 4);

  // Completion must repaint even if the checkpoint count and final SHA match.
  await render({ checkpoints });
  assert.doesNotMatch(status.textContent, /Refreshing|Loading/);
  assert.strictEqual(charts.children.length, 4);
  await render({ checkpoints, stale: true });
  assert.match(status.textContent, /Refreshing/);

  await render({ checkpoints: [], building: true });
  assert.strictEqual(charts.children.length, 0);
  await render({ checkpoints: [] });
  assert.doesNotMatch(status.textContent, /Loading/);
  await render({ checkpoints: [], error: 'history unavailable' });
  assert.match(status.textContent, /progress build failed: history unavailable/);
  console.log('progress loading tests passed');
}
main().catch(error => { console.error(error); process.exitCode = 1; });
