const assert = require('assert');
const fs = require('fs');
const os = require('os');
const path = require('path');
const { execFileSync } = require('child_process');
const { progressForCommit } = require('./server');

const repo = fs.mkdtempSync(path.join(os.tmpdir(), 'trellis-substantiveness-progress-'));
function git(...args) {
  return execFileSync('git', ['-C', repo, ...args], { encoding: 'utf8' }).trim();
}
try {
  git('init', '-q');
  git('config', 'user.name', 'Viewer test');
  git('config', 'user.email', 'viewer-test@example.invalid');
  fs.mkdirSync(path.join(repo, '.trellis-history'));
  const state = {
    phase: 'TheoremStating',
    node_kinds: { Assumptions: 'Definition', Rejected: 'Proof', Pending: 'Proof', Accepted: 'Proof' },
    node_role: { Assumptions: 'under_model_assumptions' },
    substantiveness_status: { Rejected: 'FalseAsStated', Accepted: 'Pass' },
    substantiveness_approved_fingerprints: { Rejected: 'fp', Accepted: 'fp' },
    live: { substantiveness_current_fingerprints: { Rejected: 'fp', Accepted: 'fp' } },
  };
  function progress() {
    fs.writeFileSync(path.join(repo, '.trellis-history/supervisor_state.json'), JSON.stringify({ state }));
    git('add', '.');
    git('commit', '-qm', 'Checkpoint');
    return progressForCommit(repo, git('rev-parse', 'HEAD'));
  }
  const initial = progress();
  assert.strictEqual(initial.all.total, 4);
  assert.strictEqual(initial.all.substantiveness_passing, 2);
  assert.strictEqual(initial.all_proofs_only.substantiveness_passing, 1);
  assert.strictEqual(initial.all.corr_passing, 0); // exemption is lane-specific

  // The name alone is insufficient, and stale approvals never pass.
  state.node_role = {};
  state.live.substantiveness_current_fingerprints.Accepted = 'changed';
  assert.strictEqual(progress().all.substantiveness_passing, 0);

  // Nor may a helper acquire the exemption by copying only the role.
  state.node_role = { Pending: 'under_model_assumptions' };
  assert.strictEqual(progress().all.substantiveness_passing, 0);
} finally {
  fs.rmSync(repo, { recursive: true, force: true });
}
console.log('viewer substantiveness progress tests passed');
