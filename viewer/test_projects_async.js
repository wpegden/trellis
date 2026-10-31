const assert = require('assert');
const fs = require('fs');
const http = require('http');
const os = require('os');
const path = require('path');
const cp = require('child_process');

const root = fs.mkdtempSync(path.join(os.tmpdir(), 'trellis-viewer-projects-async-'));
process.env.PROJECTS_ROOT = root;
process.env.STATIC_OUT = path.join(root, 'static');
process.env.TRELLIS_VIEWER_READ_TOKEN = '0';
process.env.TRELLIS_VIEWER_BIND = '127.0.0.1';
for (const slug of ['alpha', 'beta']) {
  const repo = path.join(root, slug), runtime = `${repo}-runtime`;
  fs.mkdirSync(repo);
  fs.mkdirSync(runtime);
  fs.writeFileSync(path.join(repo, 'trellis.config.json'), '{}');
  fs.writeFileSync(path.join(runtime, 'runtime_metadata.json'), JSON.stringify({ repo_path: repo }));
  fs.writeFileSync(path.join(runtime, 'protocol_state.json'), JSON.stringify({
    phase: 'ProofFormalization', stage: 'Worker', cycle: 3,
  }));
}

const originalExec = cp.execFile;
const originalSync = cp.execFileSync;
const originalNow = Date.now;
const probes = [];
let clock = originalNow();
Date.now = () => clock;
cp.execFile = (command, args, options, callback) => {
  assert.strictEqual(command, 'bash');
  assert.strictEqual(path.basename(args[0]), 'trellis_pause.sh');
  assert.strictEqual(args[1], 'status');
  probes.push({ slug: path.basename(args[3]), callback });
};
cp.execFileSync = (command, args, ...rest) => {
  assert(!args.some(arg => String(arg).endsWith('trellis_pause.sh')),
    'HTTP project cards must not fall back to synchronous pause probes');
  return originalSync(command, args, ...rest);
};
const { app } = require('./server');

async function waitFor(predicate) {
  for (let i = 0; i < 1000; i++) {
    if (predicate()) return;
    await new Promise(resolve => setTimeout(resolve, 2));
  }
  throw new Error('expected asynchronous status probe did not start');
}

const server = app.listen(0, '127.0.0.1', async () => {
  function get(url) {
    return new Promise((resolve, reject) => {
      const request = http.get({ host: '127.0.0.1', port: server.address().port, path: url }, res => {
        let body = '';
        res.on('data', chunk => { body += chunk; });
        res.on('end', () => resolve({ status: res.statusCode, payload: JSON.parse(body) }));
      }).on('error', reject);
      request.setTimeout(2000, () => request.destroy(new Error('HTTP request stalled')));
    });
  }
  try {
    let finished = false;
    const first = get('/trellis/api/projects.json').then(value => { finished = true; return value; });
    const second = get('/trellis/api/projects.json');
    await waitFor(() => probes.length === 1);
    assert.strictEqual(probes[0].slug, 'alpha');
    const health = await get('/trellis/api/health.json');
    assert.strictEqual(health.status, 200);
    assert.strictEqual(finished, false, 'health must respond while project status is pending');
    probes[0].callback(null, JSON.stringify({ state: 'running', wrapper_pid: 123 }));
    await waitFor(() => probes.length === 2);
    assert.strictEqual(probes[1].slug, 'beta');
    // The first sample expires while the second project is being inspected.
    // Assembly must retain that sample, never start a synchronous re-probe.
    clock += 10000;
    probes[1].callback(new Error('status unavailable'));
    const [a, b] = await Promise.all([first, second]);
    assert.strictEqual(a.status, 200);
    assert.deepStrictEqual(a, b, 'concurrent landing requests share one status sweep');
    assert.strictEqual(probes.length, 2, 'each project is probed only once');
    const cards = Object.fromEntries(a.payload.projects.map(card => [card.slug, card]));
    assert.strictEqual(cards.alpha.pause_state, 'running');
    assert.strictEqual(cards.beta.pause_state, 'unknown', 'failed status must not report a stopped supervisor');
    assert.strictEqual(cards.beta.lifecycle, 'unknown');
    console.log('async projects tests passed');
  } catch (error) {
    console.error(error);
    process.exitCode = 1;
  } finally {
    server.close();
    cp.execFile = originalExec;
    cp.execFileSync = originalSync;
    Date.now = originalNow;
    fs.rmSync(root, { recursive: true, force: true });
  }
});
