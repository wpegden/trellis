// Read-only projection of the native ledger. Never expose launch recipes or
// compiler capability records to the browser.
const fs = require('fs');
const path = require('path');
const { execFile } = require('child_process');
const { storageWait } = require('./storage_wait');

function read(file) {
  try { return JSON.parse(fs.readFileSync(file, 'utf8')); } catch { return null; }
}

// Coalesce reads of the same atomically published snapshot. Do not cache read
// failures or mistake an unreadable ledger for a disabled parallel run.
const snapshots = new Map();
async function readState(file) {
  let st, archivedSignature = '';
  const identity = value => `${value.dev}:${value.ino}:${value.size}:${value.mtimeMs}:${value.ctimeMs}`;
  try { st = await fs.promises.stat(file); }
  catch (error) {
    if (error.code !== 'ENOENT') throw error;
    // A retired task keeps only its transcript index. It cannot authorize
    // dispatch; malformed records must not broaden the transcript selection.
    try {
      if (path.basename(file) !== 'protocol_state.json') throw Object.assign(new Error('no display record'), { code: 'ENOENT' });
      st = await fs.promises.stat(path.join(path.dirname(file), 'display-state.json'));
      archivedSignature = ':display';
    } catch (missingDisplay) {
      if (missingDisplay.code !== 'ENOENT') throw missingDisplay;
      // Compatibility with previously archived private snapshots.
      try { st = await fs.promises.stat(file + '.retained.json'); }
      catch (missing) { if (missing.code === 'ENOENT') return null; throw missing; }
      archivedSignature = ':gzip:' + identity(await fs.promises.stat(file + '.gz'));
    }
  }
  const signature = identity(st) + archivedSignature;
  const saved = snapshots.get(file);
  // A replacement snapshot can arrive while the previous projection is still
  // reading. Finish that read before starting another helper for this file.
  if (saved?.pending) {
    await saved.promise;
    return readState(file);
  }
  if (saved?.signature === signature) return saved.promise;
  const entry = { signature, pending: true, promise: new Promise((resolve, reject) => {
    execFile('python3', [path.join(__dirname, 'parallel_state.py'), file],
      { encoding: 'utf8', timeout: 30000, maxBuffer: 16 * 1024 * 1024 }, (error, stdout) => {
        if (error) return reject(error);
        try { resolve(JSON.parse(stdout)); } catch (error) { reject(error); }
      });
  }) };
  snapshots.set(file, entry);
  try { return await entry.promise; }
  catch (error) {
    if (snapshots.get(file) === entry) snapshots.delete(file);
    throw error;
  } finally { entry.pending = false; }
}

function hostCheckBlocked(runtime, task) {
  if (task.status.status !== 'checking') return null;
  const marker = read(path.join(runtime, 'dispatch', String(task.id), 'host-check-blocked.json'));
  if (marker?.version !== 1 || marker.state !== 'parked' || marker.task_id !== task.id
      || marker.candidate_sha256 !== task.status.candidate_sha256 || typeof marker.reason !== 'string') return null;
  return { summary: 'Host check blocked: ' + marker.reason.slice(0, 2000),
    evidence: marker.evidence, slot_retained: true };
}

