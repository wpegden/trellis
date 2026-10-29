'use strict';
// Read-only session navigation. A transcript directory is retained evidence,
// never a liveness signal. Live rows require a running, exactly owned tmux pane.
const fs = require('fs');
const path = require('path');
const crypto = require('crypto');
const { execFileSync } = require('child_process');

function read(file) {
  try { return JSON.parse(fs.readFileSync(file, 'utf8')); } catch { return null; }
}
const requestCache = new Map();
function requestMetadata(file) {
  try {
    const stat = fs.statSync(file), signature = `${stat.ino}:${stat.size}:${stat.mtimeMs}:${stat.ctimeMs}`;
    const cached = requestCache.get(file);
    if (cached?.signature === signature) return cached.value;
    const full = read(file);
    if (!full) return null;
    const value = Object.fromEntries(['request_id', 'kind', 'provider', 'session_name',
      'artifact_prefix', 'artifact', 'log_dir'].map(key => [key, full[key]]));
    requestCache.set(file, { signature, value });
    if (requestCache.size > 256) requestCache.delete(requestCache.keys().next().value);
    return value;
  } catch { return null; }
}
function names(dir) {
  try { return fs.readdirSync(dir, { withFileTypes: true }).filter(e => e.isDirectory()).map(e => e.name); }
  catch { return []; }
}
function runtimeNamespace(root) {
  if (path.basename(path.dirname(path.dirname(root))) === 'dispatch') {
    const owner = path.dirname(path.dirname(path.dirname(root)));
    const identity = crypto.createHash('sha256').update(fs.realpathSync(root)).digest('hex').slice(0, 10);
    return `${runtimeNamespace(owner).slice(0, 22)}-task-${path.basename(path.dirname(root))}-${identity}`;
  }
  const raw = path.basename(root).replace(/-runtime$/, '') || 'runtime';
  return raw.replace(/[^\p{L}\p{N}_-]/gu, '-').replace(/^[-_]+|[-_]+$/g, '').slice(0, 48) || 'runtime';
}
function socketForExecution(execution) {
  // Same identity as trellis.dispatch.process.socket_name(execution).
  return 'trellis-task-' + crypto.createHash('sha256').update(execution).digest('hex').slice(0, 24);
}
function activeExecutions(directory) {
  const turns = path.join(directory, 'turns');
  return [path.join(directory, 'execution'), ...names(turns).filter(n => /^[1-9][0-9]*$/.test(n))
    .sort((a, b) => Number(a) - Number(b)).map(n => path.join(turns, n, 'execution'))]
    .filter(execution => {
      try { return fs.realpathSync(execution) === execution && launcherAlive(execution); }
      catch { return false; }
    });
}
function serialExecutions(runtime) {
  try {
    const root = fs.realpathSync(runtime), attempts = path.join(root, 'worker-attempts');
    const current = read(path.join(attempts, 'current.json'));
    if (!/^[0-9]+-[a-f0-9]{16}$/.test(current?.directory || '')) return [];
    const directory = path.join(attempts, current.directory);
    return fs.realpathSync(directory) === directory ? activeExecutions(directory) : [];
  } catch { return []; }
}
function socketForRuntime(root) {
  if (root && path.basename(path.dirname(path.dirname(root))) === 'dispatch') {
    const directory = fs.realpathSync(path.dirname(root));
    return socketForExecution(activeExecutions(directory).at(-1) || path.join(directory, 'execution'));
  }
  const execution = root && serialExecutions(root).at(-1);
  if (execution) return socketForExecution(execution);
  return process.env.TRELLIS_TMUX_SOCKET || 'trellis';
}
function launcherAlive(execution) {
  const record = read(path.join(execution, 'process.json'));
  const permit = read(path.join(execution, 'permit.json'));
  if (!record || !permit || !Number.isSafeInteger(record.pid) || record.pid <= 0
      || ['pid', 'start_ticks', 'boot_id'].some(k => record[k] !== permit[k])
      || fs.existsSync(path.join(execution, 'cleanup.json'))) return false;
  try {
    const stat = fs.readFileSync(`/proc/${record.pid}/stat`, 'utf8');
    const fields = stat.slice(stat.lastIndexOf(')') + 2).split(/\s+/);
    return fields[0] !== 'Z' && fields[19] === String(record.start_ticks)
      && fs.readFileSync('/proc/sys/kernel/random/boot_id', 'utf8').trim() === record.boot_id;
  } catch { return false; }
}
function processEvidence(pid) {
  const queue = [Number(pid)], seen = new Set(), paths = [];
  let active = false, provider = null, model = null;
  while (queue.length && seen.size < 256) {
    const current = queue.shift();
    if (!current || seen.has(current)) continue;
    seen.add(current);
    try {
      const args = fs.readFileSync(`/proc/${current}/cmdline`, 'utf8').split('\0').filter(Boolean);
      paths.push(...args.filter(arg => path.isAbsolute(arg)));
      // Mount arguments can end in /codex too. Only the executable or an
      // interpreter's script identifies a provider; its child may add the model.
      const program = /^(?:node|nodejs|bun|bash|sh|dash|zsh|python[\d.]*)$/.test(path.basename(args[0] || ''))
        ? args[1] : args[0];
      const agent = /(?:^|\/)(codex|claude|gemini)(?:\.[a-z]+)?$/.exec(program || '');
      if (agent || args.some(arg => /-burst\.sh$/.test(arg))) active = true;
      if (agent && (!provider || provider === agent[1])) {
        provider = agent[1];
        const at = args.findIndex(arg => arg === '--model' || arg === '-m');
        model ||= at >= 0 ? args[at + 1] || null : null;
      }
      queue.push(...fs.readFileSync(`/proc/${current}/task/${current}/children`, 'utf8').trim().split(/\s+/).map(Number));
    } catch { /* Processes can finish between the tmux and proc reads. */ }
  }
  return { paths, active, provider, model };
}
const paneCache = new Map();
function livePanes(socket = socketForRuntime(null)) {
  const cached = paneCache.get(socket);
  if (cached && Date.now() - cached.time < 2000) return cached.rows;
  let rows = [];
  try {
    rows = execFileSync('tmux', ['-L', socket,
      'list-panes', '-a', '-F', '#{session_name}\t#{pane_dead}\t#{pane_current_path}\t#{pane_start_path}\t#{pane_pid}'],
    { encoding: 'utf8', timeout: 3000, stdio: ['ignore', 'pipe', 'ignore'] })
      .trim().split('\n').map(line => {
        const [name, dead, cwd, start, pid] = line.split('\t');
        return { name, dead: dead !== '0', cwd, start, pid, socket };
      }).filter(p => !p.dead && /^trellis-/.test(p.name) && !/^trellis-(?:checker|create|run|viewer)[-_]/.test(p.name))
      .map(p => ({ ...p, ...processEvidence(p.pid) })).filter(p => p.active);
  } catch {}
  paneCache.set(socket, { time: Date.now(), rows });
  if (paneCache.size > 128) paneCache.delete(paneCache.keys().next().value);
  return rows;
}
function ownedPane(pane, repo) {
  if (!pane.active) return false;
  return [...(pane.paths || []), pane.cwd, pane.start].some(cwd => {
    if (!cwd) return false;
    try {
      let resolved;
      try { resolved = fs.realpathSync(cwd); } catch { resolved = path.resolve(cwd); }
      const relative = path.relative(fs.realpathSync(repo), resolved);
      return relative === '' || (!relative.startsWith('..' + path.sep) && relative !== '..' && !path.isAbsolute(relative));
    } catch { return false; }
  });
}
function modified(file) {
  try { return fs.statSync(file).mtimeMs; } catch { return null; }
}
const ATTEMPT = /^(?:[1-9][0-9]*|(?:review|verifier|exclusive)-[0-9]+)$/;
const REVISION = /^\d+-[a-z_]+$/;

