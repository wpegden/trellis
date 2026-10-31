const assert = require('node:assert/strict');
const fs = require('node:fs');
const os = require('node:os');
const path = require('node:path');
const vm = require('node:vm');
const http = require('node:http');
const childProcess = require('node:child_process');
const { test, after } = require('node:test');

const root = fs.mkdtempSync(path.join(os.tmpdir(), 'trellis-provider-retry-'));
process.env.PROJECTS_ROOT = root;
process.env.STATIC_OUT = path.join(root, 'static');
const repo = path.join(root, 'retry-run');
fs.mkdirSync(path.join(repo, '.trellis'), { recursive: true });
fs.writeFileSync(path.join(repo, 'trellis.config.json'), '{}');
fs.mkdirSync(`${repo}-runtime`);
fs.writeFileSync(`${repo}-runtime/runtime_metadata.json`, JSON.stringify({ repo_path: repo }));
const retry = { code: 'server_overloaded', request_id: 11, turn: 2, ordinal: 1,
  evidence_sha256: 'a'.repeat(64), next_retry_at_ms: 121000, remaining_ms: 180000,
  resume_allowed: true };
const calls = [];
const exec = childProcess.execFileSync;
childProcess.execFileSync = (command, args, options) => {
  if (command === 'python3' && args[0] === '-m' && args[1] === 'trellis.viewer_adapter' && args[2] === 'transport-resume') {
    const body = JSON.parse(options.input);
    calls.push({ command, args, body });
    // The route test substitutes the Python boundary. Backend production
    // fixtures separately verify the receipt and native eligibility checks.
    const expected = body.task_id === undefined ? retry : { ...retry, task_id: 7 };
    assert.deepEqual(body, expected);
    return JSON.stringify({ ok: true, permit_issued: true, provider_retry: retry });
  }
  return exec(command, args, options);
};
const serverCode = require('./server');
after(() => { childProcess.execFileSync = exec; fs.rmSync(root, { recursive: true, force: true }); });
const info = { slug: 'retry-run', repoPath: repo, stateDir: path.join(repo, '.trellis'), repoType: 'trellis' };

test('backoff is live, and halt, Ready and pause retain precedence', () => {
  const status = { controller: 'preparation', state: 'provider_backoff', wrapper_pid: 99,
    preparation: { status: 'provider_backoff', provider_retry: retry } };
  const row = serverCode.pauseAttention(info, status, { halted: false });
  assert.equal(row.id, 'provider_backoff');
  assert.equal(row.resumable, false, 'this is not the supervisor restart action');
  assert.deepEqual(row.provider_retry, retry);
  assert.equal(serverCode.supervisorContextFromStatus(status).up, true);
  assert.equal(serverCode.classifyLifecycle({ preparation: true, pause_state: status.state }).lifecycle, 'live');
  assert.equal(serverCode.pauseAttention(info, status, { halted: true }), null);
  for (const state of ['engineering_ready', 'engineering_unresolved', 'preparation_paused']) {
    assert.notEqual(serverCode.pauseAttention(info, { ...status, state }, { halted: false }).id, 'provider_backoff');
  }
  assert.equal(serverCode.pauseAttention(info, { state: 'running', provider_retry: retry }, { halted: false }).id, 'provider_backoff');
});

function httpRequest(server, body, authorized) {
  const headers = { 'Content-Type': 'application/json' };
  if (authorized) headers[serverCode.CONTROL_HEADER] = serverCode.ensureControlToken(root);
  return new Promise((resolve, reject) => {
    const request = http.request({ host: '127.0.0.1', port: server.address().port,
      path: '/trellis/retry-run/api/control/pause/transport-resume', method: 'POST', headers }, response => {
      let raw = ''; response.on('data', part => raw += part);
      response.on('end', () => resolve({ status: response.statusCode, body: JSON.parse(raw) }));
    });
    request.on('error', reject); request.end(JSON.stringify(body));
  });
}

test('registered control route preserves exact retry identity, rejects stale input and uses no restart', async () => {
  const server = serverCode.app.listen(0, '127.0.0.1');
  await new Promise(resolve => server.once('listening', resolve));
  try {
    assert(serverCode.CONTROL_ROUTE_TAILS.includes('pause/transport-resume'));
    assert.notEqual((await httpRequest(server, retry, false)).status, 200);
    assert.equal(calls.length, 0);
    assert.equal((await httpRequest(server, retry, true)).status, 200);
    assert.deepEqual(calls.at(-1).args, ['-m', 'trellis.viewer_adapter', 'transport-resume', repo]);
    assert.equal((await httpRequest(server, { ...retry, task_id: 7 }, true)).status, 200);
    assert.equal((await httpRequest(server, { ...retry, evidence_sha256: 'b'.repeat(64) }, true)).status, 409);
    assert.equal((await httpRequest(server, { ...retry, clear_halt: true }, true)).status, 409);
    const before = calls.length;
    fs.writeFileSync(`${repo}-runtime/system_feedback_halt.json`, JSON.stringify({ reason: 'synthetic halt' }));
    assert.equal((await httpRequest(server, retry, true)).status, 409);
    assert.equal(calls.length, before, 'halt blocks the adapter before any permit');
    assert(fs.existsSync(`${repo}-runtime/system_feedback_halt.json`));
  } finally {
    fs.rmSync(`${repo}-runtime/system_feedback_halt.json`, { force: true });
    await new Promise(resolve => server.close(resolve));
  }
});

