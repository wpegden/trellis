// Exercise the production AttemptProcess launcher and its per-attempt tmux
// sockets. No fixture sees or modifies a live run's shared socket.
const assert = require('assert');
const fs = require('fs');
const os = require('os');
const path = require('path');
const { execFileSync } = require('child_process');
const root = fs.mkdtempSync(path.join(os.tmpdir(), 'trellis-session-sockets-'));
process.env.PROJECTS_ROOT = root;
process.env.TRELLIS_TMUX_SOCKET = `trellis-test-sockets-${process.pid}`;
const { sessionViews, buildChatCalls, buildChatEvents } = require('./server');
const { runtimeNamespace, socketForRuntime } = require('./session_views');
const write = (file, value) => { fs.mkdirSync(path.dirname(file), { recursive: true }); fs.writeFileSync(file, JSON.stringify(value)); };
const repo = path.join(root, 'example'), runtime = repo + '-runtime';
const project = { repoPath: repo, stateDir: path.join(repo, '.trellis'), repoType: 'trellis' };
const launches = [];
const python = source => execFileSync('python3', ['-c', source], { cwd: path.dirname(__dirname), encoding: 'utf8', timeout: 20000 });
function attempt(id, requestId, kind, lanes) {
  const directory = path.join(runtime, 'dispatch', id), work = path.join(directory, 'repo'), state = path.join(directory, 'runtime');
  write(path.join(work, 'trellis.config.json'), {});
  write(path.join(state, 'runtime_metadata.json'), { repo_path: work });
  write(path.join(state, 'protocol_state.json'), { cycle: 2, in_flight_request: { id: requestId, kind, cycle: 2, active_node: 'Goal' } });
  const role = kind.toLowerCase();
  const names = lanes.map(lane => `trellis-${runtimeNamespace(state)}-${role}-${requestId}-${lane}`);
  for (const lane of lanes) {
    const artifact = `trellis_${role}_${requestId}_${role === 'review' ? 'decision' : role === 'worker' ? 'result' : lane}`;
    const dir = path.join(work, '.trellis/chats/live', artifact);
    fs.mkdirSync(dir, { recursive: true });
    fs.writeFileSync(path.join(dir, 'output.log'), JSON.stringify({ type: 'item.completed', item: { type: 'agent_message', text: lane } }) + '\n');
  }
  return { directory, work, state, names, lanes };
}
const launcherCode = `import json,os,subprocess,sys,time
p=json.load(sys.stdin)
for name,lane in zip(p['names'],p['lanes']):
 # An ancestor with a provider-shaped mount path must not hide the real child.
 command=['bash',p['agent'],'--model','test-'+lane]
 wrapper='import subprocess; subprocess.run('+repr(command)+')'
 subprocess.run(['tmux','-L',os.environ['TRELLIS_TMUX_SOCKET'],'new-session','-d','-s',name,'-c',p['work'],sys.executable,'-c',wrapper,'--ro-bind','/sdk/codex','/sdk/codex'],check=True)
time.sleep(120)
`;
function launch(info, turn = null) {
  const execution = path.join(info.directory, ...(turn == null ? [] : ['turns', String(turn)]), 'execution');
  launches.push(execution);
  const args = JSON.stringify({ ...info, execution, agent: path.join(root, 'codex'), code: launcherCode });
  const result = JSON.parse(python(`import json,sys\nfrom trellis.dispatch.process import AttemptProcess,socket_name\np=json.loads(${JSON.stringify(args)})\na=AttemptProcess(p['execution'])\nr=a.start_or_adopt([sys.executable,'-c',p['code']],p)\na.permit(r)\nprint(json.dumps({'socket':socket_name(a.root),'record':r}))`));
  assert.strictEqual(result.socket, socketForRuntime(info.state), 'same socket identity as production Python');
  return { ...info, ...result, execution };
}
async function waitLive(project, group) {
  for (let i = 0; i < 80; i++) {
    const live = await sessionViews.live(project);
    assert.deepStrictEqual(live.warnings, []);
    const call = live.calls.find(c => c.group === group);
    if (call) return call;
    await new Promise(resolve => setTimeout(resolve, 50));
  }
  assert.fail(`owned provider did not become live: ${group}`);
}
(async () => {
  try {
    write(path.join(repo, 'trellis.config.json'), {});
    write(path.join(runtime, 'runtime_metadata.json'), { repo_path: repo });
    write(path.join(runtime, 'protocol_state.json'), { cycle: 2 });
    fs.writeFileSync(path.join(root, 'codex'), '#!/bin/sh\nsleep 120\n');
    const workerInfo = attempt('1', 7, 'Worker', ['worker']);
    const worker = launch(workerInfo, 1);
    const reviewer = launch(attempt('review-8', 8, 'Review', ['reviewer']));
    const verifier = launch(attempt('verifier-2', 9, 'Corr', ['v1', 'v2']));
    // Same exact session name on the main socket must not replace a task lane.
    execFileSync('tmux', ['-L', process.env.TRELLIS_TMUX_SOCKET, 'new-session', '-d', '-s', worker.names[0], '-c', worker.work, 'bash', path.join(root, 'codex'), '--model', 'wrong-socket']);
    for (let i = 0; i < 60; i++) {
      try {
        const names = execFileSync('tmux', ['-L', verifier.socket, 'ls', '-F', '#S'], { encoding: 'utf8', stdio: ['ignore', 'pipe', 'ignore'] });
        if (names.includes(verifier.names[1])) break;
      } catch {}
      await new Promise(resolve => setTimeout(resolve, 50));
    }
    const live = await sessionViews.live(project);
    assert.deepStrictEqual(live.warnings, []);
    assert.deepStrictEqual(live.calls.map(c => c.group), ['attempt:1', 'attempt:review-8', 'attempt:verifier-2', 'attempt:verifier-2']);
    assert.deepStrictEqual(live.calls.map(c => c.model), ['test-worker', 'test-reviewer', 'test-v1', 'test-v2']);
    assert(live.calls.every(c => c.provider === 'codex' && c.tmux_socket.startsWith('trellis-task-')));
    const selected = await sessionViews.calls(project, 'attempt:review-8');
    assert.strictEqual(selected.calls[0].tmux_socket, reviewer.socket);
    assert.strictEqual(selected.calls[0].tmux_session, reviewer.names[0]);
    const events = await sessionViews.events(project, 'attempt:review-8', selected.calls[0].key);
    assert.strictEqual(events.events[0].text, 'reviewer');
    // TUI capture resolves the task's socket as well, never a same-name main pane.
    const privateProject = { repoPath: worker.work, stateDir: path.join(worker.work, '.trellis'), repoType: 'trellis', runtimeRoot: worker.state };
    const call = buildChatCalls(privateProject, 'live').calls[0];
    const tui = buildChatEvents(privateProject, 'live', call.artifact_id, 'tui', call);
    assert.strictEqual(tui.tmux_socket, worker.socket);
    assert.strictEqual(tui.missing, false);
    // Stale durable PID identity must not discover an otherwise existing socket.
    write(path.join(worker.execution, 'process.json'), { ...worker.record, start_ticks: worker.record.start_ticks + 1 });
    write(path.join(worker.execution, 'permit.json'), { ...worker.record, start_ticks: worker.record.start_ticks + 1 });
    assert(!(await sessionViews.live(project)).calls.some(c => c.group === 'attempt:1'));
    write(path.join(worker.execution, 'process.json'), worker.record);
    write(path.join(worker.execution, 'permit.json'), worker.record);
    // A completed attempt is history even while its old pane still exists.
    write(path.join(worker.execution, 'cleanup.json'), worker.record);
    assert(!(await sessionViews.live(project)).calls.some(c => c.group === 'attempt:1'));
    fs.unlinkSync(path.join(worker.execution, 'cleanup.json'));
    // A correction uses a new socket but retains the same task transcript.
    python(`import json\nfrom pathlib import Path\nfrom trellis.dispatch.process import AttemptProcess\np=Path(${JSON.stringify(worker.execution)})\nAttemptProcess(p).stop_and_reap(json.loads((p/'process.json').read_text()))`);
    const correction = launch(workerInfo, 2);
    assert.notStrictEqual(correction.socket, worker.socket);
    for (let i = 0; i < 60; i++) {
      try {
        if (execFileSync('tmux', ['-L', correction.socket, 'ls', '-F', '#S'],
          { encoding: 'utf8', stdio: ['ignore', 'pipe', 'ignore'] }).includes(correction.names[0])) break;
      } catch {}
      await new Promise(resolve => setTimeout(resolve, 50));
    }
    const repaired = await waitLive(project, 'attempt:1');
    assert.strictEqual(repaired?.tmux_socket, correction.socket);
    const retained = await sessionViews.calls(project, 'attempt:1');
    assert.strictEqual((await sessionViews.events(project, 'attempt:1', retained.calls[0].key)).events[0].text, 'worker');
    // Serial submissions keep canonical chat artifacts but use the same owned
    // turn launcher. Discovery must not require a parallel dispatch directory.
    const serialRepo = path.join(root, 'serial'), serialRuntime = serialRepo + '-runtime';
    write(path.join(serialRepo, 'trellis.config.json'), {});
    write(path.join(serialRuntime, 'runtime_metadata.json'), { repo_path: serialRepo });
    write(path.join(serialRuntime, 'protocol_state.json'), { cycle: 3,
      in_flight_request: { id: 21, kind: 'Worker', cycle: 3, active_node: 'Goal' } });
    const serialHistory = path.join(serialRepo, '.trellis-history/event-log/cycle-000003.jsonl');
    write(serialHistory, { cycle: 3, index: 1,
      commands: [{ command: 'issue_request', request: { id: 21, kind: 'Worker', cycle: 3 } }] });
    fs.appendFileSync(serialHistory, '\n');
    const serialDirectory = path.join(serialRuntime, 'worker-attempts', '21-0123456789abcdef');
    write(path.join(serialRuntime, 'worker-attempts/current.json'), { directory: path.basename(serialDirectory), request_id: 21 });
    const serialArtifact = path.join(serialRepo, '.trellis/chats/live/trellis_worker_21_result');
    write(path.join(serialArtifact, 'call.json'), { provider: 'codex', model: 'serial-model' });
    fs.writeFileSync(path.join(serialArtifact, 'output.log'), JSON.stringify({ type: 'item.completed',
      item: { type: 'agent_message', text: 'serial worker' } }) + '\n');
    const serial = launch({ directory: serialDirectory, work: serialRepo, state: serialRuntime,
      names: [`trellis-${runtimeNamespace(serialRuntime)}-worker-21-worker`], lanes: ['worker'] }, 1);
    for (let i = 0; i < 60; i++) {
      try {
        if (execFileSync('tmux', ['-L', serial.socket, 'ls', '-F', '#S'],
          { encoding: 'utf8', stdio: ['ignore', 'pipe', 'ignore'] }).includes(serial.names[0])) break;
      } catch {}
      await new Promise(resolve => setTimeout(resolve, 50));
    }
    const serialProject = { repoPath: serialRepo, stateDir: path.join(serialRepo, '.trellis'), repoType: 'trellis' };
    await waitLive(serialProject, 'cycle:live');
    const serialLive = await sessionViews.live(serialProject);
    assert.deepStrictEqual(serialLive.warnings, []);
    assert.strictEqual(serialLive.calls.length, 1);
    assert.strictEqual(serialLive.calls[0].tmux_socket, serial.socket);
    assert.strictEqual((await sessionViews.events(serialProject, 'cycle:live', serialLive.calls[0].key)).events[0].text, 'serial worker');
    write(path.join(serialRuntime, 'worker-attempts/current.json'), { directory: '../escape' });
    assert.strictEqual((await sessionViews.live(serialProject)).calls.length, 0);
    // No retained transcript, idle shell or sibling runtime implies a live row.
    const other = { ...project, repoPath: path.join(root, 'other'), stateDir: path.join(root, 'other/.trellis') };
    write(path.join(other.repoPath, 'trellis.config.json'), {});
    write(path.join(other.repoPath + '-runtime', 'runtime_metadata.json'), { repo_path: other.repoPath });
    assert.strictEqual((await sessionViews.live(other)).calls.length, 0);
    console.log('production per-attempt socket discovery, lane identity, model metadata, history and TUI tests passed');
  } finally {
    for (const execution of launches) {
      try { python(`import json\nfrom pathlib import Path\nfrom trellis.dispatch.process import AttemptProcess\np=Path(${JSON.stringify(execution)})\nAttemptProcess(p).stop_and_reap(json.loads((p/'process.json').read_text()))`); }
      catch (error) { console.error('fixture cleanup failed:', error.message); process.exitCode = 1; }
    }
    try { execFileSync('tmux', ['-L', process.env.TRELLIS_TMUX_SOCKET, 'kill-server']); } catch {}
    fs.rmSync(root, { recursive: true, force: true });
  }
})().catch(error => { console.error(error); process.exitCode = 1; });
