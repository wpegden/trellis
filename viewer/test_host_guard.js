// Two defences that cover READS, which the control token never did.
//
//   1. Host validation — the DNS-rebinding defence. A page at evil.com that
//      re-resolves to 127.0.0.1 becomes same-origin with the viewer and can
//      read every run; the one thing it cannot change is the Host header.
//      The allowlist is DERIVED (localhost, loopback literals, this
//      machine's own names, the bind address), so an ordinary install needs
//      no configuration — the failure that got the previous version removed.
//
//   2. Token-gated reads — when the viewer is bound off loopback, a read
//      needs the same `${BASE}/<token>/` prefix the controls use.
//
// The point of both is READS. A check that only guards writes closes nothing
// here, so most of what follows exercises plain GETs of run data.

const assert = require('assert');
const fs = require('fs');
const http = require('http');
const net = require('net');
const os = require('os');
const path = require('path');
const { spawn, spawnSync } = require('child_process');

const root = fs.mkdtempSync(path.join(os.tmpdir(), 'trellis-viewer-host-'));
process.env.PROJECTS_ROOT = root;
process.env.STATIC_OUT = path.join(root, 'static-out');
// One project, so the read endpoints under test have something to disclose
// and the static export has something to write. `trellis.config.json` is what
// makes a directory under the projects root count as a run.
fs.mkdirSync(path.join(root, 'demorun', '.trellis', 'viewer'), { recursive: true });
fs.writeFileSync(path.join(root, 'demorun', 'trellis.config.json'), '{}\n');

const SERVER_PATH = path.join(__dirname, 'server.js');
const {
  app,
  ALLOWED_HOSTS,
  HOST_ENV_VAR,
  READS_REQUIRE_TOKEN,
  READ_TOKEN_MODE,
  bindIsLoopback,
  buildAllowedHosts,
  controlTokenPath,
  healthProbePath,
  hostIsAllowed,
  hostRefusal,
  isLoopbackLiteral,
  machineHostNames,
  writeStatic,
  normalizeHostName,
} = require(SERVER_PATH);

const BASE = process.env.BASE_PATH || '/trellis';

// `setHost: false` lets a case send NO Host header at all (the HTTP/1.0
// case); otherwise Node synthesizes one and that case is untestable.
function get(port, urlPath, hostHeader, opts) {
  return new Promise((resolve, reject) => {
    const headers = {};
    if (hostHeader !== null && hostHeader !== undefined) headers.host = hostHeader;
    if (opts && opts.accept) headers.accept = opts.accept;
    const r = http.request({
      host: '127.0.0.1',
      port,
      method: 'GET',
      path: urlPath,
      headers,
      setHost: false,
    }, res => {
      let data = '';
      res.setEncoding('utf-8');
      res.on('data', c => { data += c; });
      res.on('end', () => resolve({ status: res.statusCode, headers: res.headers, body: data }));
    });
    r.on('error', reject);
    r.end();
  });
}

// A genuinely Host-less request. Node's own parser answers 400 to an
// HTTP/1.1 request with no Host (the header is mandatory in 1.1), so the
// only way to reach Express without one is to speak HTTP/1.0 over a raw
// socket — which is exactly the client the absent-Host allowance is for.
function rawGetHttp10(port, urlPath) {
  return new Promise((resolve, reject) => {
    const sock = net.connect(port, '127.0.0.1', () => {
      sock.write(`GET ${urlPath} HTTP/1.0\r\n\r\n`);
    });
    let data = '';
    sock.setEncoding('utf-8');
    sock.on('data', c => { data += c; });
    sock.on('end', () => {
      const m = /^HTTP\/1\.[01] (\d{3})/.exec(data);
      resolve({ status: m ? Number(m[1]) : 0, body: data });
    });
    sock.on('error', reject);
  });
}

function freePort() {
  return new Promise((resolve, reject) => {
    const srv = net.createServer();
    srv.on('error', reject);
    srv.listen(0, '127.0.0.1', () => {
      const { port } = srv.address();
      srv.close(() => resolve(port));
    });
  });
}

