const assert = require('assert');
const fs = require('fs');
const vm = require('vm');
const source = fs.readFileSync(require.resolve('./server'), 'utf8');
const sweep = source.slice(source.indexOf('let budgetSweepRunning ='), source.indexOf('setInterval(sweepBudgetPauses'));

(async () => {
  const order = [];
  let discoveryFails = false;
  const context = vm.createContext({
    setImmediate, console: { error() {} },
    discoverProjects() {
      if (discoveryFails) throw Error('temporary read failure');
      return [1, 2, 3].map(slug => ({ slug }));
    },
    resolveRepoPath(slug) { return { slug }; },
    async refreshPauseStatus() {},
    evaluateBudgetPause({ slug }) {
      order.push(`budget:${slug}`);
      setImmediate(() => order.push(`http:${slug}`));
      if (slug === 2) throw Error('one project failed');
    },
    evaluateGatePark({ slug }) { order.push(`gate:${slug}`); },
  });
  vm.runInContext(sweep, context);
  const first = vm.runInContext('sweepBudgetPauses()', context);
  await vm.runInContext('sweepBudgetPauses()', context);
  await first;
  await new Promise(setImmediate);
  assert.deepStrictEqual(order, [
    'budget:1', 'gate:1', 'http:1', 'budget:2', 'gate:2', 'http:2', 'budget:3', 'gate:3', 'http:3',
  ], 'HTTP gets a turn between projects; concurrent ticks do not duplicate sweeps');
  discoveryFails = true;
  await vm.runInContext('sweepBudgetPauses()', context);
  discoveryFails = false;
  order.length = 0;
  await vm.runInContext('sweepBudgetPauses()', context);
  assert(order.includes('gate:3'), 'a failed sweep must release the running guard');

  const unknownGate = vm.createContext({
    readPauseConfig: () => ({ gate_park_after_minutes: 10 }),
    runtimeRootForProject: () => '/runtime',
    pauseStatusCached: () => ({ state: 'running' }),
    readGateIdentityCheap: () => null,
    gateOpenMinutes() { assert.fail('unknown gate must not clear or advance a retained timer'); },
    runPauseScript() { assert.fail('unknown gate must not pause the run'); },
  });
  vm.runInContext(source.slice(source.indexOf('function evaluateGatePark('), source.indexOf('// The floor has to be enforced')), unknownGate);
  assert.strictEqual(vm.runInContext('evaluateGatePark({}).reason', unknownGate), 'gate_unknown');

  // Exercise the actual option placement used by both automatic pause paths.
  const calls = [];
  vm.runInContext(source.slice(source.indexOf('function runPauseScript('), source.indexOf('function pauseStatusRaw(')), context);
  Object.assign(context, {
    PAUSE_SCRIPT: '/pause.sh', TRELLIS_ROOT: '/', runtimeRootForProject: () => '/runtime',
    execFileSync(command, args) { calls.push(args); },
    projectInfo: { repoPath: '/repo' }, reason: 'reason with spaces',
  });
  for (const kind of ['quota_budget', 'gate_parked']) {
    const line = source.split('\n').find(line => line.includes('runPauseScript(projectInfo,') && line.includes(`'${kind}'`));
    vm.runInContext(line, context);
    const args = calls.at(-1);
    assert.deepStrictEqual(Array.from(args.slice(0, 4)), ['/pause.sh', 'arm', '/runtime', '/repo']);
    assert.strictEqual(args.at(-1), 'reason with spaces');
  }

  const callbacks = [];
  const cache = new Map();
  let currentRepo = '/repo';
  const probe = vm.createContext({
    pauseStatusCache: cache, pauseStatusGeneration: new Map(), _adapterInFlight: new Map(),
    fs: { realpathSync: () => currentRepo, existsSync: () => false },
    path: require('path'), Date, PAUSE_STATUS_TTL_MS: 3000,
    TRELLIS_ROOT: '/', PAUSE_SCRIPT: '/pause.sh',
    runtimeRootForProject: () => '/runtime',
    execFile(command, args, options, callback) { callbacks.push(callback); },
    info: { slug: 'current', repoPath: '/alias' },
  });
  const coalescer = source.slice(source.indexOf('function _adapterInFlightOnce('), source.indexOf('// Augment a viewer-state payload'));
  vm.runInContext(coalescer + source.slice(source.indexOf('async function refreshPauseStatus('), source.indexOf('// Is this run finished?')), probe);
  const refresh = () => vm.runInContext('refreshPauseStatus(info)', probe);
  const a = refresh(), b = refresh();
  await new Promise(setImmediate);
  assert.strictEqual(callbacks.length, 1, 'simultaneous status requests share one subprocess');
  vm.runInContext('invalidatePauseStatus(info)', probe);
  callbacks.shift()(null, '{"state":"running"}');
  await Promise.all([a, b]);
  assert.strictEqual(cache.size, 0, 'a probe begun before a control action cannot restore stale status');
  const c = refresh();
  await new Promise(setImmediate);
  callbacks.shift()(null, '{"state":"paused"}');
  await c;
  assert.strictEqual(cache.get('current').value.state, 'paused');
  await refresh();
  assert.strictEqual(callbacks.length, 0, 'fresh completed probes are cached');
  currentRepo = '/new-repo';
  const d = refresh();
  await new Promise(setImmediate);
  callbacks.shift()(null, '{"state":"running"}');
  await d;
  assert.strictEqual(cache.get('current').repoPath, '/new-repo', 'alias handoff requires a new probe');
  console.log('pause sweep tests passed');
})().catch(error => { console.error(error); process.exitCode = 1; });
