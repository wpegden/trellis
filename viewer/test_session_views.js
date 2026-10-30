const assert = require('assert');
const fs = require('fs');
const os = require('os');
const path = require('path');
const { execFileSync, spawn } = require('child_process');
const root = fs.mkdtempSync(path.join(os.tmpdir(), 'trellis-session-views-'));
process.env.PROJECTS_ROOT = root;
process.env.TRELLIS_TMUX_SOCKET = `trellis-test-sessions-${process.pid}`;
const { sessionViews, buildChatCalls } = require('./server');
const { runtimeNamespace, ownedPane, processEvidence } = require('./session_views');
const tmux = (...args) => execFileSync('tmux', ['-L', process.env.TRELLIS_TMUX_SOCKET, ...args], { stdio: ['ignore', 'pipe', 'pipe'] });
const write = (file, value) => { fs.mkdirSync(path.dirname(file), { recursive: true }); fs.writeFileSync(file, JSON.stringify(value)); };
const repo = path.join(root, 'example'), runtime = repo + '-runtime';
const project = { repoPath: repo, stateDir: path.join(repo, '.trellis'), repoType: 'trellis' };
function chat(where, id, text = id, metadata = {}) {
  const dir = path.join(where, '.trellis/chats/live', id);
  write(path.join(dir, 'call.json'), { provider: 'codex', model: 'test-model', ...metadata });
  fs.writeFileSync(path.join(dir, 'output.log'), JSON.stringify({ type: 'item.completed', item: { type: 'agent_message', text } }) + '\n');
}
function attempt(id, requestId, kind = 'Worker') {
  const base = path.join(runtime, 'dispatch', id), work = path.join(base, 'repo'), state = path.join(base, 'runtime');
  write(path.join(work, 'trellis.config.json'), {});
  write(path.join(state, 'runtime_metadata.json'), { repo_path: work });
  write(path.join(state, 'protocol_state.json'), { cycle: 2, in_flight_request: { id: requestId, kind, cycle: 2, active_node: 'Goal' } });
  write(path.join(base, 'prepared.json'), { request_id: requestId });
  return { work, state };
}
const panes = [];
function start(name, work, active = true) {
  tmux('new-session', '-d', '-s', name, '-c', work, 'sleep', '120');
  panes.push({ name, cwd: work, start: work, active, paths: [] });
}
(async () => {
  try {
    write(path.join(repo, 'trellis.config.json'), {});
    write(path.join(runtime, 'runtime_metadata.json'), { repo_path: repo });
    write(path.join(runtime, 'protocol_state.json'), { cycle: 2, in_flight_request: { id: 10, kind: 'Review', cycle: 2 }, dispatch: {
      config: { worker_slots: 2 }, tasks: {
        1: { id: 1, launched_at_ms: 100, status: { status: 'resolved', resolved_at_ms: 200 }, binding: { model: 'old-model' }, assignment: { task_kind: 'ordinary', scope: { nodes: ['Goal'] } } },
        2: { id: 2, retry_of: 1, launched_at_ms: 300, status: { status: 'running' }, binding: { model: 'new-model' }, assignment: { task_kind: 'ordinary', scope: { nodes: ['Goal'] } } },
      }, verifiers: { jobs: {} }, receipts: {}, reviewer: null,
    } });
    const log = path.join(repo, '.trellis-history/event-log/cycle-000002.jsonl');
    write(log, { cycle: 2, index: 1, commands: [{ command: 'issue_request', request: { id: 10, kind: 'Review', cycle: 2 } }] });
    fs.appendFileSync(log, '\n');
    chat(repo, 'trellis_review_10_decision');
    chat(repo, 'trellis_worker_1_result', 'An archived worker must not be live');
    start('trellis-example-review-10-reviewer', repo);
    // Same request number in another runtime must never cross-wire.
    const other = path.join(root, 'other'); fs.mkdirSync(other);
    start('trellis-other-review-10-reviewer', other);
    const first = attempt('1', 8); chat(first.work, 'trellis_worker_8_result');
    const second = attempt('2', 9); chat(second.work, 'trellis_worker_9_result');
    chat(second.work, 'trellis_worker_8_result', 'cloned unrelated transcript');
    start(`trellis-${runtimeNamespace(second.state)}-worker-9-worker`, second.work);
    const review = attempt('review-12', 12, 'Review'); chat(review.work, 'trellis_review_12_decision');
    start(`trellis-${runtimeNamespace(review.state)}-review-12-reviewer`, review.work);
    const verifier = attempt('verifier-3', 11, 'Corr');
    chat(verifier.work, 'trellis_corr_11_v1'); chat(verifier.work, 'trellis_corr_11_v2');
    start(`trellis-${runtimeNamespace(verifier.state)}-corr-11-v1`, verifier.work);
    start(`trellis-${runtimeNamespace(verifier.state)}-corr-11-v2`, verifier.work);
    const revision = path.join(runtime, 'pv-revision'); write(path.join(revision, 'state.json'), { sequence: 2, phase: 'audit' });
    for (const [number, role] of [[1, 'worker'], [2, 'audit'], [3, 'scope']]) {
      const name = `${String(number).padStart(5, '0')}-${role}`;
      write(path.join(revision, 'requests', name, 'request.json'), { request_id: `review-${number}-${role}`,
        artifact_prefix: `revision-${name}`, provider: { provider: 'codex', model: 'revision-model' },
        session_name: `trellis-pv-revision-review-${number}-${role}` });
      chat(revision, `revision-${name}`, role, number === 1 ? { ended_at_ms: 1000 } : {});
    }
    write(path.join(revision, 'requests/00001-worker/execution.json'), { exit_code: 0 });
    start('trellis-pv-revision-review-1-worker', revision); // retained idle/completed pane
    start('trellis-pv-revision-review-2-audit', revision);
    start('trellis-pv-revision-review-3-scope', other); // same-name session in another campaign
    const live = await sessionViews.live(project, panes);
    assert.strictEqual(live.warnings.length, 0);
    assert.deepStrictEqual(live.calls.map(call => call.group).sort(), [
      'attempt:2', 'attempt:review-12', 'attempt:verifier-3', 'attempt:verifier-3', 'cycle:live', 'revision:00002-audit',
    ]);
    assert(live.calls.every(call => !call.tmux_session.includes('other')));
    assert(!ownedPane({ active: false, cwd: revision }, revision), 'idle shells cannot be live agents');
    assert(ownedPane({ active: true, paths: [path.join(revision, '.trellis/logs/request-burst.sh')] }, revision), 'sandbox launcher paths bind ownership even when pane cwd is elsewhere');
    const calls = buildChatCalls(project, 'live');
    assert.strictEqual(calls.calls.find(c => c.request_id === 10).tmux_session, 'trellis-example-review-10-reviewer');
    const selected = await sessionViews.calls(project, 'attempt:2');
    assert.strictEqual(selected.calls.length, 1);
    assert.strictEqual(selected.calls[0].artifact_id, 'trellis_worker_9_result');
    const result = await sessionViews.events(project, 'attempt:2', selected.calls[0].key);
    assert.strictEqual(result.events[0].text, 'trellis_worker_9_result');
    await assert.rejects(sessionViews.events(project, 'attempt:2', 'attempt:2|trellis_worker_8_result|8|'), /no longer available/);
    await assert.rejects(sessionViews.calls(project, 'attempt:../../elsewhere'), /unavailable/);
    let history = await sessionViews.groups(project);
    const task = history.groups.find(g => g.id === 'task:1');
    assert.deepStrictEqual(task.attempts.map(a => a.id), ['attempt:2', 'attempt:1']);
    assert.strictEqual(task.attempts[0].model, 'new-model');
    assert(history.groups.some(g => g.id === 'revision:00001-worker'), 'finished revisions remain navigable');
    // Cycle evidence is always read from its original Git tag. No quarantine
    // repository is scanned, and an edited live file cannot replace history.
    const chats = path.join(repo, '.trellis/chats');
    const git = (...args) => execFileSync('git', ['-C', chats, ...args], { stdio: 'ignore' });
    git('init'); git('config', 'user.name', 'Test'); git('config', 'user.email', 'test@example.invalid');
    const cycleDir = path.join(chats, 'cycle-0001', 'trellis_worker_1_result'); fs.mkdirSync(cycleDir, { recursive: true });
    fs.copyFileSync(path.join(chats, 'live/trellis_worker_1_result/output.log'), path.join(cycleDir, 'output.log'));
    git('add', '.'); git('commit', '-m', 'archive'); git('tag', 'cycle-1');
    chat(repo, 'trellis_worker_1_result', 'changed live file');
    const archive = await sessionViews.calls(project, 'cycle:1');
    const old = archive.calls.find(c => c.artifact_id === 'trellis_worker_1_result');
    const evidence = await sessionViews.events(project, 'cycle:1', old.key);
    assert.strictEqual(evidence.events[0].text, 'An archived worker must not be live');
    assert.strictEqual(evidence.provenance, 'Immutable Git tag cycle-1');
    history = await sessionViews.groups(project);
    assert(history.groups.some(g => g.id === 'cycle:1'));
    // Preparation remains available after handoff but cannot be called live.
    const prep = path.join(root, 'preparation'); fs.mkdirSync(prep);
    const pp = { repoPath: prep, stateDir: path.join(prep, '.trellis'), repoType: 'trellis' };
    write(path.join(prep, '.trellis/modeling/state.json'), { stage: 'adaptation', sequence: 9 });
    write(path.join(prep, '.trellis/modeling/requests/preparation-00009-planner.request.json'), {
      request_id: 9, session_name: 'trellis-preparation-review-9-audit', provider: { provider: 'codex', model: 'planner-model' }, artifact: { phase: 'adaptation' },
    });
    chat(prep, 'preparation-00009-planner');
    start('trellis-preparation-review-9-audit', prep);
    const preparing = await sessionViews.live(pp, panes);
    assert.strictEqual(preparing.calls.length, 1); assert.strictEqual(preparing.calls[0].phase, 'adaptation');
    const prepHistory = await sessionViews.groups(pp);
    assert(prepHistory.groups.some(g => g.category === 'Preparation'));
    assert.strictEqual((await sessionViews.live(pp, [])).calls.length, 0, 'no stale transcript can imply liveness');
    // Real process evidence recognizes a running burst wrapper, never a bare
    // sleep/shell. This test is independent of the viewer's shared tmux socket.
    const script = path.join(root, 'test-burst.sh'); fs.writeFileSync(script, 'sleep 30\n');
    const child = spawn('/bin/bash', [script], { detached: true, stdio: 'ignore' });
    try {
      await new Promise(resolve => setTimeout(resolve, 40));
      const evidence = processEvidence(child.pid);
      assert(evidence.active); assert(evidence.paths.includes(script));
    } finally { process.kill(-child.pid, 'SIGTERM'); }
    const wrapper = spawn(process.execPath, ['-e', 'setTimeout(() => {}, 30000)', '/sdk/codex'],
      { detached: true, stdio: 'ignore' });
    try {
      await new Promise(resolve => setTimeout(resolve, 40));
      const evidence = processEvidence(wrapper.pid);
      assert.strictEqual(evidence.provider, null, 'a mount path is not a provider command');
      assert.strictEqual(evidence.active, false, 'a provider-shaped argument cannot imply liveness');
    } finally { process.kill(-wrapper.pid, 'SIGTERM'); }
    console.log('session view tests passed');
  } finally {
    try { tmux('kill-server'); } catch {}
    fs.rmSync(root, { recursive: true, force: true });
  }
})().catch(error => { console.error(error); process.exitCode = 1; });
