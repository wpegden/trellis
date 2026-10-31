// Read the real preparation lock through the viewer's status path, then verify
// that a public-alias handoff immediately switches back to native status.
const assert = require('assert');
const fs = require('fs');
const os = require('os');
const path = require('path');
const { spawn } = require('child_process');
const { once } = require('events');
const { pauseStateResponse, supervisorContextFromStatus, projectCard,
  classifyCreateJob, createJobsRoot, createJobCard, createJobSettings } = require('./server');
const root = fs.mkdtempSync(path.join(os.tmpdir(), 'trellis-preparation-status-'));
const writeJson = (file, value) => {
  fs.mkdirSync(path.dirname(file), { recursive: true });
  fs.writeFileSync(file, JSON.stringify(value));
};
async function main() {
  for (const route of ['rust_aeneas', 'lean_native']) {
    const slug = `paper-${route}`;
    const jobDir = path.join(createJobsRoot(root), slug);
    const preparation = path.join(jobDir, 'preparation');
    const repo = path.join(root, slug);
    const statePath = path.join(preparation, '.trellis/modeling/state.json');
    const state = { stage: 'performance', status: 'active', sequence: 9,
      verification_protocol: route === 'lean_native' ? 'paper-lean-preparation/v1' : 'native-v1',
      next: { role: 'performance' } };
    const job = { slug, create_flow: 'paper-implementation', route, repo,
      preparation_root: preparation, native_repo: path.join(jobDir, 'native'),
      runtime_root: path.join(jobDir, 'native-runtime') };
    writeJson(path.join(jobDir, 'job.json'), job);
    writeJson(path.join(jobDir, 'status.json'), { state: 'failed', updated_ts: 990 });
    const failedIntake = createJobCard(slug, { root, now: 1000, tmuxSessions: [] });
    assert.strictEqual(failedIntake.bucket, 'create_failed');
    assert.strictEqual(failedIntake.retry_eligible, false);
    assert.match(failedIntake.retry_blocked_reason, /fresh name/);
    writeJson(statePath, state);
    fs.symlinkSync(preparation, repo);
    const opts = { root, now: 1000, tmuxSessions: [] };
    for (const status of ['failed', 'stopped']) {
      writeJson(path.join(jobDir, 'status.json'), { state: status, updated_ts: 990 });
      const card = createJobCard(slug, opts);
      assert.strictEqual(card.bucket, 'create_failed');
      assert.strictEqual(card.retry_eligible, true);
      assert.strictEqual(card.retry_mode, 'resume-preparation');
      assert.strictEqual(card.delete_eligible, false);
      assert.strictEqual(card.preparation.role, 'performance');
    }
    for (const halt of ['system_feedback_halt.json', 'checker_disagreement_halt.json']) {
      const marker = path.join(`${preparation}-runtime`, halt);
      writeJson(marker, { reason: 'fixture halt' });
      const card = createJobCard(slug, opts);
      assert.strictEqual(card.retry_eligible, false);
      assert.match(card.retry_blocked_reason, /halt/);
      fs.unlinkSync(marker);
    }
    for (const dir of [job.native_repo, job.runtime_root]) {
      fs.mkdirSync(dir);
      assert.strictEqual(createJobCard(slug, opts).retry_eligible, false, 'partial export blocks preparation retry');
      fs.rmdirSync(dir);
    }
    writeJson(statePath, { ...state, verification_protocol: 'wrong-protocol' });
    assert.strictEqual(createJobCard(slug, opts).retry_eligible, false);
    writeJson(statePath, state);
    fs.unlinkSync(repo);
    fs.symlinkSync(root, repo);
    assert.match(createJobCard(slug, opts).retry_blocked_reason, /project link/);
  }
  for (const state of ['queued', 'running']) {
    assert.strictEqual(classifyCreateJob({ state, create_flow: 'paper-implementation',
      updated_ts: 1, now: 1000, tmux_alive: false, stale_secs: 30 }).effective_state, 'interrupted');
  }
  const parallel = { enabled: true, worker_slots: 3 };
  assert.deepStrictEqual(createJobSettings({ parallel_dispatch: parallel }).parallel_dispatch, parallel);

  const prep = path.join(root, 'preparation');
  const statePath = path.join(prep, '.trellis/modeling/state.json');
  writeJson(statePath, { stage: 'adaptation', status: 'active', sequence: 12, next: { role: 'reviewer' } });
  writeJson(path.join(prep, 'trellis.config.json'), {});
  const alias = path.join(root, 'current');
  fs.symlinkSync(prep, alias);
  const info = { slug: 'current', repoPath: alias, stateDir: path.join(alias, '.trellis'), repoType: 'trellis' };
  const lock = path.join(prep, '.trellis/modeling/runner.lock');
  const child = spawn('python3', ['-c',
    'import fcntl,sys\nf=open(sys.argv[1],"w")\nfcntl.flock(f,fcntl.LOCK_EX)\nprint("ready",flush=True)\nsys.stdin.read()', lock],
    { stdio: ['pipe', 'pipe', 'inherit'] });
  try {
    await once(child.stdout, 'data', { signal: AbortSignal.timeout(5000) });
    const live = pauseStateResponse(info);
    assert.strictEqual(live.state, 'preparing');
    assert.strictEqual(live.wrapper_pid, child.pid);
    assert.strictEqual(live.attention, null);
    assert.strictEqual(live.resumable, false);
    assert.strictEqual(supervisorContextFromStatus(live).up, true);
    const card = projectCard(info);
    assert.strictEqual(card.lifecycle, 'live');
    assert.strictEqual(card.phase, 'Modeling');
    assert.strictEqual(card.stage, 'adaptation');

    // A same-slug handoff must bypass the cached preparation status.
    const native = path.join(root, 'native');
    writeJson(path.join(native, 'trellis.config.json'), {});
    writeJson(`${native}-runtime/runtime_metadata.json`, { repo_path: native });
    writeJson(`${native}-runtime/pause_request.json`, { kind: 'manual', reason: 'fixture pause' });
    fs.unlinkSync(alias);
    fs.symlinkSync(native, alias);
    const handedOff = pauseStateResponse(info);
    assert.strictEqual(handedOff.state, 'paused');
    assert.strictEqual(handedOff.controller, undefined);
    assert.strictEqual(handedOff.attention.id, 'operator_pause');
    fs.unlinkSync(alias);
    fs.symlinkSync(prep, alias);

    child.stdin.end();
    await once(child, 'exit');
    const stopped = pauseStateResponse(info);
    assert.strictEqual(stopped.state, 'preparation_stopped');
    assert.strictEqual(stopped.attention.id, 'preparation_stopped');
    assert.strictEqual(supervisorContextFromStatus(stopped).up, false);
    assert.strictEqual(stopped.resumable, false);

    writeJson(`${prep}-runtime/system_feedback_halt.json`, { reason: 'fixture halt' });
    const halted = pauseStateResponse(info);
    assert.strictEqual(halted.attention, null, 'the halt banner owns an explained stop');
    console.log('preparation status tests passed');
  } finally {
    if (child.exitCode === null) child.kill();
  }
}
main().catch(error => { console.error(error); process.exitCode = 1; })
  .finally(() => fs.rmSync(root, { recursive: true, force: true }));