function create(d) {
  function attemptPanes(runtime) {
    if (!runtime) return [];
    let dispatch;
    try { dispatch = fs.realpathSync(path.join(runtime, 'dispatch')); } catch { return []; }
    return names(dispatch).filter(name => ATTEMPT.test(name)).flatMap(attempt => {
      try {
        const project = d.parallelDispatch.attemptProject(runtime, attempt);
        if (!activeExecutions(path.dirname(project.runtimeRoot)).length) return [];
        return livePanes(socketForRuntime(project.runtimeRoot));
      } catch { return []; } // Finished or removed during discovery.
    });
  }
  function revisionProject(project) {
    const runtime = d.runtimeRootForProject(project);
    if (!runtime) return null;
    const repoPath = path.join(runtime, 'pv-revision');
    return fs.existsSync(path.join(repoPath, 'state.json'))
      ? { repoPath, stateDir: path.join(repoPath, '.trellis'), repoType: 'trellis', protocolSnapshot: null } : null;
  }
  function requestCall(project, artifact, request, role) {
    const meta = d.readLiveCallJson(project.stateDir, artifact);
    return { artifact_id: artifact, request_id: request?.request_id ?? null,
      kind: request?.kind || role, role, provider: meta?.provider || request?.provider?.provider || null,
      model: meta?.model || request?.provider?.model || null,
      started_at_ms: meta?.started_at_ms || null, ended_at_ms: meta?.ended_at_ms || null,
      phase: request?.artifact?.phase || null, session_id: meta?.session_id || null,
      has_transcript: d.chatDirHasTranscript(project, 'live', artifact), tmux_session: null };
  }
  function revisionCalls(project) {
    const revision = revisionProject(project);
    if (!revision) return [];
    return names(path.join(revision.repoPath, 'requests')).filter(name => REVISION.test(name))
      .sort().reverse().flatMap(name => {
        const request = requestMetadata(path.join(revision.repoPath, 'requests', name, 'request.json'));
        if (!request || request.artifact_prefix !== `revision-${name}`) return [];
        const role = name.replace(/^\d+-/, '');
        return [{ project: revision, request, group: `revision:${name}`,
          completed: fs.existsSync(path.join(revision.repoPath, 'requests', name, 'execution.json')),
          call: requestCall(revision, request.artifact_prefix, request, role) }];
      });
  }
  function preparationCalls(project) {
    const preparation = d.preparationChatProject(project);
    if (!preparation) return [];
    let files = [];
    try { files = fs.readdirSync(path.join(preparation.stateDir, 'modeling', 'requests')); } catch {}
    return files.sort().reverse().flatMap(name => {
      const match = /^(preparation-\d+-([a-z_]+))\.request\.json$/.exec(name);
      if (!match) return [];
      const request = requestMetadata(path.join(preparation.stateDir, 'modeling', 'requests', name));
      if (!request) return [];
      return [{ project: preparation, request, group: `preparation:${match[1]}`,
        completed: !preparation.preparation_live,
        call: requestCall(preparation, match[1], request, match[2]) }];
    });
  }
  async function attemptCalls(project, attempt) {
    const privateProject = d.parallelDispatch.attemptProject(d.runtimeRootForProject(project), attempt);
    const snapshot = await d.chatProjectSnapshot(privateProject, 'live');
    return { project: snapshot, calls: d.buildChatCalls(snapshot, 'live', { showStale: true }).calls };
  }
  function decorate(call, group, label) {
    const key = `${group}|${call.artifact_id || ''}|${call.request_id || ''}|${call.lane || ''}`;
    return { ...call, key, group, group_label: label };
  }
  async function live(project, panes) {
    const runtime = d.runtimeRootForProject(project);
    panes ??= [...livePanes(), ...attemptPanes(runtime),
      ...(runtime ? serialExecutions(runtime).flatMap(execution => livePanes(socketForExecution(execution))) : [])];
    const calls = [];
    const warnings = [];
    if (runtime) {
      const namespace = runtimeNamespace(runtime);
      const projects = new Map();
      for (const pane of panes) {
        const parsed = d.parseBurstSessionName(pane.name);
        if (!parsed) continue;
        if (parsed.runtime === namespace && ownedPane(pane, project.repoPath)) {
          projects.set('cycle:live', { project, label: 'Current cycle', socket: pane.socket || socketForRuntime(null) });
        } else if (parsed.runtime.startsWith(`${namespace.slice(0, 22)}-task-`)) {
          const match = /-task-(.+)-[a-f0-9]{10}$/.exec(parsed.runtime);
          if (!match || !ATTEMPT.test(match[1])) continue;
          try {
            const info = d.parallelDispatch.attemptProject(runtime, match[1]);
            if (pane.socket && pane.socket !== socketForRuntime(info.runtimeRoot)) continue;
            if (parsed.runtime === runtimeNamespace(info.runtimeRoot) && ownedPane(pane, info.repoPath)) {
              projects.set(`attempt:${match[1]}`, { project: info, label: attemptLabel(match[1]), socket: pane.socket || socketForRuntime(null) });
            }
          } catch (error) { warnings.push(`${attemptLabel(match[1])}: ${error.message}`); }
        }
      }
      // Only running attempts need a compact protocol read. Never walk private
      // historical snapshots (some are hundreds of MB) during a live poll.
      await Promise.all([...projects].map(async ([group, entry]) => {
        try {
          const snapshot = await d.chatProjectSnapshot(entry.project, 'live');
          const matching = new Map(panes.filter(p => (p.socket || socketForRuntime(null)) === entry.socket
            && ownedPane(p, entry.project.repoPath)).map(p => [p.name, p]));
          for (const call of d.buildChatCalls(snapshot, 'live', { showStale: true,
            tmuxSocket: entry.socket, tmuxSessions: [...matching.keys()] }).calls) {
            const pane = matching.get(call.tmux_session);
            if (pane) calls.push(decorate({ ...call, tmux_socket: entry.socket,
              provider: call.provider || pane.provider, model: call.model || pane.model }, group, entry.label));
          }
        } catch (error) { warnings.push(`${entry.label}: ${error.message}`); }
      }));
    }
    for (const entry of [...revisionCalls(project), ...preparationCalls(project)]) {
      if (entry.completed || entry.call.ended_at_ms) continue;
      const pane = panes.find(p => ownedPane(p, entry.project.repoPath)
        && (p.name === entry.request.session_name || /^-r\d+$/.test(p.name.slice(entry.request.session_name.length))
          && p.name.startsWith(entry.request.session_name)));
      if (!pane) continue;
      calls.push(decorate({ ...entry.call, tmux_session: pane.name, tmux_socket: pane.socket || socketForRuntime(null) }, entry.group,
        entry.group.startsWith('revision:') ? `Revision #${Number(entry.group.slice(9).split('-')[0])}`
          : `Preparation · ${entry.call.phase || entry.call.role}`));
    }
    return { calls: calls.sort((a, b) => a.group.localeCompare(b.group) || String(a.lane).localeCompare(String(b.lane))), warnings };
  }
  function attemptLabel(attempt) {
    const [role, id] = attempt.split('-');
    return id ? `${({ review: 'Reviewer', verifier: 'Verifier', exclusive: 'Worker' })[role]} #${id}` : `Task #${attempt}`;
  }
  async function groups(project) {
    const runtime = d.runtimeRootForProject(project);
    const snapshot = runtime ? await d.parallelDispatch.readState(path.join(runtime, 'protocol_state.json')) : null;
    const ledger = snapshot?.dispatch;
    const rows = [];
    const attempts = runtime ? names(path.join(runtime, 'dispatch')).filter(name => ATTEMPT.test(name)) : [];
    const tasks = ledger?.tasks || {};
    const grouped = new Map();
    const verifiers = new Map(Object.values(ledger?.verifiers?.jobs || {}).map(job => [`verifier-${job.id}`, job]));
    const assignments = new Map((ledger?.receipts || []).flatMap(receipt =>
      (receipt.decisions || []).filter(decision => decision.task_id != null).map(decision => [decision.task_id, receipt.review_id])));
    for (const attempt of attempts) {
      const task = tasks[attempt];
      const verifier = verifiers.get(attempt);
      let root = task;
      const seen = new Set();
      while (root?.retry_of != null && tasks[root.retry_of] && !seen.has(root.id)) {
        seen.add(root.id); root = tasks[root.retry_of];
      }
      const groupId = task ? `task:${root.id}` : `attempt:${attempt}`;
      const category = task ? 'Tasks' : attempt.startsWith('review-') ? 'Reviewers'
        : attempt.startsWith('verifier-') ? 'Verifiers' : 'Workers';
      const timestamp = task?.launched_at_ms || modified(path.join(runtime, 'dispatch', attempt, 'prepared.json'));
      const status = task?.status || verifier?.status;
      const outcome = status?.outcome || status?.resolution || status?.status
        || (fs.existsSync(path.join(runtime, 'dispatch', attempt, 'execution', 'result.json')) ? 'finished' : 'retained attempt');
      const nodes = task?.assignment?.scope?.nodes || [];
      const option = { id: `attempt:${attempt}`, label: attemptLabel(attempt), category,
        model: task?.binding?.model || '', role: task?.assignment?.task_kind || verifier?.identity?.lane || category,
        started_at_ms: timestamp, ended_at_ms: status?.resolved_at_ms || null,
        outcome: typeof outcome === 'string' ? outcome : outcome.kind || outcome.status || 'resolved',
        nodes, retry_of: task?.retry_of ?? null,
        assignment_review: task && assignments.get(task.id) || null,
        report_summary: task?.report?.summary || '',
        rejection_reasons: task?.report?.rejection_reasons || [] };
      if (!option.model) {
        // Read only tiny call metadata for this request. Never load the cloned
        // protocol or assignment payload simply to populate a navigation row.
        const prepared = read(path.join(runtime, 'dispatch', attempt, 'prepared.json'));
        const chatRoot = path.join(runtime, 'dispatch', attempt, 'repo', '.trellis', 'chats', 'live');
        const prefix = prepared?.request_id == null ? null : new RegExp(`^trellis_[a-z_]+_${prepared.request_id}(?:_|$)`);
        const metadata = prefix ? names(chatRoot).filter(name => prefix.test(name)).map(name => read(path.join(chatRoot, name, 'call.json'))).filter(Boolean) : [];
        option.models = [...new Set(metadata.map(call => call.model).filter(Boolean))];
        option.model = option.models.join(' / ');
        option.started_at_ms = metadata.find(call => call.started_at_ms)?.started_at_ms || timestamp;
        option.ended_at_ms = metadata.find(call => call.ended_at_ms)?.ended_at_ms || option.ended_at_ms;
      }
      if (!grouped.has(groupId)) grouped.set(groupId, { ...option, id: groupId,
        label: task ? `Task #${root.id}` : option.label, attempts: [] });
      const group = grouped.get(groupId);
      group.attempts.push(option);
      group.started_at_ms = Math.max(group.started_at_ms || 0, timestamp || 0);
    }
    rows.push(...grouped.values());
    for (const group of rows) group.attempts.sort((a, b) => (b.started_at_ms || 0) - (a.started_at_ms || 0));
    for (const entry of [...revisionCalls(project), ...preparationCalls(project)]) {
      const revision = entry.group.startsWith('revision:');
      const number = entry.call.artifact_id.match(/-(\d+)-/)[1];
      const option = { id: entry.group, label: `${revision ? 'Revision' : 'Preparation'} #${Number(number)} · ${entry.call.role.replace(/_/g, ' ')}`,
        category: revision ? 'Revision' : 'Preparation', role: entry.call.role, model: entry.call.model,
        phase: entry.call.phase, started_at_ms: entry.call.started_at_ms || modified(path.join(entry.project.stateDir, 'chats/live', entry.call.artifact_id)),
        ended_at_ms: entry.call.ended_at_ms, outcome: entry.call.ended_at_ms || entry.completed ? 'finished' : 'retained request' };
      rows.push({ ...option, attempts: [option] });
    }
    rows.sort((a, b) => (b.started_at_ms || 0) - (a.started_at_ms || 0) || b.label.localeCompare(a.label, undefined, { numeric: true }));
    // Cycles remain their own section. Parallel tasks are navigated by task
    // and attempt, not flattened into the canonical cycle dropdown.
    const cycles = [];
    try {
      const repo = path.join(project.stateDir, 'chats');
      const tags = execFileSync('git', ['-C', repo, 'tag', '--list', 'cycle-*'], { encoding: 'utf8', timeout: 3000, stdio: ['ignore', 'pipe', 'ignore'] });
      for (const tag of tags.split('\n')) {
        const match = /^cycle-(\d+)$/.exec(tag);
        if (match) cycles.push(Number(match[1]));
      }
    } catch {}
    for (const cycle of ['live', ...[...new Set(cycles)].sort((a, b) => b - a)]) {
      const option = { id: `cycle:${cycle}`, category: 'Cycles', label: cycle === 'live'
        ? `Current cycle${snapshot?.cycle == null ? '' : ` · ${snapshot.cycle}`}` : `Cycle ${cycle}`,
        provenance: cycle === 'live' ? 'Current cycle working files' : `Git tag cycle-${cycle}` };
      rows.push({ ...option, attempts: [option] });
    }
    return { groups: rows, parallel: !!ledger };
  }
  async function selected(project, group) {
    if (group.startsWith('attempt:') && ATTEMPT.test(group.slice(8))) {
      const result = await attemptCalls(project, group.slice(8));
      return { ...result, cycle: 'live', provenance: `Retained private attempt ${group.slice(8)}` };
    }
    if (/^cycle:(live|\d+)$/.test(group)) {
      const cycle = group.slice(6);
      const info = await d.chatProjectSnapshot(project, cycle);
      return { project: info, cycle, calls: d.buildChatCalls(info, cycle).calls,
        provenance: cycle === 'live' ? 'Current cycle working files' : `Immutable Git tag cycle-${cycle}` };
    }
    const entry = [...revisionCalls(project), ...preparationCalls(project)].find(e => e.group === group);
    if (!entry) throw new Error('Transcript group is unavailable');
    return { project: { ...entry.project, protocolSnapshot: null }, cycle: 'live', calls: [entry.call],
      provenance: group.startsWith('revision:') ? `Retained revision request ${group.slice(9)}` : `Retained preparation request ${group.slice(12)}` };
  }
  async function calls(project, group) {
    const value = await selected(project, group);
    return { calls: value.calls.map(call => decorate(call, group, group)), provenance: value.provenance };
  }
  async function events(project, group, key) {
    const value = await selected(project, group);
    // Resolve only a call that belongs to the selected task/request/snapshot;
    // callers cannot use a valid artifact name to read a cloned sibling log.
    const call = value.calls.find(call => decorate(call, group, group).key === key);
    if (!call) throw new Error('This session is no longer available in the selected group');
    if (!call.artifact_id) return { events: [], missing: true };
    const result = d.buildChatEvents(value.project, value.cycle, call.artifact_id, 'transcript', call);
    return { ...result, provenance: value.provenance };
  }
  return { live, groups, calls, events };
}
module.exports = { create, runtimeNamespace, socketForRuntime, livePanes, launcherAlive, ownedPane, processEvidence };
