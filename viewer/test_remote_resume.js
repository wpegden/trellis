'use strict';
const assert = require('assert');
const { resumeCreateArgs } = require('./remote_resume');
const body = { slug: 'continued', kind: 'resume', remote_url: 'git@example.org:owner/tablet.git',
  handoff_confirmed: true, lean_parallelism: 3 };
assert.deepStrictEqual(resumeCreateArgs(body), ['start-resume', 'continued', '--remote-url', body.remote_url,
  '--handoff-confirmed', '--lean-parallelism', '3']);
assert(!resumeCreateArgs(body).includes('--paper'));
assert.throws(() => resumeCreateArgs({ ...body, handoff_confirmed: false }), /previous writer/);
assert.throws(() => resumeCreateArgs({ ...body, remote_url: 'https://secret@example.org/repo' }), /credentials/);
assert.throws(() => resumeCreateArgs({ ...body, lean_parallelism: 0 }), /positive integer/);
assert.throws(() => resumeCreateArgs({ ...body, branch: '--upload-pack=evil' }), /branch/);
console.log('remote resume viewer inputs: passed');

// Exercise the authenticated route ordering: resume requires neither paper
// upload nor the new-run Loogle decision, and remains under the control token.
const fs = require('fs');
const os = require('os');
const path = require('path');
const http = require('http');
const root = fs.mkdtempSync(path.join(os.tmpdir(), 'trellis-resume-viewer-'));
process.env.PROJECTS_ROOT = root;
process.env.STATIC_OUT = path.join(root, 'static');
delete process.env.TRELLIS_RESUME_PROFILE;
const server = require('./server');
function request(listener, method, url, token, value) {
  return new Promise((resolve, reject) => {
    const body = value ? JSON.stringify(value) : '';
    const req = http.request({ hostname:'127.0.0.1', port:listener.address().port, method, path:url,
      headers:{ 'Content-Type':'application/json', ...(token ? {[server.CONTROL_HEADER]:token} : {}) } }, res => {
      let data=''; res.on('data', chunk => { data += chunk; });
      res.on('end', () => resolve({status:res.statusCode, body:data}));
    });
    req.on('error', reject); req.end(body);
  });
}
(async () => {
  const listener = server.app.listen(0, '127.0.0.1');
  await new Promise(resolve => listener.once('listening', resolve));
  try {
    const token = server.ensureControlToken(root);
    const denied = await request(listener, 'POST', '/trellis/api/control/create-jobs', null, body);
    assert.strictEqual(denied.status, 403);
    const response = await request(listener, 'POST', '/trellis/api/control/create-jobs', token, body);
    assert.strictEqual(response.status, 400);
    assert.match(response.body, /TRELLIS_RESUME_PROFILE/);
    assert.doesNotMatch(response.body, /loogle_required|upload_missing/);
    const landing = await request(listener, 'GET', `/trellis/${token}/`, null);
    assert.strictEqual(landing.status, 200);
    assert.match(landing.body, /Resume run/);
    assert.doesNotMatch(landing.body, /<script src="remote_resume_client/);
    console.log('remote resume authenticated route: passed');
  } finally {
    await new Promise(resolve => listener.close(resolve));
    fs.rmSync(root, {recursive:true, force:true});
  }
})().catch(error => { console.error(error); process.exitCode=1; });
