const assert = require('assert');
const fs = require('fs');
const os = require('os');
const path = require('path');
const { parallelState } = require('./parallel_dispatch');
const root = fs.mkdtempSync(path.join(os.tmpdir(), 'trellis-pipeline-viewer-'));
const task = (id, status, released, lightweight) => ({ id, status: { status },
  execution_capacity_released: released,
  binding: { model: lightweight ? 'gpt-6-luna' : 'gpt-6.1-sol', lightweight },
  assignment: { profile: lightweight ? 'light' : 'heavy', scope: {} } });
async function project(config, tasks) {
  fs.writeFileSync(path.join(root, 'protocol_state.json'), JSON.stringify({
    dispatch: { config, tasks: Object.fromEntries(tasks.map(t => [t.id, t])) },
  }));
  return parallelState(root);
}
(async () => { try {
  const config = { worker_slots: 3, heavy_worker_slots: 1, max_in_flight_tasks: 5 };
  let tasks = [task(1, 'checking', true, false), task(2, 'running', false, false)];
  let result = await project(config, tasks);
  assert.strictEqual(result.agent_limits.active, 1);
  assert.strictEqual(result.agent_limits.active_heavy, 1);
  assert.strictEqual(result.in_flight_tasks, 2);
  assert.strictEqual(result.available_slots, 2);
  tasks[0].status.status = 'reserved';
  result = await project(config, tasks);
  assert.strictEqual(result.available_slots, 2, 'blocked heavy repair leaves light capacity');
  tasks[1].status.status = 'stopping';
  tasks[1].execution_capacity_released = true;
  result = await project(config, tasks);
  assert.strictEqual(result.agent_limits.active, 0);
  assert.strictEqual(result.available_slots, 0, 'ready correction precedes fresh admission');
  tasks[0].status.status = 'checking';
  result = await project({ ...config, max_in_flight_tasks: 3 },
    [...tasks, task(3, 'checking', false, true)]);
  assert.strictEqual(result.agent_limits.active, 1, 'legacy checking retains its reservation');
  assert.strictEqual(result.available_slots, 0, 'checking backlog is bounded');
  result = await project({ ...config, max_in_flight_tasks: null }, tasks);
  assert.strictEqual(result.agent_limits.active, 2, 'legacy policy counts all unresolved tasks');
  assert.strictEqual(result.available_slots, 1);
  const legacyCap = { worker_slots: 3, sol_worker_slots: 1, max_in_flight_tasks: 5 };
  tasks = [task(1, 'reserved', true, false), task(2, 'running', false, false)];
  tasks[1].binding.model = 'custom-SOL-model';
  result = await project(legacyCap, tasks);
  assert.strictEqual(result.available_slots, 2, 'Sol cap uses exact case-insensitive model component');
  console.log('parallel pipeline viewer checks passed');
} finally { fs.rmSync(root, { recursive: true, force: true }); }
})().catch(error => { console.error(error); process.exitCode = 1; });
