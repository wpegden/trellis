// Landing page: lifecycle classification and the host snapshot.
//
// Every state in the plan's lifecycle table is built as a real fixture tree
// and classified through the real code path — including `live`, which runs an
// actual process whose argv `scripts/trellis_pause.sh` pgreps for, so the
// authority for "is the supervisor up" is the script rather than a stub.

const assert = require('assert');
const fs = require('fs');
const os = require('os');
const path = require('path');
const { spawn } = require('child_process');

const root = fs.mkdtempSync(path.join(os.tmpdir(), 'trellis-viewer-landing-'));
process.env.PROJECTS_ROOT = root;
process.env.STATIC_OUT = path.join(root, 'static-out');

const {
  LIFECYCLE_ORDER,
  LIFECYCLE_LABELS,
  classifyLifecycle,
  readProtocolHeadCheap,
  checkerLiveness,
  sidecarHealth,
  parseMeminfo,
  parseSwaps,
  diskUsage,
  providerAuthPresence,
  hostSnapshot,
  quotaSummaryForProject,
  projectsIndex,
} = require('./server');

function writeJson(file, value) {
  fs.mkdirSync(path.dirname(file), { recursive: true });
  fs.writeFileSync(file, JSON.stringify(value, null, 2));
}

// A project is a directory holding trellis.config.json; its runtime is the
// `<repo>-runtime` sibling, identified by runtime_metadata.json.
function makeProject(slug, { runtime = true, protocol = null, files = {} } = {}) {
  const repo = path.join(root, slug);
  fs.mkdirSync(repo, { recursive: true });
  writeJson(path.join(repo, 'trellis.config.json'), { project: slug });
  if (!runtime) return repo;
  const runtimeRoot = `${repo}-runtime`;
  writeJson(path.join(runtimeRoot, 'runtime_metadata.json'), { repo_path: repo });
  if (protocol) writeJson(path.join(runtimeRoot, 'protocol_state.json'), protocol);
  for (const [rel, value] of Object.entries(files)) {
    writeJson(path.join(runtimeRoot, rel), value);
  }
  return repo;
}

const RUNNING = { phase: 'ProofFormalization', stage: 'VerifyPaper', cycle: 42 };

function cardsBySlug() {
  const idx = projectsIndex({ fresh: true });
  const bySlug = {};
  for (const card of idx.projects) bySlug[card.slug] = card;
  return { idx, bySlug };
}

