// Called by the synthetic native-controller fixture while its owner is waiting.
// The real HTTP route and Python adapter may publish only that fixture's permit.
const assert = require('node:assert/strict');
const fs = require('node:fs');
const os = require('node:os');
const path = require('node:path');
const http = require('node:http');

const repo = fs.realpathSync(process.argv[2]);
assert(repo.startsWith(fs.realpathSync(os.tmpdir()) + path.sep), 'integration requires a temporary fixture');
const retry = JSON.parse(fs.readFileSync(process.argv[3], 'utf8'));
process.env.PROJECTS_ROOT = path.dirname(repo);
process.env.STATIC_OUT = path.join(path.dirname(repo), 'synthetic-viewer-static');
const { app, CONTROL_HEADER, ensureControlToken } = require('./server');
const token = ensureControlToken(process.env.PROJECTS_ROOT);

function post(server, body, authorized = true) {
  const headers = { 'Content-Type': 'application/json' };
  if (authorized) headers[CONTROL_HEADER] = token;
  return new Promise((resolve, reject) => {
    const request = http.request({ host: '127.0.0.1', port: server.address().port,
      path: `/trellis/${path.basename(repo)}/api/control/pause/transport-resume`, method: 'POST', headers }, response => {
      let raw = ''; response.on('data', part => raw += part);
      response.on('end', () => resolve({ status: response.statusCode, body: JSON.parse(raw) }));
    });
    request.on('error', reject); request.end(JSON.stringify(body));
  });
}

async function main() {
  const server = app.listen(0, '127.0.0.1');
  await new Promise(resolve => server.once('listening', resolve));
  try {
    assert.notEqual((await post(server, retry, false)).status, 200);
    const stale = await post(server, { ...retry, evidence_sha256: '0'.repeat(64) });
    assert.equal(stale.status, 409);
    const accepted = await post(server, retry);
    assert.equal(accepted.status, 200, JSON.stringify(accepted.body));
    assert.equal(accepted.body.permit_issued, true);
    const duplicate = await post(server, retry);
    assert([200, 409].includes(duplicate.status));
    if (duplicate.status === 200) assert.equal(duplicate.body.permit_issued, true);
    console.log(JSON.stringify({ ok: true, request_id: retry.request_id, turn: retry.turn,
      ordinal: retry.ordinal, http_status: accepted.status, stale_status: stale.status,
      duplicate_status: duplicate.status, uid: process.getuid() }));
  } finally { await new Promise(resolve => server.close(resolve)); }
}
main().catch(error => { console.error(error); process.exitCode = 1; });