// Derived constants under a given env, read out of a fresh process. The
// values are module-level consts, so a matrix needs one process per row.
function constantsUnder(env) {
  const script = 'const s = require(process.argv[1]);'
    + ' console.log(JSON.stringify({'
    + ' readsRequireToken: s.READS_REQUIRE_TOKEN,'
    + ' readTokenMode: s.READ_TOKEN_MODE,'
    + ' hostCheckDisabled: s.HOST_CHECK_DISABLED,'
    + ' allowed: [...s.ALLOWED_HOSTS].sort() }));';
  const out = spawnSync(process.execPath, ['-e', script, SERVER_PATH], {
    encoding: 'utf-8',
    env: { ...process.env, ...env },
  });
  assert.strictEqual(out.status, 0, `probe failed: ${out.stderr}`);
  return JSON.parse(out.stdout.trim().split('\n').pop());
}

const children = [];
async function startChildServer(env) {
  const port = await freePort();
  const child = spawn(process.execPath, [SERVER_PATH], {
    env: {
      ...process.env,
      PORT: String(port),
      PROJECTS_ROOT: root,
      STATIC_OUT: path.join(root, 'static-out'),
      ...env,
    },
    stdio: ['ignore', 'pipe', 'pipe'],
  });
  children.push(child);
  let log = '';
  child.stdout.on('data', d => { log += d; });
  child.stderr.on('data', d => { log += d; });
  for (let i = 0; i < 100; i += 1) {
    try {
      // The health path is deliberately the readiness probe: it is the one
      // route the read gate always lets through, so this same poll works in
      // both regimes — which is exactly what start_viewer.sh depends on.
      const r = await get(port, healthProbePath(), `127.0.0.1:${port}`);
      if (r.status === 200) return { port, child, log: () => log };
    } catch { /* not up yet */ }
    await new Promise(r => setTimeout(r, 100));
  }
  throw new Error(`child server never became ready. log:\n${log}`);
}

function stopChildren() {
  for (const c of children) { try { c.kill('SIGKILL'); } catch {} }
}

