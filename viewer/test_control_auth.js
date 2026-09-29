// Control-plane security: the bind, the token file, the guard, and how the
// token reaches the browser.
//
// Delivery is by URL path: `${BASE}/<token>/…` behaves exactly like
// `${BASE}/…` with the controls enabled, and every plain URL keeps working
// untouched without them. Two earlier schemes are dead and must not come
// back — inferring trust from a loopback socket (any TCP forwarder defeats
// it) and a Host allowlist (it refused the operator's own hostname). What
// authorizes a WRITE is unchanged: the X-Trellis-Control header, which a
// cross-origin page cannot forge.

const assert = require('assert');
const fs = require('fs');
const http = require('http');
const os = require('os');
const path = require('path');
const { spawnSync } = require('child_process');

// PROJECTS_ROOT is read at require time, so it must be set before the import
// below. An empty fixture root also gives us the "landing page with no runs"
// case for free.
const root = fs.mkdtempSync(path.join(os.tmpdir(), 'trellis-viewer-control-'));
process.env.PROJECTS_ROOT = root;
process.env.STATIC_OUT = path.join(root, 'static-out');

const {
  app,
  BIND_HOST,
  CONTROL_HEADER,
  CONTROL_PREFIX,
  CONTROL_ROUTE_TAILS,
  CONTROL_ROUTE_METHODS,
  CONTROL_TOKEN_MARKER,
  CONTROL_TOKEN_RE,
  bindIsLoopback,
  pathIsPrivateToUs,
  mintControlToken,
  slugsInRoot,
  basePathFor,
  controlTokenPath,
  ensureControlToken,
  controlAuthFailure,
  controlDeliveryDecision,
  controlBootstrapScript,
  readPublicHtml,
} = require('./server');

const BASE = process.env.BASE_PATH || '/trellis';

function req(server, method, urlPath, headers, body) {
  return new Promise((resolve, reject) => {
    const r = http.request({
      host: '127.0.0.1',
      port: server.address().port,
      method,
      path: urlPath,
      headers: headers || {},
    }, res => {
      let data = '';
      res.setEncoding('utf-8');
      res.on('data', c => { data += c; });
      res.on('end', () => resolve({ status: res.statusCode, headers: res.headers, body: data }));
    });
    r.on('error', reject);
    if (body !== undefined) r.write(body);
    r.end();
  });
}

function fakeReq(overrides) {
  return {
    headers: { host: '127.0.0.1:3301', ...((overrides && overrides.headers) || {}) },
    query: (overrides && overrides.query) || {},
    controlMode: Boolean(overrides && overrides.controlMode),
    socket: { remoteAddress: '127.0.0.1' },
  };
}