const html = fs.readFileSync(path.join(__dirname, 'public/index.html'), 'utf8');
function sourceFunction(name, source = html) {
  const match = source.match(new RegExp('(?:async )?function ' + name + '\\([^]*?\\n\\}'));
  assert(match, name); return match[0];
}

test('rendered banner counts down and sends only the exact permit action', () => {
  const actions = [], el = { style: {}, innerHTML: '' };
  const context = { Date, Math, document: { getElementById: () => el }, escHtml: String,
    pauseState: { state: 'provider_backoff', attention: serverCode.providerRetryAttention(retry) },
    pauseBudgetLine: () => '', attentionRowHtml: row => JSON.stringify(row.body),
    controlAvailable: () => true, appendAttentionAction: (...args) => actions.push(args.slice(1)) };
  vm.createContext(context);
  vm.runInContext(sourceFunction('providerRetryText') + '\n' + sourceFunction('renderPauseBanner'), context);
  assert.equal(context.providerRetryText(retry, 1000), 'Retry 1 in 120s; 3 min execution allowance remaining');
  assert.equal(context.providerRetryText(retry, 122000), 'Retry 1 ready; 3 min execution allowance remaining');
  context.renderPauseBanner();
  assert.equal(actions[0][0], 'Retry now'); assert.equal(actions[0][1], 'transport-resume');
  assert.deepEqual(JSON.parse(JSON.stringify(actions[0][2].body)), retry);
  actions.length = 0; context.controlAvailable = () => false; context.renderPauseBanner();
  assert.equal(actions.length, 0);
  context.controlAvailable = () => true;
  context.pauseState.attention = serverCode.providerRetryAttention({ ...retry, resume_allowed: false });
  context.renderPauseBanner(); assert.equal(actions.length, 0);
});

test('rendered parallel row keeps task and wrapper identities distinct', async () => {
  const button = { dataset: { transportTask: '7' } }, posts = [];
  const tasks = { innerHTML: '', querySelectorAll: selector => selector === '[data-transport-task]'
    && tasks.innerHTML.includes('data-transport-task') ? [button] : [] };
  const summary = {};
  const data = { enabled: true, phase: 'ProofFormalization', agent_limits: { active: 1 },
    capacity: { worker_slots: 3, compiler_slots: 1 }, verifiers: [], tasks: [{ id: 7, profile: 'light',
      scope: { nodes: [] }, status: { status: 'reserved' }, attempt: '7', provider_retry: retry }] };
  const context = { Date, Math, detailMode: 'parallel', parallelRefreshTimer: null, clearTimeout() {}, setTimeout() {},
    document: { getElementById: id => id === 'parallel-tasks' ? tasks : summary },
    fetch: async () => ({ json: async () => data }), apiPath: x => x, escHtml: String,
    detailTable: (head, rows) => JSON.stringify(rows), controlAvailable: () => true,
    doPauseAction: (action, body) => posts.push({ action, body }) };
  vm.createContext(context);
  vm.runInContext(sourceFunction('providerRetryText') + '\n' + sourceFunction('renderParallelDetail'), context);
  await context.renderParallelDetail();
  assert.match(tasks.innerHTML, /Retry now/); button.onclick();
  assert.deepEqual(JSON.parse(JSON.stringify(posts)), [{ action: 'transport-resume', body: { ...retry, task_id: 7 } }]);
  for (const field of ['pausing', 'operational_stop', 'draining']) {
    data[field] = { reason: 'synthetic stop' };
    await context.renderParallelDetail();
    assert.doesNotMatch(tasks.innerHTML, /Retry now/);
    delete data[field];
  }
});

test('landing card carries the current retry identity through its project control path', async () => {
  const landing = fs.readFileSync(path.join(__dirname, 'public/landing.html'), 'utf8');
  const posts = [];
  const element = (tag, cls, text) => ({ tag, text, childNodes: [], appendChild(child) { this.childNodes.push(child); } });
  const context = { Date, Math, JSON, window: { __TRELLIS_CONTROL__: { token: 'synthetic' } }, el: element,
    projectHref: slug => `/trellis/${slug}/`, fmtAge: () => '', controlHeaders: () => ({ 'X-Trellis-Control': 'synthetic' }),
    createFlowAction() { throw new Error('Unexpected job control'); }, refresh() {}, alert(message) { throw new Error(message); },
    fetch: async (url, options) => { posts.push({ url, options }); return { ok: true, json: async () => ({ ok: true }) }; } };
  vm.createContext(context); vm.runInContext(sourceFunction('renderCreateCard', landing), context);
  const card = context.renderCreateCard({ slug: 'fixture', effective_state: 'running', preparation: {
    stage: 'engineering', status: 'provider_backoff', sequence: 11, provider_retry: retry } });
  const buttons = card.childNodes.flatMap(x => x.childNodes).filter(x => x.tag === 'button');
  const button = buttons.find(x => x.text === 'Retry now'); assert(button); await button.onclick();
  assert.equal(posts[0].url, '/trellis/fixture/api/control/pause/transport-resume');
  assert.deepEqual(JSON.parse(posts[0].options.body), retry);
});