async function parallelState(runtime, now = Date.now()) {
  if (!runtime) return { enabled: false };
  const state = await readState(path.join(runtime, 'protocol_state.json'));
  const ledger = state?.dispatch;
  if (!ledger) return { enabled: false };
  const rows = Object.values(ledger.tasks || {}).sort((a, b) => b.id - a.id);
  const active = rows.filter(t => t.status.status !== 'resolved');
  const pipelined = ledger.config.max_in_flight_tasks != null;
  const providers = pipelined ? active.filter(t => !t.execution_capacity_released) : active;
  const profiles = ledger.config.profiles || {};
  const classified = Object.keys(profiles).length > 0
    && Object.values(profiles).every(p => typeof p.lightweight === 'boolean');
  const isLight = t => (t.binding.lightweight ?? profiles[t.assignment.profile]?.lightweight) === true;
  const heavy = providers.filter(t => !isLight(t)).length;
  const isSol = t => t.binding.model.split('-').some(part => part.toLowerCase() === 'sol');
  const sol = providers.filter(isSol).length;
  let available = Math.max(0, ledger.config.worker_slots - providers.length);
  if (pipelined) {
    const repairReady = available > 0 && active.some(t => t.status.status === 'reserved'
      && t.execution_capacity_released && (ledger.config.heavy_worker_slots != null
        ? isLight(t) || heavy < ledger.config.heavy_worker_slots
        : ledger.config.sol_worker_slots == null || !isSol(t) || sol < ledger.config.sol_worker_slots));
    available = repairReady ? 0 : Math.min(available, Math.max(0, ledger.config.max_in_flight_tasks - active.length));
  }
  const telemetry = read(path.join(runtime, 'dispatch/telemetry.json')) || {};
  const tasks = rows.filter(t => t.status.status !== 'resolved' || rows.indexOf(t) < 100).map(t => ({
    id: t.id, profile: t.assignment.profile, model: t.binding.model,
    compiler: t.assignment.compiler, task_kind: t.assignment.task_kind,
    scope: t.assignment.scope, status: t.status, retry_of: t.retry_of,
    report: t.report, retry_refusal: t.retry_refusal,
    elapsed_ms: t.launched_at_ms == null ? null : Math.max(0, (t.status.resolved_at_ms ?? now) - t.launched_at_ms),
    timeout_seconds: t.binding.timeout_seconds,
    checker: telemetry.tasks?.[String(t.id)] || null,
    host_check_blocked: hostCheckBlocked(runtime, t),
    provider_retry: t.provider_retry || null,
    attempt: String(t.id),
  }));
  const verifiers = Object.values(ledger.verifiers?.jobs || {}).sort((a,b) => b.id-a.id)
    .filter((j,i) => j.status.status !== 'resolved' || i < 100).map(j => ({
      id: j.id, lane: j.identity.lane, compiler: j.identity.compiler,
      status: j.status, required: j.required, reviewer_requested: j.reviewer_requested,
      attempt: `verifier-${j.id}`,
      ...(j.coarse_nodes ? { coarse_nodes: j.coarse_nodes } : {}),
    }));
  const stop = read(path.join(runtime, 'dispatch/operational-stop.json'));
  return { enabled: true, phase: state.phase, stage: state.stage, cycle: state.cycle,
    ...(state.coarse_goals ? { coarse_goals: state.coarse_goals } : {}),
    capacity: ledger.config, available_slots: available,
    ...(pipelined ? { in_flight_tasks: active.length } : {}),
    agent_limits: { workers: ledger.config.worker_slots,
      heavy: ledger.config.heavy_worker_slots ?? ledger.config.sol_worker_slots ?? ledger.config.worker_slots,
      expected_heavy: ledger.config.heavy_worker_slots ?? null,
      active: providers.length, active_heavy: heavy, configurable: classified },
    draining: ledger.draining, reviewer: ledger.reviewer, tasks, verifiers,
    dispatch_receipts: (ledger.receipts || []).slice().reverse(),
    pausing: read(path.join(runtime, 'pause_request.json')),
    storage_wait: storageWait(runtime),
    operational_stop: stop ? { process: stop.process, returncode: stop.returncode, reason: stop.reason } : null,
  };
}

async function changeLimits(runtime, value) {
  if (!runtime) throw new Error('No parallel runtime is available');
  return new Promise((resolve, reject) => {
    const child = execFile('python3', [path.join(__dirname, 'parallel_control.py'), runtime],
      // Do not kill the lock owner while its native child is committing a
      // checkpoint. A slow control request must finish its transaction.
      { encoding: 'utf8', maxBuffer: 1024 * 1024 }, (error, stdout) => {
        let result;
        try { result = JSON.parse(stdout); } catch { /* use the process diagnostic */ }
        if (error || result?.error) return reject(new Error(result?.error || error.message));
        resolve(result);
      });
    child.stdin.on('error', () => {}); // An early exit is reported by the callback.
    child.stdin.end(JSON.stringify(value));
  });
}

function attemptProject(runtime, attempt) {
  if (!/^(?:[1-9][0-9]*|(?:review|verifier|exclusive)-[0-9]+)$/.test(attempt || '')) {
    throw new Error('invalid parallel attempt');
  }
  const root = fs.realpathSync(path.join(runtime, 'dispatch'));
  const directory = fs.realpathSync(path.join(root, attempt));
  if (path.dirname(directory) !== root) throw new Error('attempt escapes dispatch');
  const repoPath = fs.realpathSync(path.join(directory, 'repo'));
  if (repoPath !== path.join(directory, 'repo')) throw new Error('attempt repository is not local');
  const runtimeRoot = path.join(directory, 'runtime');
  if (read(path.join(runtimeRoot, 'runtime_metadata.json'))?.repo_path !== repoPath) {
    throw new Error('attempt repository identity changed');
  }
  return { repoPath, stateDir: path.join(repoPath, '.trellis'), repoType: 'trellis', runtimeRoot };
}

module.exports = { parallelState, attemptProject, readState, changeLimits };