async function main() {
  // -------------------------------------------------------------------
  // 1. Host normalization: every spelling of the same name folds together,
  //    because each un-folded form is an allowlist bypass.
  // -------------------------------------------------------------------
  assert.strictEqual(normalizeHostName('Some.Host.Example.com'), 'some.host.example.com', 'case');
  assert.strictEqual(normalizeHostName('some.host.example.com:3301'), 'some.host.example.com', 'port');
  assert.strictEqual(normalizeHostName('some.host.example.com.'), 'some.host.example.com', 'trailing dot');
  assert.strictEqual(normalizeHostName('some.host.example.com.:3301'), 'some.host.example.com', 'dot + port');
  assert.strictEqual(normalizeHostName('[::1]:3301'), '::1', 'bracketed IPv6 with port');
  assert.strictEqual(normalizeHostName('[::1]'), '::1', 'bracketed IPv6');
  assert.strictEqual(normalizeHostName('::1'), '::1', 'bare IPv6 is not truncated at its first colon');
  assert.strictEqual(normalizeHostName('127.0.0.1:3301'), '127.0.0.1', 'IPv4 with port');
  assert.strictEqual(normalizeHostName(undefined), '', 'absent');

  // The whole 127.0.0.0/8, both IPv6 loopback spellings, and nothing else.
  assert.ok(isLoopbackLiteral('127.0.0.1'));
  assert.ok(isLoopbackLiteral('127.9.9.9'), '127.0.0.0/8 is all loopback, not just .0.0.1');
  assert.ok(isLoopbackLiteral('::1') && isLoopbackLiteral('0:0:0:0:0:0:0:1'));
  assert.ok(!isLoopbackLiteral('128.0.0.1'));
  assert.ok(!isLoopbackLiteral('12.7.0.0.1'));
  assert.ok(!isLoopbackLiteral('evil.com'));
  assert.ok(!isLoopbackLiteral('127.0.0.999'), 'octets are range-checked');

  // A loopback BIND is loopback across the whole /8 — this decides whether
  // reads get gated, and 127.0.0.2 is not public.
  assert.ok(bindIsLoopback('127.0.0.1') && bindIsLoopback('::1') && bindIsLoopback('localhost'));
  assert.ok(bindIsLoopback('127.0.0.2'), '127.0.0.0/8 is reachable only from this machine');
  assert.ok(!bindIsLoopback('0.0.0.0') && !bindIsLoopback('192.168.1.10'));

  // -------------------------------------------------------------------
  // 2. The allowlist is DERIVED — no configuration, and it contains this
  //    machine's real names, so the operator's existing bookmark works.
  // -------------------------------------------------------------------
  const derived = machineHostNames();
  assert.ok(derived.has(normalizeHostName(os.hostname())), 'os.hostname() must be allowed');
  assert.ok(ALLOWED_HOSTS.has('localhost'), 'localhost must be allowed');
  assert.ok(ALLOWED_HOSTS.has('127.0.0.1'), 'the default bind address must be allowed');

  // `hostname -f` where the platform has it: the FQDN is what a browser
  // actually puts in the address bar, and Node exposes no API for it.
  const fq = spawnSync('hostname', ['-f'], { encoding: 'utf-8' });
  if (fq.status === 0 && fq.stdout.trim()) {
    const fqdn = normalizeHostName(fq.stdout);
    assert.ok(derived.has(fqdn), `the fully-qualified name ${fqdn} must be derived with no configuration`);
    assert.ok(hostIsAllowed(fqdn), `${fqdn} must be accepted`);
    assert.ok(hostIsAllowed(normalizeHostName(`${fqdn}:3301`)), 'FQDN with a port');
    assert.ok(hostIsAllowed(normalizeHostName(`${fqdn.toUpperCase()}.`)), 'FQDN, uppercase, trailing dot');
  }

  // The rebinding case, and the reason all of this exists.
  assert.ok(!hostIsAllowed('evil.com'));
  assert.ok(!hostIsAllowed('attacker.example.net'));
  // Loopback literals never need listing.
  assert.ok(hostIsAllowed('127.0.0.1') && hostIsAllowed('127.9.9.9') && hostIsAllowed('::1'));
  // Absent Host: a browser always sends one, so this cannot be the attack.
  assert.ok(hostIsAllowed(''), 'HTTP/1.0 clients omit Host and must not be broken');

  // -------------------------------------------------------------------
  // 3. Refusal is ACTIONABLE: it names the rejected Host and the env var.
  // -------------------------------------------------------------------
  const refusal = hostRefusal({ headers: { host: 'evil.com:3301' } });
  assert.ok(refusal, 'a foreign Host must be refused');
  assert.strictEqual(refusal.status, 403);
  assert.strictEqual(refusal.error, 'host_not_allowed');
  assert.strictEqual(refusal.host, 'evil.com', 'the rejected name is reported normalized');
  assert.strictEqual(refusal.envVar, HOST_ENV_VAR);
  assert.ok(refusal.detail.includes('evil.com'), 'the refusal names the Host that was rejected');
  assert.ok(refusal.detail.includes(HOST_ENV_VAR), 'the refusal names the env var that would allow it');
  assert.ok(refusal.detail.includes(`${HOST_ENV_VAR}=*`), 'the refusal names the opt-out');
  assert.ok(refusal.allowed.includes('localhost'), 'the refusal lists what IS accepted');
  assert.strictEqual(hostRefusal({ headers: { host: 'localhost' } }), null);

  // A hostile Host must not smuggle control characters into the body.
  const nasty = hostRefusal({ headers: { host: 'ev il\n<script>.com' } });
  assert.ok(!/[\x00-\x1f]/.test(nasty.host), 'echoed Host is printable-only');

  // Explicit opt-out, as a pure function.
  assert.ok(hostIsAllowed('evil.com', undefined, true), '* disables the check');
  assert.strictEqual(hostRefusal({ headers: { host: 'evil.com' } }, undefined, true), null);
  // An extended set accepts the added name and still refuses others.
  const extended = new Set([...ALLOWED_HOSTS, 'viewer.example.org']);
  assert.ok(hostIsAllowed('viewer.example.org', extended));
  assert.ok(!hostIsAllowed('other.example.org', extended));

  // -------------------------------------------------------------------
  // 4. END TO END ON A READ. This is the exposure the change closes, so it
  //    is asserted against a live server on real GETs of run data — not on
  //    a control route.
  // -------------------------------------------------------------------
  const srv = app.listen(0, '127.0.0.1');
  await new Promise(r => srv.on('listening', r));
  const port = srv.address().port;

  const READ_PATHS = [
    `${BASE}/`,                     // the landing page
    `${BASE}/api/projects.json`,    // every run on the host
    `${BASE}/api/host.json`,        // host state
  ];

  for (const p of READ_PATHS) {
    for (const h of ['localhost', `localhost:${port}`, `127.0.0.1:${port}`, '127.9.9.9', `[::1]:${port}`]) {
      const r = await get(port, p, h);
      assert.notStrictEqual(r.status, 403, `${p} with Host ${h} must not be refused (got ${r.status})`);
    }
    // The rebinding case: a name that is not ours, on a READ.
    const bad = await get(port, p, 'evil.com');
    assert.strictEqual(bad.status, 403, `${p} with a foreign Host must be refused`);
    assert.ok(bad.body.includes(HOST_ENV_VAR), `${p} refusal names ${HOST_ENV_VAR}`);
    assert.ok(bad.body.includes('evil.com'), `${p} refusal names the rejected Host`);
    assert.strictEqual(bad.headers['x-content-type-options'], 'nosniff');
    // Nothing about any run leaked in the refusal.
    assert.ok(!/protocol_state|"slug"/.test(bad.body), 'a refusal must carry no run state');
  }

  // The machine's own FQDN, end to end — the operator's bookmark.
  if (fq.status === 0 && fq.stdout.trim()) {
    const fqdn = normalizeHostName(fq.stdout);
    for (const spelling of [fqdn, `${fqdn}:${port}`, fqdn.toUpperCase(), `${fqdn}.`, `${fqdn}.:${port}`]) {
      const r = await get(port, `${BASE}/api/projects.json`, spelling);
      assert.strictEqual(r.status, 200, `Host ${spelling} must be served (got ${r.status})`);
    }
  }

  // No Host header at all (HTTP/1.0): served, because a browser cannot
  // produce this — Host is mandatory in HTTP/1.1 and is not under a page's
  // control — so it is never the rebinding case, and refusing it would
  // break plain clients for nothing.
  const noHost = await rawGetHttp10(port, `${BASE}/api/projects.json`);
  assert.strictEqual(noHost.status, 200, 'an absent Host must not be refused');

  // The guard runs AHEAD of the control-token prefix: a foreign Host is
  // refused before the secret is even compared.
  const token = fs.readFileSync(controlTokenPath(root), 'utf-8').trim();
  const badTokenPath = await get(port, `${BASE}/${token}/api/projects.json`, 'evil.com');
  assert.strictEqual(badTokenPath.status, 403);
  assert.ok(badTokenPath.body.includes('host_not_allowed') || badTokenPath.body.includes(HOST_ENV_VAR));

  // A JSON client gets JSON; a browser gets readable text. Both actionable.
  const asJson = await get(port, `${BASE}/api/projects.json`, 'evil.com', { accept: 'application/json' });
  assert.strictEqual(asJson.status, 403);
  const parsed = JSON.parse(asJson.body);
  assert.strictEqual(parsed.error, 'host_not_allowed');
  assert.strictEqual(parsed.envVar, HOST_ENV_VAR);

  // Health check: reachable, and it is what start_viewer.sh polls.
  const health = await get(port, healthProbePath(), `127.0.0.1:${port}`);
  assert.strictEqual(health.status, 200, 'the health probe must answer');
  assert.strictEqual(JSON.parse(health.body).ok, true);
  const startScript = fs.readFileSync(path.join(__dirname, '..', 'scripts', 'start_viewer.sh'), 'utf-8');
  assert.ok(
    startScript.includes('/api/health.json'),
    'start_viewer.sh must poll the health path the read gate exempts',
  );

  srv.close();

  // -------------------------------------------------------------------
  // 5. The env var extends the allowlist, and `*` turns the check off.
  //    Asserted in real processes, because these are read at load.
  // -------------------------------------------------------------------
  const extendedEnv = constantsUnder({ [HOST_ENV_VAR]: 'viewer.example.org, Alias.Example.NET.' });
  assert.ok(extendedEnv.allowed.includes('viewer.example.org'), 'the env var extends the allowlist');
  assert.ok(extendedEnv.allowed.includes('alias.example.net'), 'env entries are normalized too');
  assert.ok(extendedEnv.allowed.includes('localhost'), 'extending must not replace the derived names');
  assert.strictEqual(extendedEnv.hostCheckDisabled, false);

  const disabledEnv = constantsUnder({ [HOST_ENV_VAR]: '*' });
  assert.strictEqual(disabledEnv.hostCheckDisabled, true, '* disables the check');

  // End to end: an extending name is served, an unlisted one is not; and
  // under `*` even the rebinding Host is served.
  const ext = await startChildServer({ [HOST_ENV_VAR]: 'viewer.example.org' });
  assert.strictEqual((await get(ext.port, `${BASE}/api/projects.json`, 'viewer.example.org')).status, 200);
  assert.strictEqual((await get(ext.port, `${BASE}/api/projects.json`, 'other.example.org')).status, 403);
  ext.child.kill();

  const off = await startChildServer({ [HOST_ENV_VAR]: '*' });
  assert.strictEqual((await get(off.port, `${BASE}/api/projects.json`, 'evil.com')).status, 200,
    '* must serve any Host');
  assert.ok(off.log().includes('Host check DISABLED'), 'disabling the check must be logged');
  off.child.kill();

  // -------------------------------------------------------------------
  // 6. Token-gated reads: the rule, by bind and by override.
  // -------------------------------------------------------------------
  // Default (loopback bind): reads stay open, exactly as before.
  assert.strictEqual(READ_TOKEN_MODE, 'auto');
  assert.strictEqual(READS_REQUIRE_TOKEN, false, 'a loopback viewer keeps open reads');

  const MATRIX = [
    [{ TRELLIS_VIEWER_BIND: '127.0.0.1' }, false, 'auto + loopback -> open'],
    [{ TRELLIS_VIEWER_BIND: '127.0.0.2' }, false, 'auto + 127.0.0.0/8 -> open'],
    [{ TRELLIS_VIEWER_BIND: '::1' }, false, 'auto + IPv6 loopback -> open'],
    [{ TRELLIS_VIEWER_BIND: '0.0.0.0' }, true, 'auto + wildcard -> gated'],
    [{ TRELLIS_VIEWER_BIND: '203.0.113.9' }, true, 'auto + public address -> gated'],
    [{ TRELLIS_VIEWER_BIND: '0.0.0.0', TRELLIS_VIEWER_READ_TOKEN: '0' }, false, 'override off'],
    [{ TRELLIS_VIEWER_BIND: '127.0.0.1', TRELLIS_VIEWER_READ_TOKEN: '1' }, true, 'override on'],
  ];
  for (const [env, expected, why] of MATRIX) {
    assert.strictEqual(constantsUnder(env).readsRequireToken, expected, why);
  }

  // Behaviour under the gate, end to end. Bound to loopback with the
  // override on, so the test never puts a port on a public interface.
  const gated = await startChildServer({ TRELLIS_VIEWER_READ_TOKEN: '1' });
  const gtok = fs.readFileSync(controlTokenPath(root), 'utf-8').trim();

  for (const p of READ_PATHS) {
    const refused = await get(gated.port, p, `127.0.0.1:${gated.port}`);
    assert.strictEqual(refused.status, 403, `${p} must require the token when reads are gated`);
    assert.ok(refused.body.includes('TRELLIS_VIEWER_READ_TOKEN'), 'the refusal names its env var');
    assert.ok(!/"slug"|protocol_state/.test(refused.body), 'a gated refusal must carry no run state');

    const withToken = await get(gated.port, `${BASE}/${gtok}${p.slice(BASE.length)}`, `127.0.0.1:${gated.port}`);
    assert.strictEqual(withToken.status, 200, `${p} must be served under the token prefix`);
  }

  // The health probe stays open under the gate — start_viewer.sh must still
  // be able to tell that the viewer came up.
  const gatedHealth = await get(gated.port, healthProbePath(), `127.0.0.1:${gated.port}`);
  assert.strictEqual(gatedHealth.status, 200, 'the health probe must survive the read gate');
  assert.strictEqual(JSON.parse(gatedHealth.body).readsRequireToken, true);

  // Both regimes are logged at startup, so which one is in force is never a
  // guess.
  assert.ok(gated.log().includes('reads REQUIRE the token path'), 'the gated regime is logged');
  assert.ok(gated.log().includes('not an on-path observer'), 'the plaintext caveat is stated at startup');
  gated.child.kill();

  const openReads = await startChildServer({ TRELLIS_VIEWER_READ_TOKEN: '0' });
  assert.strictEqual((await get(openReads.port, `${BASE}/api/projects.json`, `127.0.0.1:${openReads.port}`)).status, 200);
  assert.ok(openReads.log().includes('reads are OPEN'), 'the open regime is logged');
  openReads.child.kill();

  // -------------------------------------------------------------------
  // 7. The static export is untouched. It is the artifact meant for
  //    publication, it is produced on the filesystem rather than served
  //    through any of this, and it must never carry a token.
  // -------------------------------------------------------------------
  // It is produced on the FILESYSTEM, so neither the Host check nor the read
  // gate — both of which are HTTP middleware — can touch it.
  writeStatic();
  const exported = path.join(root, 'static-out', 'demorun');
  assert.ok(fs.existsSync(exported), 'writeStatic still produces the export');
  const exportedIndex = path.join(exported, 'index.html');
  assert.ok(fs.existsSync(exportedIndex), 'the export still carries an index.html');
  assert.strictEqual(
    fs.realpathSync(exportedIndex),
    fs.realpathSync(path.join(__dirname, 'public', 'index.html')),
    'the export still links straight to the shipped, tokenless page',
  );
  assert.ok(
    !fs.readFileSync(exportedIndex, 'utf-8').includes('"token"'),
    'the exported page must carry no control token',
  );
  const shipped = fs.readFileSync(path.join(__dirname, 'public', 'index.html'), 'utf-8');
  assert.ok(
    shipped.includes('window.__TRELLIS_CONTROL__ = null; /*TRELLIS_CONTROL_TOKEN*/'),
    'the file the static export symlinks must keep its tokenless marker',
  );
  const builder = fs.readFileSync(path.join(__dirname, '..', 'scripts', 'build_public_tablet_viewer.py'), 'utf-8');
  assert.ok(
    !/127\.0\.0\.1:3301|localhost:3301/.test(builder),
    'the static builder must not depend on reaching the running viewer',
  );

  console.log('host-guard + token-gated reads: all assertions passed');
}

main()
  .then(() => { stopChildren(); process.exit(0); })
  .catch(e => { stopChildren(); console.error(e); process.exit(1); });
