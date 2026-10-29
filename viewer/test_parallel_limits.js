const assert = require('assert');
const fs = require('fs');
const vm = require('vm');
const path = require('path');
function node() {
  return { textContent: '', hidden: false, disabled: false, value: '', max: 16, listeners: {}, classes: new Set(),
    addEventListener(k, fn) { this.listeners[k] = fn; }, setAttribute(k, v) { this[k] = v; },
    append() {}, style: { setProperty(k, v) { this[k] = v; } },
    get classList() { return { toggle: (k, v) => v ? this.classes.add(k) : this.classes.delete(k) }; } };
}
async function scenario(legacy) {
  const rows = ['workers', 'heavy'].map(limit => {
    const row = node(); row.dataset = { limit };
    row.nodes = Object.fromEntries(['input', 'output', '.agent-limit-track', '.agent-limit-current'].map(k => [k, node()]));
    row.querySelector = k => row.nodes[k]; return row;
  });
  const note = node(), message = node(), label = node(), panel = node();
  note.textContent = 'Active work continues.';
  panel.querySelectorAll = () => rows;
  panel.querySelector = k => ({'#agent-limits-note': note, '#agent-limits-message': message,
    '[for="agent-limit-heavy"]': label}[k]);
  let limits = {workers: 8, heavy: 6, expected_heavy: legacy ? null : 6,
    active: 5, active_heavy: 3, configurable: true};
  const posts = [];
  const context = { document: {hidden: false, head: node(), getElementById: () => null, querySelector: () => node(),
      createElement: tag => tag === 'div' ? panel : node()},
    controlAvailable: vm.runInNewContext('(' + fs.readFileSync(path.join(__dirname, 'public/index.html'), 'utf8').match(/function controlAvailable\(\) \{[\s\S]*?\n\}/)[0] + ')', {window: {__TRELLIS_CONTROL__: {token: 'fixture'}}}), controlHeaders: x => ({...x, 'X-Trellis-Control': 'fixture'}),
    apiPath: x => '/api/' + x, setTimeout() {}, clearTimeout() {},
    fetch: async (url, options) => {
      if (options) {
        assert.strictEqual(url, '/api/control/parallel-limits');
        assert.strictEqual(options.method, 'POST');
        assert.strictEqual(options.headers['X-Trellis-Control'], 'fixture');
        const body = JSON.parse(options.body); posts.push(body);
        limits = {...limits, workers: body.worker_slots, heavy: body.heavy_worker_slots,
          expected_heavy: body.heavy_worker_slots};
        return {ok: true, json: async () => ({worker_slots: limits.workers, heavy_worker_slots: limits.heavy})};
      }
      return {ok: true, json: async () => ({enabled: true, agent_limits: {...limits}})};
    } };
  vm.runInNewContext(fs.readFileSync(path.join(__dirname, 'public/parallel_limits.js'), 'utf8'), context);
  await new Promise(setImmediate);
  assert.strictEqual(rows[0].querySelector('output').textContent, '5 active · limit 8');
  assert.strictEqual(label.textContent, legacy ? 'Heavy agents (new limit)' : 'Heavy agents');
  assert.strictEqual(note.textContent.includes('Saving enables'), legacy);
  const input = rows[0].querySelector('input');
  input.value = legacy ? 8 : 2;
  input.listeners.input();
  assert.strictEqual(posts.length, 0, 'dragging does not issue repeated control mutations');
  await input.listeners.change();
  assert.deepStrictEqual(posts, [{expected_worker_slots: 8, expected_heavy_worker_slots: legacy ? null : 6,
    worker_slots: legacy ? 8 : 2, heavy_worker_slots: 6}]);
  assert.strictEqual(limits.active, 5, 'in-flight work remains represented after lowering the limit');
  assert.strictEqual(rows[0].classes.has('over-limit'), !legacy);
  assert(message.textContent.includes('Active work continues'));
  assert.strictEqual(input.disabled, false);
}
(async () => { await scenario(false); await scenario(true); console.log('parallel limit controls passed'); })()
  .catch(error => { console.error(error); process.exitCode = 1; });