async function main() {
  // -----------------------------------------------------------------
  // 1. Bind. Loopback unless asked otherwise, asserted on the socket.
  // -----------------------------------------------------------------
  assert.strictEqual(BIND_HOST, '127.0.0.1', 'default bind must be loopback');
  assert.ok(bindIsLoopback('127.0.0.1') && bindIsLoopback('::1') && bindIsLoopback('localhost'));
  assert.ok(!bindIsLoopback('0.0.0.0') && !bindIsLoopback('192.168.1.10'));

  const probe = `
    const s = require(${JSON.stringify(path.join(__dirname, 'server.js'))});
    const srv = s.startServer();
    srv.on('listening', () => {
      console.log('BOUND=' + srv.address().address);
      srv.close();
      process.exit(0);
    });
  `;
  const childEnv = {
    ...process.env,
    PORT: '0',
    PROJECTS_ROOT: root,
    STATIC_OUT: path.join(root, 'static-out'),
  };
  // The wide-bind warning is a console.warn, so stdout alone would miss it.
  const runProbe = (extraEnv, script) => {
    const out = spawnSync(process.execPath, ['-e', script || probe], {
      env: { ...childEnv, ...extraEnv },
      encoding: 'utf-8',
      timeout: 30000,
    });
    return { ...out, all: `${out.stdout || ''}\n${out.stderr || ''}` };
  };

  const defaultOut = runProbe({});
  assert.strictEqual(defaultOut.status, 0, `bind probe failed: ${defaultOut.all}`);
  assert.ok(/BOUND=127\.0\.0\.1/.test(defaultOut.all), `default bind was not loopback:\n${defaultOut.all}`);
  assert.ok(!/NOT loopback/.test(defaultOut.all), 'loopback bind must not print the wide-bind warning');
  // The startup banner is now the delivery mechanism, so it must carry the
  // bootstrap URL — an operator with no way to get the token has no controls.
  assert.ok(/control plane ON — controls live under the token path/.test(defaultOut.all),
    `startup banner must print the control path:\n${defaultOut.all}`);
  assert.ok(new RegExp(`${BASE}/[A-Za-z0-9]{8}/`).test(defaultOut.all),
    `startup banner must show the token path:\n${defaultOut.all}`);

  const wideOut = runProbe({ TRELLIS_VIEWER_BIND: '0.0.0.0' });
  assert.ok(/BOUND=0\.0\.0\.0/.test(wideOut.all), `opt-in wide bind did not take effect:\n${wideOut.all}`);
  assert.ok(/NOT loopback/.test(wideOut.all), 'wide bind must announce itself');
  assert.ok(/TRELLIS_VIEWER_CONTROL=0/.test(wideOut.all), 'wide bind must name the off switch');

  // S7 (audit finding, REFUTED and pinned): Express 5 wires the listen
  // callback as the server's 'error' handler (`server.once('error', done)` in
  // application.js), so EADDRINUSE reaches `if (err)` and exits cleanly. If
  // anyone "fixes" the supposedly-dead branch away, this fails.
  const busy = http.createServer();
  await new Promise(r => busy.listen(0, '127.0.0.1', r));
  const busyPort = busy.address().port;
  const clash = runProbe({ PORT: String(busyPort) }, `require(${JSON.stringify(path.join(__dirname, 'server.js'))}).startServer();`);
  busy.close();
  assert.strictEqual(clash.status, 1, `EADDRINUSE must exit 1, got ${clash.status}:\n${clash.all}`);
  assert.ok(/listen failed on 127\.0\.0\.1:\d+: EADDRINUSE/.test(clash.all),
    `EADDRINUSE must report cleanly, not crash:\n${clash.all}`);

  // -----------------------------------------------------------------
  // 2. The token file: generated once, 0600, reused, and never adopted
  //    from something another user could control.
  // -----------------------------------------------------------------
  const tokenRoot = fs.mkdtempSync(path.join(os.tmpdir(), 'trellis-viewer-token-'));
  const token = ensureControlToken(tokenRoot);
  assert.match(token, /^[A-Za-z0-9]{8}$/, 'token must be 8 alphanumerics');
  assert.ok(CONTROL_TOKEN_RE.test(token));
  assert.strictEqual(ensureControlToken(tokenRoot), token, 'token must be reused, not regenerated');
  const tokenFile = controlTokenPath(tokenRoot);
  assert.strictEqual(fs.statSync(tokenFile).mode & 0o777, 0o600, 'token file must be 0600');
  assert.strictEqual(fs.statSync(path.dirname(tokenFile)).mode & 0o777, 0o700, 'token dir must be 0700');

  // A truncated or otherwise malformed token is replaced, never adopted.
  for (const junk of [token.slice(0, 4), 'nope', '', 'has space', 'toolongtoken', 'bad-char']) {
    fs.writeFileSync(tokenFile, `${junk}\n`);
    assert.match(ensureControlToken(tokenRoot), /^[A-Za-z0-9]{8}$/,
      `malformed token (${JSON.stringify(junk)}) must be replaced`);
  }

  // Minting must never produce a token equal to an existing project slug —
  // `${BASE}/<that slug>/` would flip into control mode and the project would
  // become unreachable.
  const collideRoot = fs.mkdtempSync(path.join(os.tmpdir(), 'trellis-viewer-collide-'));
  const everyToken = new Set();
  for (let i = 0; i < 200; i += 1) everyToken.add(mintControlToken(collideRoot));
  assert.ok(everyToken.size > 150, 'minting must not be degenerate');
  // Fill the root with every token just minted, then mint again: the new one
  // must avoid all of them.
  for (const t of everyToken) fs.mkdirSync(path.join(collideRoot, t), { recursive: true });
  const slugs = slugsInRoot(collideRoot);
  assert.strictEqual(slugs.size, everyToken.size);
  for (let i = 0; i < 50; i += 1) {
    assert.ok(!slugs.has(mintControlToken(collideRoot)), 'minted token collided with a project slug');
  }
  fs.rmSync(collideRoot, { recursive: true, force: true });

  // S5: adoption must verify what it is adopting. A world-readable token, or
  // a symlink pointing at an attacker-controlled file, must be discarded
  // rather than trusted.
  const planted = ensureControlToken(tokenRoot);
  fs.chmodSync(tokenFile, 0o644);
  const afterLoosePerms = ensureControlToken(tokenRoot);
  assert.notStrictEqual(afterLoosePerms, planted, 'a group/other-readable token must be replaced');
  assert.strictEqual(fs.statSync(tokenFile).mode & 0o777, 0o600);

  const decoy = path.join(tokenRoot, 'decoy-token');
  fs.writeFileSync(decoy, 'AAAAAAAA\n', { mode: 0o600 });
  fs.unlinkSync(tokenFile);
  fs.symlinkSync(decoy, tokenFile);
  const afterSymlink = ensureControlToken(tokenRoot);
  assert.notStrictEqual(afterSymlink, 'AAAAAAAA', 'a symlinked token file must never be adopted');
  assert.ok(!fs.lstatSync(tokenFile).isSymbolicLink(), 'the symlink must be replaced by a real file');
  assert.deepStrictEqual(pathIsPrivateToUs(decoy).ok, true);
  assert.strictEqual(pathIsPrivateToUs(path.join(tokenRoot, 'missing')).reason, 'missing');

  // A directory we cannot make private is refused outright — silently using
  // it would be a promise we cannot keep.
  const openRoot = fs.mkdtempSync(path.join(os.tmpdir(), 'trellis-viewer-open-'));
  fs.mkdirSync(path.join(openRoot, '.trellis-viewer'), { recursive: true });
  fs.chmodSync(path.join(openRoot, '.trellis-viewer'), 0o777);
  const wasChmodable = (() => {
    try { ensureControlToken(openRoot); return true; } catch { return false; }
  })();
  // We own it, so it is repaired to 0700 rather than refused.
  assert.ok(wasChmodable, 'a directory we own should be repaired, not refused');
  assert.strictEqual(fs.statSync(path.join(openRoot, '.trellis-viewer')).mode & 0o777, 0o700);

  // -----------------------------------------------------------------
  // 3. The guard, as a pure function.
  // -----------------------------------------------------------------
  const good = fakeReq({ headers: { [CONTROL_HEADER.toLowerCase()]: token } });
  assert.strictEqual(controlAuthFailure(good, { token, enabled: true }), null, 'correct token must pass');

  const missing = controlAuthFailure(fakeReq(), { token, enabled: true });
  assert.strictEqual(missing.status, 403);
  assert.strictEqual(missing.error, 'control_token_missing');

  const wrong = controlAuthFailure(
    fakeReq({ headers: { [CONTROL_HEADER.toLowerCase()]: 'xxxxxxxx' } }), { token, enabled: true });
  assert.strictEqual(wrong.status, 403);
  assert.strictEqual(wrong.error, 'control_token_invalid');

  // timingSafeEqual throws on unequal buffer lengths — exactly the shape of
  // an attacker-controlled input.
  const shortTok = controlAuthFailure(
    fakeReq({ headers: { [CONTROL_HEADER.toLowerCase()]: 'a' } }), { token, enabled: true });
  assert.strictEqual(shortTok.error, 'control_token_invalid');

  const off = controlAuthFailure(good, { token, enabled: false });
  assert.strictEqual(off.status, 503);
  assert.strictEqual(off.error, 'control_plane_disabled');

  // -----------------------------------------------------------------
  // 4. THE CSRF PROPERTY.
  //
  // A cross-origin page can make the browser send a POST with cookies, an
  // Origin, and a form content-type. It cannot attach a custom header without
  // a CORS preflight, and this server grants no CORS.
  // -----------------------------------------------------------------
  const csrf = controlAuthFailure(fakeReq({
    headers: {
      origin: 'https://evil.example',
      referer: 'https://evil.example/page',
      cookie: 'session=whatever',
      'content-type': 'application/x-www-form-urlencoded',
    },
  }), { token, enabled: true });
  assert.ok(csrf, 'a cross-origin-shaped request must never be authorized');
  assert.strictEqual(csrf.error, 'control_token_missing');

  // -----------------------------------------------------------------
  // 5. Delivery is by URL PATH, and by nothing else.
  //
  // A page served plainly gets no token however local it looks; a page
  // served under the token prefix gets it. That single rule is what replaced
  // the socket-locality guess and the Host allowlist.
  // -----------------------------------------------------------------
  assert.strictEqual(controlDeliveryDecision(fakeReq()).deliver, false,
    'a plain request must never be handed the token, however local it looks');
  assert.ok(!controlBootstrapScript(fakeReq()).includes('"token"'));
  assert.strictEqual(controlDeliveryDecision(fakeReq({ controlMode: true })).deliver, true);
  assert.ok(controlBootstrapScript(fakeReq({ controlMode: true })).includes('"token"'));

  // Links built for a request must carry the prefix when it is present and
  // must never invent one when it is absent.
  assert.strictEqual(basePathFor(fakeReq()), BASE);
  assert.strictEqual(basePathFor(fakeReq({ controlMode: true })), `${BASE}/${ensureControlToken(root)}`);
  assert.strictEqual(basePathFor(undefined), BASE);

  // The marker must exist verbatim in both pages, or injection silently
  // becomes a no-op and every control action fails at runtime.
  for (const page of ['index.html', 'landing.html']) {
    const html = readPublicHtml(page);
    assert.ok(html.includes(CONTROL_TOKEN_MARKER), `${page} lost the control-token injection marker`);
    // The token now rides in the page's own URL, so no client-side storage
    // of it may exist: nothing may copy it into localStorage (which outlives
    // the tab) or sessionStorage (superseded and no longer needed).
    assert.ok(!/(local|session)Storage\.\w+\([^)]*CONTROL/i.test(html),
      `${page} must not stash the control token in web storage`);
  }

  // -----------------------------------------------------------------
  // 7. The nginx rules, parsed out of the installers.
  //
  // Two properties, and they pull against each other:
  //   * a TOKENLESS control request must be denied — in every spelling
  //     Express accepts, which includes mis-cased and trailing-slash forms
  //     (Express routes are case-insensitive by default and slash-tolerant,
  //     so a case-sensitive `$`-anchored rule was walkable around); and
  //   * a TOKEN-PREFIXED request must reach the viewer, or remote control
  //     cannot work at all — which is the whole point of the path scheme.
  // Order decides it: the token-subtree proxy rule must come FIRST, because
  // nginx takes the first matching regex location.
  // -----------------------------------------------------------------
  for (const rel of ['../scripts/install_viewer_nginx.sh', '../scripts/install_public_trellis_viewer.sh']) {
    const text = fs.readFileSync(path.join(__dirname, rel), 'utf-8');
    const subst = raw => raw
      .replace(/\$\{BASE_PATH\}|\{base\}/g, BASE)
      .replace(/\\\$/g, '$')
      .replace(/\{\{(\d)/g, '{$1').replace(/(\d)\}\}/g, '$1}')
      .replace(/^"|"$/g, '');

    // Each `location <modifier> <pattern> {` with the body that follows it.
    const blocks = [...text.matchAll(/location\s+(~\*?)\s+("?[^\s]+"?)\s*\{([\s\S]{0,600}?)\n\s{4}\}/g)]
      .map(m => ({ pattern: subst(m[2]), body: m[3], insensitive: m[1] === '~*' }));
    const denies = blocks.filter(b => /deny all/.test(b.body));
    const proxies = blocks.filter(b => /proxy_pass/.test(b.body));
    assert.ok(denies.length >= 2, `${rel} lost its deny rules`);

    const tokenRule = proxies.find(b => /A-Za-z0-9\]\{8\}/.test(b.pattern));
    assert.ok(tokenRule, `${rel} has no rule sending the control-token subtree to the viewer`);
    assert.ok(text.indexOf(tokenRule.pattern.replace(/[$]/g, '\\$')) < text.indexOf('deny all')
      || text.search(/A-Za-z0-9\]\{8\}/) < text.indexOf('deny all'),
      `${rel}: the token-subtree rule must precede the deny rules`);

    const covered = (pat, url, ci) => new RegExp(pat, ci ? 'i' : '').test(url);
    for (const tail of CONTROL_ROUTE_TAILS) {
      for (const prefix of [`${BASE}/api/control/`, `${BASE}/api/`,
                            `${BASE}/proj/api/control/`, `${BASE}/proj/api/`]) {
        for (const spelling of [`${prefix}${tail}`, `${prefix}${tail}/`,
                                `${prefix}${tail}`.replace(/\/api\//, '/API/')]) {
          assert.ok(denies.some(d => covered(d.pattern, spelling, d.insensitive)),
            `${rel}: no deny rule covers tokenless ${spelling}`);
        }
      }
      // The token-prefixed twin must be handled by the proxy rule and must
      // NOT be swallowed by a deny rule that precedes it.
      const prefixed = `${BASE}/AbCd1234/api/control/${tail}`;
      assert.ok(covered(tokenRule.pattern, prefixed, true),
        `${rel}: the token-subtree rule does not cover ${prefixed}`);
    }
  }

  // -----------------------------------------------------------------
  // 8. Over real HTTP: the middleware wiring is the actual risk.
  // -----------------------------------------------------------------
  const liveToken = ensureControlToken(root);
  const server = app.listen(0, '127.0.0.1');
  await new Promise(res => server.once('listening', res));
  try {
    assert.ok(CONTROL_ROUTE_TAILS.length >= 6, 'every mutating family must register through the guard');
    for (const tail of ['pause/arm', 'pause/disarm', 'pause/resume', 'pause/config', 'feedback', 'external-codex/toggle']) {
      assert.ok(CONTROL_ROUTE_TAILS.includes(tail), `${tail} is not behind the control guard`);
    }
    // The run-creation family (M3b): every endpoint — its two reads included
    // — rides the same guard and the same nginx deny coverage. M3c adds the
    // loogle probe, a read on the same terms; the derived model/effort
    // suggestion lists (create-options.json) are another such read.
    for (const tail of ['uploads', 'create-jobs', 'create-jobs.json', 'create-status/:slug',
                        'create-jobs/:slug/resolve', 'create-jobs/:slug/confirm',
                        'create-jobs/:slug/retry', 'create-jobs/:slug/delete',
                        'loogle-probe.json', 'create-options.json']) {
      assert.ok(CONTROL_ROUTE_TAILS.includes(tail), `${tail} is not behind the control guard`);
    }
    assert.strictEqual(CONTROL_ROUTE_METHODS['create-jobs.json'], 'get');
    assert.strictEqual(CONTROL_ROUTE_METHODS['create-status/:slug'], 'get');
    assert.strictEqual(CONTROL_ROUTE_METHODS['loogle-probe.json'], 'get');
    assert.strictEqual(CONTROL_ROUTE_METHODS['create-options.json'], 'get');

    // The guard walk below issues a REAL request to every tail with the
    // correct token; stub the loogle probe's runner so that walk neither
    // depends on nor perturbs whatever live service holds port 8088.
    const loogleStubDir = path.join(root, 'loogle-stubs');
    fs.mkdirSync(loogleStubDir, { recursive: true });
    const loogleStub = (name, body) => {
      const file = path.join(loogleStubDir, name);
      fs.writeFileSync(file, `#!/usr/bin/env bash\n${body}\n`, { mode: 0o755 });
      return `bash ${file}`;
    };
    process.env.TRELLIS_LOOGLE_PROBE_CMD =
      loogleStub('ok.sh', 'printf \'{"count": 1, "hits": [{"name": "Nat"}]}\'');

    for (const tail of CONTROL_ROUTE_TAILS) {
      // Reads in the control namespace are GETs; everything else POSTs. The
      // guard must refuse both identically. (`:slug` rides through as the
      // literal segment — the route still matches, which is the point: the
      // GUARD answers before any handler validation.)
      const method = (CONTROL_ROUTE_METHODS[tail] || 'post').toUpperCase();
      const body = method === 'POST' ? '{}' : undefined;
      for (const urlPath of [`${BASE}/${CONTROL_PREFIX}/${tail}`, `${BASE}/api/${tail}`]) {
        const bare = await req(server, method, urlPath, { 'content-type': 'application/json' }, body);
        assert.strictEqual(bare.status, 403, `${method} ${urlPath} answered ${bare.status} with no token`);
        assert.strictEqual(JSON.parse(bare.body).error, 'control_token_missing');

        const badTok = await req(server, method, urlPath,
          { 'content-type': 'application/json', [CONTROL_HEADER]: 'f'.repeat(64) }, body);
        assert.strictEqual(badTok.status, 403, `${method} ${urlPath} accepted a wrong token`);
        assert.strictEqual(JSON.parse(badTok.body).error, 'control_token_invalid');

        // With the right token the guard hands off. The fixture root holds
        // no projects, so the handler then fails on project resolution —
        // which is precisely the proof that authorization passed.
        const okTok = await req(server, method, urlPath,
          { 'content-type': 'application/json', [CONTROL_HEADER]: liveToken }, body);
        assert.notStrictEqual(okTok.status, 403, `${method} ${urlPath} rejected the correct token`);
        const parsed = JSON.parse(okTok.body);
        assert.ok(!parsed.error || !/control_/.test(parsed.error),
          `${method} ${urlPath} still reported an auth error with the right token: ${okTok.body}`);
      }
    }

    // S3 at the app layer: a mis-cased control path must not reach a handler
    // at all now that routing is case-sensitive.
    const miscased = await req(server, 'POST', `${BASE}/api/CONTROL/pause/arm`,
      { 'content-type': 'application/json', [CONTROL_HEADER]: liveToken }, '{}');
    assert.strictEqual(miscased.status, 404, `mis-cased control path reached a handler (${miscased.status})`);
    const miscasedLegacy = await req(server, 'POST', `${BASE}/api/PAUSE/arm`,
      { 'content-type': 'application/json', [CONTROL_HEADER]: liveToken }, '{}');
    assert.strictEqual(miscasedLegacy.status, 404, 'mis-cased legacy path reached a handler');

    // The project-scoped variants are gated too.
    for (const p of [`${BASE}/someproject/${CONTROL_PREFIX}/pause/arm`, `${BASE}/someproject/api/pause/arm`]) {
      const scoped = await req(server, 'POST', p, { 'content-type': 'application/json' }, '{}');
      assert.strictEqual(scoped.status, 403, `${p} was not gated`);
    }

    // -----------------------------------------------------------------
    // 8b. The loogle probe endpoint: every branch of loogle_json.sh's exit
    // taxonomy, driven through real HTTP with stub runners, plus one live
    // run of the REAL script whose verdict depends on what actually holds
    // port 8088 right now — so it asserts membership in the enumerated
    // states, not a pinned one.
    // -----------------------------------------------------------------
    const probeUrl = `${BASE}/${CONTROL_PREFIX}/loogle-probe.json`;
    const probeGet = async () => {
      const r = await req(server, 'GET', probeUrl, { [CONTROL_HEADER]: liveToken });
      assert.strictEqual(r.status, 200);
      return JSON.parse(r.body);
    };

    // Reachable and answering (stubbed clean JSON, set above for the walk).
    const probeUp = await probeGet();
    assert.strictEqual(probeUp.state, 'answering');
    assert.match(probeUp.detail, /1 hit/);
    assert.strictEqual(probeUp.source, 'scripts/loogle_json.sh');
    assert.ok(probeUp.timeout_secs <= 10, 'the probe must be short — the wizard never hangs on it');

    // Reachable but the service reports an error (the mid-startup answer).
    process.env.TRELLIS_LOOGLE_PROBE_CMD =
      loogleStub('starting.sh', 'printf \'{"error": "The backend process is starting up"}\'');
    assert.strictEqual((await probeGet()).state, 'erroring');

    // Not reachable: the script's curl exit 7 branch, message riding through.
    process.env.TRELLIS_LOOGLE_PROBE_CMD = loogleStub('down.sh',
      'echo "Loogle is not reachable at 127.0.0.1:8088 (connection refused)." >&2; exit 7');
    const probeDown = await probeGet();
    assert.strictEqual(probeDown.state, 'unreachable');
    assert.match(probeDown.detail, /not reachable at 127\.0\.0\.1:8088/);

    // Timed out / other failure: the script's catch-all branch.
    process.env.TRELLIS_LOOGLE_PROBE_CMD = loogleStub('slow.sh',
      'echo "Loogle query failed or timed out after 4s (curl exit 28)." >&2; exit 28');
    const probeSlow = await probeGet();
    assert.strictEqual(probeSlow.state, 'erroring');
    assert.match(probeSlow.detail, /timed out/);

    // The real script, live: whatever holds 8088 today, the verdict must be
    // one of the enumerated states and carry a human-readable detail.
    delete process.env.TRELLIS_LOOGLE_PROBE_CMD;
    const probeLive = await probeGet();
    assert.ok(['answering', 'erroring', 'unreachable'].includes(probeLive.state),
      `live probe returned an unknown state: ${JSON.stringify(probeLive)}`);
    assert.ok(probeLive.detail && probeLive.detail.length > 0, 'live probe must explain itself');

    // Read-only routes stay open — under a Host this viewer answers to. The
    // Host check now covers reads (see test_host_guard.js): an unrecognized
    // proxy Host is refused, and says how to allow it. `localhost` is on the
    // derived allowlist with no configuration, so ordinary viewing is
    // untouched.
    const proxied = await req(server, 'GET', `${BASE}/api/host.json`, { host: 'public.example' });
    assert.strictEqual(proxied.status, 403, 'an unrecognized proxy Host is refused on reads too');
    assert.ok(proxied.body.includes('TRELLIS_VIEWER_ALLOWED_HOSTS'),
      'the refusal must name the env var that would allow the proxy Host');

    const hostJson = await req(server, 'GET', `${BASE}/api/host.json`, { host: 'localhost' });
    assert.strictEqual(hostJson.status, 200);
    const host = JSON.parse(hostJson.body);
    assert.ok(host.memory.total_bytes > 0);
    assert.strictEqual(host.control.bind_is_loopback, true);

    const projJson = await req(server, 'GET', `${BASE}/api/projects.json`);
    assert.strictEqual(projJson.status, 200);
    assert.deepStrictEqual(JSON.parse(projJson.body).projects, []);

    // -----------------------------------------------------------------
    // 9. Page delivery over HTTP.
    // -----------------------------------------------------------------
    // The landing page renders with no live run present...
    const landing = await req(server, 'GET', `${BASE}/`);
    assert.strictEqual(landing.status, 200);
    assert.ok(/text\/html/.test(landing.headers['content-type']));
    assert.ok(/No projects found/.test(landing.body), 'landing page must carry an empty state');
    assert.strictEqual(landing.headers['cache-control'], 'no-store');
    assert.strictEqual(landing.headers['referrer-policy'], 'no-referrer',
      'the bootstrap URL carries the token, so Referer must never leave the origin');
    // ...and, being a bare loopback request, carries NO token. This is the
    // S2 regression pin: before the fix this page held the real secret.
    assert.ok(!landing.body.includes(liveToken), 'a bare loopback request must NOT receive the token');
    assert.ok(/__TRELLIS_CONTROL__ = null/.test(landing.body));

    // The token prefix is what delivers it — and the prefixed URL must
    // behave exactly like the plain one otherwise.
    const ctl = `${BASE}/${liveToken}`;
    const bootstrapped = await req(server, 'GET', `${ctl}/`);
    assert.strictEqual(bootstrapped.status, 200);
    assert.ok(bootstrapped.body.includes(liveToken), 'the token path must deliver the token');
    assert.ok(!bootstrapped.body.includes(CONTROL_TOKEN_MARKER), 'the marker must be consumed by injection');
    assert.ok(/No projects found/.test(bootstrapped.body), 'the prefixed landing page is the same page');

    // `${BASE}/<token>` without the trailing slash is the landing page too.
    const noSlash = await req(server, 'GET', ctl);
    assert.strictEqual(noSlash.status, 200);
    assert.ok(noSlash.body.includes(liveToken));

    // A WRONG 8-character prefix is just an unknown project: it must not
    // deliver the token, and must not error.
    const wrongPrefix = await req(server, 'GET', `${BASE}/AAAAAAAA/`);
    assert.strictEqual(wrongPrefix.status, 200, 'a wrong token prefix must not error');
    assert.ok(!wrongPrefix.body.includes(liveToken), 'a wrong token prefix must deliver nothing');
    assert.ok(/Proof Tablet Viewer/.test(wrongPrefix.body), 'it is treated as a project slug');

    // Every existing URL still resolves, unchanged and tokenless.
    for (const p of [`${BASE}/`, BASE, `${BASE}/someproject/`, `${BASE}/api/host.json`,
                     `${BASE}/api/projects.json`, `${BASE}/someproject/api/projects.json`]) {
      const r = await req(server, 'GET', p);
      assert.ok(r.status === 200 || r.status === 302, `${p} regressed to ${r.status}`);
      assert.ok(!r.body.includes(liveToken), `${p} must not carry the token`);
    }

    // ...and each has a working prefixed twin that DOES carry controls.
    for (const p of [`${ctl}/`, `${ctl}/someproject/`, `${ctl}/api/host.json`,
                     `${ctl}/api/projects.json`, `${ctl}/someproject/api/projects.json`]) {
      const r = await req(server, 'GET', p);
      assert.strictEqual(r.status, 200, `${p} did not resolve under the token prefix`);
    }
    // The HTML twins carry the token; the JSON twins are ordinary data.
    const prefixedRun = await req(server, 'GET', `${ctl}/someproject/`);
    assert.ok(prefixedRun.body.includes(liveToken), 'a run viewer under the prefix must get controls');

    // Redirects must stay in control mode, and must not invent a prefix.
    const redir = await req(server, 'GET', `${ctl}/someproject`);
    assert.strictEqual(redir.status, 302);
    assert.strictEqual(redir.headers.location, `${ctl}/someproject/`);
    const plainRedir = await req(server, 'GET', `${BASE}/someproject`);
    assert.strictEqual(plainRedir.status, 302);
    assert.strictEqual(plainRedir.headers.location, `${BASE}/someproject/`);

    // Mutations under the prefix STILL require the header — the path is how
    // the browser learns the token, the header is what authorizes the write.
    const prefixedNoHeader = await req(server, 'POST', `${ctl}/${CONTROL_PREFIX}/pause/arm`,
      { 'content-type': 'application/json' }, '{}');
    assert.strictEqual(prefixedNoHeader.status, 403, 'the path alone must not authorize a write');
    assert.strictEqual(JSON.parse(prefixedNoHeader.body).error, 'control_token_missing');
    const prefixedWithHeader = await req(server, 'POST', `${ctl}/${CONTROL_PREFIX}/pause/arm`,
      { 'content-type': 'application/json', [CONTROL_HEADER]: liveToken }, '{}');
    assert.notStrictEqual(prefixedWithHeader.status, 403);

    // The operator's own hostname must work — a Host allowlist used to
    // refuse exactly this, which is what this scheme corrects. Derived from
    // this machine (never a literal): the guard allows the names the host
    // actually goes by, so a pinned name only passes on the box it was
    // written on.
    const ownHost = os.hostname();
    const byName = await req(server, 'POST', `${ctl}/${CONTROL_PREFIX}/pause/arm`,
      { 'content-type': 'application/json', [CONTROL_HEADER]: liveToken, host: ownHost }, '{}');
    assert.notStrictEqual(byName.status, 403, 'browsing by hostname must not be refused');
    const pageByName = await req(server, 'GET', `${ctl}/`, { host: ownHost });
    assert.ok(pageByName.body.includes(liveToken), 'the token path must work by hostname too');

    // Arriving through a reverse proxy is normal here (nginx fronts this
    // viewer on 443), so it changes nothing: the path decides.
    const proxiedPlain = await req(server, 'GET', `${BASE}/`, { 'x-forwarded-for': '203.0.113.9' });
    assert.ok(!proxiedPlain.body.includes(liveToken));
    const proxiedCtl = await req(server, 'GET', `${ctl}/`, { 'x-forwarded-for': '203.0.113.9' });
    assert.ok(proxiedCtl.body.includes(liveToken), 'a proxied request under the token path gets controls');

    // The old `${BASE}/` behaviour is still reachable for bookmarks.
    const goto = await req(server, 'GET', `${BASE}/?goto=default`);
    assert.ok(goto.status === 200 || goto.status === 302);

    // Deep links into a run viewer are untouched.
    const deep = await req(server, 'GET', `${BASE}/someproject/`);
    assert.strictEqual(deep.status, 200);
    assert.ok(/Proof Tablet Viewer/.test(deep.body));
    assert.ok(!deep.body.includes(liveToken), 'a bare run-viewer load must not carry the token either');
  } finally {
    server.close();
  }

  for (const dir of [root, tokenRoot, openRoot]) fs.rmSync(dir, { recursive: true, force: true });
  console.log('control auth tests passed');
}

main().then(() => process.exit(0)).catch(e => {
  console.error(e);
  process.exit(1);
});