async function main() {
  // -----------------------------------------------------------------
  // 1. The classifier as a pure function: every state, and the ordering
  //    decisions that are easy to regress.
  // -----------------------------------------------------------------
  const base = {
    has_runtime: true,
    has_launch_env: true,
    has_run_evidence: true,
    pause_state: 'down',
    pause_requested: false,
    halted: false,
    phase_complete: false,
  };
  const lc = over => classifyLifecycle({ ...base, ...over }).lifecycle;

  assert.strictEqual(lc({ has_runtime: false }), 'never_started');
  assert.strictEqual(lc({ phase_complete: true }), 'finished');
  assert.strictEqual(lc({ halted: true }), 'halted');
  assert.strictEqual(lc({ pause_state: 'paused', pause_requested: true }), 'paused');
  assert.strictEqual(lc({ pause_state: 'running' }), 'live');
  assert.strictEqual(lc({ has_launch_env: false, has_run_evidence: false }), 'initialized');
  assert.strictEqual(lc({}), 'abandoned');

  // Terminal beats everything: a Complete run whose supervisor is somehow
  // still up, or which still carries an old marker, is finished.
  assert.strictEqual(lc({ phase_complete: true, pause_state: 'running' }), 'finished');
  assert.strictEqual(lc({ phase_complete: true, halted: true }), 'finished');
  // A halt is why the run is down, and trellis_pause.sh refuses to resume
  // over one, so it outranks a pause request.
  assert.strictEqual(lc({ halted: true, pause_state: 'paused', pause_requested: true }), 'halted');
  // `arming` — pause requested, supervisor still up — reads as paused, and
  // says so, because the run is on its way down.
  const arming = classifyLifecycle({ ...base, pause_state: 'arming', pause_requested: true });
  assert.strictEqual(arming.lifecycle, 'paused');
  assert.match(arming.reason, /next checkpoint/);
  // A run that has advanced cycles is never "initialized", whatever its
  // launch_env says: launch_env.json postdates the older runs on this host.
  assert.strictEqual(lc({ has_launch_env: false, has_run_evidence: true }), 'abandoned');

  // Pin the whole ladder. test_attention_state.js had to be updated when a
  // new tier appeared, which is the behaviour we want here too: adding a
  // lifecycle state must break this line and force a case for it below,
  // rather than quietly rendering as an unstyled card.
  //
  // The ladder is PRESENTATION order — how much the run wants the operator's
  // eye — and is deliberately not the classifier precedence asserted above.
  // A run that is halted, parked, executing, or half-launched comes first; the
  // inert buckets follow, with `never_started` (a scaffolded directory that has
  // never executed anything) last of all.
  assert.deepStrictEqual(LIFECYCLE_ORDER, [
    'halted', 'paused', 'live', 'initialized', 'finished', 'abandoned', 'never_started',
  ], 'a lifecycle state changed — add its classifier case and its landing-page colour');
  // The regression that motivated this order: a live run must not sort below
  // runs that have never started or have already finished. Those buckets fill
  // up on a long-lived host and buried the run the operator had just launched.
  const rankOf = state => LIFECYCLE_ORDER.indexOf(state);
  for (const inert of ['finished', 'abandoned', 'never_started']) {
    assert.ok(rankOf('live') < rankOf(inert),
      `a live run must render above "${inert}" runs`);
  }
  for (const wants of ['halted', 'paused', 'initialized']) {
    assert.ok(rankOf(wants) < rankOf('finished'),
      `"${wants}" needs attention and must render above finished runs`);
  }
  const produced = new Set([
    lc({ has_runtime: false }), lc({ phase_complete: true }), lc({ halted: true }),
    lc({ pause_state: 'paused', pause_requested: true }), lc({ pause_state: 'running' }),
    lc({ has_launch_env: false, has_run_evidence: false }), lc({}),
  ]);
  for (const state of LIFECYCLE_ORDER) {
    assert.ok(produced.has(state), `no classifier input produces "${state}" — is it reachable?`);
    assert.ok(LIFECYCLE_LABELS[state], `lifecycle "${state}" has no human label`);
  }

  // -----------------------------------------------------------------
  // 1b. In-page link building must carry the control-token prefix when it
  //     is present and must never invent one when it is absent.
  //
  // These pages have no build step and no module system, so the functions
  // are lifted out of the HTML and evaluated against a stubbed location —
  // which is the only way to cover them at all.
  // -----------------------------------------------------------------
  const lift = (file, names, pathname) => {
    const html = fs.readFileSync(path.join(__dirname, 'public', file), 'utf-8');
    const src = names.map(name => {
      const m = html.match(new RegExp(`\nfunction ${name}\\([^)]*\\) \\{[\\s\\S]*?\n\\}`));
      assert.ok(m, `${file} no longer defines ${name}()`);
      return m[0];
    }).join('\n');
    // eslint-disable-next-line no-new-func
    return new Function('window', `${src}\nreturn { ${names.join(', ')} };`)(
      { location: { pathname } },
    );
  };

  {
    // Landing page, served plainly and under the token prefix.
    const plain = lift('landing.html', ['basePath', 'apiPath', 'projectHref'], '/trellis/');
    assert.strictEqual(plain.basePath(), '/trellis');
    assert.strictEqual(plain.projectHref('current'), '/trellis/current/');
    assert.strictEqual(plain.apiPath('projects.json'), '/trellis/api/projects.json');

    const ctl = lift('landing.html', ['basePath', 'apiPath', 'projectHref'], '/trellis/Ab3xY9Zq/');
    assert.strictEqual(ctl.basePath(), '/trellis/Ab3xY9Zq');
    assert.strictEqual(ctl.projectHref('current'), '/trellis/Ab3xY9Zq/current/',
      'a card on the control landing page must stay in control mode');
    assert.strictEqual(ctl.apiPath('projects.json'), '/trellis/Ab3xY9Zq/api/projects.json');
  }

  {
    // Run viewer: its API and control paths are derived from its own URL, so
    // the prefix threads through with no extra machinery.
    const plain = lift('index.html', ['appBasePath', 'apiPath', 'controlPath'], '/trellis/current/');
    assert.strictEqual(plain.apiPath('progress.json'), '/trellis/current/api/progress.json');
    assert.strictEqual(plain.controlPath('pause/arm'), '/trellis/current/api/control/pause/arm');

    const ctl = lift('index.html', ['appBasePath', 'apiPath', 'controlPath'], '/trellis/Ab3xY9Zq/current/');
    assert.strictEqual(ctl.apiPath('progress.json'), '/trellis/Ab3xY9Zq/current/api/progress.json');
    assert.strictEqual(ctl.controlPath('pause/arm'),
      '/trellis/Ab3xY9Zq/current/api/control/pause/arm',
      'a control POST from a prefixed page must stay under the prefix');
    // The "all runs" back-link is one segment up, so it keeps the prefix.
    assert.strictEqual(ctl.appBasePath().replace(/\/[^/]*$/, ''), '/trellis/Ab3xY9Zq');
  }

  // -----------------------------------------------------------------
  // 2. Cheap protocol-state read.
  // -----------------------------------------------------------------
  assert.strictEqual(readProtocolHeadCheap(null), null);
  assert.strictEqual(readProtocolHeadCheap(path.join(root, 'nope')), null);

  // -----------------------------------------------------------------
  // 3. Fixture trees, one per lifecycle state, classified end to end.
  // -----------------------------------------------------------------
  makeProject('never', { runtime: false });
  makeProject('fresh');
  makeProject('done', { protocol: { phase: 'Complete', stage: 'Done', cycle: 105 } });
  makeProject('halted', {
    protocol: RUNNING,
    files: { 'system_feedback_halt.json': { cycle: 42, reason: 'test marker' } },
  });
  makeProject('parked', {
    protocol: RUNNING,
    files: { 'pause_request.json': { by: 'operator', reason: 'test', kind: 'manual' } },
  });
  makeProject('gone', { protocol: RUNNING });
  // No runtime sibling of its own. Before the ownership pass this repo
  // inherited whichever `*-runtime` the fallback scan found first, which on
  // the live host attached two archived repos to a running run.
  makeProject('orphan', { runtime: false });
  // A runtime whose owning repo is NOT a discovered project (no
  // trellis.config.json). Nothing in the project list contests this one, so
  // only an on-disk ownership check keeps `orphan` off it.
  fs.mkdirSync(path.join(root, 'undiscovered'), { recursive: true });
  writeJson(path.join(root, 'undiscovered-runtime', 'runtime_metadata.json'),
    { repo_path: path.join(root, 'undiscovered') });
  writeJson(path.join(root, 'undiscovered-runtime', 'protocol_state.json'), RUNNING);

  {
    const { idx, bySlug } = cardsBySlug();
    assert.strictEqual(bySlug.never.lifecycle, 'never_started');
    assert.strictEqual(bySlug.fresh.lifecycle, 'initialized');
    assert.strictEqual(bySlug.done.lifecycle, 'finished');
    assert.strictEqual(bySlug.done.phase, 'Complete');
    assert.strictEqual(bySlug.done.cycle, 105);
    assert.strictEqual(bySlug.halted.lifecycle, 'halted');
    assert.deepStrictEqual(bySlug.halted.halt.markers, ['system_feedback']);
    assert.strictEqual(bySlug.parked.lifecycle, 'paused');
    assert.strictEqual(bySlug.parked.pause_state, 'paused');
    assert.strictEqual(bySlug.gone.lifecycle, 'abandoned');

    // Ownership: a repo with no runtime of its own never adopts another's,
    // and the owner keeps it.
    assert.strictEqual(bySlug.orphan.lifecycle, 'never_started');
    assert.strictEqual(bySlug.orphan.runtime_root, null);
    // The audit's follow-up: the cross-project contest only fires when the
    // OWNER is itself a discovered project. `undiscovered` below owns a
    // runtime but has no trellis.config.json, so nothing contests the claim
    // — and an orphan repo would silently adopt a live run's runtime again.
    // Ownership must therefore be decided against the filesystem, not only
    // against the project list.
    assert.ok(!bySlug.undiscovered, 'a repo without trellis.config.json is not a project');
    assert.strictEqual(bySlug.never.runtime_root, null);
    assert.strictEqual(bySlug.done.runtime_root, path.join(root, 'done-runtime'));
    // A disowned inheritance is reported rather than hidden.
    assert.ok(bySlug.orphan.runtime_root_disowned, 'a disowned runtime should be named on the card');
    assert.match(bySlug.orphan.lifecycle_reason, /belongs to another repo/);

    // Cards are grouped most-terminal-first so the landing page can render
    // them in order without sorting client-side.
    const seen = idx.projects.map(p => p.lifecycle);
    const ranks = seen.map(s => LIFECYCLE_ORDER.indexOf(s));
    for (let i = 1; i < ranks.length; i += 1) {
      assert.ok(ranks[i] >= ranks[i - 1], `cards out of lifecycle order: ${seen.join(', ')}`);
    }

    // Every card must be clickable through to its own run viewer.
    for (const card of idx.projects) {
      assert.strictEqual(card.href, `${idx.base_path}/${card.slug}/`);
    }
  }

  // -----------------------------------------------------------------
  // 4. `live`, against a real process.
  //
  // trellis_pause.sh finds the supervisor with
  //   pgrep -f "bash .*trellis\.sh run <runtime_root>"
  // so a process with that argv is what the authority actually looks for.
  // -----------------------------------------------------------------
  const liveRepo = makeProject('livey', { protocol: RUNNING });
  const liveRuntime = `${liveRepo}-runtime`;
  const fakeSupervisor = path.join(root, 'scripts', 'trellis.sh');
  fs.mkdirSync(path.dirname(fakeSupervisor), { recursive: true });
  fs.writeFileSync(fakeSupervisor, '#!/usr/bin/env bash\nsleep 120\n');
  fs.chmodSync(fakeSupervisor, 0o755);

  const child = spawn('bash', [fakeSupervisor, 'run', liveRuntime], { stdio: 'ignore' });
  try {
    await new Promise(r => setTimeout(r, 400));
    const { bySlug } = cardsBySlug();
    assert.strictEqual(bySlug.livey.lifecycle, 'live',
      `expected live, got ${bySlug.livey.lifecycle} (${bySlug.livey.lifecycle_reason})`);
    assert.strictEqual(bySlug.livey.supervisor.state, 'running');
    assert.ok(bySlug.livey.supervisor.pid > 0);

    // Same process, plus a pause request: the run is arming, and the card
    // must not keep claiming it is simply live. `pauseStatusCached` holds
    // its answer for 3s, so wait that out rather than reaching past the
    // cache — the landing page lives with the same delay.
    writeJson(path.join(liveRuntime, 'pause_request.json'), { by: 'operator', kind: 'manual' });
    await new Promise(r => setTimeout(r, 3400));
    const armed = cardsBySlug().bySlug.livey;
    assert.strictEqual(armed.lifecycle, 'paused');
    assert.strictEqual(armed.pause_state, 'arming');
  } finally {
    child.kill('SIGKILL');
  }

  // -----------------------------------------------------------------
  // 5. Per-run liveness: checker and sidecar.
  // -----------------------------------------------------------------
  assert.strictEqual(checkerLiveness(null).state, 'unknown');
  const noChecker = checkerLiveness(liveRuntime);
  assert.strictEqual(noChecker.state, 'not_running');
  assert.strictEqual(noChecker.socket_present, false);

  // A pid file naming a live process that is NOT the checker must read down:
  // this is the stale-pid-file trap the sidecar module was written about.
  writeJson(path.join(liveRuntime, 'checker-state', 'server.pid'), 0);
  fs.writeFileSync(path.join(liveRuntime, 'checker-state', 'server.pid'), String(process.pid));
  const recycled = checkerLiveness(liveRuntime);
  assert.strictEqual(recycled.state, 'not_running');
  assert.match(recycled.detail, /recycled/);

  // A pid that is not in /proc at all is a stale file, not a running server.
  fs.writeFileSync(path.join(liveRuntime, 'checker-state', 'server.pid'), '2147480000');
  const stale = checkerLiveness(liveRuntime);
  assert.strictEqual(stale.state, 'not_running');
  assert.match(stale.detail, /stale pid file/);

  // A running checker is recognised by its own cmdline. `/proc` is injected
  // so this needs no real checker server.
  const procRoot = path.join(root, 'proc');
  fs.mkdirSync(path.join(procRoot, '4242'), { recursive: true });
  fs.writeFileSync(path.join(procRoot, '4242', 'cmdline'),
    'python3\0-m\0trellis.checker.server\0/some/runtime\0');
  fs.writeFileSync(path.join(liveRuntime, 'checker-state', 'server.pid'), '4242');
  fs.mkdirSync(path.join(liveRuntime, 'sockets'), { recursive: true });
  fs.writeFileSync(path.join(liveRuntime, 'sockets', 'checker.sock'), '');
  const up = checkerLiveness(liveRuntime, { procRoot });
  assert.strictEqual(up.state, 'running');
  assert.strictEqual(up.pid, 4242);
  assert.strictEqual(up.socket_present, true);

  // The sidecar is off by default, and "no sidecar directory" is answered
  // without paying for a Python interpreter.
  const noSidecar = sidecarHealth(liveRuntime);
  assert.strictEqual(noSidecar.state, 'not_running');
  assert.strictEqual(noSidecar.configured, false);
  assert.match(noSidecar.detail, /no sidecar directory/);
  assert.strictEqual(sidecarHealth(null).state, 'unknown');

  // With a sidecar directory present the answer comes from
  // trellis/sidecar/health.py — the module that exists because seven
  // incidents came from consumers inventing their own liveness rule. It must
  // never be identified by process name.
  fs.mkdirSync(path.join(liveRuntime, 'sidecar'), { recursive: true });
  const probed = sidecarHealth(liveRuntime);
  assert.ok(['running', 'not_running', 'unknown'].includes(probed.state), `unexpected state ${probed.state}`);
  assert.strictEqual(probed.state, 'not_running', 'no daemon holds the pid lock in a fixture tree');

  // -----------------------------------------------------------------
  // 6. Quota summary — the tail read must agree with the whole file.
  // -----------------------------------------------------------------
  const quotaRepo = path.join(root, 'done');
  const quotaLog = path.join(quotaRepo, '.trellis', 'logs', 'quota-snapshots.jsonl');
  fs.mkdirSync(path.dirname(quotaLog), { recursive: true });
  const rows = [];
  for (let i = 0; i < 400; i += 1) {
    rows.push(JSON.stringify({
      ts: 1700000000 + i,
      provider: 'codex',
      ok: true,
      plan_tier: 'Pro',
      windows: [{ name: 'weekly', pct_used: i % 100, pct_used_kind: 'exact', resets_at_repr: '15:00 on 19 Jul' }],
    }));
  }
  rows.push(JSON.stringify({
    ts: 1700009999, provider: 'claude', ok: true,
    windows: [{ name: 'weekly', pct_used: 12, pct_used_kind: 'exact' }],
  }));
  fs.writeFileSync(quotaLog, `${rows.join('\n')}\n`);

  const quota = quotaSummaryForProject({ repoPath: quotaRepo });
  assert.strictEqual(quota.codex.weekly_budget.pct_used, 399 % 100);
  assert.strictEqual(quota.claude.weekly_budget.pct_used, 12);
  // A tail small enough to land mid-line must drop the fragment rather than
  // reporting a parse error or a wrong row.
  const tiny = quotaSummaryForProject({ repoPath: quotaRepo }, { tailBytes: 300 });
  assert.ok(tiny.claude, 'the newest row must survive a small tail read');
  assert.deepStrictEqual(quotaSummaryForProject({ repoPath: path.join(root, 'never') }), {});

  // -----------------------------------------------------------------
  // 7. Host snapshot.
  // -----------------------------------------------------------------
  const mem = parseMeminfo([
    'MemTotal:       65704584 kB',
    'MemFree:         8858908 kB',
    'MemAvailable:   52802324 kB',
    'SwapTotal:      16777212 kB',
    'SwapFree:       12000000 kB',
  ].join('\n'));
  assert.strictEqual(mem.MemTotal, 65704584 * 1024);
  assert.strictEqual(mem.SwapFree, 12000000 * 1024);

  const swaps = parseSwaps('Filename\t\t\t\tType\t\tSize\t\tUsed\t\tPriority\n/swapfile\tfile\t\t16777212\t4777212\t-2\n');
  assert.strictEqual(swaps.length, 1);
  assert.strictEqual(swaps[0].name, '/swapfile');
  assert.strictEqual(swaps[0].type, 'file');
  assert.strictEqual(swaps[0].size_bytes, 16777212 * 1024);

  const du = diskUsage(root);
  assert.ok(du.total_bytes > 0 && du.avail_bytes >= 0);
  assert.ok(du.pct_used >= 0 && du.pct_used <= 100);
  assert.ok(diskUsage(path.join(root, 'no-such-dir')).error);

  // Presence and age only — these are provider OAuth credentials.
  const fakeHome = path.join(root, 'home');
  fs.mkdirSync(path.join(fakeHome, '.codex'), { recursive: true });
  fs.writeFileSync(path.join(fakeHome, '.codex', 'auth.json'), '{"secret":"must-not-be-read"}');
  const auth = providerAuthPresence(fakeHome);
  assert.deepStrictEqual(auth.map(a => a.provider).sort(), ['claude', 'codex', 'gemini']);
  const codexAuth = auth.find(a => a.provider === 'codex');
  assert.strictEqual(codexAuth.present, true);
  assert.ok(codexAuth.mtime_ms > 0);
  assert.ok(!('contents' in codexAuth) && !('token' in codexAuth));
  assert.strictEqual(auth.find(a => a.provider === 'gemini').present, false);

  // The requirement is reported next to the measurement, honestly, for each
  // side of both documented thresholds (32 GB minimum, 48 GB recommended).
  const procFixture = path.join(root, 'proc-host');
  fs.mkdirSync(procFixture, { recursive: true });
  fs.writeFileSync(path.join(procFixture, 'loadavg'), '1.73 2.01 2.03 2/2908 3504119\n');
  fs.writeFileSync(path.join(procFixture, 'swaps'), 'Filename\tType\tSize\tUsed\tPriority\n');
  const withRam = gb => {
    fs.writeFileSync(path.join(procFixture, 'meminfo'),
      `MemTotal:       ${gb * 1024 * 1024} kB\nMemAvailable:    1000000 kB\nSwapTotal:             0 kB\nSwapFree:              0 kB\n`);
    return hostSnapshot({ procRoot: procFixture, home: fakeHome, diskPaths: [root] });
  };
  assert.strictEqual(withRam(16).memory.verdict, 'below_minimum');
  assert.strictEqual(withRam(32).memory.verdict, 'meets_minimum');
  assert.strictEqual(withRam(64).memory.verdict, 'meets_recommended');

  const snap = withRam(64);
  assert.strictEqual(snap.memory.requirement_min_gb, 32);
  assert.strictEqual(snap.memory.requirement_recommended_gb, 48);
  assert.strictEqual(snap.load['1m'], 1.73);
  assert.strictEqual(snap.swap.present, false);
  assert.match(snap.swap.note, /OOM/, 'a host with no swap must be told what that costs');
  assert.strictEqual(snap.disks.length, 1);
  assert.strictEqual(snap.control.bind_is_loopback, true);

  // A host that cannot be read degrades instead of throwing.
  const blind = hostSnapshot({ procRoot: path.join(root, 'no-proc'), home: fakeHome, diskPaths: [root] });
  assert.strictEqual(blind.memory.verdict, 'unknown');
  assert.strictEqual(blind.load, null);

  fs.rmSync(root, { recursive: true, force: true });
  console.log('landing index tests passed');
}

main().then(() => process.exit(0)).catch(e => {
  console.error(e);
  process.exit(1);
});
