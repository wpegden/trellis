const express = require('express');
const compression = require('compression');
const fs = require('fs');
const path = require('path');
const os = require('os');
const crypto = require('crypto');
const { execSync, execFile, execFileSync, spawn, spawnSync } = require('child_process');
const { StringDecoder } = require('string_decoder');

// ---------------------------------------------------------------------------
// Process-level resilience.
//
// The viewer serves live run state read off disk (protocol_state, viewer
// state, event logs, the chats git repo, local-closure sidecars). An operator
// killing a supervisor and wiping `<project>-runtime` and/or resetting the
// chats git repo OUT FROM UNDER a running viewer means any of those reads can
// hit ENOENT, a half-written JSON file, or a git repo mid-`reset`. Most read
// paths are wrapped per-request, but a single uncaught error — especially an
// *unhandled promise rejection* from an async adapter spawn or a stream error
// event with no listener — would otherwise terminate the whole Node process
// under Node >=15, taking down EVERY project's view plus the prompts UI.
//
// These two guards convert any such escape into a logged, non-fatal event so
// the process keeps serving. They are a backstop only: the per-request
// try/catch blocks below still degrade individual projects gracefully (empty /
// "unavailable" payload) so a healthy project is never affected by a sibling
// being wiped.
//
// Installed ONLY when this file is the entry point. The test files import it
// as a library, and a process-wide crash suppressor turns every failed
// assertion in them into a logged line plus exit code 0 — i.e. a test suite
// that cannot fail.
if (require.main === module) {
  process.on('unhandledRejection', (reason) => {
    const msg = reason && reason.stack ? reason.stack : (reason && reason.message) || reason;
    console.error('[viewer] unhandledRejection (kept alive):', msg);
  });
  process.on('uncaughtException', (err) => {
    const msg = err && err.stack ? err.stack : (err && err.message) || err;
    console.error('[viewer] uncaughtException (kept alive):', msg);
  });
}

const app = express();
// Route paths are matched EXACTLY as written. Express defaults to
// case-insensitive routing, which meant `POST /api/CONTROL/pause/resume`
// reached the real handler while sailing past a case-sensitive nginx deny
// rule — the token still refused it, but the deny rule is supposed to be a
// structural layer under the token, and a layer that can be spelled around
// is not one. The nginx rules are now case-insensitive too; this closes the
// same hole at the app.
//
// `strict routing` is deliberately NOT set: it would make `/x` and `/x/`
// distinct across all ~7700 lines of hand-registered routes, which is a real
// regression risk for no gain here. The trailing-slash variants are handled
// in the nginx patterns instead.
app.set('case sensitive routing', true);
// FIRST, ahead of everything: refuse requests whose Host is not one of this
// machine's names. This is the DNS-rebinding defence and it has to cover
// reads, so it cannot sit behind any route. See `hostGuardMiddleware`.
app.use(hostGuardMiddleware);
// gzip all responses. Cuts the ~12 MB viewer-state payload to ~1.5 MB on the
// wire (~8x) and helps every other JSON endpoint similarly.
app.use(compression());
// Strips an optional `${BASE}/<control-token>` prefix before anything routes,
// so every route below is written and matched exactly as it always was.
// Must stay ahead of every route registration in this file.
app.use(controlPrefixMiddleware);
// Optionally requires that prefix on READS too — on when the viewer is bound
// somewhere other than loopback. Must follow the prefix middleware, which is
// what sets `req.tokenPresented`.
app.use(readTokenMiddleware);
const PORT = process.env.PORT || 3300;
const BASE = process.env.BASE_PATH || '/trellis';
const PROMPTS_BASE = process.env.PROMPTS_BASE || '/prompts';
const STATIC_OUT = process.env.STATIC_OUT || path.join(os.homedir(), 'trellis-web');
const PROJECTS_ROOT = process.env.PROJECTS_ROOT || path.join(os.homedir(), 'math');
const DEFAULT_PROJECT_SLUG = process.env.DEFAULT_PROJECT_SLUG || '';
const LEGACY_REPO_PATH = process.env.REPO_PATH || '';
const TRELLIS_ROOT = path.resolve(__dirname, '..');
const TMUX_SOCKET = process.env.TRELLIS_TMUX_SOCKET || 'trellis';
function tmuxArgs(...args) { return ['-L', TMUX_SOCKET, ...args]; }

// ===========================================================================
// CONTROL PLANE — where the viewer stops being a read-only window.
//
// The viewer runs as the operator, un-sandboxed, next to provider OAuth
// tokens and an SSH key, and its mutating endpoints stop and relaunch
// supervisors. Three independent layers guard that, so no single mistake
// exposes it:
//
//   1. BIND. `app.listen(PORT, cb)` passes no host, which makes Node bind
//      0.0.0.0 — the viewer was reachable from the whole network while
//      INSTALLATION.md claimed 127.0.0.1. Loopback is now the default and a
//      wider bind is an explicit, loudly logged env opt-in.
//   2. TOKEN IN A CUSTOM HEADER. Every mutating endpoint requires
//      `X-Trellis-Control: <token>`. A *custom* header is the CSRF defence:
//      a browser cannot attach one to a cross-origin request without a CORS
//      preflight, and this server sends no CORS headers, so the preflight
//      fails and the request is never made. Do not add a CORS middleware,
//      and do not move this to a cookie — a cookie rides along
//      automatically and would hand the whole control plane to any page the
//      operator happens to visit.
//   3. PATH NAMESPACE. Every mutating endpoint answers under
//      `/api/control/`, so one nginx `deny all` excludes the entire control
//      plane from a public exposure (README §7 documents that exposure).
//
// `TRELLIS_VIEWER_CONTROL=0` turns the control plane off entirely — that is
// the switch to throw before exposing this viewer to anything but loopback.
// ===========================================================================

// Loopback by default. Anything else is a deliberate remote-access choice
// and says so at startup.
const BIND_HOST = process.env.TRELLIS_VIEWER_BIND || '127.0.0.1';

// On by default: the viewer is meant to be the normal way to drive a run,
// and the token plus the nginx deny-rule are the actual protection layers.
const CONTROL_ENABLED = String(
  process.env.TRELLIS_VIEWER_CONTROL === undefined ? '1' : process.env.TRELLIS_VIEWER_CONTROL,
) !== '0';

const CONTROL_HEADER = 'X-Trellis-Control';
const CONTROL_HEADER_LC = CONTROL_HEADER.toLowerCase();
const CONTROL_PREFIX = 'api/control';
// Replaced at HTML-serve time; left verbatim in the file on disk so the
// static export (which symlinks straight to public/index.html) never
// carries a token. See `controlBootstrapScript`.
const CONTROL_TOKEN_MARKER = 'window.__TRELLIS_CONTROL__ = null; /*TRELLIS_CONTROL_TOKEN*/';

// Whether a bind address is reachable only from this machine. The whole of
// 127.0.0.0/8 is, not just 127.0.0.1 — and this now decides whether reads get
// gated (see READS_REQUIRE_TOKEN), so calling 127.0.0.2 "public" would gate a
// viewer nobody outside the host can reach. `isLoopbackLiteral` is defined
// below; both are function declarations, so the order does not matter.
function bindIsLoopback(host) {
  return host === 'localhost' || isLoopbackLiteral(String(host || '').toLowerCase());
}

// ===========================================================================
// HOST VALIDATION — the DNS-rebinding defence, DERIVED rather than configured.
//
// The attack: a page at `evil.com` sets a short TTL, then re-resolves the name
// to 127.0.0.1. The browser now treats `http://evil.com:3301/` as same-origin
// with the viewer, so the same-origin policy stops protecting us and the page
// can read every response. The token defeats this for CONTROLS (the page does
// not know the secret path segment), but reads were never authenticated, so a
// rebound page could pull back papers, chat transcripts and protocol state
// from every run on the host. That is what this closes.
//
// The one thing the attacking page CANNOT change is the Host header: the
// browser sends the name from the address bar, `evil.com`. So requiring Host
// to name this server ends it. The check therefore has to run on READS —
// guarding only writes would close nothing, because reads are the exposure.
//
// An earlier version of this check was removed, and rightly: it was a manual
// env var that refused the operator's own hostname out of the box, so the
// normal way of reaching the viewer (by name, through nginx) broke until you
// configured it. The fix is not to drop the defence but to stop demanding
// configuration for the names we can work out ourselves:
//
//   * `localhost`
//   * any loopback literal — 127.0.0.0/8 and ::1, in bracketed or bare form
//   * this machine's own names, from `os.hostname()` and the fully-qualified
//     name (see `machineHostNames`)
//   * whatever `TRELLIS_VIEWER_BIND` names, when it is not a wildcard: the
//     operator picked that address, and browsing to it must work
//
// None of those can be produced by rebinding. An IP literal never can — the
// rebound page arrives under a NAME — and the rest are ours.
// ===========================================================================

const HOST_ENV_VAR = 'TRELLIS_VIEWER_ALLOWED_HOSTS';

// Normalize a Host header (or an allowlist entry) to one comparable name.
// Every one of these forms is the same host, and each is an allowlist bypass
// if it is not folded away first:
//
//   "Some.Host.Example.com:3301" -> "some.host.example.com"  (case, port)
//   "some.host.example.com."     -> "some.host.example.com"  (absolute form)
//   "[::1]:3301"                -> "::1"                   (brackets, port)
//   "127.0.0.1:3301"            -> "127.0.0.1"
function normalizeHostName(raw) {
  let s = String(raw === undefined || raw === null ? '' : raw).trim().toLowerCase();
  if (!s) return '';
  if (s.startsWith('[')) {
    // Bracketed IPv6. The brackets exist precisely because the address
    // contains colons, so the port is whatever follows the closing bracket.
    const close = s.indexOf(']');
    if (close === -1) return '';
    s = s.slice(1, close);
  } else {
    // Strip a port only when there is exactly one colon. More than one means
    // a bare (unbracketed) IPv6 address, which is malformed in a Host header
    // but does turn up; truncating it there would corrupt the address.
    const colon = s.indexOf(':');
    if (colon !== -1 && s.indexOf(':', colon + 1) === -1) s = s.slice(0, colon);
  }
  while (s.endsWith('.')) s = s.slice(0, -1);
  return s;
}

// 127.0.0.0/8 (the whole /8, not just 127.0.0.1) plus the IPv6 loopback in
// both its compressed and expanded spellings.
function isLoopbackLiteral(name) {
  if (name === '::1' || name === '0:0:0:0:0:0:0:1') return true;
  if (name === '::ffff:127.0.0.1') return true;
  const m = /^(\d{1,3})\.(\d{1,3})\.(\d{1,3})\.(\d{1,3})$/.exec(name);
  if (!m) return false;
  const octets = m.slice(1).map(Number);
  if (octets.some(o => o > 255)) return false;
  return octets[0] === 127;
}

// This machine's own names, obtained with no new dependency and no
// configuration. `os.hostname()` gives the short name (e.g. "myhost"); the
// fully-qualified name is what a browser actually puts in the address bar
// ("<your-host>.example.com"), and Node has no API for it, so we ask the system:
//
//   `hostname -f`  the canonical FQDN, the direct answer where it exists
//   `hostname -d`  the domain, composed with the short name where -f does not
//
// Both are wrapped: a platform without them (or without `hostname` at all)
// degrades to the short name rather than failing to start. Resolved once at
// load, so the per-request check is a Set lookup.
function shellHostName(args) {
  try {
    return String(execFileSync('hostname', args, {
      encoding: 'utf-8',
      timeout: 2000,
      stdio: ['ignore', 'pipe', 'ignore'],
    }) || '').trim();
  } catch {
    return '';
  }
}

function machineHostNames() {
  const names = new Set();
  const add = (raw) => {
    const n = normalizeHostName(raw);
    if (n) names.add(n);
  };
  let short = '';
  try { short = os.hostname(); } catch { /* keep going with what we have */ }
  add(short);
  add(shellHostName(['-f']));
  const domain = shellHostName(['-d']);
  if (short && domain) add(`${short}.${domain}`);
  return names;
}

const HOST_ENV_ENTRIES = String(process.env[HOST_ENV_VAR] || '')
  .split(',')
  .map(s => s.trim())
  .filter(Boolean);

// `*` is the explicit opt-out, for someone who genuinely does not want the
// check — an exotic proxy, a name we cannot derive, a deployment where the
// operator has decided the check is not theirs to fight.
const HOST_CHECK_DISABLED = HOST_ENV_ENTRIES.includes('*');

function buildAllowedHosts() {
  const set = new Set(['localhost']);
  for (const n of machineHostNames()) set.add(n);
  // The bind address, when it is a real address rather than a wildcard.
  // Binding 10.0.0.5 and then being refused at http://10.0.0.5:3301/ would
  // be exactly the dead end that got the last version of this removed.
  if (BIND_HOST && BIND_HOST !== '0.0.0.0' && BIND_HOST !== '::') {
    const n = normalizeHostName(BIND_HOST);
    if (n) set.add(n);
  }
  for (const entry of HOST_ENV_ENTRIES) {
    const n = normalizeHostName(entry);
    if (n && n !== '*') set.add(n);
  }
  return set;
}

const ALLOWED_HOSTS = buildAllowedHosts();

// `allowed`/`disabled` are for tests; production passes nothing.
function hostIsAllowed(name, allowed, disabled) {
  if (disabled === undefined ? HOST_CHECK_DISABLED : disabled) return true;
  // No Host at all. HTTP/1.0 clients and hand-rolled sockets omit it; a
  // BROWSER never does — Host is mandatory in HTTP/1.1 and is not under the
  // page's control — so an absent Host cannot be the rebinding case this
  // check exists for, and refusing it would break plain clients for nothing.
  if (!name) return true;
  if (isLoopbackLiteral(name)) return true;
  return (allowed || ALLOWED_HOSTS).has(name);
}

// The whole decision as a pure function of the request, so a test can assert
// it without an HTTP server. Returns null to allow, or the refusal to send.
function hostRefusal(req, allowed, disabled) {
  const name = normalizeHostName(((req && req.headers) || {}).host);
  if (hostIsAllowed(name, allowed, disabled)) return null;
  // Echoed back to the operator who hit this, so keep it printable and
  // bounded. The response is text/plain or JSON and no CORS header is sent
  // anywhere in this server, so the rebound page cannot read it anyway.
  const shown = name.replace(/[^\x20-\x7e]/g, '').slice(0, 120) || '(absent)';
  const known = [...(allowed || ALLOWED_HOSTS)].sort().join(', ');
  return {
    status: 403,
    error: 'host_not_allowed',
    host: shown,
    envVar: HOST_ENV_VAR,
    allowed: [...(allowed || ALLOWED_HOSTS)].sort(),
    detail: `Trellis viewer: refused a request for Host "${shown}".\n`
      + '\n'
      + 'This viewer only answers to its own names, which is what stops a page\n'
      + 'you visit from re-pointing its own hostname at this machine and reading\n'
      + 'every run on it (DNS rebinding).\n'
      + '\n'
      + `Accepted here: ${known}, plus any 127.x.x.x or ::1 literal.\n`
      + '\n'
      + 'If you reach this viewer under another name — an alias, or a proxy that\n'
      + 'rewrites Host — add it and restart:\n'
      + '\n'
      + `    ${HOST_ENV_VAR}=${shown === '(absent)' ? 'your.name.here' : shown}\n`
      + '\n'
      + `Several names are comma-separated. ${HOST_ENV_VAR}=* disables the check.\n`,
  };
}

function hostGuardMiddleware(req, res, next) {
  const refusal = hostRefusal(req);
  if (!refusal) { next(); return; }
  const { status, detail, ...rest } = refusal;
  res.status(status);
  res.set('X-Content-Type-Options', 'nosniff');
  const wantsJson = typeof req.accepts === 'function' && req.accepts(['text', 'json']) === 'json';
  if (wantsJson) res.json({ detail, ...rest });
  else res.type('text/plain').send(detail);
}

// ===========================================================================
// TOKEN-GATED READS — for a viewer that is deliberately exposed.
//
// Bound to loopback (the default), reads stay open exactly as they always
// were: the only things that can reach the port are already on the host.
// Bound anywhere else, reads require the same `${BASE}/<token>/…` prefix the
// controls use.
//
// The reason is the unpublished work. A viewer on a public interface serves
// every run's paper, chat transcripts, event logs and protocol state to
// anyone who finds the port, and finding a port is what scanners do. This
// does NOT defend against an on-path observer of plaintext HTTP — the token
// rides in the URL, so on plain HTTP it is on the wire, and anyone able to
// read the traffic reads the token and everything it opens. What it converts
// is "everything is public" into "you need the URL", which is the difference
// between a scanner and a targeted attacker.
//
// `TRELLIS_VIEWER_READ_TOKEN` overrides in both directions:
//   auto (default)  on when the bind is not loopback, off when it is
//   1               always require the token on reads
//   0               never require it
// ===========================================================================

const READ_TOKEN_MODE = String(process.env.TRELLIS_VIEWER_READ_TOKEN || 'auto').trim().toLowerCase();
const READS_REQUIRE_TOKEN = READ_TOKEN_MODE === '1' ? true
  : READ_TOKEN_MODE === '0' ? false
    : !bindIsLoopback(BIND_HOST);

// One path stays open whatever the gate says: a liveness probe. A process
// supervisor has to be able to ask "is it listening?" without holding a
// secret — `scripts/start_viewer.sh` polls exactly this after launch — and
// the answer names no run and carries no run state.
function healthProbePath() {
  return `${BASE}/api/health.json`;
}

function readTokenMiddleware(req, res, next) {
  if (!READS_REQUIRE_TOKEN) { next(); return; }
  if (req && req.tokenPresented) { next(); return; }
  const url = String((req && req.url) || '');
  const pathPart = url.split('?')[0];
  if (pathPart === healthProbePath()) { next(); return; }
  res.status(403);
  res.set('X-Content-Type-Options', 'nosniff');
  const detail = 'Trellis viewer: this viewer is bound to a non-loopback address, so reads\n'
    + 'require the control-token URL. Browse the token path printed in the viewer\n'
    + 'log at startup:\n'
    + '\n'
    + `    ${BASE}/<token>/\n`
    + '\n'
    + 'The token is in <projects root>/.trellis-viewer/control-token.\n'
    + 'Set TRELLIS_VIEWER_READ_TOKEN=0 to serve reads without it.\n';
  const wantsJson = typeof req.accepts === 'function' && req.accepts(['text', 'json']) === 'json';
  if (wantsJson) res.json({ error: 'read_token_required', detail, envVar: 'TRELLIS_VIEWER_READ_TOKEN' });
  else res.type('text/plain').send(detail);
}

// ---------------------------------------------------------------------------
// THE CONTROL TOKEN LIVES IN THE URL PATH.
//
//   https://<your-host>.example.com/trellis/<token>/          -> landing, controls on
//   https://<your-host>.example.com/trellis/<token>/current   -> that run, controls on
//   https://<your-host>.example.com/trellis/current           -> unchanged, no controls
//
// The prefix is a bootstrap-and-authorization device, not a routing tree: one
// middleware strips it and every existing route sees the URL it always saw.
//
// This replaces an earlier scheme that tried to infer trust from the socket
// (loopback peer => hand over the token). That was wrong — any TCP forwarder
// re-originates from loopback, and a DNS-rebound page reaches loopback too —
// and the fixes it grew (a Host allowlist, a query-param bootstrap,
// sessionStorage) were machinery this deployment does not need. nginx fronts
// this viewer on 443, so the operator browses by name from off-host; a Host
// allowlist would have refused that outright. A path token defeats rebinding
// on its own: the rebound page does not know the token, so it cannot
// construct the URL that turns controls on.
//
// The header is still what AUTHORIZES a write. The path is only how the
// browser learns the token; `X-Trellis-Control` on every mutation is what a
// cross-origin page cannot forge, and that property is unchanged.

// 8 characters of [A-Za-z0-9]. Short enough to type and to live in a URL,
// ~2^47.6 of entropy against a remote guesser. See SECURITY.md for what
// putting it in the path does and does not cost.
const CONTROL_TOKEN_ALPHABET = 'ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789';
const CONTROL_TOKEN_LENGTH = 8;
const CONTROL_TOKEN_RE = /^[A-Za-z0-9]{8}$/;

function controlTokenDir(root) {
  return path.join(root === undefined ? PROJECTS_ROOT : root, '.trellis-viewer');
}

function controlTokenPath(root) {
  return path.join(controlTokenDir(root), 'control-token');
}

// Directory names directly under the projects root — i.e. every string that
// could be a project slug in a URL. The token must not be one of them, or
// `${BASE}/<that name>/` would flip into control mode instead of opening the
// project, making the project unreachable.
function slugsInRoot(root) {
  const base = root === undefined ? PROJECTS_ROOT : root;
  try {
    return new Set(fs.readdirSync(base, { withFileTypes: true })
      .filter(e => e.isDirectory() || e.isSymbolicLink())
      .map(e => e.name));
  } catch {
    return new Set();
  }
}

// `crypto.randomInt` is rejection-sampled, so no modulo bias across the
// 62-character alphabet.
function mintControlToken(root) {
  const taken = slugsInRoot(root);
  for (let attempt = 0; attempt < 1000; attempt += 1) {
    let token = '';
    for (let i = 0; i < CONTROL_TOKEN_LENGTH; i += 1) {
      token += CONTROL_TOKEN_ALPHABET[crypto.randomInt(0, CONTROL_TOKEN_ALPHABET.length)];
    }
    if (!taken.has(token)) return token;
  }
  throw new Error('could not mint a control token that does not collide with a project name');
}

// Is this path safe to hold a secret — ours, a real file, and unreadable by
// anyone else?
//
// `ensureControlToken` ADOPTS whatever it finds, so on a group- or
// world-writable projects root another local user could pre-plant a token
// they know, or a symlink pointing somewhere they can read, and the viewer
// would trust it. Adoption therefore has to verify what it is adopting.
function pathIsPrivateToUs(target, { expectDir = false } = {}) {
  let st;
  try {
    // lstat, never stat: a symlink must be REJECTED, not followed to a
    // benign-looking target that its owner can repoint at any moment.
    st = fs.lstatSync(target);
  } catch {
    return { ok: false, reason: 'missing' };
  }
  if (st.isSymbolicLink()) return { ok: false, reason: 'is a symlink' };
  if (expectDir && !st.isDirectory()) return { ok: false, reason: 'is not a directory' };
  if (!expectDir && !st.isFile()) return { ok: false, reason: 'is not a regular file' };
  if (typeof process.getuid === 'function' && st.uid !== process.getuid()) {
    return { ok: false, reason: `is owned by uid ${st.uid}, not us` };
  }
  if (st.mode & 0o077) {
    return { ok: false, reason: `is readable or writable by others (mode ${(st.mode & 0o777).toString(8)})` };
  }
  return { ok: true };
}

// One secret per host, persisted so a viewer restart does not invalidate
// every open tab. 32 random bytes, hex. It never leaves this directory except
// into a page served to a request that already proved it knows the token.
function ensureControlToken(root) {
  const dir = controlTokenDir(root);
  const file = controlTokenPath(root);

  fs.mkdirSync(dir, { recursive: true, mode: 0o700 });
  const dirCheck = pathIsPrivateToUs(dir, { expectDir: true });
  if (!dirCheck.ok) {
    if (dirCheck.reason === 'missing') throw new Error(`control-token directory ${dir} could not be created`);
    // Try to repair a directory we own; refuse outright if we do not own it,
    // because then someone else can swap the token under us at will and any
    // guarantee we make about it is false.
    try {
      fs.chmodSync(dir, 0o700);
    } catch { /* fall through to the recheck */ }
    const recheck = pathIsPrivateToUs(dir, { expectDir: true });
    if (!recheck.ok) {
      throw new Error(
        `refusing to keep a control token in ${dir}: it ${recheck.reason}. `
        + 'Another user could read or replace it. Fix the directory, or point '
        + 'PROJECTS_ROOT somewhere private.',
      );
    }
  }

  const fileCheck = pathIsPrivateToUs(file);
  if (fileCheck.ok) {
    let existing = '';
    try { existing = fs.readFileSync(file, 'utf-8').trim(); } catch {}
    if (CONTROL_TOKEN_RE.test(existing)) return existing;
  } else if (fileCheck.reason !== 'missing') {
    console.warn(`[viewer] discarding control token at ${file}: it ${fileCheck.reason}`);
    try { fs.unlinkSync(file); } catch {}
  }

  const token = mintControlToken(root);
  const tmp = `${file}.tmp.${process.pid}`;
  try { fs.unlinkSync(tmp); } catch {}
  // wx: never write through a symlink an attacker planted at the temp name.
  fs.writeFileSync(tmp, `${token}\n`, { mode: 0o600, flag: 'wx' });
  try { fs.chmodSync(tmp, 0o600); } catch {}
  fs.renameSync(tmp, file);
  return token;
}

let CONTROL_TOKEN_MEMO = null;
function controlToken() {
  if (CONTROL_TOKEN_MEMO) return CONTROL_TOKEN_MEMO;
  CONTROL_TOKEN_MEMO = ensureControlToken();
  return CONTROL_TOKEN_MEMO;
}

// Constant-time compare that does not leak the secret's length either.
function secretsEqual(a, b) {
  if (typeof a !== 'string' || typeof b !== 'string') return false;
  const ha = crypto.createHash('sha256').update(a).digest();
  const hb = crypto.createHash('sha256').update(b).digest();
  return crypto.timingSafeEqual(ha, hb);
}

// The whole authorization decision, as a pure function of the request, so a
// test can assert it without an HTTP server. Returns null to allow, or the
// response body (with its status) to refuse with.
//
// `overrides` exists only for tests — production passes nothing.
function controlAuthFailure(req, overrides) {
  const enabled = overrides && 'enabled' in overrides ? overrides.enabled : CONTROL_ENABLED;
  if (!enabled) {
    return {
      status: 503,
      error: 'control_plane_disabled',
      detail: 'TRELLIS_VIEWER_CONTROL=0 — this viewer is read-only. Unset it to control runs.',
    };
  }
  const headers = (req && req.headers) || {};
  const supplied = headers[CONTROL_HEADER_LC];
  if (typeof supplied !== 'string' || supplied === '') {
    return {
      status: 403,
      error: 'control_token_missing',
      // Naming the header is safe: knowing it exists does not let a
      // cross-origin page set it.
      detail: `${CONTROL_HEADER} header required on every mutating request.`,
      header: CONTROL_HEADER,
    };
  }
  const expected = overrides && 'token' in overrides ? overrides.token : controlToken();
  if (!secretsEqual(supplied.trim(), expected)) {
    return { status: 403, error: 'control_token_invalid', detail: `${CONTROL_HEADER} did not match.` };
  }
  return null;
}

// ---------------------------------------------------------------------------
// The optional `${BASE}/<token>` prefix, threaded in one place.
//
// This runs before every route. If the first path segment after BASE is the
// control token, it is removed from `req.url` and `req.controlMode` is set;
// everything downstream — all ~200 hand-registered routes, the static mount,
// the project `:project` params — then sees exactly the URL it saw before
// this feature existed. That is the whole mechanism: no route was touched,
// no parallel routing tree exists.
//
// Two things must be careful:
//   * the comparison is constant-time, because this segment is compared
//     against a secret on every request; and
//   * a segment that is NOT the token falls straight through and is treated
//     as a project slug, so `${BASE}/current/` keeps working and a wrong
//     8-character guess is an ordinary unknown project, not an error.
function controlPrefixMiddleware(req, res, next) {
  req.controlMode = false;
  // Set whenever the URL carried the token, whether or not the control plane
  // is on. `controlMode` is about DELIVERING controls; `tokenPresented` is
  // about having proved you know the URL, which is what a token-gated READ
  // needs. They differ only under TRELLIS_VIEWER_CONTROL=0, which is exactly
  // the read-only public exposure the read gate is for — so the prefix has to
  // keep working there.
  req.tokenPresented = false;
  if (!CONTROL_ENABLED && !READS_REQUIRE_TOKEN) { next(); return; }
  const url = req.url || '';
  if (!url.startsWith(`${BASE}/`)) { next(); return; }

  const queryAt = url.indexOf('?');
  const pathPart = queryAt === -1 ? url : url.slice(0, queryAt);
  const query = queryAt === -1 ? '' : url.slice(queryAt);
  const rest = pathPart.slice(BASE.length + 1);
  const slashAt = rest.indexOf('/');
  const firstSegment = slashAt === -1 ? rest : rest.slice(0, slashAt);
  if (!firstSegment) { next(); return; }

  let candidate = firstSegment;
  try { candidate = decodeURIComponent(firstSegment); } catch { /* malformed escape: not the token */ }

  let expected;
  try { expected = controlToken(); } catch { next(); return; }
  if (!secretsEqual(candidate, expected)) { next(); return; }

  req.tokenPresented = true;
  req.controlMode = CONTROL_ENABLED;
  const tail = slashAt === -1 ? '' : rest.slice(slashAt);
  req.url = `${BASE}${tail || '/'}${query}`;
  next();
}

// The base every link and redirect on THIS request must be built from. In
// control mode it carries the token forward so navigation stays in control
// mode; otherwise it is the plain base, and it never invents a prefix.
// `tokenPresented` and not merely `controlMode`: when reads are token-gated
// the prefix has to survive every redirect and generated link, or the first
// navigation drops the token and lands on a 403.
function basePathFor(req) {
  if (!(req && (req.controlMode || req.tokenPresented))) return BASE;
  try {
    return `${BASE}/${controlToken()}`;
  } catch {
    return BASE;
  }
}

function requireControlToken(req, res, next) {
  const failure = controlAuthFailure(req);
  if (failure) {
    const { status, ...body } = failure;
    res.status(status).json(body);
    return;
  }
  next();
}

// Every mutating route registered by the viewer, in registration order.
// Exported so the nginx installer's deny-list and the tests can be checked
// against the code rather than against a stale copy of it.
const CONTROL_ROUTE_TAILS = [];
// tail -> 'post' | 'get'. The create-status/create-jobs.json reads live in
// the control namespace too (design §4: the whole create surface is
// token-gated and nginx-denied as one block), and the auth test walks
// CONTROL_ROUTE_TAILS issuing real requests — it needs to know the verb.
const CONTROL_ROUTE_METHODS = {};

// One call registers four paths: the canonical control-namespaced pair and
// the legacy pair the shipped index.html used before this change. Both pairs
// carry the token guard — the namespace is for the nginx deny-rule, not for
// authorization — and the legacy pair exists to keep bookmarks and any
// hand-rolled curl working for one release.
function controlRoutePaths(tail) {
  return [
    `${BASE}/${CONTROL_PREFIX}/${tail}`,
    `${BASE}/:project/${CONTROL_PREFIX}/${tail}`,
    `${BASE}/api/${tail}`,
    `${BASE}/:project/api/${tail}`,
  ];
}

function registerControlRoute(tail, middlewares, handler) {
  CONTROL_ROUTE_TAILS.push(tail);
  CONTROL_ROUTE_METHODS[tail] = 'post';
  for (const routePath of controlRoutePaths(tail)) {
    app.post(routePath, requireControlToken, ...middlewares, handler);
  }
}

// Token-gated GET twin. Same four paths, same guard: these reads expose a
// create job's log and paper metadata, which belong behind the same door as
// the writes (and behind the same nginx deny rule — the tail is recorded in
// CONTROL_ROUTE_TAILS so the installers' coverage check sees it).
function registerControlReadRoute(tail, handler) {
  CONTROL_ROUTE_TAILS.push(tail);
  CONTROL_ROUTE_METHODS[tail] = 'get';
  for (const routePath of controlRoutePaths(tail)) {
    app.get(routePath, requireControlToken, handler);
  }
}

// Read-only companion: registers the `${BASE}/api/x` + `${BASE}/:project/api/x`
// pair that every existing GET writes out by hand. New routes only — the
// existing hand-registered ones are left alone.
function registerReadRoute(tail, handler) {
  app.get(`${BASE}/api/${tail}`, handler);
  app.get(`${BASE}/:project/api/${tail}`, handler);
}

function isValidProjectSlug(slug) {
  return typeof slug === 'string' && /^[A-Za-z0-9._-]+$/.test(slug);
}

function configPathForRepo(repoPath) {
  if (fs.existsSync(path.join(repoPath, 'trellis.config.json'))) {
    return path.join(repoPath, 'trellis.config.json');
  }
  return path.join(repoPath, 'lagent.config.json');
}

function repoTypeForRepo(repoPath) {
  if (fs.existsSync(path.join(repoPath, 'trellis.config.json'))) return 'trellis';
  if (fs.existsSync(path.join(repoPath, 'lagent.config.json'))) return 'legacy';
  return '';
}

// Backend source-file extension for a run, read directly from the project
// config's `workflow.default_target` (sign-off #4: the adapter-bypassing
// metrics / download paths read config themselves). `isabelle_hol` -> "thy",
// anything else (absent / lean) -> "lean". Mirrors the Python
// `_resolve_backend_descriptor`. So a Lean run reads `.lean` exactly as
// before.
function backendNodeExtForRepo(repoPath) {
  try {
    const cfg = JSON.parse(fs.readFileSync(configPathForRepo(repoPath), 'utf8'));
    const target = String((cfg.workflow && cfg.workflow.default_target) || '').trim().toLowerCase();
    return target === 'isabelle_hol' ? 'thy' : 'lean';
  } catch {
    return 'lean';
  }
}

function viewerApiDir(projectInfo) {
  if (projectInfo.repoType === 'trellis') {
    return path.join(projectInfo.stateDir, 'viewer');
  }
  return path.join(projectInfo.repoPath, '.agent-supervisor', 'viewer');
}

function viewerTempDir(stateDir) {
  return path.join(stateDir, 'tmp', 'viewer');
}

function chatsRepoDir(projectInfo) {
  if (projectInfo.repoType === 'trellis') {
    return path.join(projectInfo.stateDir, 'chats');
  }
  return path.join(projectInfo.repoPath, '.agent-supervisor', 'chats');
}

function chatRepoCandidates(projectInfo) {
  const repos = [];
  const addRepo = (repoPath) => {
    if (!repoPath || !fs.existsSync(path.join(repoPath, '.git'))) return;
    if (!repos.includes(repoPath)) repos.push(repoPath);
  };
  addRepo(chatsRepoDir(projectInfo));
  // Only the live chats repo. We deliberately do NOT scan
  // rewind-quarantine repos: after a rewind the live run reuses the same
  // cycle/request numbering, so a quarantine's cycle tags collide and
  // bleed a prior attempt's artifacts into the live cycle view. Rewound
  // artifacts must never appear in the viewer.
  return repos;
}

function chatRepoForCycle(projectInfo, cycle) {
  for (const repoPath of chatRepoCandidates(projectInfo)) {
    if (hasChatCycleTag(repoPath, cycle)) return repoPath;
  }
  return null;
}

function discoverProjects() {
  if (LEGACY_REPO_PATH) {
    const repoType = repoTypeForRepo(LEGACY_REPO_PATH);
    return [{
      slug: path.basename(LEGACY_REPO_PATH),
      repoPath: LEGACY_REPO_PATH,
      repoType,
      stateDir: repoType === 'trellis'
        ? path.join(LEGACY_REPO_PATH, '.trellis')
        : path.join(LEGACY_REPO_PATH, '.agent-supervisor'),
    }];
  }
  if (!fs.existsSync(PROJECTS_ROOT)) return [];
  const entryIsProjectDir = (entry) => {
    if (entry.isDirectory()) return true;
    if (!entry.isSymbolicLink()) return false;
    try {
      return fs.statSync(path.join(PROJECTS_ROOT, entry.name)).isDirectory();
    } catch {
      return false;
    }
  };
  // `readdir` itself can throw if PROJECTS_ROOT is being deleted/recreated
  // underneath us (operator wiping runtimes). Degrade to "no projects" rather
  // than letting the error propagate into every endpoint that discovers
  // projects (default-slug resolution runs on essentially every request).
  let dirEntries;
  try {
    dirEntries = fs.readdirSync(PROJECTS_ROOT, { withFileTypes: true });
  } catch {
    return [];
  }
  return dirEntries
    .filter(entry => entryIsProjectDir(entry) && isValidProjectSlug(entry.name))
    .map(entry => {
      const repoPath = path.join(PROJECTS_ROOT, entry.name);
      const repoType = repoTypeForRepo(repoPath);
      return {
        slug: entry.name,
        repoPath,
        repoType,
        stateDir: repoType === 'trellis'
          ? path.join(repoPath, '.trellis')
          : path.join(repoPath, '.agent-supervisor'),
      };
    })
    .filter(entry => !!entry.repoType)
    .sort((a, b) => a.slug.localeCompare(b.slug));
}

function defaultProjectSlug() {
  if (DEFAULT_PROJECT_SLUG) return DEFAULT_PROJECT_SLUG;
  const projects = discoverProjects();
  return projects.length ? projects[0].slug : '';
}

function defaultPromptsProjectSlug() {
  const projects = discoverProjects();
  if (projects.some(project => project.slug === 'current')) return 'current';
  return defaultProjectSlug();
}

function resolveRepoPath(project) {
  const slug = project || defaultProjectSlug();
  if (!isValidProjectSlug(slug)) throw new Error(`Invalid project: ${project}`);
  const repoPath = LEGACY_REPO_PATH && slug === path.basename(LEGACY_REPO_PATH)
    ? LEGACY_REPO_PATH
    : path.join(PROJECTS_ROOT, slug);
  const repoType = repoTypeForRepo(repoPath);
  if (!(LEGACY_REPO_PATH && repoPath === LEGACY_REPO_PATH) && !repoType) {
    throw new Error(`Unknown project: ${slug}`);
  }
  const stateDir = repoType === 'trellis'
    ? path.join(repoPath, '.trellis')
    : path.join(repoPath, '.agent-supervisor');
  return { slug, repoPath, repoType, stateDir };
}

function readJsonFile(filePath) {
  return JSON.parse(fs.readFileSync(filePath, 'utf-8'));
}

function forEachFileLine(filePath, onLine) {
  const fd = fs.openSync(filePath, 'r');
  const decoder = new StringDecoder('utf8');
  const buf = Buffer.allocUnsafe(1024 * 1024);
  let pending = '';
  try {
    while (true) {
      const bytes = fs.readSync(fd, buf, 0, buf.length, null);
      if (!bytes) break;
      const text = pending + decoder.write(buf.subarray(0, bytes));
      let start = 0;
      while (true) {
        const idx = text.indexOf('\n', start);
        if (idx < 0) break;
        onLine(text.slice(start, idx));
        start = idx + 1;
      }
      pending = text.slice(start);
    }
    pending += decoder.end();
    if (pending) onLine(pending);
  } finally {
    fs.closeSync(fd);
  }
}

// Stream `filePath` starting at byte `startOffset`. Yields each newline-
// terminated line via `onLine`; the trailing partial line (no `\n`) is left
// unconsumed. Returns the byte offset right after the last `\n` consumed —
// always a safe resume point on the next call. Used for incremental
// tail-reads of append-only logs.
//
// Works in raw bytes (mirroring the Python helper in chat_history.py): the
// previous StringDecoder-based implementation lost the exact byte boundary
// when a multi-byte UTF-8 codepoint straddled the 1 MiB read boundary
// (decoder-buffered continuation bytes weren't reflected in `pending`'s
// byte length, so the returned resume offset was too high and the next
// call skipped 1–3 bytes — corrupting the following JSON record).
function forEachFileLineFromOffset(filePath, startOffset, onLine) {
  const fd = fs.openSync(filePath, 'r');
  const buf = Buffer.allocUnsafe(1024 * 1024);
  let pending = Buffer.alloc(0);
  let position = startOffset;        // next file byte to read
  let consumedOffset = startOffset;  // file offset just past last consumed '\n'
  try {
    while (true) {
      const bytes = fs.readSync(fd, buf, 0, buf.length, position);
      if (!bytes) break;
      position += bytes;
      // `chunk` is either the freshly-read slice (a view into `buf`, only
      // safe within this iteration) or a fresh buffer concatenating leftover
      // bytes from the previous iteration with the new read.
      const chunk = pending.length
        ? Buffer.concat([pending, buf.subarray(0, bytes)])
        : buf.subarray(0, bytes);
      let start = 0;
      while (true) {
        const idx = chunk.indexOf(0x0A, start);  // '\n'
        if (idx < 0) break;
        onLine(chunk.subarray(start, idx).toString('utf8'));
        consumedOffset += (idx - start) + 1;
        start = idx + 1;
      }
      // The trailing partial-line bytes become next iteration's `pending`.
      // Must be its own buffer because `buf` will be overwritten on the next
      // read; Buffer.from copies.
      const trailing = chunk.subarray(start);
      pending = trailing.length ? Buffer.from(trailing) : Buffer.alloc(0);
    }
    return consumedOffset;
  } finally {
    fs.closeSync(fd);
  }
}

function git(repoPath, args) {
  return execSync(`git ${args}`, {
    cwd: repoPath,
    encoding: 'utf-8',
    timeout: 10000,
    // Chat output.logs (esp. audits) can be multi-MB; the Node default
    // 1MB maxBuffer made `git show` ENOBUFS-fail, which the callers
    // swallow, dropping all entries (only the prompt survives) and
    // erroring the viewer on past-cycle artifacts.
    maxBuffer: 256 * 1024 * 1024,
    stdio: ['ignore', 'pipe', 'ignore'],
  }).trim();
}

const gitRefExistsCache = new Set();

function gitRefExists(repoPath, ref) {
  const key = `${repoPath}\0${ref}`;
  if (gitRefExistsCache.has(key)) return true;
  try {
    execFileSync('git', ['-C', repoPath, 'rev-parse', '--verify', '--quiet', `${ref}^{commit}`], {
      encoding: 'utf-8',
      timeout: 10000,
      stdio: ['ignore', 'ignore', 'ignore'],
    });
    gitRefExistsCache.add(key);
    return true;
  } catch {
    return false;
  }
}

function chatCycleTag(cycle) {
  return `cycle-${Number(cycle)}`;
}

function hasChatCycleTag(chatsRepo, cycle) {
  const n = Number(cycle);
  return Number.isFinite(n) && gitRefExists(chatsRepo, chatCycleTag(n));
}

function trellisAdapter(projectInfo, command, extraArgs = [], stdinObject = null) {
  const env = {
    ...process.env,
    PYTHONPATH: process.env.PYTHONPATH
      ? `${TRELLIS_ROOT}:${process.env.PYTHONPATH}`
      : TRELLIS_ROOT,
  };
  const args = ['-m', 'trellis.viewer_adapter', command, projectInfo.repoPath, ...extraArgs];
  const options = {
    cwd: TRELLIS_ROOT,
    env,
    encoding: 'utf-8',
    timeout: 30000,
    // 1 GiB: the run's live-state JSON has grown past the old 32 MiB cap
    // (protocol_state is ~48 MB+ and climbing), which made the adapter fail
    // and the DAG/state view render blank. Mirrors SUPERVISOR_STATE_MAX_BUFFER.
    maxBuffer: 1024 * 1024 * 1024,
  };
  if (stdinObject !== null) {
    options.input = JSON.stringify(stdinObject);
  }
  const raw = execFileSync('python3', args, options);
  return JSON.parse(raw);
}

// Async variant of `trellisAdapter` — uses `spawn` so the Node.js event
// loop stays unblocked while python is computing. Use this from any
// endpoint that is HOT (called on every page load / auto-refresh / tab
// open) so concurrent browsers don't serialize behind each other's
// `execFileSync` calls. Returns a Promise of the parsed JSON payload.
//
// Pairs with `_adapterInFlightOnce` for request coalescing: callers
// that want concurrent fetches to share one python spawn should wrap
// the call with `_adapterInFlightOnce(key, () => trellisAdapterAsync(…))`.
function trellisAdapterAsync(projectInfo, command, extraArgs = [], stdinObject = null) {
  return new Promise((resolve, reject) => {
    const env = {
      ...process.env,
      PYTHONPATH: process.env.PYTHONPATH
        ? `${TRELLIS_ROOT}:${process.env.PYTHONPATH}`
        : TRELLIS_ROOT,
    };
    const args = ['-m', 'trellis.viewer_adapter', command, projectInfo.repoPath, ...extraArgs];
    const child = spawn('python3', args, {
      cwd: TRELLIS_ROOT,
      env,
      stdio: ['pipe', 'pipe', 'pipe'],
    });
    let stdout = '';
    let stderr = '';
    let stdoutBytes = 0;
    const MAX = 1024 * 1024 * 1024;  // 1 GiB — live-state JSON outgrew 32 MiB (see trellisAdapter)
    const timer = setTimeout(() => {
      try { child.kill('SIGTERM'); } catch {}
      reject(new Error(`trellisAdapterAsync(${command}) timed out after 30s`));
    }, 30000);
    child.stdout.on('data', (chunk) => {
      stdoutBytes += chunk.length;
      if (stdoutBytes > MAX) {
        try { child.kill('SIGTERM'); } catch {}
        reject(new Error(`trellisAdapterAsync(${command}) stdout exceeded ${MAX} bytes`));
        return;
      }
      stdout += chunk.toString('utf-8');
    });
    child.stderr.on('data', (chunk) => { stderr += chunk.toString('utf-8'); });
    child.on('error', (err) => { clearTimeout(timer); reject(err); });
    child.on('close', (code) => {
      clearTimeout(timer);
      if (code !== 0) {
        reject(new Error(`trellisAdapterAsync(${command}) exited ${code}: ${stderr.slice(0, 500)}`));
        return;
      }
      try { resolve(JSON.parse(stdout)); }
      catch (e) { reject(new Error(`trellisAdapterAsync(${command}) bad JSON: ${e.message}`)); }
    });
    if (stdinObject !== null) {
      try { child.stdin.end(JSON.stringify(stdinObject)); }
      catch (e) { /* child may have already errored */ }
    } else {
      try { child.stdin.end(); } catch {}
    }
  });
}

// ---------------------------------------------------------------------------
// Historical-cycle cache
//
// Past cycles are immutable: once `cycle-N` is tagged, its viewer_state, chats,
// and diff never change. The slow path is `python3 -m trellis.viewer_adapter
// state-at N` (cold-start ~150-300ms each) and chained `git show` calls. Those
// dominate latency when the user drags the cycle slider.
//
// Strategy: in-memory cache, keyed by (projectKey, kind, cycle), populated on
// first request. Historical prewarm exists as an opt-in mode, but it is
// disabled by default because the underlying adapter/git calls are synchronous
// and can block current DAG loads on large live runs.
// ---------------------------------------------------------------------------

function projectCacheKey(projectInfo) {
  return `${projectInfo.repoPath}::${projectInfo.repoType || ''}`;
}

const cycleStateCache = new Map();   // key: `${projectKey}|${cycle}` -> data
const cycleChatsCache = new Map();   // key: `${projectKey}|${cycle}` -> data
const cycleDiffCache = new Map();    // key: `${projectKey}|${cycle}` -> string
const cyclesListCache = new Map();   // key: projectKey -> { data, ts }
const CYCLES_LIST_TTL_MS = 30 * 1000;
const ENABLE_HISTORICAL_PREWARM = process.env.TRELLIS_VIEWER_PREWARM === '1';

function cycleEntryKey(projectInfo, cycle) {
  return `${projectCacheKey(projectInfo)}|${cycle}`;
}

function getCachedHistoricalViewerState(projectInfo, cycle) {
  const key = cycleEntryKey(projectInfo, cycle);
  if (cycleStateCache.has(key)) return cycleStateCache.get(key);
  const data = readHistoricalViewerState(projectInfo, cycle);
  // augmentViewerStateClosure and thinViewerStatePayload are function
  // declarations defined later in the file, so they're hoisted.
  if (typeof augmentViewerStateClosure === 'function') {
    augmentViewerStateClosure(data);
  }
  if (typeof augmentViewerStateAttention === 'function') {
    augmentViewerStateAttention(data);
  }
  if (typeof thinViewerStatePayload === 'function') {
    thinViewerStatePayload(data, projectCacheKey(projectInfo));
  }
  cycleStateCache.set(key, data);
  return data;
}

function getCachedHistoricalChats(projectInfo, cycle) {
  const key = cycleEntryKey(projectInfo, cycle);
  if (cycleChatsCache.has(key)) return cycleChatsCache.get(key);
  const data = readHistoricalChats(projectInfo, cycle);
  cycleChatsCache.set(key, data);
  return data;
}

function getCachedCycleDiff(projectInfo, cycle) {
  const key = cycleEntryKey(projectInfo, cycle);
  if (cycleDiffCache.has(key)) return cycleDiffCache.get(key);
  const data = getCycleDiff(projectInfo.repoPath, cycle);
  cycleDiffCache.set(key, data);
  return data;
}

function getCachedCyclesList(projectInfo) {
  const key = projectCacheKey(projectInfo);
  const entry = cyclesListCache.get(key);
  const now = Date.now();
  if (entry && (now - entry.ts) < CYCLES_LIST_TTL_MS) return entry.data;
  const data = getCyclesFromGit(projectInfo);
  cyclesListCache.set(key, { data, ts: now });
  if (ENABLE_HISTORICAL_PREWARM) schedulePrewarm(projectInfo, data);
  return data;
}

// Optional historical prewarm: walk all known cycles and prefetch state+chats.
// Each item still performs synchronous work, so this is off unless explicitly
// enabled with TRELLIS_VIEWER_PREWARM=1.

const prewarmQueued = new Set();   // projectKey|cycle keys already enqueued
const prewarmQueue = [];           // entries: { projectInfo, cycle }
let prewarmWorkerRunning = false;

function schedulePrewarm(projectInfo, cyclesList) {
  if (!Array.isArray(cyclesList)) return;
  for (const c of cyclesList) {
    const cycle = (c && typeof c === 'object') ? c.cycle : c;
    if (!Number.isInteger(cycle)) continue;
    const key = cycleEntryKey(projectInfo, cycle);
    if (prewarmQueued.has(key)) continue;
    if (cycleStateCache.has(key) && cycleChatsCache.has(key)) continue;
    prewarmQueued.add(key);
    prewarmQueue.push({ projectInfo, cycle });
  }
  if (!prewarmWorkerRunning) startPrewarmWorker();
}

function startPrewarmWorker() {
  prewarmWorkerRunning = true;
  const tick = () => {
    const item = prewarmQueue.shift();
    if (!item) {
      prewarmWorkerRunning = false;
      return;
    }
    const { projectInfo, cycle } = item;
    const stateKey = cycleEntryKey(projectInfo, cycle);
    try {
      if (!cycleStateCache.has(stateKey)) getCachedHistoricalViewerState(projectInfo, cycle);
    } catch {}
    try {
      if (!cycleChatsCache.has(stateKey)) getCachedHistoricalChats(projectInfo, cycle);
    } catch {}
    setImmediate(tick);
  };
  setImmediate(tick);
}

function readLiveViewerState(projectInfo) {
  if (projectInfo.repoType === 'trellis') {
    return trellisAdapter(projectInfo, 'live-state');
  }
  return readJsonFile(path.join(projectInfo.repoPath, '.agent-supervisor', 'viewer_state.json'));
}

function readHistoricalViewerState(projectInfo, cycle) {
  if (projectInfo.repoType === 'trellis') {
    return trellisAdapter(projectInfo, 'state-at', [String(cycle)]);
  }
  const tag = `cycle-${cycle}`;
  const raw = git(projectInfo.repoPath, `show ${tag}:.agent-supervisor/viewer_state.json`);
  return JSON.parse(raw);
}

function chatCycleDir(cycle) {
  return `cycle-${String(cycle).padStart(4, '0')}`;
}

function readTextFileSafe(filePath) {
  try {
    return fs.readFileSync(filePath, 'utf-8');
  } catch {
    return '';
  }
}

function listWorkingTreeChatArtifacts(repoPath, cycle) {
  const root = path.join(repoPath, chatCycleDir(cycle));
  if (!fs.existsSync(root)) return [];
  return sortArtifactNames(fs.readdirSync(root, { withFileTypes: true })
    .filter(entry => entry.isDirectory())
    .map(entry => entry.name)
  );
}

function listGitChatArtifacts(repoPath, cycle) {
  const chatsRepo = repoPath;
  if (!fs.existsSync(path.join(chatsRepo, '.git'))) return [];
  if (!hasChatCycleTag(chatsRepo, cycle)) return [];
  const tag = chatCycleTag(cycle);
  const prefix = chatCycleDir(cycle) + '/';
  try {
    const files = git(chatsRepo, `ls-tree -r --name-only ${tag} -- ${prefix}`)
      .split('\n')
      .filter(Boolean);
    return sortArtifactNames(Array.from(new Set(
      files
        .filter(name => name.startsWith(prefix))
        .map(name => name.slice(prefix.length).split('/')[0])
        .filter(Boolean)
    )));
  } catch {
    return [];
  }
}

function readWorkingTreeChatFiles(repoPath, cycle, artifact) {
  const dir = path.join(repoPath, chatCycleDir(cycle), artifact);
  return {
    prompt: readTextFileSafe(path.join(dir, 'prompt.txt')),
    output: readTextFileSafe(path.join(dir, 'output.log')),
    transcriptJsonl: readTextFileSafe(path.join(dir, 'transcript.jsonl')),
    transcriptJson: readTextFileSafe(path.join(dir, 'transcript.json')),
  };
}

function readGitChatFiles(repoPath, cycle, artifact, gitPrefix = null) {
  const chatsRepo = repoPath;
  if (!hasChatCycleTag(chatsRepo, cycle)) {
    return { prompt: '', output: '', transcriptJsonl: '', transcriptJson: '' };
  }
  const tag = chatCycleTag(cycle);
  const base = `${gitPrefix || chatCycleDir(cycle)}/${artifact}`;
  const read = (name) => {
    try {
      return git(chatsRepo, `show ${tag}:${base}/${name}`);
    } catch {
      return '';
    }
  };
  return {
    prompt: read('prompt.txt'),
    output: read('output.log'),
    transcriptJsonl: read('transcript.jsonl'),
    transcriptJson: read('transcript.json'),
  };
}

function artifactTitle(name) {
  let attempt = null;
  let base = name;
  let m = name.match(/^(.*)_attempt_(\d+)$/);
  if (m) {
    base = m[1];
    attempt = Number(m[2]);
  }
  if (base === 'worker_handoff') return attempt ? `Worker attempt ${attempt}` : 'Worker';
  if (base === 'reviewer_decision') return attempt ? `Reviewer attempt ${attempt}` : 'Reviewer';
  m = base.match(/^correspondence_result_(\d+)$/);
  if (m) return attempt ? `Correspondence ${Number(m[1]) + 1} attempt ${attempt}` : `Correspondence ${Number(m[1]) + 1}`;
  m = base.match(/^nl_proof_(.+)_(\d+)$/);
  if (m) {
    const title = `Soundness ${m[1]} (${Number(m[2]) + 1})`;
    return attempt ? `${title} attempt ${attempt}` : title;
  }
  const fallback = base.replace(/_/g, ' ');
  return attempt ? `${fallback} attempt ${attempt}` : fallback;
}

function artifactSortKey(name) {
  let attempt = 0;
  let base = name;
  let m = name.match(/^(.*)_attempt_(\d+)$/);
  if (m) {
    base = m[1];
    attempt = Number(m[2]);
  }
  if (base === 'worker_handoff') return [0, 0, '', attempt, name];
  if (base === 'reviewer_decision') return [3, 0, '', attempt, name];
  m = base.match(/^correspondence_result_(\d+)$/);
  if (m) return [1, Number(m[1]), '', attempt, name];
  m = base.match(/^nl_proof_(.+)_(\d+)$/);
  if (m) return [2, Number(m[2]), String(m[1]), attempt, name];
  return [4, 0, base, attempt, name];
}

function sortArtifactNames(names) {
  return [...names].sort((a, b) => {
    const ka = artifactSortKey(a);
    const kb = artifactSortKey(b);
    for (let i = 0; i < ka.length; i++) {
      if (ka[i] < kb[i]) return -1;
      if (ka[i] > kb[i]) return 1;
    }
    return 0;
  });
}

function collectTextParts(value, parts) {
  if (typeof value === 'string') {
    const trimmed = value.trim();
    if (trimmed) parts.push(trimmed);
    return;
  }
  if (Array.isArray(value)) {
    for (const item of value) collectTextParts(item, parts);
    return;
  }
  if (!value || typeof value !== 'object') return;
  if (typeof value.text === 'string') {
    const trimmed = value.text.trim();
    if (trimmed) parts.push(trimmed);
  }
  for (const key of ['content', 'parts', 'chunks', 'value']) {
    if (key in value) collectTextParts(value[key], parts);
  }
}

function normalizeTranscriptEntry(role, text, kind = 'message', title = '') {
  const trimmed = (text || '').trim();
  if (!trimmed) return null;
  return { role: role || 'entry', kind, title: title || '', text: trimmed };
}

function parseCodexOutputEntries(text) {
  const entries = [];
  for (const rawLine of (text || '').split(/\r?\n/)) {
    const line = rawLine.trim();
    if (!line) continue;
    let rec;
    try {
      rec = JSON.parse(line);
    } catch {
      continue;
    }
    if (rec.type === 'item.completed' && rec.item && rec.item.type === 'agent_message') {
      const entry = normalizeTranscriptEntry('assistant', rec.item.text || '', 'message', 'Assistant');
      if (entry) entries.push(entry);
      continue;
    }
    if (rec.item && rec.item.type === 'command_execution' && (rec.type === 'item.completed' || rec.type === 'item.started')) {
      const command = String(rec.item.command || '').trim();
      const output = String(rec.item.aggregated_output || '').trim();
      const label = rec.type === 'item.started' ? 'Command (running)' : 'Command';
      const combined = [command, output].filter(Boolean).join('\n\n');
      const entry = normalizeTranscriptEntry('tool', combined, 'command', label);
      if (entry) entries.push(entry);
    }
  }
  return entries;
}

function parseJsonlTranscriptEntries(text) {
  const entries = [];
  for (const rawLine of (text || '').split(/\r?\n/)) {
    const line = rawLine.trim();
    if (!line) continue;
    let rec;
    try {
      rec = JSON.parse(line);
    } catch {
      continue;
    }
    const msg = rec.message && typeof rec.message === 'object' ? rec.message : rec;
    const role = msg.role || rec.role || rec.type || '';
    const parts = [];
    collectTextParts(msg.content ?? rec.content ?? msg, parts);
    const entry = normalizeTranscriptEntry(role, parts.join('\n\n'), 'message', role || 'Entry');
    if (entry) entries.push(entry);
  }
  return entries;
}

function parseJsonTranscriptEntries(text) {
  let data;
  try {
    data = JSON.parse(text);
  } catch {
    return [];
  }
  const entries = [];
  const messages = Array.isArray(data?.messages) ? data.messages : [];
  for (const msg of messages) {
    const role = msg.role || msg.author || msg.speaker || '';
    const parts = [];
    collectTextParts(msg.content ?? msg.parts ?? msg, parts);
    const entry = normalizeTranscriptEntry(role, parts.join('\n\n'), 'message', role || 'Entry');
    if (entry) entries.push(entry);
  }
  if (entries.length) return entries;
  const parts = [];
  collectTextParts(data, parts);
  const fallback = normalizeTranscriptEntry('entry', parts.join('\n\n'), 'message', 'Transcript');
  return fallback ? [fallback] : [];
}

function buildArtifactChatData(name, files) {
  const entries = [];
  if (files.prompt) {
    entries.push({
      role: 'prompt',
      kind: 'prompt',
      title: 'Prompt',
      text: files.prompt.trim(),
    });
  }
  if (files.output) entries.push(...parseCodexOutputEntries(files.output));
  else if (files.transcriptJsonl) entries.push(...parseJsonlTranscriptEntries(files.transcriptJsonl));
  else if (files.transcriptJson) entries.push(...parseJsonTranscriptEntries(files.transcriptJson));
  return {
    id: name,
    title: artifactTitle(name),
    entries,
    hasTranscript: Boolean(files.output || files.transcriptJsonl || files.transcriptJson),
  };
}

function currentInFlightCycle(projectInfo) {
  const viewer = readLiveViewerState(projectInfo);
  return Number(viewer?.meta?.in_flight_cycle || viewer?.state?.cycle || 0);
}

function readLiveChats(projectInfo) {
  if (projectInfo.repoType === 'trellis') {
    return trellisAdapter(projectInfo, 'chats');
  }
  const cycle = currentInFlightCycle(projectInfo);
  if (!cycle) return { cycle: 0, source: 'live', artifacts: [] };
  const chatsRoot = chatsRepoDir(projectInfo);
  const artifacts = listWorkingTreeChatArtifacts(chatsRoot, cycle)
    .map(name => buildArtifactChatData(name, readWorkingTreeChatFiles(chatsRoot, cycle, name)));
  return { cycle, source: 'live', artifacts };
}

function readHistoricalChats(projectInfo, cycle) {
  if (projectInfo.repoType === 'trellis') {
    const { names, prefixes, repos } = listCandidateChatDirs(projectInfo, cycle);
    if (names.length) {
      const artifacts = sortArtifactNames(names)
        .map(name => buildArtifactChatData(
          name,
          readGitChatFiles(repos[name], cycle, name, prefixes[name])
        ));
      return { cycle, source: 'git', artifacts };
    }
    return trellisAdapter(projectInfo, 'chats-at', [String(cycle)]);
  }
  const chatsRoot = chatsRepoDir(projectInfo);
  const artifacts = listGitChatArtifacts(chatsRoot, cycle)
    .map(name => buildArtifactChatData(name, readGitChatFiles(chatsRoot, cycle, name)));
  return { cycle, source: 'git', artifacts };
}

function getCyclesFromGit(projectInfo) {
  if (projectInfo.repoType === 'trellis') {
    return trellisAdapter(projectInfo, 'cycles');
  }
  const repoPath = projectInfo.repoPath;
  let tags;
  try {
    tags = git(repoPath, 'tag -l "cycle-*" --sort=version:refname').split('\n').filter(t => /^cycle-\d+$/.test(t));
  } catch { return []; }

  return tags.map(tag => {
    const cycle = parseInt(tag.replace('cycle-', ''), 10);
    let hash = '', timestamp = '', subject = '';
    try {
      const log = git(repoPath, `log -1 --format=%H%n%aI%n%s ${tag}`);
      const parts = log.split('\n');
      hash = parts[0] || '';
      timestamp = parts[1] || '';
      subject = parts[2] || '';
    } catch {}

    // Read cycle_meta.json from that commit
    let meta = {};
    try {
      const raw = git(repoPath, `show ${tag}:.agent-supervisor/cycle_meta.json`);
      meta = JSON.parse(raw);
    } catch {}

    return { cycle, hash, timestamp, message: subject, ...meta };
  });
}

function getCycleDiff(repoPath, cycle) {
  const tag = `cycle-${cycle}`;
  const prevTag = `cycle-${cycle - 1}`;
  try {
    // Check if previous tag exists
    git(repoPath, `rev-parse ${prevTag}`);
    return git(repoPath, `diff ${prevTag} ${tag} -- Tablet/`);
  } catch {
    try {
      // First cycle — diff against empty tree
      return git(repoPath, `diff 4b825dc642cb6eb9a060e54bf899d15f3bc9 ${tag} -- Tablet/`);
    } catch { return ''; }
  }
}

function ensureSymlink(linkPath, targetPath) {
  fs.mkdirSync(path.dirname(linkPath), { recursive: true });
  try {
    const existing = fs.lstatSync(linkPath);
    if (existing.isSymbolicLink() && fs.readlinkSync(linkPath) === targetPath) return;
    fs.rmSync(linkPath, { recursive: true, force: true });
  } catch {}
  fs.symlinkSync(targetPath, linkPath);
}

function writeProjectStatic(projectInfo, { writeRoot = false } = {}) {
  const { slug } = projectInfo;
  const roots = [path.join(STATIC_OUT, slug)];
  if (writeRoot) roots.unshift(STATIC_OUT);
  const apiTarget = viewerApiDir(projectInfo);
  const htmlSrc = path.join(__dirname, 'public', 'index.html');

  for (const root of roots) {
    fs.mkdirSync(root, { recursive: true });
    ensureSymlink(path.join(root, 'api'), apiTarget);
    if (fs.existsSync(htmlSrc)) {
      ensureSymlink(path.join(root, 'index.html'), htmlSrc);
    }
  }
}

function writeStatic() {
  try {
    const projects = discoverProjects();
    const defaultSlug = defaultProjectSlug();
    for (const projectInfo of projects) {
      writeProjectStatic(projectInfo, { writeRoot: projectInfo.slug === defaultSlug });
    }
  } catch (e) {
    console.error('Static write error:', e.message);
  }
}

function projectFromRequest(req) {
  return req.params.project || defaultProjectSlug();
}

// ---------------------------------------------------------------------------
// Token delivery: a page served under the `${BASE}/<token>` prefix gets the
// token injected, and no other page does.
//
// That is the whole rule now. The browser learned the token by being pointed
// at the URL; handing it back into the page is what lets the page put it in
// the `X-Trellis-Control` header on every mutation, which is still the thing
// that authorizes a write and still the thing a cross-origin page cannot
// forge.
//
// The static export symlinks `~/trellis-web/<slug>/index.html` straight to
// `public/index.html`, which never passes through this function — so the
// exported copy stays tokenless by construction.
const PUBLIC_HTML_CACHE = new Map();
const LANDING_HTML_TRANSFORMS = [];

// Optional, release-removable viewer modules may add their client chunk at
// serve time. Keeping the tag out of landing.html means removing the module
// removes the browser surface too, rather than leaving a dead control behind.
function registerLandingHtmlTransform(transform) {
  if (typeof transform !== 'function') throw new Error('landing HTML transform must be a function');
  LANDING_HTML_TRANSFORMS.push(transform);
}

function applyLandingHtmlTransforms(html, req) {
  let rendered = html;
  for (const transform of LANDING_HTML_TRANSFORMS) rendered = transform(rendered, req);
  return rendered;
}

function readPublicHtml(fileName) {
  const file = path.join(__dirname, 'public', fileName);
  const stat = fs.statSync(file);
  const key = `${file}:${stat.mtimeMs}:${stat.size}`;
  const cached = PUBLIC_HTML_CACHE.get(fileName);
  if (cached && cached.key === key) return cached.html;
  const html = fs.readFileSync(file, 'utf-8');
  PUBLIC_HTML_CACHE.set(fileName, { key, html });
  return html;
}

// Why this request may (or may not) be handed the token. Returns a reason
// string when the answer is no, so the page can say what to do about it.
function controlDeliveryDecision(req) {
  if (!CONTROL_ENABLED) return { deliver: false, reason: 'TRELLIS_VIEWER_CONTROL=0' };
  if (!(req && req.controlMode)) {
    return { deliver: false, reason: 'no control token in the URL path' };
  }
  // An unusable token file must degrade to "no controls", never to a 500 on
  // every page load.
  try {
    controlToken();
  } catch (e) {
    return { deliver: false, reason: `control token unavailable: ${e.message}` };
  }
  return { deliver: true, reason: 'served under the control-token path prefix' };
}

function controlBootstrapScript(req) {
  const decision = controlDeliveryDecision(req);
  if (!decision.deliver) {
    // The reason is operator-facing and must not become an injection point.
    const safe = String(decision.reason).replace(/[^\x20-\x7e]/g, '').replace(/[<>*/\\]/g, '');
    return `window.__TRELLIS_CONTROL__ = null; /* ${safe} */`;
  }
  // `</script>` cannot appear in hex, but serialize defensively anyway —
  // this string is spliced into a script element.
  const payload = JSON.stringify({ header: CONTROL_HEADER, token: controlToken() })
    .replace(/</g, '\\u003c');
  return `window.__TRELLIS_CONTROL__ = ${payload};`;
}

function sendHtmlWithControlToken(req, res, fileName) {
  let html;
  try {
    html = readPublicHtml(fileName);
  } catch (e) {
    res.status(500).type('text/plain').send(`cannot read ${fileName}: ${e.message}`);
    return;
  }
  // A document carrying a secret must not be written to the browser's disk
  // cache or to any shared cache in front of it.
  res.set('Cache-Control', 'no-store');
  // The token is in the URL PATH now, and a Referer header carries the full
  // path — so these pages, which load KaTeX from a CDN, would otherwise
  // announce the token to that CDN on every subresource request. Suppressing
  // Referer entirely still covers it under the path scheme, exactly as it did
  // under the old query-string scheme.
  res.set('Referrer-Policy', 'no-referrer');
  if (fileName === 'landing.html') {
    html = applyLandingHtmlTransforms(html, req);
  }
  res.type('html').send(html.replace(CONTROL_TOKEN_MARKER, controlBootstrapScript(req)));
}

function sendIndex(req, res) {
  sendHtmlWithControlToken(req, res, 'index.html');
}

// `${BASE}/` was a bare redirect to the default project. It now serves the
// landing page; `?goto=default` preserves the old behaviour for anyone who
// bookmarked it, and every `${BASE}/:project/` deep link is untouched.
function sendLanding(req, res) {
  if (String((req.query && req.query.goto) || '') === 'default') {
    const slug = defaultProjectSlug();
    if (slug) {
      // Stay in control mode across the redirect when we arrived in it.
      res.redirect(`${basePathFor(req)}/${slug}/`);
      return;
    }
  }
  sendHtmlWithControlToken(req, res, 'landing.html');
}

function sendPromptsIndex(_req, res) {
  res.sendFile(path.join(__dirname, 'public', 'prompts.html'));
}

function handleDownloadTablet(res, project) {
  const projectInfo = resolveRepoPath(project);
  const { repoPath, stateDir, repoType } = projectInfo;
  let state = {};
  let nodeEntries = [];
  const configPath = configPathForRepo(repoPath);
  // Backend split, via the same config-derived descriptor the file-copy
  // below uses: `thy` marks an `isabelle_hol` run (Isabelle README and
  // kernel-recorded protected list); anything else renders the historical
  // Lean package byte-identically.
  const backendExt = backendNodeExtForRepo(repoPath);
  const isIsabelle = backendExt === 'thy';
  if (repoType === 'trellis') {
    const viewerState = readLiveViewerState(projectInfo);
    state = viewerState.state || {};
    const nodes = viewerState.nodes || {};
    nodeEntries = Object.entries(nodes).filter(([name]) => name !== 'Preamble');
  } else {
    const tablet = JSON.parse(fs.readFileSync(path.join(stateDir, 'tablet.json'), 'utf-8'));
    state = JSON.parse(fs.readFileSync(path.join(stateDir, 'state.json'), 'utf-8'));
    nodeEntries = Object.entries(tablet.nodes || {}).filter(([name]) => name !== 'Preamble');
  }
  const tabletDir = path.join(repoPath, 'Tablet');
  const paperDir = path.join(repoPath, 'paper');
  const paperFiles = fs.existsSync(paperDir)
    ? fs.readdirSync(paperDir).filter((f) => fs.statSync(path.join(paperDir, f)).isFile()).sort()
    : [];

  const nodeList = nodeEntries.map(([n, nd]) => {
      return `  - ${n}: ${nd.status || 'open'} (${nd.kind || '?'})${nd.difficulty ? ', ' + nd.difficulty : ''}${nd.title ? ' -- ' + nd.title : ''}`;
  }).join('\n');

  let mainResultTargets = [];
  if (fs.existsSync(configPath)) {
    try {
      const config = JSON.parse(fs.readFileSync(configPath, 'utf-8'));
      const rawTargets = (((config || {}).workflow || {}).main_result_targets);
      if (Array.isArray(rawTargets)) {
        mainResultTargets = rawTargets;
      }
    } catch (_err) {
      mainResultTargets = [];
    }
  }
  // Isabelle protected list source: the kernel's own per-target protected
  // closure, read from the protocol state's `live` observation slot
  // (`live.protected_closure_nodes_per_target` — the map AdvancePhase
  // Approve unions into `approved_targets.protected_closure_nodes`,
  // `engine.rs`). Populated per worker burst by the backend's closure
  // recorder, so it is the kernel's actual protection contract.
  let kernelClosureByTarget = {};
  try {
    const rt = runtimeRootForProject(projectInfo);
    if (rt) {
      const ps = JSON.parse(fs.readFileSync(path.join(rt, 'protocol_state.json'), 'utf-8'));
      const liveSlot = (ps && typeof ps.live === 'object' && ps.live) || {};
      if (typeof liveSlot.protected_closure_nodes_per_target === 'object' && liveSlot.protected_closure_nodes_per_target) {
        kernelClosureByTarget = liveSlot.protected_closure_nodes_per_target;
      }
    }
  } catch (_err) { kernelClosureByTarget = {}; }

  const targetList = (mainResultTargets.length ? mainResultTargets : []).map((target) => {
    const label = String((target || {}).tex_label || '').trim();
    const hasStart = Number.isInteger(target?.start_line);
    const hasEnd = Number.isInteger(target?.end_line);
    let lineText = '';
    if (hasStart && hasEnd) {
      lineText = target.start_line === target.end_line
        ? `line ${target.start_line}`
        : `lines ${target.start_line}-${target.end_line}`;
    }
    if (label && lineText) return `- \`${label}\` (${lineText})`;
    if (label) return `- \`${label}\``;
    if (lineText) return `- ${lineText}`;
    return '- (invalid target entry)';
  }).join('\n');
  const targetSection = targetList || '- No configured main-result targets found in `lagent.config.json`.';
  const trustedEntries = Object.values((state || {}).trusted_main_result_target_state || {}).filter((entry) => entry && typeof entry === 'object');
  const trustedProtectedNodes = [...new Set(trustedEntries.flatMap((entry) => [
    ...(((entry || {}).protected_nodes) || []),
    ...(((entry || {}).nodes) || []),
  ].map((name) => String(name || '').trim()).filter(Boolean)))].sort();
  const pendingProtectedNodes = [...new Set((((state || {}).last_review || {}).protected_nodes || []).map((name) => String(name || '').trim()).filter(Boolean))].sort();
  const pendingTargetNodes = [...new Set((((state || {}).last_review || {}).protected_target_nodes || []).map((name) => String(name || '').trim()).filter(Boolean))].sort();
  // Live coverage = current target → covering-node map. Mirrors what the
  // kernel's `approved_target_nodes()` snapshots at AdvancePhase
  // (`model.rs:2012`). Used as a fallback when no AdvancePhase decision
  // is in flight yet (`state.last_review.protected_*` empty) and no prior
  // advance has been approved (`trusted_main_result_target_state` empty).
  const liveCoverage = (state || {}).coverage || {};
  const liveCoverageRoots = [...new Set(Object.values(liveCoverage).flatMap((nodes) => (nodes || []).map((name) => String(name || '').trim()).filter(Boolean)))].sort();
  const protectedRootsRaw = pendingTargetNodes.length
    ? pendingTargetNodes
    : [...new Set(trustedEntries.flatMap((entry) => (((entry || {}).nodes) || []).map((name) => String(name || '').trim()).filter(Boolean)))].sort();
  const protectedRoots = protectedRootsRaw.length ? protectedRootsRaw : liveCoverageRoots;
  // Narrow Lean semantic closure of the protected roots (per
  // `scripts/lean_semantic_fingerprint.lean`'s closure policy: theorem
  // → walk type only; def → walk type and value; stop at the
  // `Tablet.*` boundary). The `extras` set is the project-defined
  // descendants the seed roots transitively reference in their *type*
  // signatures — i.e., the additional nodes whose meaning-bearing
  // changes would actually shift the targets' meaning, as opposed to
  // proof-only support which the closure policy excludes. Returned as
  // `{ <root>: [<descendant>, ...], ... }`. On any error (no runtime,
  // missing cache, etc.) we fall back to an empty closure so the
  // README still renders the target-root section.
  let semanticClosureByRoot = {};
  let semanticClosureExtras = [];
  let semanticClosureFootnote = '';
  // Lean runs only: the adapter's `semantic-closure` reads the Lean payload
  // sidecar cache (`scripts/lean_semantic_fingerprint.lean` output), which
  // an `isabelle_hol` run never writes — the Isabelle protected list comes
  // from `kernelClosureByTarget` below instead.
  if (repoType === 'trellis' && protectedRoots.length && !isIsabelle) {
    const closureArgs = protectedRoots.flatMap((root) => ['--node', root]);
    try {
      const closurePayload = trellisAdapter(projectInfo, 'semantic-closure', closureArgs);
      if (closurePayload && closurePayload.ok && closurePayload.closures) {
        semanticClosureByRoot = closurePayload.closures || {};
        const extras = new Set();
        const protectedRootSet = new Set(protectedRoots);
        for (const [, descendants] of Object.entries(semanticClosureByRoot)) {
          for (const name of (descendants || [])) {
            const trimmed = String(name || '').trim();
            if (trimmed && !protectedRootSet.has(trimmed)) {
              extras.add(trimmed);
            }
          }
        }
        semanticClosureExtras = [...extras].sort();
        const missing = protectedRoots.filter((root) => semanticClosureByRoot[root] === null);
        if (missing.length) {
          semanticClosureFootnote = `\n\n_Note: no cached Lean semantic-closure payload was found for ${missing.map((m) => `\`${m}\``).join(', ')}. The reviewer should treat the listed extras as a lower bound and check the type signatures by hand for any project-defined symbol they reference._`;
        }
      } else if (closurePayload && closurePayload.error) {
        semanticClosureFootnote = `\n\n_Note: could not compute the Lean semantic closure (${closurePayload.error}). The "Type-Surface Definitions" list below may be incomplete; the reviewer should also vet any project-defined symbol named in the target-root \`.lean\` files' type signatures._`;
      }
    } catch (err) {
      semanticClosureFootnote = `\n\n_Note: semantic-closure lookup failed (${err && err.message ? err.message : err}); the "Type-Surface Definitions" list below may be incomplete._`;
    }
  }
  // Isabelle: the kernel's recorded closure, unioned across targets, minus
  // the roots (those are labelled as roots) and Preamble (defensive — the
  // kernel-side recorder already filters it). Empty on Lean runs, so the
  // Lean union below is unchanged.
  let kernelClosureExtras = [];
  if (isIsabelle) {
    const extras = new Set();
    const protectedRootSet = new Set(protectedRoots);
    for (const nodes of Object.values(kernelClosureByTarget)) {
      for (const name of (Array.isArray(nodes) ? nodes : [])) {
        const trimmed = String(name || '').trim();
        if (trimmed && trimmed !== 'Preamble' && !protectedRootSet.has(trimmed)) {
          extras.add(trimmed);
        }
      }
    }
    kernelClosureExtras = [...extras].sort();
  }
  const protectedNodesUnion = [...new Set([...(pendingProtectedNodes.length ? pendingProtectedNodes : trustedProtectedNodes), ...protectedRoots, ...semanticClosureExtras, ...kernelClosureExtras])].sort();
  const protectedIntro = (() => {
    if (!protectedNodesUnion.length) {
      return 'No protected-node snapshot is currently available.';
    }
    const baseStanza = pendingProtectedNodes.length
      ? 'If you approve this package, the following nodes will become protected from later meaning-bearing changes without renewed expert review.'
      : (pendingTargetNodes.length || trustedProtectedNodes.length
        ? 'These nodes are currently protected from later meaning-bearing changes without renewed expert review.'
        : 'These are the nodes whose meaning will become protected from later changes without renewed expert review if you approve this package. The list is derived from the live target-coverage map: the target roots below, plus the project-defined definitions the kernel would reach by walking each root\'s Lean type signature (per `scripts/lean_semantic_fingerprint.lean`\'s closure policy — proof bodies are excluded, so lemmas used only in proofs do not appear here).');
    return baseStanza;
  })();
  const protectedSection = (() => {
    if (!protectedNodesUnion.length) {
      return '- (no protected nodes available)';
    }
    const labelOf = (name) => {
      const isRoot = protectedRoots.includes(name);
      const isExtra = semanticClosureExtras.includes(name);
      if (isRoot) return ' (target root)';
      if (isExtra) {
        const sources = Object.entries(semanticClosureByRoot)
          .filter(([, descendants]) => Array.isArray(descendants) && descendants.includes(name))
          .map(([root]) => `\`${root}\``);
        if (sources.length) {
          return ` (in semantic closure of ${sources.join(', ')})`;
        }
        return ' (in semantic closure of a target root)';
      }
      return '';
    };
    return protectedNodesUnion.map((name) => `- \`${name}\`${labelOf(name)}`).join('\n') + semanticClosureFootnote;
  })();
  const protectedClosureExplanation = protectedNodesUnion.length
    ? `

The list above is exactly what the kernel will treat as protected. It is the union of:

- the **target roots** — the per-target covering nodes ${protectedRoots.length ? `(\`${protectedRoots.join('`, `')}\`)` : ''} that the kernel snapshots into \`approved_target_nodes\` at AdvancePhase, AND
- the **type-surface definitions** they reach — the project-defined definitions transitively named in those roots' Lean *type* signatures (or in the values of definitions reached the same way). Proof-body content is intentionally excluded: per the closure policy in \`scripts/lean_semantic_fingerprint.lean\`, a theorem's proof can change without changing what the theorem *means*, so lemmas used only inside a proof do not enter this set.

If any node on the list above changes its Lean or \`.tex\` meaning later, the per-target paper-faithfulness fingerprint diverges from the snapshot the kernel took at this AdvancePhase, and the system reopens this gate for renewed expert review. Other supporting nodes — proof-only lemmas, helper definitions reached only through proof bodies, etc. — are *not* on the protected list and may evolve freely without rebooting expert review, so long as the protected nodes' meanings stay fixed.`
    : '';
  const paperReferenceText = paperFiles.length === 0
    ? 'the `paper/` directory in this zip'
    : paperFiles.length === 1
      ? `\`paper/${paperFiles[0]}\``
      : paperFiles.map((f) => `\`paper/${f}\``).join(', ');

  let readme = `# Proof Tablet Snapshot

This package is for external expert review of statement correspondence.

## Expert Task

The files in this zip are part of a project to formalize results from the paper provided here as ${paperReferenceText}. The particular formalization targets are listed below.

This project has just completed its initial phase: constructing a coarse skeleton of the paper, organized as a DAG of nodes, where each node contains a Lean statement and a corresponding \`.tex\` statement. At this stage, most proof-bearing Lean files still contain \`sorry\` in place of completed proofs.

The next phase of the project is to replace those \`sorry\`s with valid Lean proofs, possibly while adding further supporting nodes, until every Lean statement in this package has a sorry-free proof.

Your task is to decide whether, given the current coarse DAG of Lean/\`.tex\` node pairs, successful completion of that later proof-writing phase would genuinely amount to a formalization of the target paper results, provided the statements and definitions currently present here are not changed.

The natural way to make that judgment is to check that:
- the \`.tex\` statements genuinely cover the formalization targets listed below;
- the Lean statements genuinely correspond to those \`.tex\` statements;
- the supporting nodes form a paper-faithful support package for those targets, rather than introducing unnecessary or paper-distorting claims.

Do not assume that the name of a project-defined symbol, node title, or surrounding prose tells you its real meaning. For any non-Mathlib definition or statement introduced in this package, you should verify the meaning directly from the Lean code and the matching \`.tex\` file, and if necessary from the surrounding tablet files that use it.

A confident judgment should not depend on how the remaining proofs are later filled in, or on what additional support nodes might later be added, so long as the current statement/definition package is left unchanged.

This is not a request to review whether the Lean proofs are finished. In this package, \`sorry\` is expected in proof-bearing Lean declarations. The question is whether the statements are the right ones, and whether they are organized in a way that is sufficient for a faithful formalization of the paper's main results.

## Configured Main-Result Targets

- These are the paper items that matter for this review:
${targetSection}

## Protected Nodes

${protectedIntro}

${protectedSection}${protectedClosureExplanation}

## What To Check

- Each configured target should be covered by one or more non-\`helper\` nodes whose Lean and \`.tex\` statements genuinely match.
- If multiple non-\`helper\` nodes share a target, they should together completely cover that target.
- For every node on the **Protected Nodes** list above (target roots **and** type-surface definitions), confirm the Lean declaration's *type* faithfully matches the paired \`.tex\` statement. The list is exactly what's being frozen by your approval; you do not need to vouch for the meaning of any other supporting node.
- Project-defined definitions in that list are part of the correspondence task: do not trust their intended meaning from naming alone; verify that each such Lean definition really captures the concept used in the matching \`.tex\` statement and in the target roots that reference it.
- Other supporting nodes may exist in the package and may even be referenced by the protected nodes' *proofs*. They are not on the protected list and you are not being asked to vouch for them; they may freely evolve later as long as the protected nodes' meanings stay fixed.
- The relevant question is whether the protected node set above is good enough to treat as the trusted coarse formalization of the paper's main results.

## What May Change Without Further Expert Review

After this package is approved, later work may continue without renewed expert review as long as none of the **Protected Nodes** listed above change Lean/\`.tex\` meaning.

- Lean proof-body work is allowed on every node, including replacing \`sorry\` in protected proof-bearing nodes.
- Proof-only edits to \`.tex\` are allowed on every node.
- New supporting nodes can be added, and existing non-protected supporting nodes can be edited, refactored, or removed — provided the **Protected Nodes** themselves keep the same Lean type signatures and the same \`.tex\` statement meaning.
- Definitions outside the protected list may be edited or removed even if a protected node's *proof* used to reference them, because proof-only support is intentionally outside the protected meaning surface.

## What Would Require Renewed Expert Review

- Changing the meaning of a Lean or \`.tex\` statement for any **Protected Node** listed above (target root or type-surface definition).
- Adding, removing, or swapping the nodes that cover a configured target.
- Changing the configured target list itself.
- Adding a new project-defined definition into a protected node's *type* signature (this would extend the type-surface and is not pre-approved).

## Nodes

${nodeList}

## Structure

- \`Tablet/Preamble.lean\` — shared imports (no definitions here)
- \`Tablet/<name>.lean\` — Lean 4 declaration (theorem/lemma/def)
- \`Tablet/<name>.tex\` — natural-language statement and, for proof-bearing nodes, a rigorous NL proof
- \`paper/\` — source paper files, including ${paperReferenceText}
`;

  // ── Isabelle README ───────────────────────────────────────────────────
  // An `isabelle_hol` run ships `.thy` theories, one per node, and its
  // protected list is the kernel's own record: the target roots plus
  // `kernelClosureByTarget` (read from the runtime protocol state above).
  // The Lean semantic-closure machinery has no Isabelle analogue, so this
  // branch neither runs it nor borrows its type-surface language. The Lean
  // template above renders untouched on Lean runs.
  if (isIsabelle) {
    const isaProtectedIntro = (() => {
      if (!protectedNodesUnion.length) {
        return 'No protected-node snapshot is currently available.';
      }
      if (pendingProtectedNodes.length) {
        return 'If you approve this package, the following nodes will become protected from later meaning-bearing changes without renewed expert review.';
      }
      if (pendingTargetNodes.length || trustedProtectedNodes.length) {
        return 'These nodes are currently protected from later meaning-bearing changes without renewed expert review.';
      }
      return 'These are the nodes whose meaning will become protected from later changes without renewed expert review if you approve this package. The list is derived from the live target-coverage map: the target roots below, plus the protected closure the kernel has recorded for each target.';
    })();
    const isaLabelOf = (name) => {
      if (protectedRoots.includes(name)) return ' (target root)';
      const sources = Object.entries(kernelClosureByTarget)
        .filter(([, nodes]) => Array.isArray(nodes) && nodes.some((n) => String(n || '').trim() === name))
        .map(([target]) => `\`${target}\``);
      if (sources.length) return ` (recorded protected closure of ${sources.join(', ')})`;
      return '';
    };
    const isaProtectedSection = protectedNodesUnion.length
      ? protectedNodesUnion.map((name) => `- \`${name}\`${isaLabelOf(name)}`).join('\n')
      : '- (no protected nodes available)';
    const isaRootsParen = protectedRoots.length ? `(\`${protectedRoots.join('`, `')}\`) ` : '';
    const isaClosureStanza = kernelClosureExtras.length
      ? `It is the union of:

- the **target roots** — the per-target covering nodes ${isaRootsParen}that the kernel snapshots into \`approved_target_nodes\` at AdvancePhase, AND
- the **recorded protected closure** — the project-defined nodes named in those covering statements, as recorded by the kernel (\`${kernelClosureExtras.join('`, `')}\`).`
      : `It holds the target roots — the per-target covering nodes ${isaRootsParen}that the kernel snapshots into \`approved_target_nodes\` at AdvancePhase. The kernel state records no protected closure for the configured targets.`;
    const isaProtectedClosureExplanation = protectedNodesUnion.length
      ? `

The list above is exactly what the kernel will treat as protected. ${isaClosureStanza}

The reviewer should verify the definitions those statements actually depend on. If any node on the list above changes its Isabelle or \`.tex\` meaning later, the per-target paper-faithfulness fingerprint diverges from the snapshot the kernel took at this AdvancePhase, and the system reopens this gate for renewed expert review. Supporting nodes off the list may evolve without renewed expert review, so long as the protected nodes' meanings stay fixed.`
      : '';
    readme = `# Proof Tablet Snapshot

This package is for external expert review of statement correspondence.

## Expert Task

The files in this zip are part of a project to formalize results from the paper provided here as ${paperReferenceText}. The particular formalization targets are listed below.

This project has just completed its initial phase: constructing a coarse skeleton of the paper, organized as a DAG of nodes, where each node contains an Isabelle/HOL statement and a corresponding \`.tex\` statement. At this stage, most proof-bearing theory files still contain \`sorry\` in place of completed proofs.

The next phase of the project is to replace those \`sorry\`s with valid Isabelle proofs, possibly while adding further supporting nodes, until every Isabelle statement in this package has a sorry-free proof.

Your task is to decide whether, given the current coarse DAG of Isabelle/\`.tex\` node pairs, successful completion of that later proof-writing phase would genuinely amount to a formalization of the target paper results, provided the statements and definitions currently present here are not changed.

The natural way to make that judgment is to check that:
- the \`.tex\` statements genuinely cover the formalization targets listed below;
- the Isabelle statements genuinely correspond to those \`.tex\` statements;
- the supporting nodes form a paper-faithful support package for those targets, rather than introducing unnecessary or paper-distorting claims.

Do not assume that the name of a project-defined symbol, node title, or surrounding prose tells you its real meaning. For any definition or statement this package introduces beyond the Isabelle library, you should verify the meaning directly from the theory text and the matching \`.tex\` file, and if necessary from the surrounding tablet files that use it.

A confident judgment should not depend on how the remaining proofs are later filled in, or on what additional support nodes might later be added, so long as the current statement/definition package is left unchanged.

This is not a request to review whether the proofs are finished. In this package, \`sorry\` is expected in proof-bearing theory files. The question is whether the statements are the right ones, and whether they are organized in a way that is sufficient for a faithful formalization of the paper's main results.

## Configured Main-Result Targets

- These are the paper items that matter for this review:
${targetSection}

## Protected Nodes

${isaProtectedIntro}

${isaProtectedSection}${isaProtectedClosureExplanation}

## What To Check

- Each configured target should be covered by one or more non-\`helper\` nodes whose Isabelle and \`.tex\` statements genuinely match.
- If multiple non-\`helper\` nodes share a target, they should together completely cover that target.
- For every node on the **Protected Nodes** list above, confirm the stated proposition — the statement text before the proof command (\`proof\`, \`by\`, or \`sorry\`) — faithfully matches the paired \`.tex\` statement. The list is exactly what's being frozen by your approval.
- Definitions on that list are part of the correspondence task: check that each one really captures the concept used in the matching \`.tex\` statement and in the statements that reference it.
- The relevant question is whether the protected node set above is good enough to treat as the trusted coarse formalization of the paper's main results.

## What May Change Without Further Expert Review

After this package is approved, later work may continue without renewed expert review as long as none of the **Protected Nodes** listed above change Isabelle/\`.tex\` meaning.

- Proof work is allowed on every node, including replacing \`sorry\` in protected proof-bearing nodes.
- Proof-only edits to \`.tex\` are allowed on every node.
- New supporting nodes can be added, and existing non-protected supporting nodes can be edited, refactored, or removed — provided the **Protected Nodes** themselves keep the same stated propositions and the same \`.tex\` statement meaning.

## What Would Require Renewed Expert Review

- Changing the meaning of the Isabelle or \`.tex\` statement for any **Protected Node** listed above.
- Adding, removing, or swapping the nodes that cover a configured target.
- Changing the configured target list itself.

## Nodes

${nodeList}

## Structure

- \`Tablet/Preamble.thy\` — shared imports (no definitions here)
- \`Tablet/<name>.thy\` — one Isabelle/HOL theory per node (\`theory Tablet_<name> imports … begin … end\`)
- \`Tablet/<name>.tex\` — natural-language statement and, for proof-bearing nodes, a rigorous NL proof
- \`paper/\` — source paper files, including ${paperReferenceText}
`;
  }

  const tempDir = viewerTempDir(stateDir);
  fs.mkdirSync(tempDir, { recursive: true });
  const tmpDir = fs.mkdtempSync(path.join(tempDir, 'tablet-snapshot-'));
  const snapDir = path.join(tmpDir, 'tablet-snapshot');
  fs.mkdirSync(path.join(snapDir, 'Tablet'), { recursive: true });
  fs.mkdirSync(path.join(snapDir, 'paper'), { recursive: true });

  if (fs.existsSync(tabletDir)) {
    // Include the backend's source files in the snapshot. For Lean this is
    // `.lean` (the historical set); an Isabelle run also ships its `.thy`.
    // `.tex` is always included. Adding `.thy` is a no-op for a Lean Tablet
    // dir (no `.thy` files there), so the Lean zip is byte-identical.
    const srcExt = `.${backendExt}`;
    for (const f of fs.readdirSync(tabletDir)) {
      if (f.endsWith('.lean') || f.endsWith('.tex') || f.endsWith(srcExt)) {
        fs.copyFileSync(path.join(tabletDir, f), path.join(snapDir, 'Tablet', f));
      }
    }
  }
  if (fs.existsSync(paperDir)) {
    // `paper/` may contain subdirectories (e.g. `refs/` with registered
    // reference papers, `revision/` on revision runs) — copy recursively
    // so the review zip carries the grounding documents too.
    for (const f of fs.readdirSync(paperDir)) {
      fs.cpSync(path.join(paperDir, f), path.join(snapDir, 'paper', f), { recursive: true });
    }
  }
  // PV expert-review context: ship the goal + trusted-base documents when
  // present. Absent on math runs (no-op; math zip stays byte-identical).
  for (const f of ['GOAL.md', 'CORRESPONDENCE.md', 'SOUNDNESS.md', 'tcb_manifest.json', 'APPROVED_AXIOMS.json', 'PROPOSED_ASSUMPTIONS.json', 'ASSUMPTIONS_REVIEW.md']) {
    const p = path.join(repoPath, f);
    if (fs.existsSync(p)) {
      fs.copyFileSync(p, path.join(snapDir, f));
    }
  }

  fs.writeFileSync(path.join(snapDir, 'README.md'), readme);

  const zipPath = path.join(tmpDir, 'tablet-snapshot.zip');
  // `zip` is absent on minimal hosts; python3's zipfile module is the
  // portable fallback (recurses directories).
  try {
    execSync(`cd "${tmpDir}" && zip -r "${zipPath}" tablet-snapshot/`, { timeout: 10000 });
  } catch (_zipErr) {
    execSync(`cd "${tmpDir}" && python3 -m zipfile -c "${zipPath}" tablet-snapshot/`, { timeout: 30000 });
  }

  res.setHeader('Content-Type', 'application/zip');
  res.setHeader('Content-Disposition', `attachment; filename="tablet-snapshot-cycle${state.cycle || 0}.zip"`);
  const zipStream = fs.createReadStream(zipPath);
  let cleaned = false;
  const cleanup = () => {
    if (cleaned) return;
    cleaned = true;
    try {
      fs.rmSync(tmpDir, { recursive: true, force: true });
    } catch {}
  };
  zipStream.pipe(res);
  zipStream.on('close', cleanup);
  zipStream.on('error', cleanup);
  res.on('close', cleanup);
}

// Parse a tmux burst session name into (runtime_namespace, role,
// request_id, lane, retry).
// Examples:
//   "trellis-trellis-smoke-worker-128-worker"      -> { role: "worker",   request_id: 128, lane: "worker",   retry: 0 }
//   "trellis-trellis-smoke-review-129-reviewer"    -> { role: "review",   request_id: 129, lane: "reviewer", retry: 0 }
//   "trellis-trellis-smoke-corr-136-v2"            -> { role: "corr",     request_id: 136, lane: "v2",       retry: 0 }
//   "trellis-trellis-smoke-worker-158-worker-r2"   -> { role: "worker",   request_id: 158, lane: "worker",   retry: 2 }
// Multi-lane verifier requests (paper / corr / sound) use a `v<N>` lane
// suffix; tmux_backend.py:3263 appends `-r{attempt}` to the base session
// name on each restart so a worker that bounced once becomes
// `...-worker-r1`, twice → `-r2`, etc. Returns null if the name doesn't
// match any burst pattern we care about.
function parseBurstSessionName(name) {
  if (typeof name !== 'string') return null;
  if (!name.startsWith('trellis-')) return null;
  if (name === 'trellis_viewer') return null;
  if (name.startsWith('trellis-run-')) return null;
  const body = name.slice('trellis-'.length);
  const m = body.match(/^(.+)-(worker|review|reviewer|paper|corr|sound|stuck_math_audit)-(\d+)-(worker|reviewer|v\d+|audit)(?:-r(\d+))?$/);
  if (!m) return null;
  const [, runtime, role, reqId, suffix, retryStr] = m;
  return {
    session: name,
    runtime,
    role,
    suffix,
    lane: suffix,
    request_id: Number(reqId),
    retry: retryStr ? Number(retryStr) : 0,
  };
}

// Parse a single JSON line from a codex output.log into a compact,
// render-friendly shape. Unknown shapes pass through as kind="other" with
// the raw line preserved so the UI can still show something.
function normalizeBurstLogEvent(line) {
  const trimmed = line.trim();
  if (!trimmed) return null;
  let rec;
  try {
    rec = JSON.parse(trimmed);
  } catch {
    return { kind: 'other', raw: trimmed };
  }
  const type = String(rec.type || '');
  const item = rec.item && typeof rec.item === 'object' ? rec.item : null;
  if (type === 'thread.started') {
    return { kind: 'thread_started', thread_id: rec.thread_id || '' };
  }
  if (type === 'turn.started') return { kind: 'turn_started' };
  if (type === 'turn.completed') {
    return { kind: 'turn_completed', usage: rec.usage || null };
  }
  if ((type === 'item.completed' || type === 'item.started') && item) {
    const itemType = String(item.type || '');
    const status = type === 'item.completed' ? 'completed' : 'in_progress';
    if (itemType === 'agent_message') {
      return { kind: 'agent_message', status, id: item.id || '', text: item.text || '' };
    }
    if (itemType === 'command_execution') {
      return {
        kind: 'command_execution', status,
        id: item.id || '',
        command: item.command || '',
        aggregated_output: item.aggregated_output || '',
        exit_code: item.exit_code == null ? null : Number(item.exit_code),
      };
    }
    if (itemType === 'file_change') {
      return {
        kind: 'file_change', status,
        id: item.id || '',
        changes: Array.isArray(item.changes) ? item.changes.map((c) => ({
          path: String(c.path || ''),
          kind: String(c.kind || ''),
        })) : [],
      };
    }
    // Unknown item type — pass-through.
    return { kind: 'item_other', status, id: item.id || '', item_type: itemType, raw: rec };
  }
  return { kind: 'other', raw: trimmed };
}

// ============================================================================
// Unified "chats" endpoints — one tab in the viewer for both live + historical
// burst transcripts, with structured per-provider event rendering.
// ============================================================================

// The kind tags the bridge writes into a canonical artifact dir name
// (`trellis/runtime/bridge.py:_artifact_name`). Same vocabulary `kindTag`
// produces from the kernel's request kind, so a dir-derived kind and an
// event-log-derived kind are directly comparable.
const CANONICAL_ARTIFACT_KIND_TAGS = new Set([
  'worker', 'review', 'paper', 'corr', 'sound', 'audit', 'stuck_math_audit',
]);

// Best-effort kind inference from artifact_id / scope.
function inferCallKind(artifactId) {
  const s = String(artifactId || '');
  // A canonical `trellis_<kindtag>_<id>_<suffix>` dir names its kind exactly,
  // so read it rather than sniffing for substrings. The substring rules below
  // cannot see `stuck_math_audit` or `audit` at all — nothing in either name
  // matches worker/review/paper/corr/sound — so the planning burst that opens
  // a run rendered as kind "other" everywhere the event log wasn't consulted
  // (historical cycles go through `buildHistoricalArtifactCalls`, which has no
  // burst record to fall back on).
  const canonical = s.match(/^trellis_([a-z_]+?)_\d+(?:_|$)/);
  if (canonical && CANONICAL_ARTIFACT_KIND_TAGS.has(canonical[1])) return canonical[1];
  if (s.includes('worker')) return 'worker';
  if (s.includes('review')) return 'reviewer';
  if (s.includes('paper')) return 'paper';
  if (s.includes('correspondence') || s.includes('corr')) return 'corr';
  if (s.includes('nl_proof') || s.includes('sound')) return 'sound';
  return 'other';
}

// One canonical lane identity for the two independent naming schemes that
// describe the same burst lane:
//
//   chat-dir names      `trellis_corr_38_v1`, `trellis_worker_56_result`,
//                       `trellis_stuck_math_audit_1_result`
//   tmux session names  `trellis-<ns>-corr-38-v1`, `...-worker-56-worker`,
//                       `...-stuck_math_audit-1-audit`
//
// Only verifier panels (paper / corr / sound) are genuinely multi-lane: the
// bridge issues one SingleAgentRequest per `vN` lane, each with its own chat
// dir and its own tmux session. Every other kind is single-lane and merely
// *labels* that one lane differently on each side — an audit's dir carries no
// lane at all while its tmux session is suffixed `-audit`.
//
// Comparing the raw labels therefore read "lane audit has no chat dir yet" for
// an audit whose transcript was sitting on disk, and the leftover-lane backfill
// emitted a phantom "no chat directory was located" row beside the real one.
// Collapse every single-lane label to the same key so only true `vN` lanes are
// ever tracked separately.
function canonicalLaneKey(lane) {
  const s = String(lane || '');
  return /^v\d+$/.test(s) ? s : '';
}

// Parse request_id out of an artifact_id if present (e.g. "worker_handoff_128_attempt_0").
function inferRequestId(artifactId) {
  const s = String(artifactId || '');
  // Look for _<digits>_ or trailing _<digits>
  let m = s.match(/_(\d+)(?:_|$)/);
  if (m) return Number(m[1]);
  return null;
}

// Post-bwrap-only, bursts run as the operator, so live chat artifacts are
// operator-owned and a direct read succeeds. Read directly and skip (return
// empty) on any failure — there is no separate sandbox user to sudo into.
function readTextMaybeSudo(filePath) {
  try {
    return fs.readFileSync(filePath, 'utf-8');
  } catch {
    return '';
  }
}

function tmuxSessionAlive(name) {
  try {
    execFileSync('tmux', tmuxArgs('has-session', '-t', String(name)), {
      stdio: ['ignore', 'ignore', 'ignore'],
    });
    return true;
  } catch {
    return false;
  }
}

// Read call.json from the live working tree.
function readLiveCallJson(stateDir, artifactDirName) {
  const p = path.join(stateDir, 'chats', 'live', artifactDirName, 'call.json');
  try {
    const raw = fs.readFileSync(p, 'utf-8');
    return JSON.parse(raw);
  } catch {
    return null;
  }
}

// --- Stale-session filtering ---------------------------------------------
//
// `chats/live/` is append-only on disk: when the active config swaps a
// worker/reviewer model (e.g. gemini-auto → gemini-3.1-pro-preview) the
// previous run's scope dir (`worker_proof_formalization:worker:gemini:
// gemini-auto:default` and the underscore mirror) stays put. Without
// filtering those orphans show up in the chat-calls sidebar and confuse
// the operator. We mark a dir "stale" if BOTH:
//   1. its latest file mtime is older than TRELLIS_VIEWER_STALE_HOURS
//      (default 6h), AND
//   2. the model token encoded in its name is not in the project's
//      currently-active config model set.
// Both conditions must hold. A scope dir for a still-active model that
// merely sat idle between bursts is NOT filtered.
//
// Toggle: query param `?show_stale=1` or env `TRELLIS_VIEWER_SHOW_STALE=1`
// disables filtering globally.
const STALE_CHAT_DIR_HOURS = (() => {
  const raw = Number(process.env.TRELLIS_VIEWER_STALE_HOURS);
  return Number.isFinite(raw) && raw > 0 ? raw : 6;
})();
const STALE_CHAT_DIR_MS = STALE_CHAT_DIR_HOURS * 60 * 60 * 1000;
const SHOW_STALE_ENV = (() => {
  const v = String(process.env.TRELLIS_VIEWER_SHOW_STALE || '').toLowerCase();
  return v === '1' || v === 'true' || v === 'yes';
})();

// Cache active-models per project for 30 seconds. Config changes are rare
// and the resolver walks every model-bearing role.
const _activeModelsCache = new Map(); // key: repoPath → {ts, set}
const ACTIVE_MODELS_TTL_MS = 30 * 1000;
function getActiveConfigModels(projectInfo) {
  if (!projectInfo || !projectInfo.repoPath) return new Set();
  const key = projectInfo.repoPath;
  const entry = _activeModelsCache.get(key);
  const now = Date.now();
  if (entry && (now - entry.ts) < ACTIVE_MODELS_TTL_MS) return entry.set;
  const configPath = configPathForRepo(projectInfo.repoPath);
  let config = null;
  try { config = JSON.parse(fs.readFileSync(configPath, 'utf-8')); } catch { config = null; }
  const models = new Set();
  // Generic walk rather than a hardcoded role/array list, mirroring
  // `lanesFromConfigTemplate`. The old hand list carried a key that has
  // never existed in any template or parser (`paper_faithfulness_agents`)
  // while missing real ones (`easy_close_worker`, `worker_rules`,
  // `workflow.phase_overrides`, the sidecar model) — so a live lane's
  // chats could be stale-filtered out of the dropdown. Over-collecting is
  // the safe direction here: this set only decides what stays visible.
  const collectModels = (obj, depth) => {
    if (!obj || typeof obj !== 'object' || depth > 8) return;
    if (Array.isArray(obj)) {
      for (const v of obj) collectModels(v, depth + 1);
      return;
    }
    if (typeof obj.model === 'string' && obj.model) models.add(obj.model);
    // The sidecar names its model `sidecar.model.name`, not `.model`.
    if (obj.model && typeof obj.model === 'object'
        && typeof obj.model.name === 'string' && obj.model.name) {
      models.add(obj.model.name);
    }
    if (Array.isArray(obj.fallback_models)) {
      for (const m of obj.fallback_models) if (typeof m === 'string' && m) models.add(m);
    }
    for (const v of Object.values(obj)) collectModels(v, depth + 1);
  };
  collectModels(config, 0);
  _activeModelsCache.set(key, { ts: now, set: models });
  return models;
}

// Extract the model token from a scope-form chat dir name. Returns null
// if the dir has no embedded model (e.g. `trellis_worker_56_result`).
// Supported shapes:
//   `<role>_<phase>:<role>:<provider>:<model>:<scope>`
//   `<role>_<phase>:<role>:<kind>:<provider>:<model>:...`
//   `<phase>_<role>_<provider>_<model>_<scope>` (underscore mirror)
function parseModelFromChatDirName(name) {
  if (!name || typeof name !== 'string') return null;
  // Skip artifact-id dirs that have no encoded model.
  if (/^trellis_(worker|review|paper|corr|sound)_\d+(?:_|$)/.test(name)) return null;
  const PROVIDERS = new Set(['codex', 'gemini', 'claude']);
  if (name.includes(':')) {
    const parts = name.split(':');
    for (let i = 0; i < parts.length - 1; i++) {
      if (PROVIDERS.has(parts[i])) {
        const model = parts[i + 1];
        if (model && !PROVIDERS.has(model)) return model;
      }
    }
    return null;
  }
  // Underscore mirror: split, find a provider token, take the next.
  const parts = name.split('_');
  for (let i = 0; i < parts.length - 1; i++) {
    if (PROVIDERS.has(parts[i])) {
      const model = parts[i + 1];
      if (model && !PROVIDERS.has(model)) return model;
    }
  }
  return null;
}

// Return the most-recent mtime among the dir's immediate children (and
// the dir itself). Cheap; no recursion. Returns 0 if dir is missing.
function latestMtimeForLiveChatDir(stateDir, name) {
  const dir = path.join(stateDir, 'chats', 'live', name);
  let best = 0;
  try {
    const st = fs.statSync(dir);
    best = st.mtimeMs;
    for (const entry of fs.readdirSync(dir, { withFileTypes: true })) {
      try {
        const cst = fs.statSync(path.join(dir, entry.name));
        if (cst.mtimeMs > best) best = cst.mtimeMs;
      } catch { /* ignore */ }
    }
  } catch { return 0; }
  return best;
}

// True iff `name` is a stale live chat dir for `projectInfo`. Stale means
// latest mtime older than STALE_CHAT_DIR_MS AND its encoded model is not
// in the active-model set. Dirs without an encoded model (artifact-id
// dirs) are never stale.
function isStaleLiveChatDir(projectInfo, name, activeModels, nowMs) {
  const model = parseModelFromChatDirName(name);
  if (!model) return false;
  if (activeModels && activeModels.has(model)) return false;
  const mt = latestMtimeForLiveChatDir(projectInfo.stateDir, name);
  if (!mt) return false;
  return (nowMs - mt) > STALE_CHAT_DIR_MS;
}

// Read call.json from a git-committed cycle tag.
function readGitCallJson(chatsRepo, cycle, artifactDirName) {
  if (!fs.existsSync(path.join(chatsRepo, '.git'))) return null;
  if (!hasChatCycleTag(chatsRepo, cycle)) return null;
  const tag = chatCycleTag(cycle);
  try {
    const raw = git(chatsRepo, `show ${tag}:${chatCycleDir(cycle)}/${artifactDirName}/call.json`);
    return JSON.parse(raw);
  } catch {
    return null;
  }
}

// List artifact dir names for a given cycle ('live' or number).
function listCallArtifacts(stateDir, cycle) {
  if (cycle === 'live') {
    const liveRoot = path.join(stateDir, 'chats', 'live');
    if (!fs.existsSync(liveRoot)) return [];
    return fs.readdirSync(liveRoot, { withFileTypes: true })
      .filter(e => e.isDirectory())
      .map(e => e.name);
  }
  const chatsRepo = path.join(stateDir, 'chats');
  return listGitChatArtifacts(chatsRepo, cycle);
}

// Resolve the runtime_root for this project. The supervisor stores its
// state (event_log.jsonl, protocol_state.json) OUTSIDE the repo, typically
// at a sibling path `<repo>-runtime/`. That's the convention used by
// `scripts/restart_configured_run.sh`. Fall back to scanning the parent
// of the repo for any `*-runtime` dir that contains an event_log.jsonl.
function runtimeRootForProject(projectInfo) {
  // `repoPath` may be a symlink (e.g. /path/to/trellis/math/current → real
  // path). The sibling `<repo>-runtime` convention is relative to the
  // resolved path, not the symlink itself. The runtime root is identified by
  // its `runtime_metadata.json` (the event log no longer lives here — it moved
  // to the per-cycle dir in the repo tree).
  let realRepoPath = projectInfo.repoPath;
  try { realRepoPath = fs.realpathSync(projectInfo.repoPath); } catch {}
  const direct = `${realRepoPath}-runtime`;
  if (fs.existsSync(path.join(direct, 'runtime_metadata.json'))) return direct;
  const parent = path.dirname(realRepoPath);
  if (!fs.existsSync(parent)) return null;
  let best = null;
  let parentEntries;
  try {
    // `parent` can be mid-wipe when an operator removes the project's
    // `<repo>-runtime` sibling; readdir then races the delete. Treat a failed
    // scan as "no runtime root" (the caller already handles null gracefully).
    parentEntries = fs.readdirSync(parent, { withFileTypes: true });
  } catch {
    return null;
  }
  // Authoritative pass: a runtime's own `runtime_metadata.json` names the
  // repo it serves, so match on that rather than on directory naming. Two
  // reasons. (1) Not every bootstrap uses the `<repo>-runtime` SUFFIX — the PV
  // prose template infixes it (for example, `dec2flt-prose-runtime-<tag>`), which the
  // suffix scan below cannot see at all. (2) The suffix scan falls back to
  // newest-mtime, so with several runs under one PROJECTS_ROOT it can return
  // a DIFFERENT campaign's runtime. Resolve both sides before comparing: the
  // repo path may be a symlink (`math/current`).
  for (const entry of parentEntries) {
    if (!entry.isDirectory()) continue;
    const candidate = path.join(parent, entry.name);
    const metaPath = path.join(candidate, 'runtime_metadata.json');
    if (!fs.existsSync(metaPath)) continue;
    try {
      const declared = JSON.parse(fs.readFileSync(metaPath, 'utf-8')).repo_path;
      if (!declared) continue;
      let declaredReal = declared;
      try { declaredReal = fs.realpathSync(declared); } catch {}
      if (declaredReal === realRepoPath) return candidate;
    } catch {}
  }
  for (const entry of parentEntries) {
    if (!entry.isDirectory()) continue;
    if (!entry.name.endsWith('-runtime')) continue;
    const candidate = path.join(parent, entry.name);
    const metaPath = path.join(candidate, 'runtime_metadata.json');
    if (fs.existsSync(metaPath)) {
      try {
        const mt = fs.statSync(metaPath).mtimeMs;
        if (!best || mt > best.mtime) best = { path: candidate, mtime: mt };
      } catch {}
    }
  }
  return best ? best.path : null;
}

// Per-cycle event-log directory in the tracked repo tree
// (`<repo>/.trellis-history/event-log/`). Mirrors the kernel's
// `event_log_dir_for` (runtime.rs).
function eventLogDirForProject(projectInfo) {
  if (!projectInfo || !projectInfo.repoPath) return null;
  return path.join(projectInfo.repoPath, '.trellis-history', 'event-log');
}

// Sorted per-cycle event-log files (`cycle-NNNNNN.jsonl`). Lexical filename
// order equals cycle order equals global index order. Returns [] when absent.
function eventLogCycleFiles(eventLogDir) {
  if (!eventLogDir || !fs.existsSync(eventLogDir)) return [];
  let names;
  try { names = fs.readdirSync(eventLogDir); } catch { return []; }
  return names
    .filter((n) => /^cycle-\d{6,}\.jsonl$/.test(n))
    .sort()
    .map((n) => path.join(eventLogDir, n));
}

function liveInFlightRequestId(runtimeRoot) {
  if (!runtimeRoot) return null;
  const p = path.join(runtimeRoot, 'protocol_state.json');
  if (!fs.existsSync(p)) return null;
  try {
    const state = JSON.parse(fs.readFileSync(p, 'utf8'));
    const req = state && state.in_flight_request;
    const id = req && req.id;
    return Number.isFinite(id) ? id : null;
  } catch {
    return null;
  }
}

// Scan event_log.jsonl to extract the authoritative list of bursts ever
// issued: every issue_request command inside a RuntimeStepRecord defines
// a burst whose cycle is the RuntimeStep's `cycle` field (or the most
// recent start_cycle event). Returns both a request_id → cycle map and
// the full ordered list of burst records {request_id, kind, cycle,
// active_node, mode, event_index} — the source of truth for what bursts
// the supervisor issued in each cycle. Also returns the live cycle
// number (most recent step's cycle field). This replaces filesystem
// enumeration as the authority: chats/live/ is never pruned, so we can't
// trust its dir listing.
// Cache keyed on a cheap signature of the per-cycle dir (file names + sizes +
// mtimes). Completed cycle files are immutable and only the current cycle's
// file grows, so the signature changes exactly when there's new data. On a
// change we re-walk all files in order — the per-burst map is small and the
// walk is sub-second even at thousands of cycles.
let REQUEST_CYCLE_CACHE = { signature: null, map: null, bursts: null, currentCycle: null };
function eventLogDirSignature(files) {
  const parts = [];
  for (const f of files) {
    try {
      const st = fs.statSync(f);
      parts.push(`${path.basename(f)}:${st.size}:${st.mtimeMs}`);
    } catch {}
  }
  return parts.join('|');
}
function requestCyclesFromEventLog(projectInfo) {
  const eventLogDir = eventLogDirForProject(projectInfo);
  const files = eventLogCycleFiles(eventLogDir);
  if (!files.length) return { map: new Map(), liveCycle: null, bursts: [] };

  const signature = eventLogDirSignature(files);
  if (REQUEST_CYCLE_CACHE.signature === signature && REQUEST_CYCLE_CACHE.map) {
    return {
      map: REQUEST_CYCLE_CACHE.map,
      liveCycle: REQUEST_CYCLE_CACHE.currentCycle,
      bursts: REQUEST_CYCLE_CACHE.bursts,
    };
  }

  const map = new Map();
  const bursts = [];
  let currentCycle = null;
  let eventIndex = 0;
  for (const logPath of files) {
    forEachFileLineFromOffset(logPath, 0, (line) => {
      if (!line.trim()) { eventIndex++; return; }
      let rec;
      try { rec = JSON.parse(line); } catch { eventIndex++; return; }
      const evCycle = rec.cycle;
      if (typeof evCycle === 'number') currentCycle = evCycle;
      const idx = (typeof rec.index === 'number') ? rec.index : eventIndex;
      const cmds = Array.isArray(rec.commands) ? rec.commands : [];
      for (const cmd of cmds) {
        if (cmd && cmd.command === 'issue_request' && cmd.request && typeof cmd.request.id === 'number') {
          const useCycle = (typeof evCycle === 'number') ? evCycle : currentCycle;
          if (useCycle != null) map.set(cmd.request.id, useCycle);
          bursts.push({
            request_id: cmd.request.id,
            kind: String(cmd.request.kind || ''),
            cycle: useCycle,
            active_node: cmd.request.active_node || null,
            mode: cmd.request.mode || null,
            event_index: idx,
          });
        }
      }
      eventIndex++;
    });
  }

  REQUEST_CYCLE_CACHE = { signature, map, bursts, currentCycle };
  return { map, liveCycle: currentCycle, bursts };
}

// Map from Rust `kind` enum name to the lowercase tag used in scope dirs
// and `trellis_<kind>_<id>_*` artifact dirs.
function kindTag(kind) {
  const k = String(kind || '').toLowerCase();
  if (k === 'worker') return 'worker';
  if (k === 'review') return 'review';
  if (k === 'paper') return 'paper';
  if (k === 'corr') return 'corr';
  if (k === 'sound') return 'sound';
  if (k === 'stuckmathaudit') return 'stuck_math_audit';
  // AdvanceGate / HumanGate have no chat artifact.
  return k;
}

// Return the list of candidate chat dir names for a given cycle —
// for `live` this is `chats/live/` on disk; for historical cycles it
// prefers the cycle-bound `cycle-<NNNN>/` snapshot. `live/` is only a
// fallback because old tags can contain a large cumulative live snapshot.
function listCandidateChatDirs(projectInfo, cycleParam) {
  const { stateDir } = projectInfo;
  let chatsRepo = path.join(stateDir, 'chats');
  if (cycleParam === 'live') {
    const liveRoot = path.join(stateDir, 'chats', 'live');
    if (!fs.existsSync(liveRoot)) return { names: [], prefixes: {}, repos: {}, chatsRepo };
    const names = [];
    const prefixes = {};
    const repos = {};
    for (const e of fs.readdirSync(liveRoot, { withFileTypes: true })) {
      if (!e.isDirectory()) continue;
      names.push(e.name);
      prefixes[e.name] = 'live';
      repos[e.name] = chatsRepo;
    }
    return { names, prefixes, repos, chatsRepo };
  }
  const cycle = cycleParam;
  const tag = chatCycleTag(cycle);
  const cyclePrefix = chatCycleDir(cycle) + '/';
  const livePrefix = 'live/';
  // `cycle-NNNN/` has priority over `live/` when both carry the same
  // artifact name; we prefer the cycle-bound snapshot because it isn't
  // rewritten on subsequent bursts.
  const names = [];
  const prefixes = {};
  const repos = {};
  const note = (name, prefix, repoPath) => {
    if (!name || prefixes[name]) return;
    names.push(name);
    prefixes[name] = prefix;
    repos[name] = repoPath;
  };
  const candidateRepos = chatRepoCandidates(projectInfo);
  const liveFallbacks = [];
  for (let i = 0; i < candidateRepos.length; i++) {
    const repoPath = candidateRepos[i];
    if (!hasChatCycleTag(repoPath, cycle)) continue;
    const cycleSet = new Set();
    const liveSet = new Set();
    const collect = (files, prefix, set) => {
      for (const name of files.split('\n')) {
        if (!name || !name.startsWith(prefix)) continue;
        const rest = name.slice(prefix.length);
        const first = rest.split('/')[0];
        if (first) set.add(first);
      }
    };
    try {
      collect(git(repoPath, `ls-tree -r --name-only ${tag} -- ${cyclePrefix}`), cyclePrefix, cycleSet);
    } catch {}
    if (!cycleSet.size) {
      try {
        collect(git(repoPath, `ls-tree -r --name-only ${tag} -- ${livePrefix}`), livePrefix, liveSet);
      } catch {}
    }
    for (const n of cycleSet) note(n, `cycle-${String(cycle).padStart(4, '0')}`, repoPath);
    if (cycleSet.size && i > 0) {
      return { names: sortArtifactNames(names), prefixes, repos, chatsRepo: null };
    }
    if (liveSet.size) liveFallbacks.push({ repoPath, liveSet });
  }
  if (!names.length && liveFallbacks.length) {
    const { repoPath, liveSet } = liveFallbacks[0];
    for (const n of liveSet) note(n, 'live', repoPath);
  }
  return { names: sortArtifactNames(names), prefixes, repos, chatsRepo: null };
}

// Resolve a burst (event-log record) to zero or more chat dirs using a
// fixed preference order:
//   A. Preferred `trellis_<kind>_<id>_*` artifact dirs (matches
//      worker_N_result, review_N_decision, paper/corr/sound_N_vK, etc).
//      Lanes (paper/corr/sound have v1/v2) return multiple entries.
//   B. Scope-based dirs that embed the request_id (reviewer sessions for
//      paper/corr/sound lanes): `*:{kind}:{id}:v1:*` or matching
//      non-colon equivalents `*_{kind}_{id}_v1_*`. Lane v1 and v2 are
//      separate entries.
//   C. Scope-based dirs keyed only by role/kind, with no id in the name
//      (worker session + reviewer-review session; rewritten each burst).
//      Emitted with transcript_is_stale_scope_dir: true.
//   D. If nothing matches, emit a placeholder with no dir — the frontend
//      already handles `missing: true` gracefully.
function resolveBurstArtifacts(burst, dirNames) {
  const tag = kindTag(burst.kind);
  const rid = burst.request_id;
  const results = [];
  if (!tag || rid == null) {
    return results;
  }

  // Preferred A: trellis_<tag>_<rid>(_<suffix>)?
  const preferredRe = new RegExp(`^trellis_${tag}_${rid}(?:_|$)`);
  const preferred = dirNames.filter((n) => preferredRe.test(n));
  if (preferred.length) {
    for (const name of preferred) {
      results.push({ artifact_id: name, transcript_is_stale_scope_dir: false, fallback: 'preferred' });
    }
    // Verifier requests (paper/corr/sound) can have multiple lanes (v1, v2, …).
    // The bridge currently writes the canonical short-name dir
    // `trellis_<tag>_<rid>_vN` only for the v1 (codex) lane; the v2 (gemini)
    // lane lands in a colon-form scope dir like
    // `reviewer_theorem_stating:reviewer:sound:60:v2:gemini:...:default`.
    // Without this supplement scan we'd surface only v1 in the chat dropdown.
    // TODO(bridge): normalize all verifier lanes to `trellis_<tag>_<rid>_vN`
    // and remove this supplement.
    if (tag === 'paper' || tag === 'corr' || tag === 'sound') {
      const preferredLanes = new Set();
      for (const name of preferred) {
        const lm = name.match(/_v(\d+)(?:_|$)/);
        if (lm) preferredLanes.add(`v${lm[1]}`);
      }
      const scopeLaneRe = new RegExp(`(^|:)${tag}:${rid}:(v\\d+):`);
      for (const name of dirNames) {
        const m = name.match(scopeLaneRe);
        if (!m) continue;
        const lane = m[2];
        if (preferredLanes.has(lane)) continue;
        results.push({ artifact_id: name, transcript_is_stale_scope_dir: false, fallback: 'scope_lane_supplement' });
        preferredLanes.add(lane);
      }
    }
    return results;
  }

  // Fallback B: scope dir with id embedded. The canonical colon form is
  // `reviewer_proof_formalization:reviewer:<tag>:<rid>:v1:...`; an older
  // underscore-separated mirror also exists
  // (`proof_formalization_reviewer_<tag>_<rid>_v1_...`). Prefer the colon
  // form; fall back to the underscore mirror only when no colon variant
  // exists (avoids emitting redundant duplicate entries per lane).
  const colonRe = new RegExp(`(^|:)${tag}:${rid}(?::|$)`);
  const underRe = new RegExp(`(^|_)${tag}_${rid}(?:_|$)`);
  const colonMatches = dirNames.filter((n) => colonRe.test(n));
  const underMatches = dirNames.filter((n) => underRe.test(n));
  const byId = colonMatches.length ? colonMatches : underMatches;
  if (byId.length) {
    for (const name of byId) {
      results.push({ artifact_id: name, transcript_is_stale_scope_dir: false, fallback: 'scope_with_id' });
    }
    return results;
  }

  // Fallback C: role-only scope dir (worker / reviewer-review sessions
  // that have no id and are rewritten each burst). Prefer the colon-form
  // naming (current convention) over the legacy underscore mirror; don't
  // emit redundant per-model duplicates — pick just the colon variants,
  // falling back to underscore variants if no colon form exists.
  const pickStale = (colonRe2, underRe2) => {
    const colon = dirNames.filter((n) => colonRe2.test(n));
    if (colon.length) return colon;
    return dirNames.filter((n) => !/^trellis_/.test(n) && underRe2.test(n));
  };
  // Phase-agnostic: worker/reviewer scope dirs are `<role>_<phase>:<role>:…`
  // (colon) or `<phase>_<role>_…` (underscore mirror). The old regexes pinned
  // `proof_formalization`, so a *theorem_stating* worker/reviewer scope dir
  // never resolved (the theorem-stating worker burst was invisible in the
  // viewer). Match any phase. New bursts are id-keyed (`trellis_worker_<id>_…`,
  // resolved by Preferred A above); this remains the fallback for legacy
  // id-less scope dirs.
  if (tag === 'worker') {
    const stale = pickStale(
      /^worker_[a-z_]+:worker:/,
      /^[a-z_]+_worker_(?!trellis_)/,
    );
    for (const name of stale) {
      results.push({ artifact_id: name, transcript_is_stale_scope_dir: true, fallback: 'scope_stale' });
    }
  } else if (tag === 'review') {
    const stale = pickStale(
      /^reviewer_[a-z_]+:reviewer:review(:|$)/,
      /^[a-z_]+_reviewer_review[_:]/,
    );
    for (const name of stale) {
      results.push({ artifact_id: name, transcript_is_stale_scope_dir: true, fallback: 'scope_stale' });
    }
  }
  return results;
}

// Probe whether a chat dir contains a recognizable transcript artifact
// (structured JSONL/JSON/output.log OR a raw claude-style `<uuid>.jsonl`
// session file dropped inside the scope dir).
function chatDirHasTranscript(projectInfo, cycleParam, artifactDirName, gitPrefix, chatsRepoOverride = null) {
  const { stateDir } = projectInfo;
  const CANON = ['transcript.jsonl', 'transcript.json', 'output.log'];
  if (cycleParam === 'live') {
    const dir = path.join(stateDir, 'chats', 'live', artifactDirName);
    if (!fs.existsSync(dir)) return false;
    for (const f of CANON) {
      if (fs.existsSync(path.join(dir, f))) return true;
    }
    try {
      const entries = fs.readdirSync(dir);
      if (entries.some((e) => /\.jsonl$/.test(e) && e !== 'transcript.jsonl')) return true;
    } catch {}
    return false;
  }
  const chatsRepo = chatsRepoOverride || chatRepoForCycle(projectInfo, cycleParam) || path.join(stateDir, 'chats');
  if (!hasChatCycleTag(chatsRepo, cycleParam)) return false;
  const tag = chatCycleTag(cycleParam);
  try {
    const base = `${gitPrefix || chatCycleDir(cycleParam)}/${artifactDirName}`;
    const files = git(chatsRepo, `ls-tree --name-only ${tag} -- ${base}/`)
      .split('\n').filter(Boolean);
    if (files.some(f => /\/(transcript\.jsonl|transcript\.json|output\.log)$/.test(f))) return true;
    if (files.some(f => /\/[0-9a-f-]{20,}\.jsonl$/.test(f))) return true;
    return false;
  } catch {
    return false;
  }
}

function buildHistoricalArtifactCalls(projectInfo, cycleValue, source) {
  const { names, prefixes, repos } = listCandidateChatDirs(projectInfo, cycleValue);
  const calls = [];
  for (const artifactDirName of sortArtifactNames(names)) {
    const gitPrefix = prefixes[artifactDirName] || chatCycleDir(cycleValue);
    const artifactChatsRepo = repos[artifactDirName];
    let meta = null;
    if (artifactChatsRepo) {
      try {
        const raw = git(
          artifactChatsRepo,
          `show ${chatCycleTag(cycleValue)}:${gitPrefix}/${artifactDirName}/call.json`
        );
        meta = JSON.parse(raw);
      } catch {}
    }
    const isStaleScope =
      /^worker_proof_formalization:worker:/.test(artifactDirName)
      || /^proof_formalization_worker_/.test(artifactDirName)
      || /^reviewer_proof_formalization:reviewer:review(:|$)/.test(artifactDirName)
      || /^proof_formalization_reviewer_review[_:]/.test(artifactDirName);
    calls.push({
      artifact_id: artifactDirName,
      kind: inferCallKind(artifactDirName),
      request_id: meta?.request_id ?? inferRequestId(artifactDirName),
      cycle: cycleValue,
      active_node: null,
      mode: null,
      event_index: null,
      provider: meta?.provider || null,
      model: meta?.model || null,
      role: meta?.role || null,
      session_id: meta?.session_id || null,
      scope: meta?.scope || null,
      started_at_ms: meta?.started_at_ms ?? null,
      ended_at_ms: meta?.ended_at_ms ?? null,
      has_transcript: chatDirHasTranscript(projectInfo, cycleValue, artifactDirName, gitPrefix, artifactChatsRepo),
      has_tui_pane: false,
      has_call_json: !!meta,
      transcript_is_stale_scope_dir: isStaleScope,
      tmux_session: null,
      lane: null,
    });
  }
  return { cycle: cycleValue, source, calls };
}

// Build chat-calls response for a cycle. Historical cycles are artifact-driven
// so archived chat snapshots remain cheap to browse; live cycles use the
// event_log so in-flight bursts can appear before their chat_dir is written.
function buildChatCalls(projectInfo, cycleParam, options = {}) {
  const { stateDir } = projectInfo;
  const isLive = cycleParam === 'live' || cycleParam === '' || cycleParam == null;
  const chatsRepo = path.join(stateDir, 'chats');
  const showStale = !!options.showStale || SHOW_STALE_ENV;

  let cycleValue;
  let source;
  if (isLive) {
    source = 'live';
  } else {
    const n = parseInt(String(cycleParam), 10);
    if (!Number.isFinite(n)) {
      return { cycle: cycleParam, source: 'git', calls: [], error: 'invalid cycle' };
    }
    cycleValue = n;
    source = `cycle-${n}`;
    return buildHistoricalArtifactCalls(projectInfo, cycleValue, source);
  }

  const runtimeRoot = runtimeRootForProject(projectInfo);
  const ec = requestCyclesFromEventLog(projectInfo);
  const inFlightRequestId = isLive ? liveInFlightRequestId(runtimeRoot) : null;
  if (isLive) {
    cycleValue = (ec.liveCycle != null) ? ec.liveCycle : 'live';
  }

  const burstCycle = typeof cycleValue === 'number' ? cycleValue : ec.liveCycle;
  const cycleBursts = (ec.bursts || []).filter((b) => b.cycle === burstCycle);

  const dirsKey = isLive ? 'live' : cycleValue;
  const {
    names: dirNamesRaw,
    prefixes,
    repos: artifactRepos = {},
    chatsRepo: dirsChatsRepo,
  } = listCandidateChatDirs(projectInfo, dirsKey);
  // Hide orphaned scope dirs (config swapped models — old dir lingers on
  // disk but no live burst writes to it). Only filters the LIVE listing;
  // historical cycles are immutable git snapshots.
  let dirNames = dirNamesRaw;
  if (isLive && !showStale) {
    const activeModels = getActiveConfigModels(projectInfo);
    const nowMs = Date.now();
    dirNames = dirNamesRaw.filter((n) => !isStaleLiveChatDir(projectInfo, n, activeModels, nowMs));
  }
  const readChatsRepo = isLive ? chatsRepo : (dirsChatsRepo || chatsRepo);

  // For the live cycle, enumerate tmux burst sessions once and index by
  // (request_id, role, lane). Lets us flag has_tui_pane as soon as the burst's
  // tmux session is up, even before the agent has written call.json — and
  // ALSO supports multi-lane verifier requests (paper/corr/sound spawn one
  // tmux session per lane v1/v2/...). Indexing by lane prevents v2 from
  // clobbering v1 in the map; without that, only one lane per request would
  // show up and entries would get cross-wired to whichever lane was last
  // inserted.
  const liveTmuxByRequestId = new Map();
  if (isLive) {
    try {
      const lsOut = execFileSync('tmux', tmuxArgs('ls', '-F', '#{session_name}'), {
        encoding: 'utf8', stdio: ['ignore', 'pipe', 'ignore'],
      });
      for (const name of lsOut.split('\n').map((s) => s.trim()).filter(Boolean)) {
        const parsed = parseBurstSessionName(name);
        if (parsed && parsed.request_id != null) {
          let sessions = liveTmuxByRequestId.get(parsed.request_id);
          if (!sessions) {
            sessions = [];
            liveTmuxByRequestId.set(parsed.request_id, sessions);
          }
          sessions.push(parsed);
        }
      }
    } catch {}
  }

  // Pull a `vN` lane out of an artifact_id so we can pair it with the
  // matching tmux session. Two artifact-id formats exist on disk:
  //
  //  (1) `trellis_<kind>_<id>_<suffix>` — preferred, written by the
  //      bridge to live/ (e.g. `trellis_worker_56_result`,
  //      `trellis_review_55_decision`, `trellis_corr_38_v1`).
  //  (2) Colon-form scope dirs — legacy fallback used when no preferred
  //      dir exists (e.g.
  //      `reviewer_theorem_stating:reviewer:corr:10:v2:...`).
  //
  // The lane returned here MUST match what `parseBurstSessionName`
  // returns for the corresponding tmux session ('worker', 'reviewer',
  // 'v1', 'v2', …). Without that, the dedup in the leftover-lane
  // backfill loop below misses the chat_dir-based emission and emits a
  // phantom placeholder for the same lane, causing the frontend to
  // surface "no chat directory was located" for an in-flight burst
  // even though `trellis_worker_<id>_result` is on disk.
  function laneFromArtifactId(artifactId) {
    const s = String(artifactId || '');
    if (/^trellis_worker_\d+(?:_|$)/.test(s)) return 'worker';
    if (/^trellis_review_\d+(?:_|$)/.test(s)) return 'reviewer';
    const mu = s.match(/^trellis_(?:paper|corr|sound)_\d+_(v\d+)(?:_|$)/);
    if (mu) return mu[1];
    const m = s.match(/:(v\d+):/);
    if (m) return m[1];
    if (s.startsWith('worker')) return 'worker';
    if (s.startsWith('reviewer')) return 'reviewer';
    return '';
  }

  function normalizedTmuxRole(role) {
    const r = String(role || '');
    return r === 'reviewer' ? 'review' : r;
  }

  function tmuxLanesForBurst(burst) {
    const expectedRole = normalizedTmuxRole(kindTag(burst.kind));
    const sessions = liveTmuxByRequestId.get(burst.request_id) || [];
    const byLane = new Map();
    for (const parsed of sessions) {
      if (expectedRole && normalizedTmuxRole(parsed.role) !== expectedRole) {
        continue;
      }
      const lane = parsed.lane || parsed.suffix || '';
      const existing = byLane.get(lane);
      if (!existing || (parsed.retry || 0) >= (existing.retry || 0)) {
        byLane.set(lane, parsed);
      }
    }
    return byLane;
  }

  // Pick a tmux session for a (request_id, role, lane). Returns null if no live
  // session matches. For workers/reviewers (single lane), accept any
  // session under that request_id when the lane match is empty.
  function pickTmuxSessionForLane(burst, lane) {
    if (!isLive) return null;
    const byLane = tmuxLanesForBurst(burst);
    if (!byLane.size) return null;
    if (lane && byLane.has(lane)) return byLane.get(lane).session;
    // Fall back to "the single available session" only when we don't know
    // the lane. For named lanes (v1, v2, worker, reviewer) a miss means
    // the lane's session has already exited — don't cross-wire to a
    // surviving sibling.
    if (!lane && byLane.size === 1) return byLane.values().next().value.session;
    return null;
  }

  const calls = [];
  const seenKeys = new Set();
  // Track which (request_id, lane) pairs we've emitted from chat_dir
  // matches so we can later emit placeholders for any tmux lanes that
  // don't yet have a chat_dir on disk. Lanes are keyed by their canonical
  // identity (`canonicalLaneKey`), never by the raw label, because the
  // chat-dir and tmux naming schemes spell the same lane differently.
  const emittedLanesByRequest = new Map();
  function noteEmittedLane(requestId, lane) {
    let s = emittedLanesByRequest.get(requestId);
    if (!s) { s = new Set(); emittedLanesByRequest.set(requestId, s); }
    s.add(canonicalLaneKey(lane));
  }

  for (const burst of cycleBursts) {
    const matched = resolveBurstArtifacts(burst, dirNames);
    if (matched.length === 0) {
      // A live-cycle rewind can leave completed same-cycle requests in
      // event_log.jsonl while their chat dirs have intentionally been removed.
      // Only synthesize a no-dir placeholder for the request the supervisor is
      // currently waiting on; otherwise old completed bursts look live.
      if (isLive && burst.request_id !== inFlightRequestId) {
        continue;
      }
      // No chat_dir on disk yet. Emit one placeholder per tmux lane so
      // multi-lane verifier requests don't collapse to a single entry.
      const byLane = tmuxLanesForBurst(burst);
      const lanes = byLane.size ? Array.from(byLane.entries()) : [['', null]];
      for (const [lane, parsed] of lanes) {
        const sess = parsed ? parsed.session : null;
        calls.push({
          artifact_id: null,
          kind: kindTag(burst.kind) || inferCallKind(''),
          request_id: burst.request_id,
          cycle: burst.cycle,
          active_node: burst.active_node,
          mode: burst.mode,
          event_index: burst.event_index,
          provider: null,
          model: null,
          role: null,
          session_id: null,
          scope: null,
          started_at_ms: null,
          ended_at_ms: null,
          has_transcript: false,
          has_tui_pane: !!sess,
          has_call_json: false,
          transcript_is_stale_scope_dir: false,
          tmux_session: sess,
          lane: lane || null,
        });
        noteEmittedLane(burst.request_id, lane);
      }
      continue;
    }
    for (const match of matched) {
      const artifactDirName = match.artifact_id;
      const key = `${burst.request_id}::${artifactDirName}`;
      if (seenKeys.has(key)) continue;
      seenKeys.add(key);
      const gitPrefix = isLive ? null : (prefixes[artifactDirName] || null);
      const artifactChatsRepo = isLive ? chatsRepo : (artifactRepos[artifactDirName] || readChatsRepo);
      const meta = isLive
        ? readLiveCallJson(stateDir, artifactDirName)
        : (gitPrefix
          ? (() => {
            try {
              const raw = git(artifactChatsRepo, `show ${chatCycleTag(cycleValue)}:${gitPrefix}/${artifactDirName}/call.json`);
              return JSON.parse(raw);
            } catch { return null; }
          })()
          : null);
      const hasTranscript = chatDirHasTranscript(projectInfo, dirsKey, artifactDirName, gitPrefix, artifactChatsRepo);
      // Provider detection: prefer call.json's `provider`. Fall back to
      // inferring codex from `output.log` presence because the supervisor
      // only writes call.json at burst end — during an in-flight codex
      // burst the field is otherwise null and the TUI gate below admits
      // the burst incorrectly. Codex emits its --json event stream to
      // output.log; claude/gemini drive a tmux pane and don't.
      let provider = meta?.provider || null;
      if (!provider && isLive) {
        try {
          if (fs.existsSync(path.join(stateDir, 'chats', 'live', artifactDirName, 'output.log'))) {
            provider = 'codex';
          }
        } catch { /* ignore */ }
      }
      const lane = laneFromArtifactId(artifactDirName);
      const burstTmuxSession = pickTmuxSessionForLane(burst, lane);
      // Enable the TUI toggle when there's *something* we could show:
      // a live tmux session OR the supervisor's on-disk pane.txt
      // snapshot. Without either, the pane view is empty so don't tease
      // the user with a toggle that does nothing. Codex is headless.
      const paneSnapshotExists = isLive && (() => {
        try {
          return fs.existsSync(path.join(stateDir, 'chats', 'live', artifactDirName, 'pane.txt'));
        } catch { return false; }
      })();
      const hasTuiPane = !!(
        isLive
        && provider !== 'codex'
        && (burstTmuxSession || paneSnapshotExists)
      );
      calls.push({
        artifact_id: artifactDirName,
        kind: kindTag(burst.kind) || inferCallKind(artifactDirName),
        request_id: burst.request_id,
        cycle: burst.cycle,
        active_node: burst.active_node,
        mode: burst.mode,
        event_index: burst.event_index,
        provider,
        model: meta?.model || null,
        role: meta?.role || null,
        session_id: meta?.session_id || null,
        scope: meta?.scope || null,
        started_at_ms: meta?.started_at_ms ?? null,
        ended_at_ms: meta?.ended_at_ms ?? null,
        has_transcript: hasTranscript,
        has_tui_pane: hasTuiPane,
        has_call_json: !!meta,
        transcript_is_stale_scope_dir: !!match.transcript_is_stale_scope_dir,
        git_prefix: gitPrefix,
        tmux_session: burstTmuxSession,
        lane: lane || null,
      });
      noteEmittedLane(burst.request_id, lane);
    }

    // After emitting chat_dir-based entries, fill in any tmux lanes
    // whose chat_dir hasn't appeared on disk yet (lane still in flight).
    //
    // Reaching here means the burst DID resolve to at least one chat dir, so
    // its single lane is accounted for. Only a genuine extra `vN` lane of a
    // verifier panel can still be missing, and two rules keep it to that:
    //   1. skip any lane with no `vN` identity — the burst's one lane already
    //      has a dir, whatever the tmux session chose to call it; and
    //   2. compare canonical lane keys, so a naming difference between the
    //      dir-derived and the tmux-derived label can never read as
    //      "lane still in flight".
    // Without these an audit burst (dir lane "", tmux lane "audit") grew a
    // phantom no-dir twin in the chat dropdown for as long as the cycle was
    // live.
    if (isLive) {
      const byLane = tmuxLanesForBurst(burst);
      const emitted = emittedLanesByRequest.get(burst.request_id) || new Set();
      if (byLane.size) {
        for (const [lane, parsed] of byLane) {
          const laneKey = canonicalLaneKey(lane);
          if (!laneKey) continue;
          if (emitted.has(laneKey)) continue;
          const sess = parsed ? parsed.session : null;
          calls.push({
            artifact_id: null,
            kind: kindTag(burst.kind) || inferCallKind(''),
            request_id: burst.request_id,
            cycle: burst.cycle,
            active_node: burst.active_node,
            mode: burst.mode,
            event_index: burst.event_index,
            provider: null,
            model: null,
            role: null,
            session_id: null,
            scope: null,
            started_at_ms: null,
            ended_at_ms: null,
            has_transcript: false,
            has_tui_pane: !!sess,
            has_call_json: false,
            transcript_is_stale_scope_dir: false,
            tmux_session: sess,
            lane: lane || null,
          });
          noteEmittedLane(burst.request_id, lane);
        }
      }
    }
  }

  // Order: request_id asc, then event_index asc, then artifact_id.
  calls.sort((a, b) => {
    const ar = a.request_id == null ? Infinity : a.request_id;
    const br = b.request_id == null ? Infinity : b.request_id;
    if (ar !== br) return ar - br;
    const ai = a.event_index == null ? Infinity : a.event_index;
    const bi = b.event_index == null ? Infinity : b.event_index;
    if (ai !== bi) return ai - bi;
    return String(a.artifact_id || '').localeCompare(String(b.artifact_id || ''));
  });
  return { cycle: cycleValue, source, calls };
}

// ---- per-provider event parsers -------------------------------------------

function parseClaudeTranscriptEvents(text, startMs, endMs) {
  const events = [];
  for (const rawLine of (text || '').split(/\r?\n/)) {
    const line = rawLine.trim();
    if (!line) continue;
    let rec;
    try {
      rec = JSON.parse(line);
    } catch {
      continue;
    }
    const tsStr = rec.timestamp || rec.ts || '';
    let tsMs = null;
    if (tsStr) {
      const parsed = Date.parse(tsStr);
      if (!Number.isNaN(parsed)) tsMs = parsed;
    }
    if (startMs != null && tsMs != null && tsMs < startMs) continue;
    if (endMs != null && tsMs != null && tsMs > endMs) continue;
    const type = String(rec.type || '');
    const msg = rec.message && typeof rec.message === 'object' ? rec.message : null;
    if (type === 'assistant' && msg) {
      const content = Array.isArray(msg.content) ? msg.content : [];
      for (const block of content) {
        if (!block || typeof block !== 'object') continue;
        const bt = String(block.type || '');
        if (bt === 'text') {
          events.push({ kind: 'agent_message', provider: 'claude', ts_ms: tsMs, text: String(block.text || '') });
        } else if (bt === 'thinking') {
          events.push({ kind: 'thinking', provider: 'claude', ts_ms: tsMs, thinking: String(block.thinking || block.text || '') });
        } else if (bt === 'tool_use') {
          events.push({
            kind: 'tool_call', provider: 'claude', ts_ms: tsMs,
            tool_name: String(block.name || ''),
            tool_input: block.input ?? null,
            id: String(block.id || ''),
          });
        }
      }
      if (msg.usage) {
        events.push({ kind: 'turn_completed', provider: 'claude', ts_ms: tsMs, usage: msg.usage });
      }
    } else if (type === 'user' && msg) {
      // tool_result blocks arrive via user records.
      const content = Array.isArray(msg.content) ? msg.content : null;
      if (Array.isArray(content)) {
        let anyToolResult = false;
        for (const block of content) {
          if (block && typeof block === 'object' && block.type === 'tool_result') {
            anyToolResult = true;
            let txt = '';
            if (typeof block.content === 'string') txt = block.content;
            else if (Array.isArray(block.content)) {
              txt = block.content.map((b) => (b && typeof b === 'object' && typeof b.text === 'string') ? b.text : '').join('\n');
            }
            events.push({
              kind: 'tool_result', provider: 'claude', ts_ms: tsMs,
              id: String(block.tool_use_id || ''),
              tool_output: txt,
              is_error: !!block.is_error,
            });
          }
        }
        if (!anyToolResult) {
          const parts = [];
          for (const block of content) {
            if (block && typeof block === 'object' && typeof block.text === 'string') parts.push(block.text);
            else if (typeof block === 'string') parts.push(block);
          }
          if (parts.length) {
            events.push({ kind: 'user_message', provider: 'claude', ts_ms: tsMs, text: parts.join('\n') });
          }
        }
      } else if (typeof msg.content === 'string') {
        events.push({ kind: 'user_message', provider: 'claude', ts_ms: tsMs, text: msg.content });
      }
    } else if (type === 'thinking') {
      events.push({ kind: 'thinking', provider: 'claude', ts_ms: tsMs, thinking: String(rec.thinking || rec.text || '') });
    }
  }
  return events;
}

function parseGeminiTranscriptEvents(text, startMs, endMs) {
  let data;
  try {
    data = JSON.parse(text);
  } catch {
    return [];
  }
  const events = [];
  const messages = Array.isArray(data?.messages) ? data.messages : [];
  for (const m of messages) {
    const tsMs = Number(m?.timestamp) || null;
    if (startMs != null && tsMs != null && tsMs < startMs) continue;
    if (endMs != null && tsMs != null && tsMs > endMs) continue;
    const mtype = String(m?.type || '');
    let txt = '';
    if (typeof m.content === 'string') txt = m.content;
    else if (Array.isArray(m.content)) {
      const parts = [];
      for (const p of m.content) {
        if (typeof p === 'string') parts.push(p);
        else if (p && typeof p === 'object' && typeof p.text === 'string') parts.push(p.text);
      }
      txt = parts.join('\n');
    }
    if (mtype === 'user') {
      events.push({ kind: 'user_message', provider: 'gemini', ts_ms: tsMs, text: txt });
    } else if (mtype === 'gemini') {
      // Thoughts come as `{subject, description, timestamp}` objects in
      // the current gemini transcript schema (not the earlier `{text}`
      // shape). Format each as "[subject] description" so the card view
      // surfaces the reasoning headings.
      const thoughts = Array.isArray(m.thoughts) ? m.thoughts : [];
      for (const t of thoughts) {
        let tt = '';
        if (typeof t === 'string') tt = t;
        else if (t && typeof t === 'object') {
          if (typeof t.text === 'string' && t.text) tt = t.text;
          else {
            const subj = typeof t.subject === 'string' ? t.subject.trim() : '';
            const desc = typeof t.description === 'string' ? t.description.trim() : '';
            tt = subj && desc ? `[${subj}] ${desc}` : (desc || subj);
          }
        }
        if (tt) events.push({ kind: 'thinking', provider: 'gemini', ts_ms: tsMs, thinking: tt });
      }
      if (txt) events.push({ kind: 'agent_message', provider: 'gemini', ts_ms: tsMs, text: txt });
      // Each gemini turn's actual work is in `toolCalls`. Extract each as a
      // tool_call + tool_result pair so the view shows what the agent did,
      // not just the usage stub.
      const toolCalls = Array.isArray(m.toolCalls) ? m.toolCalls : [];
      for (const tc of toolCalls) {
        if (!tc || typeof tc !== 'object') continue;
        const id = String(tc.id || '');
        const name = String(tc.name || '');
        const args = tc.args ?? tc.input ?? null;
        events.push({
          kind: 'tool_call', provider: 'gemini', ts_ms: tsMs,
          tool_name: name, tool_input: args, id,
        });
        // Result may be a list of functionResponse objects, a string
        // (resultDisplay), or absent while the call is still in flight.
        let output = null;
        if (typeof tc.resultDisplay === 'string' && tc.resultDisplay) {
          output = tc.resultDisplay;
        } else if (Array.isArray(tc.result)) {
          const parts = [];
          for (const r of tc.result) {
            const fr = r && r.functionResponse;
            if (!fr) continue;
            const resp = fr.response || {};
            if (typeof resp.output === 'string') parts.push(resp.output);
            else if (typeof resp.error === 'string') parts.push(resp.error);
            else parts.push(JSON.stringify(resp));
          }
          output = parts.join('\n');
        } else if (tc.result !== undefined) {
          output = typeof tc.result === 'string' ? tc.result : JSON.stringify(tc.result);
        }
        const isError = tc.status && String(tc.status).toLowerCase() !== 'success';
        if (output !== null || tc.status) {
          events.push({
            kind: 'tool_result', provider: 'gemini', ts_ms: tsMs,
            tool_name: name, tool_output: output, id, is_error: !!isError,
          });
        }
      }
      if (m.tokens) {
        events.push({ kind: 'turn_completed', provider: 'gemini', ts_ms: tsMs, usage: m.tokens });
      }
    } else if (mtype === 'tool') {
      // Standalone tool messages — rare; kept for older schemas.
      const name = String(m.toolName || m.name || '');
      if (m.result !== undefined || m.output !== undefined) {
        events.push({ kind: 'tool_result', provider: 'gemini', ts_ms: tsMs, tool_name: name, tool_output: m.result ?? m.output ?? txt });
      } else {
        events.push({ kind: 'tool_call', provider: 'gemini', ts_ms: tsMs, tool_name: name, tool_input: m.input ?? m.args ?? null });
      }
    }
  }
  return events;
}

function parseCodexOutputLogEvents(text) {
  const events = [];
  for (const rawLine of (text || '').split(/\r?\n/)) {
    const ev = normalizeBurstLogEvent(rawLine);
    if (!ev) continue;
    // Augment with provider tag.
    ev.provider = 'codex';
    events.push(ev);
  }
  return events;
}

function readArtifactFile(stateDir, cycle, artifactDirName, filename, gitPrefix, chatsRepoOverride = null) {
  if (cycle === 'live') {
    const p = path.join(stateDir, 'chats', 'live', artifactDirName, filename);
    return readTextMaybeSudo(p);
  }
  const chatsRepo = chatsRepoOverride || path.join(stateDir, 'chats');
  if (!hasChatCycleTag(chatsRepo, cycle)) return '';
  const tag = chatCycleTag(cycle);
  const prefixes = gitPrefix
    ? [gitPrefix]
    : [chatCycleDir(cycle), 'live'];
  for (const p of prefixes) {
    try {
      const txt = git(chatsRepo, `show ${tag}:${p}/${artifactDirName}/${filename}`);
      if (txt) return txt;
    } catch {}
  }
  return '';
}

// For scope dirs, Claude headless sessions drop their transcript as
// `<uuid>.jsonl` directly into the dir. List the dir to find them.
function listArtifactFiles(stateDir, cycle, artifactDirName, gitPrefix, chatsRepoOverride = null) {
  if (cycle === 'live') {
    const dir = path.join(stateDir, 'chats', 'live', artifactDirName);
    try {
      return fs.readdirSync(dir);
    } catch {
      return [];
    }
  }
  const chatsRepo = chatsRepoOverride || path.join(stateDir, 'chats');
  if (!hasChatCycleTag(chatsRepo, cycle)) return [];
  const tag = chatCycleTag(cycle);
  const prefixes = gitPrefix
    ? [gitPrefix]
    : [chatCycleDir(cycle), 'live'];
  for (const p of prefixes) {
    try {
      const out = git(chatsRepo, `ls-tree --name-only ${tag} -- ${p}/${artifactDirName}/`)
        .split('\n').filter(Boolean);
      const basenames = out.map((n) => n.split('/').pop()).filter(Boolean);
      if (basenames.length) return basenames;
    } catch {}
  }
  return [];
}

function buildChatEvents(projectInfo, cycleParam, callId, source, hints = {}) {
  const { stateDir } = projectInfo;
  const isLive = cycleParam === 'live' || cycleParam === '' || cycleParam == null;
  const cycleValue = isLive ? 'live' : parseInt(String(cycleParam), 10);
  if (!isLive && !Number.isFinite(cycleValue)) {
    return { error: 'invalid cycle' };
  }
  const hintedRequestId = Number.isFinite(Number(hints.request_id)) ? Number(hints.request_id) : null;
  const hintedLane = hints.lane ? String(hints.lane) : null;
  const hintedKind = hints.kind ? String(hints.kind) : null;
  const hintedProvider = hints.provider ? String(hints.provider) : null;
  const hintedRole = hints.role ? String(hints.role) : null;
  // Determine git prefix for historical cycles — scope dirs live under
  // `live/`, the few cycles with `cycle-NNNN/` artifacts live there.
  let gitPrefix = null;
  let historicalChatsRepo = null;
  if (!isLive) {
    const { prefixes, repos } = listCandidateChatDirs(projectInfo, cycleValue);
    gitPrefix = prefixes[callId] || null;
    historicalChatsRepo = repos[callId] || null;
  }
  // Resolve call meta.
  const chatsRepo = isLive
    ? path.join(stateDir, 'chats')
    : (historicalChatsRepo || path.join(stateDir, 'chats'));
  const meta = isLive
    ? readLiveCallJson(stateDir, callId)
    : (gitPrefix
      ? (() => {
        try {
          const raw = git(chatsRepo, `show ${chatCycleTag(cycleValue)}:${gitPrefix}/${callId}/call.json`);
          return JSON.parse(raw);
        } catch { return null; }
      })()
      : null);
  const isStaleScope =
    /^worker_proof_formalization:worker:/.test(callId)
    || /^proof_formalization_worker_/.test(callId)
    || /^reviewer_proof_formalization:reviewer:review(:|$)/.test(callId)
    || /^proof_formalization_reviewer_review[_:]/.test(callId);
  let provider = meta?.provider || hintedProvider || null;
  if (!provider && isLive) {
    const liveDir = path.join(stateDir, 'chats', 'live', callId);
    if (fs.existsSync(path.join(liveDir, 'output.log'))) provider = 'codex';
    else if (fs.existsSync(path.join(liveDir, 'transcript.jsonl'))) provider = 'claude';
    else if (fs.existsSync(path.join(liveDir, 'transcript.json'))) provider = 'gemini';
    else {
      // claude headless drops <uuid>.jsonl
      try {
        if (fs.readdirSync(liveDir).some((f) => /^[0-9a-f-]{20,}\.jsonl$/.test(f))) {
          provider = 'claude';
        }
      } catch {}
    }
  }

  // When call.json is missing or scope-based call.json lacks request_id, fall
  // back to the event log / UI hints so live TUI matching stays request-bound.
  let fallbackRequestId = hintedRequestId;
  let fallbackRole = hintedRole;
  if (isLive && (meta?.request_id == null || !fallbackRole)) {
    try {
      const ec = requestCyclesFromEventLog(projectInfo);
      const liveBursts = (ec.bursts || []).filter((b) => b.cycle === ec.liveCycle);
      const { names: dirNamesFb } = listCandidateChatDirs(projectInfo, 'live');
      for (const burst of liveBursts) {
        if (hintedRequestId != null && burst.request_id !== hintedRequestId) continue;
        const matches = resolveBurstArtifacts(burst, dirNamesFb);
        const isMissingHint = callId.startsWith('__missing__:') && burst.request_id === hintedRequestId;
        if (isMissingHint || matches.some((m) => m.artifact_id === callId)) {
          fallbackRequestId = burst.request_id;
          const k = String(burst.kind || '').toLowerCase();
          if (k.includes('review')) fallbackRole = 'reviewer';
          else if (k.includes('worker')) fallbackRole = 'worker';
          break;
        }
      }
    } catch {}
  }
  // Last-resort role inference from artifact_id prefix, if still unknown.
  let inferredRole = null;
  if (callId.startsWith('worker_') || callId.includes(':worker:')) inferredRole = 'worker';
  else if (callId.startsWith('reviewer_') || callId.includes(':reviewer:')) inferredRole = 'reviewer';
  const call = {
    artifact_id: callId,
    kind: hintedKind || inferCallKind(callId),
    request_id: meta?.request_id ?? hintedRequestId ?? inferRequestId(callId) ?? fallbackRequestId,
    provider,
    model: meta?.model || null,
    role: meta?.role || fallbackRole || inferredRole,
    session_id: meta?.session_id || null,
    scope: meta?.scope || null,
    started_at_ms: meta?.started_at_ms ?? null,
    ended_at_ms: meta?.ended_at_ms ?? null,
    has_transcript: false,
    // Enabled whenever there's something we could show in the pane view —
    // either a live tmux session to tail, or a pane.txt snapshot the
    // supervisor wrote when the burst ended. Codex is headless (no TUI).
    has_tui_pane: !!(
      isLive
      && provider !== 'codex'
      && (
        (provider === 'claude' || provider === 'gemini' || !provider)
        && (() => {
          try {
            return fs.existsSync(path.join(stateDir, 'chats', 'live', callId, 'pane.txt'));
          } catch { return false; }
        })()
      )
    ),
    transcript_is_stale_scope_dir: isStaleScope,
  };

  // TUI passthrough — capture-pane from the matching live tmux burst
  // session; fall back to the on-disk pane.txt snapshot the supervisor
  // writes when the burst ends. That way recently-finished verifier lanes
  // (which live 1–2 min and have their tmux session torn down on exit)
  // still render a useful pane view instead of "(pane empty)".
  if (source === 'tui') {
    const pickPaneSnapshot = () => {
      if (!isLive) return null;
      try {
        const p = path.join(stateDir, 'chats', 'live', callId, 'pane.txt');
        if (!fs.existsSync(p)) return null;
        const st = fs.statSync(p);
        const text = fs.readFileSync(p, 'utf8');
        return { text, mtimeMs: st.mtimeMs };
      } catch { return null; }
    };

    if (!isLive) {
      return { call, source: 'tui', events: [], missing: true };
    }
    let lsOut = '';
    try {
      lsOut = execFileSync('tmux', tmuxArgs('ls', '-F', '#{session_name}'), {
        encoding: 'utf8', stdio: ['ignore', 'pipe', 'ignore'],
      });
    } catch {}
    const candidates = lsOut.split('\n').map((s) => s.trim()).filter(Boolean);
    // Match heuristic: burst session names embed role + request_id and a
    // lane suffix (v1/v2 for verifiers, worker/reviewer otherwise). Prefer
    // an exact (request_id, lane) match so the v2 lane doesn't cross-wire
    // onto v1's surviving session or vice versa.
    const callLane = (() => {
      const m = String(callId || '').match(/:(v\d+):/);
      if (m) return m[1];
      if (hintedLane) return hintedLane;
      if (String(callId || '').startsWith('worker_')) return 'worker';
      if (String(callId || '').startsWith('reviewer_')) return 'reviewer';
      return null;
    })();
    let matched = null;
    let matchedLane = null;
    for (const name of candidates) {
      const parsed = parseBurstSessionName(name);
      if (!parsed) continue;
      if (call.request_id != null && parsed.request_id === call.request_id) {
        if (callLane && parsed.lane === callLane) { matched = name; matchedLane = parsed.lane; break; }
        // Tentative — keep scanning for an exact lane hit.
        if (matched == null) { matched = name; matchedLane = parsed.lane; }
      }
      if (call.request_id == null && call.role && parsed.role === call.role && matched == null) {
        matched = name; matchedLane = parsed.lane;
      }
    }
    // If the lane we want differs from what we picked tentatively, don't
    // cross-wire — drop the tentative match and fall back to pane.txt.
    if (matched && callLane && matchedLane && matchedLane !== callLane) {
      matched = null;
    }
    if (matched) {
      let paneText = '';
      try {
        paneText = execFileSync('tmux', tmuxArgs('capture-pane', '-t', matched, '-p', '-S', '-600'), {
          encoding: 'utf8', stdio: ['ignore', 'pipe', 'ignore'], maxBuffer: 4 * 1024 * 1024,
        });
      } catch (e) {
        // Capture failed (session died between ls and capture) — fall
        // through to the snapshot.
        paneText = '';
        matched = null;
      }
      if (matched) {
        return {
          call,
          source: 'tui',
          tmux_session: matched,
          events: [{ kind: 'other', provider, text: paneText }],
          missing: false,
        };
      }
    }
    const snapshot = pickPaneSnapshot();
    if (snapshot) {
      return {
        call,
        source: 'tui',
        tmux_session: null,
        snapshot_mtime_ms: snapshot.mtimeMs,
        events: [{ kind: 'other', provider, text: snapshot.text }],
        missing: false,
        note: 'Snapshot from pane.txt — burst has no live tmux session anymore.',
      };
    }
    return { call, source: 'tui', events: [], missing: true, error: 'no matching tmux session' };
  }

  // Determine filename based on provider.
  let events = [];
  let raw = '';
  let transcriptFile = null;
  let promptExcerpt = '';
  const tryClaudeUuid = () => {
    const files = listArtifactFiles(stateDir, cycleValue, callId, gitPrefix, chatsRepo);
    const uuid = files.find((f) => /^[0-9a-f-]{20,}\.jsonl$/.test(f));
    if (!uuid) return '';
    const text = readArtifactFile(stateDir, cycleValue, callId, uuid, gitPrefix, chatsRepo);
    if (text) transcriptFile = uuid;
    return text;
  };
  if (provider === 'codex') {
    transcriptFile = 'output.log';
    raw = readArtifactFile(stateDir, cycleValue, callId, 'output.log', gitPrefix, chatsRepo);
    if (raw) events = parseCodexOutputLogEvents(raw);
  } else if (provider === 'claude') {
    transcriptFile = 'transcript.jsonl';
    raw = readArtifactFile(stateDir, cycleValue, callId, 'transcript.jsonl', gitPrefix, chatsRepo);
    if (!raw) raw = tryClaudeUuid();
    if (raw) events = parseClaudeTranscriptEvents(raw, call.started_at_ms, call.ended_at_ms);
  } else if (provider === 'gemini') {
    transcriptFile = 'transcript.json';
    raw = readArtifactFile(stateDir, cycleValue, callId, 'transcript.json', gitPrefix, chatsRepo);
    if (raw) events = parseGeminiTranscriptEvents(raw, call.started_at_ms, call.ended_at_ms);
  } else {
    // Unknown provider — try each transcript form.
    raw = readArtifactFile(stateDir, cycleValue, callId, 'output.log', gitPrefix, chatsRepo);
    if (raw) {
      events = parseCodexOutputLogEvents(raw);
      call.provider = call.provider || 'codex';
      transcriptFile = 'output.log';
    } else {
      raw = readArtifactFile(stateDir, cycleValue, callId, 'transcript.jsonl', gitPrefix, chatsRepo);
      if (raw) {
        events = parseClaudeTranscriptEvents(raw, call.started_at_ms, call.ended_at_ms);
        call.provider = call.provider || 'claude';
        transcriptFile = 'transcript.jsonl';
      } else {
        raw = readArtifactFile(stateDir, cycleValue, callId, 'transcript.json', gitPrefix, chatsRepo);
        if (raw) {
          events = parseGeminiTranscriptEvents(raw, call.started_at_ms, call.ended_at_ms);
          call.provider = call.provider || 'gemini';
          transcriptFile = 'transcript.json';
        } else {
          raw = tryClaudeUuid();
          if (raw) {
            events = parseClaudeTranscriptEvents(raw, call.started_at_ms, call.ended_at_ms);
            call.provider = call.provider || 'claude';
          }
        }
      }
    }
  }

  const missing = !raw;
  call.has_transcript = !missing;

  // Always attach a prompt excerpt for stale-scope dirs so the frontend
  // can surface the caveat alongside transcript content.
  if (missing || isStaleScope) {
    promptExcerpt = readArtifactFile(stateDir, cycleValue, callId, 'prompt.txt', gitPrefix, chatsRepo).slice(0, 4000);
  }
  let note = null;
  if (isStaleScope) {
    note = 'This scope dir is rewritten every burst in this role/scope. Its content may belong to a later burst; attribution to this specific request is best-effort.';
  }
  const byteSize = Buffer.byteLength(raw || '', 'utf8');
  return {
    call,
    source: 'transcript',
    transcript_file: transcriptFile,
    events,
    size: byteSize,
    next_offset: byteSize,
    missing,
    prompt_excerpt: promptExcerpt,
    note,
  };
}

// In-process TTL caches for chat calls/events. The underlying builders
// re-parse JSONL state files on each call. For live data we cache 10s
// (run state evolves on a ~minute scale); for historical cycles, the
// data is immutable so cache lives until process exit.
const _chatCallsCache = new Map();   // key: project|cycle → {ts, value}
const _chatEventsCache = new Map();  // key: project|cycle|call|source → {ts, value}
const CHAT_LIVE_TTL_MS = 10 * 1000;

function _isLiveKey(cycle) {
  return cycle === 'live' || cycle === '';
}

function handleChatCalls(req, res, project) {
  let projectInfo;
  try {
    projectInfo = typeof project === 'string' ? resolveRepoPath(project) : project;
  } catch (e) {
    // Project dir wiped between discovery and this request — serve an empty
    // call list rather than letting the throw escape the handler.
    res.json({ cycle: null, calls: [], unavailable: true });
    return;
  }
  const cycle = (req.query.cycle || '').toString().trim();
  const cycleKey = cycle || 'live';
  // `?show_stale=1` overrides the default stale-dir filter (also see
  // TRELLIS_VIEWER_SHOW_STALE env var).
  const showStaleRaw = (req.query.show_stale || '').toString().toLowerCase();
  const showStale = showStaleRaw === '1' || showStaleRaw === 'true' || showStaleRaw === 'yes';
  const cacheKey = `${projectInfo.slug || projectInfo.repoPath}|${cycleKey}|stale=${showStale ? 1 : 0}`;
  const entry = _chatCallsCache.get(cacheKey);
  const now = Date.now();
  const ttl = _isLiveKey(cycleKey) ? CHAT_LIVE_TTL_MS : Infinity;
  if (entry && (now - entry.ts) < ttl) {
    res.json(entry.value);
    return;
  }
  try {
    const value = buildChatCalls(projectInfo, cycleKey, { showStale });
    _chatCallsCache.set(cacheKey, { ts: now, value });
    res.json(value);
  } catch (e) {
    res.status(500).json({ error: e.message });
  }
}

function handleChatEvents(req, res, project) {
  let projectInfo;
  try {
    projectInfo = typeof project === 'string' ? resolveRepoPath(project) : project;
  } catch (e) {
    res.json({ events: [], unavailable: true });
    return;
  }
  const cycle = (req.query.cycle || 'live').toString().trim();
  const callId = (req.query.call_id || '').toString().trim();
  const source = (req.query.source || 'transcript').toString().trim();
  const hints = {
    request_id: (req.query.request_id || '').toString().trim(),
    event_index: (req.query.event_index || '').toString().trim(),
    lane: (req.query.lane || '').toString().trim(),
    kind: (req.query.kind || '').toString().trim(),
    provider: (req.query.provider || '').toString().trim(),
    role: (req.query.role || '').toString().trim(),
  };
  if (!callId) {
    res.status(400).json({ error: 'missing call_id' });
    return;
  }
  if (!/^[A-Za-z0-9._+\-:]+$/.test(callId)) {
    res.status(400).json({ error: 'invalid call_id' });
    return;
  }
  const hintKey = JSON.stringify(hints);
  const cacheKey = `${projectInfo.slug || projectInfo.repoPath}|${cycle}|${callId}|${source}|${hintKey}`;
  const entry = _chatEventsCache.get(cacheKey);
  const now = Date.now();
  const isLive = _isLiveKey(cycle);
  // Per-entry TTL: cached `{missing:true}` payloads (which can happen when a
  // historical chat-events query races with `rebuild_cycle_chat_dirs`, or hits
  // a burst whose dir is being written) must NOT pin forever — otherwise a
  // transient miss permanently masks valid content for that (cycle, call_id).
  // Live entries keep their existing short TTL; live missing also short.
  const cachedTtl = entry && entry.value && entry.value.missing
    ? CHAT_LIVE_TTL_MS
    : (isLive ? CHAT_LIVE_TTL_MS : Infinity);
  if (entry && (now - entry.ts) < cachedTtl) {
    res.json(entry.value);
    return;
  }
  try {
    const value = buildChatEvents(projectInfo, cycle, callId, source, hints);
    _chatEventsCache.set(cacheKey, { ts: now, value });
    res.json(value);
  } catch (e) {
    res.status(500).json({ error: e.message });
  }
}

function handleFeedbackPost(req, res, project) {
  const { action, feedback } = req.body;
  const projectInfo = resolveRepoPath(project);
  const { repoPath, stateDir, repoType } = projectInfo;

  if (repoType === 'trellis') {
    try {
      res.json(trellisAdapter(projectInfo, 'feedback-post', [String(action || ''), '--feedback', String(feedback || '')]));
    } catch (e) {
      res.status(500).json({ error: e.message });
    }
    return;
  }
  JSON.parse(fs.readFileSync(path.join(stateDir, 'state.json'), 'utf-8'));

  if (action === 'approve') {
    const signalPath = path.join(stateDir, 'human_approve.json');
    fs.writeFileSync(signalPath, JSON.stringify({ action: 'approve', timestamp: new Date().toISOString() }));
    return res.json({ ok: true, message: 'Approval signal written. Supervisor will continue.' });
  }
  if (action === 'feedback') {
    const feedbackPath = path.join(repoPath, 'HUMAN_INPUT.md');
    fs.writeFileSync(feedbackPath, feedback || '');
    const signalPath = path.join(stateDir, 'human_feedback.json');
    fs.writeFileSync(signalPath, JSON.stringify({ action: 'feedback', feedback: feedback || '', timestamp: new Date().toISOString() }));
    const pausePath = path.join(stateDir, 'pause');
    try { fs.unlinkSync(pausePath); } catch {}
    return res.json({ ok: true, message: 'Feedback written. Supervisor will run another cycle.' });
  }
  return res.status(400).json({ error: 'action must be "approve" or "feedback"' });
}

function handleFeedbackGet(res, project) {
  const projectInfo = resolveRepoPath(project);
  const { repoPath, stateDir, repoType } = projectInfo;
  if (repoType === 'trellis') {
    res.json(trellisAdapter(projectInfo, 'feedback-get'));
    return;
  }
  const state = JSON.parse(fs.readFileSync(path.join(stateDir, 'state.json'), 'utf-8'));
  const awaiting = state.awaiting_human_input || false;
  const phase = state.phase || '';
  const lastReview = state.last_review || {};
  let humanInput = '';
  try { humanInput = fs.readFileSync(path.join(repoPath, 'HUMAN_INPUT.md'), 'utf-8'); } catch {}

  res.json({
    awaiting_input: awaiting,
    phase,
    last_review_decision: lastReview.decision || '',
    last_review_reason: lastReview.reason || '',
    human_input: humanInput,
  });
}

// Legacy URL redirect: any /leanbelt[...] path forwards to the corresponding
// trellis path under BASE. Preserves bookmarks/links from before the
// leanbelt → trellis rename (2026-05-20). 301 so caches/agents stop hitting
// the old path. Query strings carry through because we forward originalUrl.
app.get(/^\/leanbelt(\/.*)?$/, (req, res) => {
  const rest = req.originalUrl.substring('/leanbelt'.length);
  res.redirect(301, `${BASE}${rest}`);
});

app.get(BASE, sendLanding);
app.get(`${BASE}/`, sendLanding);
app.get(`${BASE}/:project`, (req, res, next) => {
  // `originalUrl` still carries any control-token prefix, which is what we
  // want here: it is the URL the browser actually asked for.
  if ((req.originalUrl || '').endsWith('/')) {
    next();
    return;
  }
  res.redirect(`${basePathFor(req)}/${req.params.project}/`);
});
app.get(`${BASE}/:project/`, sendIndex);
app.use(BASE, express.static(path.join(__dirname, 'public')));

app.get([PROMPTS_BASE, `${PROMPTS_BASE}/`], sendPromptsIndex);

app.get(`${PROMPTS_BASE}/api/projects.json`, (_req, res) => {
  try {
    const projects = discoverProjects().map(project => ({
      slug: project.slug,
      repoType: project.repoType,
    }));
    res.json({
      default_project: defaultPromptsProjectSlug(),
      projects,
    });
  } catch (e) {
    res.status(500).json({ error: e.message });
  }
});

app.get(`${PROMPTS_BASE}/api/catalog.json`, (req, res) => {
  try {
    const project = String(req.query.project || defaultPromptsProjectSlug());
    const projectInfo = resolveRepoPath(project);
    if (projectInfo.repoType !== 'trellis') {
      return res.status(400).json({ error: `prompt browser only supports trellis projects (got ${projectInfo.repoType || 'unknown'})` });
    }
    const payload = trellisAdapter(projectInfo, 'prompts-catalog');
    payload.project = projectInfo.slug;
    res.json(payload);
  } catch (e) {
    res.status(500).json({ error: e.message });
  }
});

app.get(`${PROMPTS_BASE}/api/render/:scenarioId`, (req, res) => {
  try {
    const project = String(req.query.project || defaultPromptsProjectSlug());
    const projectInfo = resolveRepoPath(project);
    if (projectInfo.repoType !== 'trellis') {
      return res.status(400).json({ error: `prompt browser only supports trellis projects (got ${projectInfo.repoType || 'unknown'})` });
    }
    const payload = trellisAdapter(projectInfo, 'prompts-render', [String(req.params.scenarioId)]);
    payload.project = projectInfo.slug;
    res.json(payload);
  } catch (e) {
    res.status(500).json({ error: e.message });
  }
});

// In-process TTL cache for live viewer-state and chats. Both endpoints
// re-read big JSON files (event_log, runtime state) on every request and
// take ~1s under load (mostly python startup + JSON parsing). Run state
// evolves on a ~minute scale, so a 30 s TTL is safe and matches the SPA's
// `AUTO_REFRESH_INTERVAL_MS`. Combined with the in-flight coalescer
// below, concurrent tabs/refreshes share a single python spawn instead
// of each spawning their own and blocking the Node.js event loop in
// sequence.
const LIVE_VIEWER_TTL_MS = 30 * 1000;
const _liveViewerStateCache = new Map();
const _liveChatsCache = new Map();

// In-flight Promise coalescer for `trellisAdapter`-style heavy calls.
// Key: arbitrary string identifying (project, command, args-fingerprint).
// Value: the Promise of the in-flight result.
//
// Concurrent callers for the same key await the SAME promise — they
// observe one python3 spawn cost instead of `n × python3` blocking the
// event loop in sequence. Once the promise settles (resolve or reject),
// the entry is deleted so the next call can re-run.
const _adapterInFlight = new Map();

function _adapterInFlightOnce(key, runner) {
  const existing = _adapterInFlight.get(key);
  if (existing) return existing;
  const p = Promise.resolve()
    .then(() => runner())
    .finally(() => _adapterInFlight.delete(key));
  _adapterInFlight.set(key, p);
  return p;
}

// Augment a viewer-state payload (live or historical) with per-node
// `closure_status` and a top-level `local_closure_summary`, sourced from the
// Patch C-A *committed* closure mirrors (`committed_local_closure_unverified_nodes`,
// `committed_local_closure_failures`, `committed_local_closure_records`) so the
// per-node colors stay tier-consistent with the committed DAG fields below
// (`committed.present_nodes` / `committed.open_nodes`). Read-only: never
// throws, always returns the input shape.
//
// Tier alignment (fix for audit LOW "viewer closure status mixes live closure
// fields with committed DAG fields"): the DAG layout and `open_nodes` are
// committed-tier; reading live-tier closure fields meant during an in-flight
// or just-accepted cycle a node could be colored from a different state-tier
// than the DAG it's drawn on. We now source closure status from committed
// mirrors so the two halves of the display agree.
//
// Defensive fallback chain (per node-shape field, in order):
//   1. committed_local_closure_* (kernel Patch C-A mirror; preferred)
//   2. local_closure_* (live-tier; fallback for pre-Patch-C-A state or migrated
//      checkpoints that don't yet carry the committed mirror)
//   3. empty (pre-Patch-C entirely; augmentation degrades to legacy
//      "verified == not in open_nodes" coloring)
//
// closure_status decision (per LOCAL_CLOSURE_IMPL_PLAN.md §9 viewer touch):
//   - "open"       in committed.open_nodes
//   - "unverified" not in committed.open_nodes AND in committed_local_closure_unverified_nodes
//   - "verified"   not in committed.open_nodes AND in committed_local_closure_records AND not unverified
//   - "absent"     not in committed.present_nodes
//
// Expected wire-shape (from trellis_adapter.py live-state / state-at):
//   payload = {
//     state: {
//       committed: { present_nodes: string[], open_nodes: string[] },
//       // Patch C-A committed mirrors (preferred source):
//       committed_local_closure_unverified_nodes?: string[],
//       committed_local_closure_records?: { [node: string]: object },
//       committed_local_closure_failures?: { [node: string]: ErrorSummary },
//       // Live-tier closure fields (fallback if committed mirrors absent):
//       local_closure_unverified_nodes?: string[],
//       local_closure_records?: { [node: string]: object },
//       local_closure_failures?: { [node: string]: ErrorSummary },
//       ...
//     },
//     nodes: { [name: string]: object },
//     ...
//   }
// Heavy per-node content (leanContent / texContent / declaration) is stripped
// from viewer-state and state-at responses and parked here so the client can
// lazy-fetch it for the *one* node it's about to render. The full live state
// is ~13 MB / ~1.6 MB gzipped, of which ~5.4 MB raw is these three string
// fields summed across ~500 nodes; the user typically views ≤1 detail panel
// at a time.
//
// key: `${projectKey}::${cycle}::${name}` -> { leanContent, texContent, declaration }
const _nodeContentCache = new Map();
const NODE_CONTENT_CACHE_MAX = 50000;

function _trimNodeContentCache() {
  if (_nodeContentCache.size <= NODE_CONTENT_CACHE_MAX) return;
  const overflow = _nodeContentCache.size - NODE_CONTENT_CACHE_MAX;
  let i = 0;
  for (const k of _nodeContentCache.keys()) {
    if (i++ >= overflow) break;
    _nodeContentCache.delete(k);
  }
}

const _DEF_RE = /^(noncomputable\s+)?def\s/m;

// Thin the viewer-state payload before it goes on the wire:
//
//   * For each node: replace `leanContent` / `texContent` / `declaration`
//     with empty strings (parked in `_nodeContentCache` for lazy fetch), and
//     precompute `isDefinition` so the DAG shape selector doesn't need the
//     full Lean text.
//
//   * Drop `state.committed_local_closure_records` AND
//     `state.local_closure_records`. The client only existence-checks records,
//     and its `getClosureStatus` fallback returns 'verified' on either branch,
//     so the records map changes nothing it renders. `closure_status` (cheap,
//     precomputed by augmentViewerStateClosure, which runs BEFORE this thinning)
//     is the real source, and record_count is taken from the committed mirror at
//     augment time. The local records map is the bulk of the payload
//     (~7.8 MB raw / ~0.5 MB gz; ~99% of `state` on one live run).
//
// Saves ~7-9 MB raw / ~1 MB gz on each fetch.
function thinViewerStatePayload(payload, projectKey) {
  if (!payload || typeof payload !== 'object') return payload;
  const cycle = (payload.state && typeof payload.state.cycle === 'number')
    ? String(payload.state.cycle) : 'live';
  const nodes = (payload.nodes && typeof payload.nodes === 'object') ? payload.nodes : {};
  for (const [name, node] of Object.entries(nodes)) {
    if (!node || typeof node !== 'object') continue;
    const lean = typeof node.leanContent === 'string' ? node.leanContent : '';
    const tex = typeof node.texContent === 'string' ? node.texContent : '';
    const decl = typeof node.declaration === 'string' ? node.declaration : '';
    if (lean || tex || decl) {
      _nodeContentCache.set(`${projectKey}::${cycle}::${name}`,
        { leanContent: lean, texContent: tex, declaration: decl });
    }
    // DAG shape selector previously did `(node.leanContent || '').match(/^...def\s/m)`.
    // Precompute the bit so we can drop leanContent from the wire payload.
    node.isDefinition = node.texEnv === 'definition' || _DEF_RE.test(lean);
    node.leanContent = '';
    node.texContent = '';
    node.declaration = '';
  }
  _trimNodeContentCache();
  if (payload.state && payload.state.committed_local_closure_records) {
    delete payload.state.committed_local_closure_records;
  }
  if (payload.state && payload.state.local_closure_records) {
    delete payload.state.local_closure_records;
  }
  payload.node_content_inline = false;
  return payload;
}

function augmentViewerStateClosure(payload) {
  if (!payload || typeof payload !== 'object') return payload;
  const state = payload.state || {};
  const committed = state.committed || {};
  const present = new Set(Array.isArray(committed.present_nodes) ? committed.present_nodes : []);
  const open = new Set(Array.isArray(committed.open_nodes) ? committed.open_nodes : []);
  // Prefer committed mirrors so coloring stays on the same tier as the
  // committed DAG; fall back to live-tier fields when the committed mirror is
  // absent (pre-Patch-C-A state, or migrated checkpoints without mirror-ready).
  const unverifiedList = Array.isArray(state.committed_local_closure_unverified_nodes)
    ? state.committed_local_closure_unverified_nodes
    : (Array.isArray(state.local_closure_unverified_nodes)
        ? state.local_closure_unverified_nodes // fallback
        : []);
  const unverified = new Set(unverifiedList);
  const failures = (state.committed_local_closure_failures && typeof state.committed_local_closure_failures === 'object')
    ? state.committed_local_closure_failures
    : ((state.local_closure_failures && typeof state.local_closure_failures === 'object')
        ? state.local_closure_failures // fallback
        : {});
  const records = (state.committed_local_closure_records && typeof state.committed_local_closure_records === 'object')
    ? state.committed_local_closure_records
    : ((state.local_closure_records && typeof state.local_closure_records === 'object')
        ? state.local_closure_records // fallback
        : {});

  const nodes = (payload.nodes && typeof payload.nodes === 'object') ? payload.nodes : {};
  const closureStatus = {};
  const allNames = new Set([
    ...Object.keys(nodes),
    ...present,
    ...unverifiedList,
  ]);
  for (const name of allNames) {
    if (!present.has(name) && !nodes[name]) {
      closureStatus[name] = 'absent';
    } else if (open.has(name)) {
      closureStatus[name] = 'open';
    } else if (unverified.has(name)) {
      closureStatus[name] = 'unverified';
    } else if (records[name]) {
      closureStatus[name] = 'verified';
    } else {
      // Sorry-free node with no record yet (pre-migration / fresh node) — treat
      // as "verified" if not in open_nodes, matching legacy color semantics so
      // existing runs without the closure layer don't suddenly turn yellow.
      closureStatus[name] = 'verified';
    }
  }

  // Failure summary: counts by status + the latest 3 messages by captured cycle.
  let transportErrors = 0, scriptErrors = 0, otherErrors = 0;
  const failureRows = [];
  for (const [node, summary] of Object.entries(failures)) {
    if (!summary || typeof summary !== 'object') continue;
    const status = String(summary.status || '');
    if (status === 'transport_error') transportErrors++;
    else if (status === 'elaboration_error' || status === 'missing_declaration' || status === 'internal_error') scriptErrors++;
    else otherErrors++;
    failureRows.push({
      node,
      status,
      stderr_excerpt: String(summary.stderr_excerpt || '').slice(0, 400),
      axiom_violations: Array.isArray(summary.axiom_violations) ? summary.axiom_violations.slice(0, 8) : [],
      strict_errors: Array.isArray(summary.strict_errors) ? summary.strict_errors.slice(0, 8) : [],
      retry_count: Number(summary.retry_count || 0),
      retry_exhausted: !!summary.retry_exhausted,
      captured_at_cycle: Number(summary.captured_at_cycle || 0),
      returncode: Number(summary.returncode || 0),
    });
  }
  failureRows.sort((a, b) => (b.captured_at_cycle || 0) - (a.captured_at_cycle || 0));
  const latestFailures = failureRows.slice(0, 3);

  payload.closure_status = closureStatus;
  payload.local_closure_summary = {
    unverified_count: unverifiedList.length,
    transport_error_count: transportErrors,
    script_error_count: scriptErrors,
    other_error_count: otherErrors,
    record_count: Object.keys(records).length,
    latest_failures: latestFailures,
  };
  return payload;
}

function readLiveViewerStateCached(projectInfo) {
  const key = projectInfo.slug || projectInfo.repoPath;
  const entry = _liveViewerStateCache.get(key);
  const now = Date.now();
  if (entry && (now - entry.ts) < LIVE_VIEWER_TTL_MS) return entry.value;
  const value = thinViewerStatePayload(
    augmentViewerStateAttention(augmentViewerStateClosure(readLiveViewerState(projectInfo))),
    projectCacheKey(projectInfo),
  );
  _liveViewerStateCache.set(key, { ts: now, value });
  return value;
}

// Async coalesced version of `readLiveViewerStateCached` for use by HTTP
// endpoints. When N tabs hit `/api/viewer-state.json` simultaneously and
// the cache is stale, only the first one spawns python; the rest await
// the same Promise. Python runs in `spawn` mode so it doesn't block the
// Node.js event loop while it computes — other endpoints remain
// responsive.
async function readLiveViewerStateCachedAsync(projectInfo) {
  const key = projectInfo.slug || projectInfo.repoPath;
  const entry = _liveViewerStateCache.get(key);
  const now = Date.now();
  if (entry && (now - entry.ts) < LIVE_VIEWER_TTL_MS) return entry.value;
  return _adapterInFlightOnce(`live-state::${key}`, async () => {
    // Re-check the cache: while we were queued behind the coalescer's
    // mutex, another caller may have populated it.
    const fresh = _liveViewerStateCache.get(key);
    if (fresh && (Date.now() - fresh.ts) < LIVE_VIEWER_TTL_MS) return fresh.value;
    let raw;
    if (projectInfo.repoType === 'trellis') {
      raw = await trellisAdapterAsync(projectInfo, 'live-state');
    } else {
      // Non-trellis repos use a sync file read; cheap, no event-loop concern.
      raw = readLiveViewerState(projectInfo);
    }
    const value = thinViewerStatePayload(
      augmentViewerStateAttention(augmentViewerStateClosure(raw)),
      projectCacheKey(projectInfo),
    );
    _liveViewerStateCache.set(key, { ts: Date.now(), value });
    return value;
  });
}

function readLiveChatsCached(projectInfo) {
  const key = projectInfo.slug || projectInfo.repoPath;
  const entry = _liveChatsCache.get(key);
  const now = Date.now();
  if (entry && (now - entry.ts) < LIVE_VIEWER_TTL_MS) return entry.value;
  const value = readLiveChats(projectInfo);
  _liveChatsCache.set(key, { ts: now, value });
  return value;
}

// API endpoints
app.get(`${BASE}/api/viewer-state.json`, async (req, res) => {
  try {
    const projectInfo = resolveRepoPath(defaultProjectSlug());
    res.json(await readLiveViewerStateCachedAsync(projectInfo));
  } catch (e) { res.status(500).json({ error: e.message }); }
});

app.get(`${BASE}/:project/api/viewer-state.json`, async (req, res) => {
  try {
    const projectInfo = resolveRepoPath(projectFromRequest(req));
    res.json(await readLiveViewerStateCachedAsync(projectInfo));
  } catch (e) { res.status(500).json({ error: e.message }); }
});

// ---------------------------------------------------------------------------
// Per-node Lean semantic closure (lazy, cached per (project, node, cycle)).
//
// The viewer adapter's `semantic-closure` command reads each node's
// most-recent `checker-state/semantic-payloads/*.json` sidecar and returns
// `{ closures: { node: [closure_nodes…] | null } }`. Each adapter call is
// fast (one disk read + one parse per requested node — ~50-100 ms warm).
// We still cache the answer here per cycle so navigating back to a node
// the user already viewed doesn't re-spawn python3.
//
// Cache is invalidated when the live `cycle` advances, so freshly-edited
// statements pick up the new closure on the next cycle commit. A node
// for which there's no cached payload (new helper just introduced)
// returns `null`; the UI shows a "no cached payload" hint and the next
// supervisor cycle will populate it.
// ---------------------------------------------------------------------------
const semanticClosureCache = new Map(); // key: `${projectKey}::${cycle}::${node}` -> closure array | null

function liveCycleFor(projectInfo) {
  try {
    const live = readLiveViewerStateCached(projectInfo);
    const c = live && live.state && live.state.cycle;
    return typeof c === 'number' ? c : 0;
  } catch {
    return 0;
  }
}

function semanticClosureHandler(projectInfo, nodeName, res) {
  if (typeof nodeName !== 'string' || !/^[A-Za-z][A-Za-z0-9_]*$/.test(nodeName)) {
    return res.status(400).json({ error: 'Invalid node name' });
  }
  const cycle = liveCycleFor(projectInfo);
  const key = `${projectInfo.projectKey || projectInfo.repoPath}::${cycle}::${nodeName}`;
  if (semanticClosureCache.has(key)) {
    return res.json({ node: nodeName, cycle, closure: semanticClosureCache.get(key), cached: true });
  }
  try {
    const payload = trellisAdapter(projectInfo, 'semantic-closure', ['--node', nodeName]);
    if (!payload || payload.ok === false) {
      return res.status(503).json({
        node: nodeName,
        cycle,
        closure: null,
        error: (payload && payload.error) || 'semantic-closure unavailable',
      });
    }
    const closures = (payload.closures || {});
    const result = Object.prototype.hasOwnProperty.call(closures, nodeName) ? closures[nodeName] : null;
    semanticClosureCache.set(key, result);
    // Bound cache size — drop oldest entries past 2000.
    if (semanticClosureCache.size > 2000) {
      const firstKey = semanticClosureCache.keys().next().value;
      semanticClosureCache.delete(firstKey);
    }
    return res.json({ node: nodeName, cycle, closure: result, cached: false });
  } catch (e) {
    return res.status(500).json({ node: nodeName, cycle, closure: null, error: e.message });
  }
}

// Per-node heavy content (leanContent / texContent / declaration). Populated
// as a side-effect of thinViewerStatePayload; lazy-fetched by the client when
// it actually opens a node-detail pane. Cache misses (LRU eviction, server
// restart) repopulate by re-reading the underlying state.
function nodeContentHandler(projectInfo, cycleParam, nodeName, res) {
  if (typeof nodeName !== 'string' || !/^[A-Za-z][A-Za-z0-9_]*$/.test(nodeName)) {
    return res.status(400).json({ error: 'Invalid node name' });
  }
  const projectKey = projectCacheKey(projectInfo);
  const wantsLive = cycleParam === 'live' || cycleParam === 'current' || cycleParam == null;
  let cycleNum = null;
  if (!wantsLive) {
    cycleNum = parseInt(cycleParam, 10);
    if (!Number.isInteger(cycleNum)) return res.status(400).json({ error: 'Invalid cycle' });
  }
  const cycleStr = wantsLive ? String(liveCycleFor(projectInfo)) : String(cycleNum);
  const key = `${projectKey}::${cycleStr}::${nodeName}`;
  let hit = _nodeContentCache.get(key);
  if (!hit) {
    try {
      if (wantsLive) {
        _liveViewerStateCache.delete(projectInfo.slug || projectInfo.repoPath);
        readLiveViewerStateCached(projectInfo);
      } else {
        cycleStateCache.delete(cycleEntryKey(projectInfo, cycleNum));
        getCachedHistoricalViewerState(projectInfo, cycleNum);
      }
      hit = _nodeContentCache.get(key);
    } catch (e) {
      return res.status(500).json({ error: e.message });
    }
  }
  if (!hit) return res.status(404).json({ error: `No content for ${nodeName} @ ${cycleStr}` });
  return res.json({ node: nodeName, cycle: cycleStr, ...hit });
}

app.get(`${BASE}/api/node-content/:cycle/:node`, (req, res) => {
  try {
    const projectInfo = resolveRepoPath(defaultProjectSlug());
    const node = String(req.params.node).replace(/\.json$/, '');
    nodeContentHandler(projectInfo, req.params.cycle, node, res);
  } catch (e) { res.status(404).json({ error: e.message }); }
});
app.get(`${BASE}/:project/api/node-content/:cycle/:node`, (req, res) => {
  try {
    const projectInfo = resolveRepoPath(projectFromRequest(req));
    const node = String(req.params.node).replace(/\.json$/, '');
    nodeContentHandler(projectInfo, req.params.cycle, node, res);
  } catch (e) { res.status(404).json({ error: e.message }); }
});

app.get(`${BASE}/api/semantic-closure/:node`, (req, res) => {
  try {
    const projectInfo = resolveRepoPath(defaultProjectSlug());
    semanticClosureHandler(projectInfo, req.params.node, res);
  } catch (e) { res.status(500).json({ error: e.message }); }
});

app.get(`${BASE}/:project/api/semantic-closure/:node`, (req, res) => {
  try {
    const projectInfo = resolveRepoPath(projectFromRequest(req));
    semanticClosureHandler(projectInfo, req.params.node, res);
  } catch (e) { res.status(500).json({ error: e.message }); }
});

app.get(`${BASE}/api/cycles.json`, (req, res) => {
  try {
    const projectInfo = resolveRepoPath(defaultProjectSlug());
    res.json(getCachedCyclesList(projectInfo));
  } catch { res.json([]); }
});

app.get(`${BASE}/:project/api/cycles.json`, (req, res) => {
  try {
    const projectInfo = resolveRepoPath(projectFromRequest(req));
    res.json(getCachedCyclesList(projectInfo));
  } catch { res.json([]); }
});

app.get(`${BASE}/api/state-at/:cycle`, (req, res) => {
  const cycle = parseInt(String(req.params.cycle).replace(/\.json$/, ''), 10);
  if (isNaN(cycle)) return res.status(400).json({ error: 'Invalid cycle' });
  try {
    const projectInfo = resolveRepoPath(defaultProjectSlug());
    res.json(getCachedHistoricalViewerState(projectInfo, cycle));
  } catch (e) {
    res.status(404).json({ error: `Cycle ${cycle} not found: ${e.message}` });
  }
});

app.get(`${BASE}/:project/api/state-at/:cycle`, (req, res) => {
  const cycle = parseInt(String(req.params.cycle).replace(/\.json$/, ''), 10);
  if (isNaN(cycle)) return res.status(400).json({ error: 'Invalid cycle' });
  try {
    const projectInfo = resolveRepoPath(projectFromRequest(req));
    res.json(getCachedHistoricalViewerState(projectInfo, cycle));
  } catch (e) {
    res.status(404).json({ error: `Cycle ${cycle} not found: ${e.message}` });
  }
});

app.get(`${BASE}/api/chats.json`, (req, res) => {
  try {
    const projectInfo = resolveRepoPath(defaultProjectSlug());
    res.json(readLiveChatsCached(projectInfo));
  } catch (e) {
    res.status(500).json({ error: e.message });
  }
});

app.get(`${BASE}/:project/api/chats.json`, (req, res) => {
  try {
    const projectInfo = resolveRepoPath(projectFromRequest(req));
    res.json(readLiveChatsCached(projectInfo));
  } catch (e) {
    res.status(500).json({ error: e.message });
  }
});

app.get(`${BASE}/api/chats-at/:cycle`, (req, res) => {
  const cycle = parseInt(String(req.params.cycle).replace(/\.json$/, ''), 10);
  if (isNaN(cycle)) return res.status(400).json({ error: 'Invalid cycle' });
  try {
    const projectInfo = resolveRepoPath(defaultProjectSlug());
    res.json(getCachedHistoricalChats(projectInfo, cycle));
  } catch (e) {
    res.status(404).json({ error: `Chat cycle ${cycle} not found: ${e.message}` });
  }
});

app.get(`${BASE}/:project/api/chats-at/:cycle`, (req, res) => {
  const cycle = parseInt(String(req.params.cycle).replace(/\.json$/, ''), 10);
  if (isNaN(cycle)) return res.status(400).json({ error: 'Invalid cycle' });
  try {
    const projectInfo = resolveRepoPath(projectFromRequest(req));
    res.json(getCachedHistoricalChats(projectInfo, cycle));
  } catch (e) {
    res.status(404).json({ error: `Chat cycle ${cycle} not found: ${e.message}` });
  }
});

app.get(`${BASE}/api/diff/:cycle`, (req, res) => {
  const cycle = parseInt(req.params.cycle, 10);
  if (isNaN(cycle)) return res.status(400).send('Invalid cycle');
  try {
    const projectInfo = resolveRepoPath(defaultProjectSlug());
    res.type('text/plain').send(getCachedCycleDiff(projectInfo, cycle));
  } catch {
    // Project wiped / chats repo unavailable — empty diff, never a 500 page.
    res.type('text/plain').send('');
  }
});

app.get(`${BASE}/:project/api/diff/:cycle`, (req, res) => {
  const cycle = parseInt(req.params.cycle, 10);
  if (isNaN(cycle)) return res.status(400).send('Invalid cycle');
  try {
    const projectInfo = resolveRepoPath(projectFromRequest(req));
    res.type('text/plain').send(getCachedCycleDiff(projectInfo, cycle));
  } catch {
    res.type('text/plain').send('');
  }
});

// API: download tablet snapshot as zip
app.get(`${BASE}/api/download-tablet`, (req, res) => {
  try {
    handleDownloadTablet(res, defaultProjectSlug());
  } catch (e) {
    res.status(500).json({ error: e.message });
  }
});

app.get(`${BASE}/:project/api/download-tablet`, (req, res) => {
  try {
    handleDownloadTablet(res, projectFromRequest(req));
  } catch (e) {
    res.status(500).json({ error: e.message });
  }
});

// Paper-defined KaTeX macros. The frontend fetches this once at init
// and merges the \newcommand bodies into renderMathInElement's macros
// option. Returns the project's paper/header.tex if present, else
// empty body. We deliberately don't 404 — a project without a header
// just gets the viewer's hardcoded defaults.
function handlePaperHeader(res, slug) {
  try {
    const projectInfo = resolveRepoPath(slug);
    const headerPath = path.join(projectInfo.repoPath, 'paper', 'header.tex');
    if (fs.existsSync(headerPath)) {
      res.type('text/plain').send(fs.readFileSync(headerPath, 'utf8'));
    } else {
      res.type('text/plain').send('');
    }
  } catch (e) {
    res.status(500).type('text/plain').send(`% error: ${e.message}`);
  }
}
app.get(`${BASE}/api/paper-header.tex`, (_req, res) => handlePaperHeader(res, defaultProjectSlug()));
app.get(`${BASE}/:project/api/paper-header.tex`, (req, res) => handlePaperHeader(res, projectFromRequest(req)));

// External-codex tracker: hidden viewer feature for marking cycles
// during which the user was running codex CLI sessions outside this
// project. Those cycles' burn deltas are tainted and should be
// dropped from β-calibration.
//
// State lives at <stateDir>/external_codex.json with shape:
//   { active: bool,
//     since_cycle: int|null, since_iso: str|null,
//     intervals: [{since_cycle, since_iso, until_cycle, until_iso}, ...] }
//
// The UI toggles via Ctrl+Shift+E. POST /toggle flips `active`:
// turning ON captures since_cycle/iso; turning OFF closes the open
// interval into `intervals`. The GET response also surfaces a
// derived `marked_cycles` (union over closed intervals + open one)
// for easy consumption by scripts/fit_codex_burn_beta.py.
function externalCodexStatePath(projectInfo) {
  return path.join(projectInfo.stateDir, 'external_codex.json');
}
function readExternalCodex(projectInfo) {
  const p = externalCodexStatePath(projectInfo);
  if (!fs.existsSync(p)) {
    return { active: false, since_cycle: null, since_iso: null, intervals: [] };
  }
  try {
    const d = JSON.parse(fs.readFileSync(p, 'utf-8'));
    return {
      active: !!d.active,
      since_cycle: d.since_cycle ?? null,
      since_iso: d.since_iso ?? null,
      intervals: Array.isArray(d.intervals) ? d.intervals : [],
    };
  } catch {
    return { active: false, since_cycle: null, since_iso: null, intervals: [] };
  }
}
function writeExternalCodex(projectInfo, state) {
  const p = externalCodexStatePath(projectInfo);
  fs.mkdirSync(path.dirname(p), { recursive: true });
  const tmp = p + '.tmp';
  fs.writeFileSync(tmp, JSON.stringify(state, null, 2));
  fs.renameSync(tmp, p);
}
function currentCycleForProject(projectInfo) {
  try {
    const v = readLiveViewerStateCached(projectInfo);
    return v?.meta?.in_flight_cycle ?? v?.state?.cycle ?? null;
  } catch {
    return null;
  }
}
function deriveMarkedCycles(state, currentCycle) {
  const set = new Set();
  for (const iv of state.intervals || []) {
    const a = iv.since_cycle;
    const b = iv.until_cycle ?? a;
    if (a == null) continue;
    for (let c = a; c <= b; c++) set.add(c);
  }
  if (state.active && state.since_cycle != null) {
    const cur = currentCycle ?? state.since_cycle;
    for (let c = state.since_cycle; c <= cur; c++) set.add(c);
  }
  return Array.from(set).sort((a, b) => a - b);
}
function externalCodexResponse(projectInfo) {
  const s = readExternalCodex(projectInfo);
  const cur = currentCycleForProject(projectInfo);
  return {
    ...s,
    current_cycle: cur,
    marked_cycles: deriveMarkedCycles(s, cur),
  };
}
function handleToggleExternalCodex(projectInfo) {
  const cur = currentCycleForProject(projectInfo);
  const s = readExternalCodex(projectInfo);
  const nowIso = new Date().toISOString();
  if (s.active) {
    s.intervals.push({
      since_cycle: s.since_cycle,
      since_iso: s.since_iso,
      until_cycle: cur,
      until_iso: nowIso,
    });
    s.active = false;
    s.since_cycle = null;
    s.since_iso = null;
  } else {
    s.active = true;
    s.since_cycle = cur;
    s.since_iso = nowIso;
  }
  writeExternalCodex(projectInfo, s);
  return externalCodexResponse(projectInfo);
}
app.get(`${BASE}/api/external-codex.json`, (_req, res) => {
  try { res.json(externalCodexResponse(resolveRepoPath(defaultProjectSlug()))); }
  catch (e) { res.status(500).json({ error: e.message }); }
});
app.get(`${BASE}/:project/api/external-codex.json`, (req, res) => {
  try { res.json(externalCodexResponse(resolveRepoPath(projectFromRequest(req)))); }
  catch (e) { res.status(500).json({ error: e.message }); }
});
registerControlRoute('external-codex/toggle', [], (req, res) => {
  try { res.json(handleToggleExternalCodex(resolveRepoPath(projectFromRequest(req)))); }
  catch (e) { res.status(500).json({ error: e.message }); }
});

// =====================================================================
// ATTENTION STATES — every "the run has stopped and wants a human" state,
// on ONE severity ladder.
//
// These states arrive from two unrelated surfaces (halt markers on disk vs
// the kernel's `gate_kind` in protocol state) but the operator sees them in
// the same place and must be able to tell them apart WITHOUT reading text.
// Two of them are semantic opposites that used to render identically:
//
//   * `advance` is a NORMAL phase boundary. Nothing is wrong; review and
//     approve.
//   * `need_input` is a PROCESS FAILURE and a last resort — the kernel
//     escalates to it when a lane cannot make progress ("the kernel
//     escalates to the fail-loud NeedInput HumanGate rather than spin",
//     `kernel/src/model.rs`). Rubber-stamping it hides the failure.
//
// Three visual tiers, and the tier is what carries the meaning:
//
//   tier 'fault'   — the supervisor has HALTED. Rendered as a SOLID FILLED
//                    bar at viewport top. Only halt markers reach this tier,
//                    so nothing lesser can ever look like one.
//   tier 'failure' — the run parked itself on a fail-loud gate. Rendered as
//                    an OUTLINED bar (dark fill, red rule) — alarming, but
//                    structurally lighter than a filled halt bar.
//   tier 'gate'    — a routine checkpoint. Rendered as a calm cyan outlined
//                    bar. Nothing is wrong.
//
// Within the fault tier, checker disagreement outranks system feedback and
// keeps the pure-red fill; system feedback takes amber. `rank` is the total
// order used to pick the primary row and to sort the stack.
// =====================================================================

// A halt row answers two questions, and a marker on disk answers only the
// first: `cause` is why the kernel stopped, and the closing sentence is
// whether the supervisor process is still there. The catalogue carries the
// parked form; `haltAttention` swaps in the exited form when the pause
// surface — the one place that measures liveness — reports the process gone.
const HALT_LIVENESS_PARKED = 'The supervisor will not dispatch new bursts.';
const HALT_LIVENESS_EXITED = 'The supervisor process has exited.';
function faultRow(fields) {
  return { ...fields, summary: `${fields.cause} ${HALT_LIVENESS_PARKED}` };
}

const ATTENTION_STATES = {
  checker_disagreement_halt: faultRow({
    id: 'checker_disagreement_halt',
    tier: 'fault',
    rank: 100,
    icon: '⛔',           // no-entry
    label: 'HALTED — CHECKER DISAGREEMENT',
    cause: 'Two independent checkers disagreed and soundness is in question.',
  }),
  system_feedback_halt: faultRow({
    id: 'system_feedback_halt',
    tier: 'fault',
    rank: 90,
    icon: '⚠',           // warning
    label: 'HALTED — SYSTEM FEEDBACK',
    cause: 'An agent emitted system feedback the kernel was configured to halt on.',
  }),
  malformed_halt_marker: faultRow({
    id: 'malformed_halt_marker',
    tier: 'fault',
    rank: 110,
    icon: '⛔',
    label: 'HALTED — MALFORMED HALT MARKER',
    cause: 'A halt marker is on disk but could not be parsed, so the halt reason is unknown. Treat as the most severe until read by hand.',
  }),
  need_input: {
    id: 'need_input',
    tier: 'failure',
    rank: 80,
    icon: '✖',           // heavy multiplication x
    label: 'PROCESS FAILURE — NEED INPUT',
    summary: 'A lane could not make progress, so the kernel parked the run on the fail-loud NeedInput gate rather than spin. This is a last resort, not a checkpoint: diagnose why the process failed before answering.',
  },
  assumption_review: {
    id: 'assumption_review',
    tier: 'gate',
    rank: 40,
    icon: '⚖',           // scales
    label: 'ASSUMPTION-REVIEW GATE',
    summary: 'Routine checkpoint. Pending under-model assumptions are waiting to be ratified into the disclosed TCB.',
  },
  protected_reapproval: {
    id: 'protected_reapproval',
    tier: 'gate',
    rank: 30,
    icon: '✎',           // pencil
    label: 'RE-APPROVAL GATE',
    summary: 'Routine checkpoint. Protected statements were reopened and need re-approval before the run continues.',
  },
  // --- pause tier -----------------------------------------------------
  // A pause is not a fault and not a gate. Nothing is broken (so it must
  // not wear fault styling, which is reserved for halts and must stay
  // unimpersonable), but the run is DOWN and will not come back without a
  // human (so the calm gate treatment would understate it). Its own tier,
  // rendered as a filled AMBER bar, is the only honest answer.
  supervisor_down_unexplained: {
    id: 'supervisor_down_unexplained',
    tier: 'failure',
    rank: 85,
    icon: '☠',
    label: 'SUPERVISOR DOWN — NO RECORDED REASON',
    summary: 'The supervisor process is gone and nothing on disk says why. A run parked at a gate polls forever with no timeout, so it does not stop on its own: an unexplained exit means it was killed from outside (OOM, tmux teardown, a cross-run session sweep). Check the run session and the supervisor log before restarting.',
  },
  quota_pause: {
    id: 'quota_pause',
    tier: 'pause',
    rank: 70,
    icon: '⏸',
    label: 'PAUSED — WEEKLY BUDGET FLOOR REACHED',
    summary: 'The weekly provider budget fell to the configured floor, so the run stopped itself cleanly at a checkpoint. Nothing is wrong and no work was lost. It will not restart on its own — resume once the budget resets.',
  },
  gate_parked: {
    id: 'gate_parked',
    tier: 'pause',
    rank: 65,
    icon: '⏸',
    label: 'PARKED — GATE LEFT OPEN',
    summary: 'A human gate went unanswered long enough that the run parked itself rather than keep a process waiting. Nothing is wrong and no work was lost: the kernel polls a gate forever, so an idle wait only accumulates exposure to being killed from outside. Resume, and the same gate re-opens for you to answer.',
  },
  operator_pause: {
    id: 'operator_pause',
    tier: 'pause',
    rank: 60,
    icon: '⏸',
    label: 'PAUSED — BY OPERATOR',
    summary: 'The run was asked to stop and stopped cleanly at a checkpoint. Nothing is wrong and no work was lost.',
  },
  pause_arming: {
    id: 'pause_arming',
    tier: 'gate',
    rank: 25,
    icon: '⏳',
    label: 'PAUSE ARMED — STOPPING AT NEXT CHECKPOINT',
    summary: 'A pause is armed. The run is still working and will stop cleanly when it reaches the next checkpoint boundary. Disarm to cancel.',
  },
  advance: {
    id: 'advance',
    tier: 'gate',
    rank: 20,
    icon: '✓',           // check
    label: 'ADVANCE GATE',
    summary: 'Routine checkpoint. The run reached a phase boundary and is waiting for approval to advance. Nothing is wrong.',
  },
  unclassified_gate: {
    id: 'unclassified_gate',
    tier: 'gate',
    rank: 10,
    icon: '❓',           // question
    label: 'HUMAN GATE',
    summary: 'The run is waiting for a human, but this runtime does not report which gate kind is open. Check the reviewer decision and the supervisor log before answering.',
  },
};

// The ONLY rows a gate kind may resolve to, and the only rows a halt marker
// kind may resolve to. Two separate tables, not one lookup into
// ATTENTION_STATES, because both keys are UNTRUSTED: `gate_kind` is read out
// of `protocol_state.json` as raw JSON and never passes through the kernel's
// serde, so a corrupt / hand-edited / future value reaches the lookup
// verbatim. With one shared table a `gate_kind` of `checker_disagreement_halt`
// resolves to the soundness-halt row — a gate wearing the loudest fault
// heading — which is exactly the confusion this whole change exists to
// prevent. `GATE_ROWS` contains no fault-tier row, so that is now impossible
// by construction rather than by convention.
const GATE_ROWS = {
  advance: ATTENTION_STATES.advance,
  need_input: ATTENTION_STATES.need_input,
  protected_reapproval: ATTENTION_STATES.protected_reapproval,
  assumption_review: ATTENTION_STATES.assumption_review,
};
const HALT_ROWS = {
  checker_disagreement: ATTENTION_STATES.checker_disagreement_halt,
  system_feedback: ATTENTION_STATES.system_feedback_halt,
};

// `hasOwnProperty`, not `table[key] || fallback`: every object inherits
// truthy `constructor` / `toString` / `valueOf` / `hasOwnProperty`, so a
// `gate_kind` of `constructor` would satisfy `||` and yield a row whose
// tier/id/label/rank are all undefined — an unstyled, unlabelled, green-
// approve panel, and a NaN in the rank comparator.
function lookupRow(table, key, fallback) {
  return Object.prototype.hasOwnProperty.call(table, key) ? table[key] : fallback;
}

// Gate row for a viewer-state `state` object, or null when no gate is open.
//
// `state.gate.kind` (kernel `GateKind`, projected by `viewer_adapter.py`) is
// authoritative. A runtime that predates that projection reports no kind at
// all: fall back to the pre-existing reviewer-decision heuristic, which can
// still recognize `need_input`, and otherwise say so via `unclassified_gate`
// instead of silently rendering the routine treatment.
function gateAttention(state) {
  if (!state || typeof state !== 'object') return null;
  const gate = (state.gate && typeof state.gate === 'object') ? state.gate : {};
  const kind = String(gate.kind || '');
  const awaiting = Boolean(state.awaiting_human_input || state.human_input_outstanding);
  if (kind && kind !== 'none') {
    return gateRow(lookupRow(GATE_ROWS, kind, ATTENTION_STATES.unclassified_gate), {
      gate_kind: kind,
      degraded: false,
      from_invalid_attempt: Boolean(gate.from_invalid_attempt),
      reason: gate.reason,
      reason_source: gate.reason_source,
      question: gate.question,
      unblocking_input: gate.unblocking_input,
      ruled_out: gate.ruled_out,
      escalated_at_cycle: gate.escalated_at_cycle,
    });
  }
  if (kind === 'none') return null;
  if (!awaiting) return null;
  // Degraded path: pre-`gate` runtime.
  const decision = normalizeDecisionToken((state.last_review || {}).decision);
  const base = decision === 'need_input'
    ? ATTENTION_STATES.need_input
    : (decision === 'advance_phase' ? ATTENTION_STATES.advance : ATTENTION_STATES.unclassified_gate);
  return gateRow(base, {
    gate_kind: '',
    degraded: true,
    from_invalid_attempt: false,
    reason: (state.last_review || {}).reason,
    reason_source: decision ? 'last_review.decision (runtime predates state.gate)' : '',
  });
}

// Assemble one gate row. THE escalation-diagnosis rule lives here and nowhere
// else: reason, its source, the GapResearch payload and the inspect pointers
// belong to the failure tier ONLY.
//
// `viewer_adapter.py` already refuses to populate those fields for a non-
// `need_input` gate, but that guard is one process away and one version away
// (a viewer can serve a runtime whose adapter predates it, or a hand-edited
// protocol state). Enforcing it again at assembly is what makes "a routine
// checkpoint never renders as an incident" a property of the row rather than
// a property of whoever filled it in.
function gateRow(base, fields) {
  const failure = base.tier === 'failure';
  const reason = failure ? String(fields.reason || '') : '';
  const reasonSource = failure ? String(fields.reason_source || '') : '';
  return {
    ...base,
    gate_kind: fields.gate_kind,
    degraded: fields.degraded,
    from_invalid_attempt: fields.from_invalid_attempt,
    reason,
    reason_source: reason ? reasonSource : '',
    question: failure ? String(fields.question || '') : '',
    unblocking_input: failure ? String(fields.unblocking_input || '') : '',
    ruled_out: (failure && Array.isArray(fields.ruled_out)) ? fields.ruled_out.map(String) : [],
    escalated_at_cycle: (failure && typeof fields.escalated_at_cycle === 'number')
      ? fields.escalated_at_cycle
      : null,
    // On the degraded path `reason_source` names a bridge artifact, not a
    // protocol-state key, so it must not be rendered as one.
    inspect: failure ? inspectPointersFor(fields.degraded ? '' : reasonSource) : [],
  };
}

// "What to inspect" pointers. A process failure must never be a bare
// "reviewer wants input" — the operator gets the files that explain it.
function inspectPointersFor(reasonSource) {
  const pointers = [];
  if (reasonSource) pointers.push(`<runtime>/protocol_state.json → ${reasonSource}`);
  pointers.push('<runtime>/protocol_state.json → stuck_math_audit (the audit that adjudicated the escalation)');
  pointers.push('Chats tab → the StuckMathAudit burst for this cycle');
  pointers.push('<runtime>/run-capture.log (supervisor log around the escalation cycle)');
  return pointers;
}

function normalizeDecisionToken(value) {
  return String(value || '')
    .trim()
    .replace(/([a-z0-9])([A-Z])/g, '$1_$2')
    .replace(/[\s-]+/g, '_')
    .replace(/_+/g, '_')
    .toLowerCase();
}

// Supervisor liveness, taken from the pause surface's own status so the halt
// banner and the pause banner answer "is the process still there?" from one
// measurement. `known` is false when the status could not be read, which
// leaves the parked wording in place rather than asserting an exit nobody
// observed.
function supervisorContextFromStatus(status) {
  const state = String((status && status.state) || '');
  const known = ['running', 'arming', 'paused', 'down'].includes(state);
  return {
    state: known ? state : 'unknown',
    known,
    up: state === 'running' || state === 'arming',
    wrapper_pid: (status && status.wrapper_pid) || null,
    resumable: Boolean(status && status.resumable),
    launch_env: (status && status.launch_env) || null,
  };
}

// Which halts the viewer's resume control may lift on its own.
//
// A system-feedback halt is an agent emission the kernel was configured to
// stop on, and the operator reads it off the banner. A checker disagreement
// is two independent checkers reaching different answers about one node, so
// soundness is the open question and the triage happens at the marker. A
// marker that will not parse holds a reason nobody has read, which carries
// the same weight. Either of those on disk withholds the control from the
// whole run rather than from its own row, because resuming resumes past
// every marker present and the strictest one governs.
const VIEWER_LIFTABLE_HALT_KIND = 'system_feedback';
const HALT_RESUME_WITHHELD =
  'Reading this halt is a hand step, so the resume control is withheld.';
function haltMarkersAreViewerLiftable(markers) {
  const list = Array.isArray(markers) ? markers : [];
  return list.length > 0 && list.every(entry =>
    entry && !entry.parse_error && entry.marker_kind === VIEWER_LIFTABLE_HALT_KIND);
}

// Fault-tier rows for a `haltStateForRuntimeRoot` payload. One row per marker
// found, so a second halt is never hidden behind the primary one, SORTED by
// rank: the caller renders them in array order, and marker discovery order is
// the fixed filename scan, not severity (a malformed system-feedback marker
// outranks a well-formed checker disagreement and must render above it).
//
// `supervisor` is a `supervisorContextFromStatus` value. A marker records why
// the kernel stopped dispatching and nothing about whether the process
// survived, so both the closing sentence and the remediation come from that
// context: once the process is gone, lifting the marker resumes nothing and
// the run has to be relaunched. The row carries the resume control for that
// relaunch, which is what keeps an explained exit to a single dialog.
function haltAttention(haltState, supervisor) {
  if (!haltState || !haltState.halted) return [];
  const markers = Array.isArray(haltState.markers) ? haltState.markers : [];
  const exited = Boolean(supervisor && supervisor.known && !supervisor.up);
  const liftable = haltMarkersAreViewerLiftable(markers);
  const resumable = Boolean(exited && supervisor.resumable && liftable);
  const remediation = resumable
    ? 'diagnostics; the resume control lifts this marker'
    : 'diagnostics; rename the marker to a .lifted-<unix_ts>.json sibling to lift the halt';
  // Said once, on the row that causes it, and only where a control would
  // otherwise have appeared: a run that was never resumable from here gains
  // no line explaining an absence.
  const withholding = exited && supervisor.resumable && !liftable;
  return markers.map(entry => {
    const marker = (entry && entry.marker && typeof entry.marker === 'object') ? entry.marker : {};
    const base = entry && entry.parse_error
      ? ATTENTION_STATES.malformed_halt_marker
      : lookupRow(HALT_ROWS, String((entry && entry.marker_kind) || ''), ATTENTION_STATES.malformed_halt_marker);
    return {
      ...base,
      summary: exited ? `${base.cause} ${HALT_LIVENESS_EXITED}` : base.summary,
      supervisor_state: supervisor ? supervisor.state : '',
      // Resume fields belong to an exited supervisor. Carrying them while the
      // process is up would offer a relaunch for a run that is still there.
      resumable,
      liftable,
      resume_withheld: (withholding && !haltMarkersAreViewerLiftable([entry])) ? HALT_RESUME_WITHHELD : '',
      launch_env: exited ? (supervisor.launch_env || null) : null,
      marker_kind: (entry && entry.marker_kind) || '',
      marker_path: (entry && entry.marker_path) || '',
      parse_error: (entry && entry.parse_error) || '',
      node: marker.active_node ? String(marker.active_node) : '',
      cycle: (marker.cycle === undefined || marker.cycle === null) ? '' : String(marker.cycle),
      burst_role: marker.burst_role ? String(marker.burst_role) : '',
      inspect: [`${(entry && entry.marker_path) || '<runtime>/halt-marker.json'} (${remediation})`],
    };
  }).sort((a, b) => b.rank - a.rank);
}

function augmentViewerStateAttention(payload) {
  if (!payload || typeof payload !== 'object') return payload;
  payload.attention_gate = gateAttention(payload.state);
  return payload;
}

// Halt-state surface for every kernel fail-loudly halt.
// Returns `{ halted: false }` when the marker is absent, else
// `{ halted: true, marker_path, marker }` with the parsed JSON body so
// the frontend can render a banner WITHOUT a second fetch.
//
// Deterministic precedence: a malformed marker is primary (so corruption can
// never be hidden by another valid marker); otherwise checker disagreement is
// primary, then system feedback. `markers` always exposes every marker found.
function haltStateForRuntimeRoot(runtimeRoot, supervisor) {
  if (!runtimeRoot) return { halted: false, reason: 'no_runtime_root' };
  const specs = [
    { kind: 'checker_disagreement', filename: 'checker_disagreement_halt.json' },
    { kind: 'system_feedback', filename: 'system_feedback_halt.json' },
  ];
  const markers = [];
  for (const spec of specs) {
    const markerPath = path.join(runtimeRoot, spec.filename);
    if (!fs.existsSync(markerPath)) continue;
    try {
      markers.push({
        marker_kind: spec.kind,
        marker_path: markerPath,
        marker: JSON.parse(fs.readFileSync(markerPath, 'utf-8')),
      });
    } catch (e) {
      markers.push({
        marker_kind: spec.kind,
        marker_path: markerPath,
        parse_error: e.message,
      });
    }
  }
  if (!markers.length) return { halted: false };
  const primary = markers.find(entry => entry.parse_error) || markers[0];
  const state = { halted: true, ...primary, markers };
  // Fault-tier attention rows, so the banner styling comes from the same
  // severity ladder the gate banner uses instead of a second hand-rolled one.
  state.attention = haltAttention(state, supervisor);
  return state;
}

function kernelHaltState(projectInfo) {
  const runtimeRoot = runtimeRootForProject(projectInfo);
  const state = haltStateForRuntimeRoot(runtimeRoot);
  if (!state.halted) return state;
  // Liveness costs a process spawn, so it is measured for a run that has a
  // marker to explain and skipped for the polling majority that do not.
  return haltStateForRuntimeRoot(
    runtimeRoot,
    supervisorContextFromStatus(pauseStatusCached(projectInfo)),
  );
}
app.get(`${BASE}/api/halt-state.json`, (_req, res) => {
  try { res.json(kernelHaltState(resolveRepoPath(defaultProjectSlug()))); }
  catch (e) { res.status(500).json({ error: e.message }); }
});
app.get(`${BASE}/:project/api/halt-state.json`, (req, res) => {
  try { res.json(kernelHaltState(resolveRepoPath(projectFromRequest(req)))); }
  catch (e) { res.status(500).json({ error: e.message }); }
});

// =====================================================================
// PAUSE / RESUME
//
// One durable, reason-carrying stopped-state, shared by every way a run
// can be deliberately stopped. `scripts/trellis_pause.sh` owns the logic
// and the file formats; this file shells out to it rather than
// reimplementing the state machine, so the operator running the script by
// hand and the viewer clicking a button cannot drift apart.
//
// Reading is cached for a few seconds because the banner polls, and each
// status call costs a bash + python3 spawn.
// =====================================================================

const PAUSE_SCRIPT = path.join(TRELLIS_ROOT, 'scripts', 'trellis_pause.sh');
// `null` is "never". Anything else is a percent-of-weekly-budget floor.
const PAUSE_THRESHOLD_CHOICES = [null, 5, 10, 25];
const PAUSE_DEFAULT_THRESHOLD = 5;
const pauseStatusCache = new Map();
const PAUSE_STATUS_TTL_MS = 3000;

function pauseConfigPath(projectInfo) {
  return path.join(projectInfo.stateDir, 'pause_config.json');
}
function readPauseConfig(projectInfo) {
  const fallback = {
    budget_threshold_pct: PAUSE_DEFAULT_THRESHOLD,
    gate_park_after_minutes: GATE_PARK_DEFAULT_MINUTES,
  };
  try {
    const d = JSON.parse(fs.readFileSync(pauseConfigPath(projectInfo), 'utf-8'));
    const raw = d.budget_threshold_pct;
    // An unrecognized value must not silently become "never" — that would
    // turn a corrupt config into a disabled safety floor. Fall back to the
    // default instead, which is the conservative direction.
    const ok = raw === null || PAUSE_THRESHOLD_CHOICES.includes(raw);
    const park = d.gate_park_after_minutes;
    const parkOk = park === null || (Number.isFinite(Number(park)) && Number(park) > 0);
    return {
      budget_threshold_pct: ok ? raw : PAUSE_DEFAULT_THRESHOLD,
      gate_park_after_minutes: parkOk ? (park === null ? null : Number(park)) : GATE_PARK_DEFAULT_MINUTES,
    };
  } catch {
    return fallback;
  }
}
function writePauseConfig(projectInfo, patch) {
  const p = pauseConfigPath(projectInfo);
  fs.mkdirSync(path.dirname(p), { recursive: true });
  // Merge, so writing one knob from the UI cannot silently reset the other
  // to its default.
  const next = { ...readPauseConfig(projectInfo), ...patch };
  const tmp = p + '.tmp';
  fs.writeFileSync(tmp, JSON.stringify(next, null, 2));
  fs.renameSync(tmp, p);
}

// `flags` follow the positionals, which is where the script's option parser
// looks for them: it takes `<action> <runtime_root> <repo_path>` first.
function runPauseScript(projectInfo, args, timeoutMs, flags) {
  const runtimeRoot = runtimeRootForProject(projectInfo);
  if (!runtimeRoot) throw new Error('no runtime root for project');
  return execFileSync('bash', [PAUSE_SCRIPT, ...args, runtimeRoot, projectInfo.repoPath, ...(flags || [])], {
    cwd: TRELLIS_ROOT, encoding: 'utf-8',
    timeout: timeoutMs || 15000, maxBuffer: 4 * 1024 * 1024,
  });
}

function pauseStatusRaw(projectInfo) {
  try {
    return JSON.parse(runPauseScript(projectInfo, ['status']));
  } catch (e) {
    return { state: 'unknown', error: e.message, request: null, resumable: false };
  }
}
function pauseStatusCached(projectInfo) {
  const key = projectInfo.slug;
  const hit = pauseStatusCache.get(key);
  const now = Date.now();
  if (hit && now - hit.at < PAUSE_STATUS_TTL_MS) return hit.value;
  const value = pauseStatusRaw(projectInfo);
  pauseStatusCache.set(key, { at: now, value });
  return value;
}
function invalidatePauseStatus(projectInfo) {
  pauseStatusCache.delete(projectInfo.slug);
}

// Is this run finished? A Complete run is *supposed* to have no supervisor,
// so it must not raise the unexplained-exit alarm.
function projectPhaseIsComplete(projectInfo) {
  try {
    const v = readLiveViewerStateCached(projectInfo);
    const phase = String(v?.state?.phase || v?.phase || '');
    return /complete/i.test(phase);
  } catch { return false; }
}

// The attention row for a pause, on the same severity ladder as gates and
// halts so the frontend styles it from one place.
//
// `haltState` is the halt-marker surface for the same run, passed in by the
// caller or read here. It decides one thing: which banner owns a stopped run.
// A marker on disk names the cycle, the request and the reason, and it is the
// fault tier of the same severity ladder this row sits on, so it outranks
// both the clean-checkpoint reading of a pause and the alarm of an
// unexplained exit. The halt banner renders it with the resume control, and
// this row stands down so the run gets one dialog.
//
// `supervisor_down_unexplained` stays for the exit with nothing on disk
// behind it — the kill from outside, which is the case worth alarming about.
//
// A supervisor that is still up is a different question and keeps its own
// row: an armed pause offers the disarm, which resolves the pause without
// touching the halt.
function pauseAttention(projectInfo, status, haltState) {
  if (!status) return null;
  const request = status.request || {};
  const kind = String(request.kind || '');
  if (status.state === 'arming') {
    return { ...ATTENTION_STATES.pause_arming, reason: String(request.reason || ''), armed_by: request.armed_by || '' };
  }
  if (status.state === 'paused' || status.state === 'down') {
    const halt = haltState === undefined ? haltStateForProject(projectInfo) : haltState;
    if (halt && halt.halted) return null;
  }
  if (status.state === 'paused') {
    const base = kind === 'quota_budget' ? ATTENTION_STATES.quota_pause
      : kind === 'gate_parked' ? ATTENTION_STATES.gate_parked
      : ATTENTION_STATES.operator_pause;
    return {
      ...base,
      reason: String(request.reason || ''),
      armed_by: request.armed_by || '',
      armed_at: request.armed_at || '',
      armed_at_cycle: request.armed_at_cycle ?? null,
      detail: request.detail || null,
      resumable: Boolean(status.resumable),
      launch_env: status.launch_env || null,
    };
  }
  if (status.state === 'down') {
    if (projectPhaseIsComplete(projectInfo)) return null;
    return {
      ...ATTENTION_STATES.supervisor_down_unexplained,
      resumable: Boolean(status.resumable),
      launch_env: status.launch_env || null,
    };
  }
  return null;
}

// Marker-only halt read, with no liveness context: the caller is deciding
// whether an exit is explained, and existence is the whole question.
function haltStateForProject(projectInfo) {
  try { return haltStateForRuntimeRoot(runtimeRootForProject(projectInfo)); }
  catch { return { halted: false }; }
}

// Weekly budget per provider, trimmed to what a banner needs. Carried on the
// pause payload so a paused run's banner can show the budget recovering (and
// when it resets) from the poll it is already making.
function pauseBudgetSummary(projectInfo) {
  const out = {};
  try {
    for (const [provider, snap] of Object.entries(latestQuotaSnapshotsForProject(projectInfo) || {})) {
      if (snap && snap.weekly_budget) out[provider] = snap.weekly_budget;
    }
  } catch { /* budget is decoration here; never fail the pause payload for it */ }
  return out;
}

function pauseStateResponse(projectInfo) {
  const status = pauseStatusCached(projectInfo);
  return {
    ...status,
    project: projectInfo.slug,
    config: readPauseConfig(projectInfo),
    threshold_choices: PAUSE_THRESHOLD_CHOICES,
    budget: pauseBudgetSummary(projectInfo),
    attention: pauseAttention(projectInfo, status),
  };
}

// Arm a pause when the weekly budget has fallen to the configured floor.
//
// Only fires against a RUNNING supervisor: arming an already-armed or
// already-stopped run would rewrite the pause record and lose the original
// reason. Returns the arming decision for logging.
function evaluateBudgetPause(projectInfo) {
  const cfg = readPauseConfig(projectInfo);
  const threshold = cfg.budget_threshold_pct;
  if (threshold === null || threshold === undefined) return { armed: false, reason: 'never' };
  let quota;
  try { quota = latestQuotaSnapshotsForProject(projectInfo); }
  catch { return { armed: false, reason: 'no_quota' }; }
  for (const [provider, snap] of Object.entries(quota || {})) {
    const wb = snap && snap.weekly_budget;
    if (!wb || !Number.isFinite(Number(wb.pct_left))) continue;
    const left = Number(wb.pct_left);
    if (left > threshold) continue;
    const status = pauseStatusCached(projectInfo);
    if (status.state !== 'running') return { armed: false, reason: `state_${status.state}` };
    const reason = `weekly ${provider} budget at ${left.toFixed(1)}% left, at or below the ${threshold}% floor`;
    try {
      runPauseScript(projectInfo, ['arm', '--kind', 'quota_budget', '--by', 'viewer:budget_floor', '--reason', reason]);
      invalidatePauseStatus(projectInfo);
      console.log(`[pause] armed budget floor for ${projectInfo.slug}: ${reason}`);
      return { armed: true, provider, pct_left: left, threshold };
    } catch (e) {
      console.error(`[pause] failed to arm budget floor for ${projectInfo.slug}: ${e.message}`);
      return { armed: false, reason: 'arm_failed', error: e.message };
    }
  }
  return { armed: false, reason: 'above_threshold' };
}

// --- gate parking -----------------------------------------------------
//
// A run parked on a HumanGate polls once a second FOREVER — there is no
// timeout anywhere in the kernel, the bridge, or trellis.sh. So a gate left
// open overnight never resolves itself; it just accumulates exposure to
// everything that can kill a process (OOM, tmux teardown, a cross-run
// session sweep). When one of those lands, the operator finds a dead run
// and no explanation, because an externally-killed supervisor writes
// nothing.
//
// Parking converts that fragile wait into a durable one: arm a pause, let
// the kernel stop cleanly at the top of its next poll iteration (the
// gate-poll branch `continue`s past the sentinel check, so this works
// while parked at a gate — see runtime_cli.rs), and hold the state on
// disk instead of in a process. Resume brings the run back and the gate
// re-opens, because the gate lives in protocol state.
//
// This removes the exposure rather than patching whichever cause is
// currently doing the killing.
const GATE_PARK_DEFAULT_MINUTES = 120;
// Bound the scan: `gate_kind` sits a couple of MB into a protocol state
// that can be 25 MB+, so read forward in chunks and stop at the match
// rather than parsing the whole document once a minute.
const GATE_SCAN_MAX_BYTES = 12 * 1024 * 1024;
const GATE_SCAN_CHUNK = 512 * 1024;

function readGateKindCheap(runtimeRoot) {
  const statePath = path.join(runtimeRoot, 'protocol_state.json');
  let fd;
  try { fd = fs.openSync(statePath, 'r'); } catch { return null; }
  try {
    const buf = Buffer.alloc(GATE_SCAN_CHUNK);
    // Carry a small tail between chunks so a match straddling a boundary
    // is not missed.
    let carry = '';
    let read = 0;
    while (read < GATE_SCAN_MAX_BYTES) {
      const n = fs.readSync(fd, buf, 0, GATE_SCAN_CHUNK, read);
      if (n <= 0) break;
      read += n;
      const text = carry + buf.toString('utf-8', 0, n);
      const m = text.match(/"gate_kind"\s*:\s*"([^"]*)"/);
      if (m) return m[1];
      carry = text.slice(-64);
    }
    return null;
  } catch {
    return null;
  } finally {
    try { fs.closeSync(fd); } catch {}
  }
}

function gateSeenPath(runtimeRoot) {
  return path.join(runtimeRoot, 'gate_seen.json');
}

// How long the current gate has been open, in minutes, or null when no gate
// is open. First observation is stamped to disk so the clock survives a
// viewer restart — otherwise restarting the viewer would silently reset
// every gate's age and the park would never fire.
function gateOpenMinutes(runtimeRoot, gateKind) {
  const seenPath = gateSeenPath(runtimeRoot);
  if (!gateKind || gateKind === 'None' || gateKind === 'none') {
    try { fs.unlinkSync(seenPath); } catch {}
    return null;
  }
  let seen = null;
  try { seen = JSON.parse(fs.readFileSync(seenPath, 'utf-8')); } catch {}
  const now = Math.floor(Date.now() / 1000);
  if (!seen || seen.gate_kind !== gateKind || !Number.isFinite(Number(seen.first_seen_epoch))) {
    seen = { gate_kind: gateKind, first_seen_epoch: now };
    try {
      const tmp = seenPath + '.tmp';
      fs.writeFileSync(tmp, JSON.stringify(seen, null, 2));
      fs.renameSync(tmp, seenPath);
    } catch {}
    return 0;
  }
  return (now - Number(seen.first_seen_epoch)) / 60;
}

function evaluateGatePark(projectInfo) {
  const cfg = readPauseConfig(projectInfo);
  const after = cfg.gate_park_after_minutes;
  if (after === null || after === undefined) return { armed: false, reason: 'never' };
  const runtimeRoot = runtimeRootForProject(projectInfo);
  if (!runtimeRoot) return { armed: false, reason: 'no_runtime_root' };
  // A gate implies a live supervisor; skip the state scan entirely for
  // every run that is not currently up.
  const status = pauseStatusCached(projectInfo);
  if (status.state !== 'running') return { armed: false, reason: `state_${status.state}` };
  const gateKind = readGateKindCheap(runtimeRoot);
  const openFor = gateOpenMinutes(runtimeRoot, gateKind);
  if (openFor === null) return { armed: false, reason: 'no_gate' };
  if (openFor < after) return { armed: false, reason: 'not_yet', open_minutes: openFor };
  const reason = `${gateKind} gate open ${Math.round(openFor)} min (over the ${after} min park threshold); ` +
    'parking durably so the wait cannot be lost to an external kill';
  try {
    runPauseScript(projectInfo, ['arm', '--kind', 'gate_parked', '--by', 'viewer:gate_park', '--reason', reason]);
    invalidatePauseStatus(projectInfo);
    console.log(`[pause] parked gate for ${projectInfo.slug}: ${reason}`);
    return { armed: true, gate_kind: gateKind, open_minutes: openFor };
  } catch (e) {
    console.error(`[pause] failed to park gate for ${projectInfo.slug}: ${e.message}`);
    return { armed: false, reason: 'arm_failed', error: e.message };
  }
}

// The floor has to be enforced whether or not anyone is looking at the
// viewer, so it runs on a timer rather than off a request.
//
// Every project is swept, including those that have never written a config.
// The alternative — only sweeping projects with a config file — would make
// the control lie: `readPauseConfig` reports the 5% default for a project
// with no file, so the Usage tab would show a floor of 5% on a run that had
// no floor at all. A displayed guarantee the mechanism does not honour is
// worse than no guarantee.
//
// This stays cheap because `evaluateBudgetPause` reads the quota JSONL first
// and returns before touching process state unless the floor is actually
// breached. Idle and finished runs cost one small file read per minute.
const BUDGET_PAUSE_POLL_MS = 60 * 1000;
function sweepBudgetPauses() {
  let projects = [];
  try { projects = discoverProjects(); } catch { return; }
  for (const project of projects) {
    let info;
    try { info = resolveRepoPath(project.slug); } catch { continue; }
    try { evaluateBudgetPause(info); }
    catch (e) { console.error(`[pause] budget sweep failed for ${project.slug}: ${e.message}`); }
    try { evaluateGatePark(info); }
    catch (e) { console.error(`[pause] gate-park sweep failed for ${project.slug}: ${e.message}`); }
  }
}
setInterval(sweepBudgetPauses, BUDGET_PAUSE_POLL_MS).unref();

app.get(`${BASE}/api/pause-state.json`, (_req, res) => {
  try { res.json(pauseStateResponse(resolveRepoPath(defaultProjectSlug()))); }
  catch (e) { res.status(500).json({ error: e.message }); }
});
app.get(`${BASE}/:project/api/pause-state.json`, (req, res) => {
  try { res.json(pauseStateResponse(resolveRepoPath(projectFromRequest(req)))); }
  catch (e) { res.status(500).json({ error: e.message }); }
});

// ---------------------------------------------------------------------------
// Update check: is a newer Trellis available at the public repo?
//
// Both sides of the comparison come from the same artifact — the first
// `## vX.Y.Z` heading of the changelog — because that is the one version
// marker that exists in every install shape: a public clone has CHANGELOG.md
// on any fetch (tags don't survive a tarball download), and a dev checkout
// has CHANGELOG.public.md. The remote read goes through raw.githubusercontent
// rather than the GitHub API to stay outside API rate limits.
//
// The check is deliberately quiet plumbing: it never throws, never blocks a
// request (state is refreshed on a timer and served from memory), logs only
// on the not-available -> available transition, and keeps the last good
// answer across transient network failures so an offline viewer does not
// flap. TRELLIS_VIEWER_NO_UPDATE_CHECK=1 disables the outbound request
// entirely for hosts that must not phone home.
const UPDATE_CHECK_URL = 'https://raw.githubusercontent.com/wpegden/trellis/main/CHANGELOG.md';
const UPDATE_CHECK_INTERVAL_MS = 6 * 60 * 60 * 1000;
const updateCheckState = {
  disabled: process.env.TRELLIS_VIEWER_NO_UPDATE_CHECK === '1',
  local_kind: null,
  local_version: null,
  local_code_date: null,
  local_label: null,
  latest_version: null,
  latest_date: null,
  update_available: false,
  checked_at: null,
  error: null,
};

// First release heading of a changelog: `## vX.Y.Z — YYYY-MM-DD` (the date
// is optional — v0.1.0's heading has none).
function parseChangelogRelease(text) {
  const m = /^##\s+(v\d+\.\d+\.\d+)\b([^\n]*)/m.exec(String(text || ''));
  if (!m) return null;
  const d = /(\d{4}-\d{2}-\d{2})/.exec(m[2] || '');
  return { version: m[1], date: d ? d[1] : null };
}

// True when `a` names a strictly newer vX.Y.Z than `b`. Anything unparseable
// compares as not-newer: a malformed remote heading must never raise a
// banner.
function versionIsNewer(a, b) {
  const parse = (v) => {
    const m = /^v(\d+)\.(\d+)\.(\d+)$/.exec(String(v || ''));
    return m ? [Number(m[1]), Number(m[2]), Number(m[3])] : null;
  };
  const ta = parse(a); const tb = parse(b);
  if (!ta || !tb) return false;
  for (let i = 0; i < 3; i++) {
    if (ta[i] !== tb[i]) return ta[i] > tb[i];
  }
  return false;
}

// What is this install? Two shapes exist:
//   * public release — ships CHANGELOG.md, whose first heading IS the
//     running version;
//   * dev checkout (trellis-dev) — carries CHANGELOG.public.md instead,
//     whose first heading names the NEXT release, not the running code, so
//     the version is useless for comparison. What is meaningful there is the
//     age of the code: git's HEAD commit date.
function localTrellisInstall() {
  const root = path.join(__dirname, '..');
  try {
    const rel = parseChangelogRelease(
      fs.readFileSync(path.join(root, 'CHANGELOG.md'), 'utf-8'));
    if (rel) return { kind: 'public', version: rel.version, code_date: rel.date };
  } catch { /* not a public install */ }
  let version = null;
  try {
    const rel = parseChangelogRelease(
      fs.readFileSync(path.join(root, 'CHANGELOG.public.md'), 'utf-8'));
    if (rel) version = rel.version;
  } catch { /* fine — git date below can carry the comparison alone */ }
  let codeDate = null;
  try {
    codeDate = execFileSync('git', ['log', '-1', '--format=%cs'], {
      cwd: root, stdio: ['ignore', 'pipe', 'ignore'], timeout: 5000,
    }).toString().trim() || null;
  } catch { /* tarball without git: no date, dev comparison stays quiet */ }
  if (version || codeDate) return { kind: 'dev', version, code_date: codeDate };
  return null;
}

// The decision, per install shape. Public installs compare versions exactly.
// Dev checkouts compare BY DATE: the banner means "a public release postdates
// the code you are running". Strictly-after, so the machine that cut today's
// release does not spend the day telling its operator about it; and any
// missing side of the comparison stays quiet — a banner must never be raised
// on a guess.
function computeUpdateAvailable(local, remote) {
  if (!local || !remote) return false;
  if (local.kind === 'public') return versionIsNewer(remote.version, local.version);
  if (!remote.date || !local.code_date) return false;
  return remote.date > local.code_date; // ISO dates compare lexicographically
}

function localInstallLabel(local) {
  if (!local) return null;
  if (local.kind === 'public') return local.version;
  return `dev checkout of ${local.code_date || 'unknown date'}`;
}

function noteLocalInstall() {
  const local = localTrellisInstall();
  updateCheckState.local_kind = local ? local.kind : null;
  updateCheckState.local_version = local ? local.version : null;
  updateCheckState.local_code_date = local ? local.code_date : null;
  updateCheckState.local_label = localInstallLabel(local);
  return local;
}

async function refreshUpdateCheck() {
  if (updateCheckState.disabled) return;
  const local = noteLocalInstall();
  try {
    const res = await fetch(UPDATE_CHECK_URL, {
      signal: AbortSignal.timeout(10_000),
      headers: { 'user-agent': 'trellis-viewer-update-check' },
    });
    if (!res.ok) throw new Error(`HTTP ${res.status}`);
    const remote = parseChangelogRelease(await res.text());
    if (!remote) throw new Error('no version heading in remote changelog');
    const wasAvailable = updateCheckState.update_available;
    updateCheckState.latest_version = remote.version;
    updateCheckState.latest_date = remote.date;
    updateCheckState.update_available = computeUpdateAvailable(local, remote);
    updateCheckState.checked_at = new Date().toISOString();
    updateCheckState.error = null;
    if (updateCheckState.update_available && !wasAvailable) {
      console.log(`[update-check] Trellis ${remote.version} is available at the public repo (this install: ${updateCheckState.local_label || 'unknown'})`);
    }
  } catch (e) {
    // Keep the previous answer; record the error for the endpoint only.
    updateCheckState.error = String((e && e.message) || e);
  }
}
if (!updateCheckState.disabled) {
  setTimeout(refreshUpdateCheck, 15_000).unref();
  setInterval(refreshUpdateCheck, UPDATE_CHECK_INTERVAL_MS).unref();
}

function updateCheckResponse() {
  if (updateCheckState.local_kind === null) noteLocalInstall();
  return { ...updateCheckState };
}
app.get(`${BASE}/api/update-check.json`, (_req, res) => res.json(updateCheckResponse()));
app.get(`${BASE}/:project/api/update-check.json`, (_req, res) => res.json(updateCheckResponse()));

function handlePauseConfig(projectInfo, body) {
  const raw = body && Object.prototype.hasOwnProperty.call(body, 'budget_threshold_pct')
    ? body.budget_threshold_pct
    : undefined;
  const value = raw === null || raw === 'never' ? null : Number(raw);
  if (!(value === null || PAUSE_THRESHOLD_CHOICES.includes(value))) {
    throw new Error(`budget_threshold_pct must be one of: never, ${PAUSE_THRESHOLD_CHOICES.filter(Boolean).join(', ')}`);
  }
  writePauseConfig(projectInfo, { budget_threshold_pct: value });
  // Apply immediately: if the budget is already at or under the new floor,
  // the operator expects the run to stop now, not up to a minute from now.
  const decision = evaluateBudgetPause(projectInfo);
  return { ...pauseStateResponse(projectInfo), applied: decision };
}
registerControlRoute('pause/config', [express.json()], (req, res) => {
  try { res.json(handlePauseConfig(resolveRepoPath(projectFromRequest(req)), req.body)); }
  catch (e) { res.status(400).json({ error: e.message }); }
});

// A resume over a live halt marker relaunches a run the bridge refuses to
// dispatch for, which goes straight back down. Lifting the marker is
// `scripts/trellis_pause.sh resume --clear-halt`: the script owns the
// detection, the rename and the rollback, and this endpoint decides only
// whether to authorize it. Authorization is an explicit `clear_halt` from the
// caller — the operator answers for it with the halt in front of them, and
// every other caller of this endpoint gets a refusal naming what is on disk.
//
// The kind rule is re-stated here to answer at the HTTP layer rather than out
// of a subprocess exit; the script re-checks it and is authoritative.
function refusePauseAction(message) {
  const e = new Error(message);
  e.statusCode = 409;
  return e;
}
function liftHaltForResume(projectInfo, options) {
  const halt = haltStateForProject(projectInfo);
  if (!halt || !halt.halted) return [];
  const markers = halt.markers || [];
  if (!haltMarkersAreViewerLiftable(markers)) {
    const kinds = markers.map(entry => (entry && entry.marker_kind) || 'unreadable').join(', ');
    throw refusePauseAction(
      `${kinds} halt marker on disk. Read it, then lift it by hand: trellis_pause.sh resume --clear-halt-any-kind.`,
    );
  }
  if (!options || options.clear_halt !== true) {
    throw refusePauseAction('A halt marker is on disk. Resume from the halt banner, which lifts it.');
  }
  // The script's own preconditions, checked before it is called rather than
  // read out of its exit status: a run it will refuse to relaunch — one whose
  // supervisor is still up, one with no recorded launch env — gets the
  // refusal that names why, and its marker never moves.
  if (!pauseStatusCached(projectInfo).resumable) {
    throw refusePauseAction('This run cannot be relaunched from here, so its halt marker stays in place.');
  }
  return ['--clear-halt'];
}

function handlePauseAction(projectInfo, action, options) {
  const flags = action === 'resume' ? liftHaltForResume(projectInfo, options) : [];
  let out;
  try {
    // `resume` relaunches a supervisor and can take a few seconds to confirm
    // the wrapper came up; the script waits up to 12s before reporting back.
    out = runPauseScript(projectInfo, [action], action === 'resume' ? 45000 : 15000, flags);
  } finally {
    // An action that failed partway can still have moved the run, so the
    // cached status is stale either way.
    invalidatePauseStatus(projectInfo);
  }
  return { ok: true, output: out, ...pauseStateResponse(projectInfo) };
}
// `resolveProject` is deferred into the try so a bad slug answers with the
// same shape every other failure here does.
function respondPauseAction(res, resolveProject, action, body) {
  try { res.json(handlePauseAction(resolveProject(), action, body)); }
  catch (e) {
    // The script reports a refused or failed resume on stderr, including
    // whether it put a lifted marker back.
    res.status(e.statusCode || 500).json({ error: e.message, output: String(e.stderr || '') });
  }
}
for (const action of ['arm', 'disarm', 'resume']) {
  registerControlRoute(`pause/${action}`, [express.json()], (req, res) => {
    respondPauseAction(res, () => resolveRepoPath(projectFromRequest(req)), action, req.body);
  });
}

// Recent system-feedback surface: tail of the unified
// `<runtime>/system_feedback_log.jsonl` stream (every non-empty
// system_feedback emission — halted, acked, or log-and-continue). With
// halting now opt-in (config `system_feedback_halt`, default off), this
// is where log-and-continue emissions become visible to the operator.
function recentSystemFeedbackForRuntimeRoot(runtimeRoot, limit) {
  if (!runtimeRoot) return { entries: [], reason: 'no_runtime_root' };
  const logPath = path.join(runtimeRoot, 'system_feedback_log.jsonl');
  if (!fs.existsSync(logPath)) return { entries: [], log_path: logPath };
  const lines = fs.readFileSync(logPath, 'utf-8').split('\n').filter(Boolean);
  const entries = lines.slice(-Math.max(1, Math.min(limit || 20, 200))).map(line => {
    try { return JSON.parse(line); }
    catch (e) { return { parse_error: e.message, raw: line }; }
  });
  return { log_path: logPath, total: lines.length, entries };
}
function recentSystemFeedback(projectInfo, limit) {
  return recentSystemFeedbackForRuntimeRoot(runtimeRootForProject(projectInfo), limit);
}
app.get(`${BASE}/api/system-feedback.json`, (req, res) => {
  try { res.json(recentSystemFeedback(resolveRepoPath(defaultProjectSlug()), Number(req.query.limit))); }
  catch (e) { res.status(500).json({ error: e.message }); }
});
app.get(`${BASE}/:project/api/system-feedback.json`, (req, res) => {
  try { res.json(recentSystemFeedback(resolveRepoPath(projectFromRequest(req)), Number(req.query.limit))); }
  catch (e) { res.status(500).json({ error: e.message }); }
});

// =====================================================================
// /api/grunts — closure-sidecar grunt pool + history.
//
// The sidecar daemon runs BESIDE the main loop and keeps its whole
// surface under `<runtime>/sidecar/`: the kernel-exported
// `candidates.json` (queue + advisory eligibility + prune mirror +
// recent closures), the daemon's `status.json` (pool size + in-flight
// assignments + per-node attempt digests), the append-only
// `ledger.jsonl`, and the `spool/` handoff dirs. The authoritative
// queue and the durable landed-closure map live in kernel state
// (`<runtime>/protocol_state.json`: `sidecar_queue`,
// `sidecar_closures`); the export is the cross-reference for per-entry
// status / blocked reasons.
//
// MOST RUNS HAVE NO SIDECAR. Absent `<runtime>/sidecar/` is the normal
// case, not an error: this returns `{enabled:false, reason}` with a 200
// so the frontend can hide the tab outright. Every file read is
// individually best-effort — a half-written or malformed JSON file
// degrades that one section (and lands in `errors[]`) instead of
// failing the response.
// =====================================================================

const GRUNT_HISTORY_MAX = 200;
const GRUNT_CLOSURES_MAX = 50;
const GRUNT_PRUNES_MAX = 50;
// Every spool lane, in the order the closure handshake walks them. The
// outcome lanes (daemon -> kernel "this generation is spent") were absent
// here, which made the one lane that matters during an incident the only
// invisible one: a queue entry that never leaves the reviewer's queue is
// an outcome record that never left `outcomes/`, and the tab showed
// nothing at all. Appended AFTER the closure lanes on purpose — the
// per-record body scan is capped at GRUNT_SPOOL_SCAN_MAX across all dirs,
// and outcome records carry no proof body.
const GRUNT_SPOOL_DIRS = [
  'pending', 'claimed', 'applied', 'rejected', 'abandoned', 'inflight',
  'outcomes', 'claimed_outcomes', 'outcomes_consumed',
];
// Spool-scan guards: at most this many records read per request, and any
// single record above the byte cap is counted-and-skipped rather than read.
const GRUNT_SPOOL_SCAN_MAX = 500;
const GRUNT_SPOOL_RECORD_MAX_BYTES = 4 * 1024 * 1024;

// Read one JSON file into `{value, error}` — never throws. `errors` (when
// given) collects `{file, error}` for the response's degradation report.
function readGruntJson(filePath, errors) {
  if (!fs.existsSync(filePath)) return null;
  try {
    return JSON.parse(fs.readFileSync(filePath, 'utf-8'));
  } catch (e) {
    if (errors) errors.push({ file: path.basename(filePath), error: e.message });
    return null;
  }
}

function asArray(v) { return Array.isArray(v) ? v : []; }
function asObject(v) { return (v && typeof v === 'object' && !Array.isArray(v)) ? v : {}; }

// Split a queue-export status (`"ready"` | `"blocked:<reason>"`) into the
// bare status plus the reason tail.
function splitQueueStatus(raw) {
  const status = typeof raw === 'string' && raw ? raw : 'unknown';
  const idx = status.indexOf(':');
  if (!status.startsWith('blocked') || idx < 0) return { status, blocked_reason: null };
  return { status: 'blocked', blocked_reason: status.slice(idx + 1) };
}

function gruntSpoolCounts(sidecarDir) {
  const counts = {};
  for (const name of GRUNT_SPOOL_DIRS) {
    const dir = path.join(sidecarDir, 'spool', name);
    try {
      counts[name] = fs.readdirSync(dir).filter((f) => f.endsWith('.json')).length;
    } catch { counts[name] = 0; }
  }
  return counts;
}

// Lean-LOC metrics for one spool record's `artifact.proof_body`. An absent
// or whitespace-only body means the grunt produced no proof text — that is
// `null` (unknown / nothing written), never 0.
function gruntBodyMetrics(body) {
  if (typeof body !== 'string') return { body_lines: null, body_bytes: null };
  const trimmed = body.trim();
  if (!trimmed) return { body_lines: null, body_bytes: null };
  return { body_lines: trimmed.split('\n').length, body_bytes: Buffer.byteLength(body, 'utf-8') };
}

// attempt_id -> {body_lines, body_bytes}, built by scanning the spool
// handoff dirs ONCE per request. The per-attempt rows the tab renders (the
// daemon's status.json digests and the ledger tail) carry no proof body —
// only the spool record does — so the body is joined in on attempt_id.
// Best-effort throughout: a malformed record lands in `errors[]` and the
// scan continues.
function gruntSpoolBodies(sidecarDir, errors) {
  const byAttempt = new Map();
  let scanned = 0;
  for (const name of GRUNT_SPOOL_DIRS) {
    if (scanned >= GRUNT_SPOOL_SCAN_MAX) break;
    const dir = path.join(sidecarDir, 'spool', name);
    let files;
    try { files = fs.readdirSync(dir).filter((f) => f.endsWith('.json')).sort(); }
    catch { continue; }
    for (const file of files) {
      if (scanned >= GRUNT_SPOOL_SCAN_MAX) break;
      scanned += 1;
      const filePath = path.join(dir, file);
      let size = 0;
      try { size = fs.statSync(filePath).size; } catch { continue; }
      if (size > GRUNT_SPOOL_RECORD_MAX_BYTES) continue;
      const record = asObject(readGruntJson(filePath, errors));
      const attemptId = String(record.attempt_id || '');
      if (!attemptId) continue;
      // A record can be seen twice across dirs mid-move; keep the one that
      // actually carries a body.
      const prior = byAttempt.get(attemptId);
      if (prior && prior.body_lines !== null) continue;
      byAttempt.set(attemptId, gruntBodyMetrics(asObject(record.artifact).proof_body));
    }
  }
  return byAttempt;
}

// Core assembler — takes a runtime root so it is directly testable
// against a fixture dir (mirrors recentSystemFeedbackForRuntimeRoot).
function gruntsStateForRuntimeRoot(runtimeRoot, opts) {
  const options = opts || {};
  const now = Number.isFinite(options.now) ? options.now : Date.now();
  const historyLimit = Math.max(1, Math.min(Number(options.historyLimit) || GRUNT_HISTORY_MAX, GRUNT_HISTORY_MAX));
  if (!runtimeRoot) return { enabled: false, reason: 'no_runtime_root' };
  const sidecarDir = path.join(runtimeRoot, 'sidecar');
  if (!fs.existsSync(sidecarDir)) return { enabled: false, reason: 'no_sidecar_dir' };

  const errors = [];
  const candidates = asObject(readGruntJson(path.join(sidecarDir, 'candidates.json'), errors));
  const status = asObject(readGruntJson(path.join(sidecarDir, 'status.json'), errors));
  const cursor = asObject(readGruntJson(path.join(sidecarDir, 'manager_cursor.json'), errors));
  const state = asObject(readGruntJson(path.join(runtimeRoot, 'protocol_state.json'), errors));

  // ---- pool + in-flight (daemon status.json) ----
  const statusGeneratedAt = Number(status.generated_at_ms);
  const inFlight = asArray(status.in_flight).map((row) => {
    const r = asObject(row);
    const started = Number(r.started_at_ms);
    return {
      node: String(r.node || ''),
      entry_seq: Number(r.entry_seq) || 0,
      grunt: Number.isFinite(Number(r.grunt)) ? Number(r.grunt) : null,
      attempt_id: String(r.attempt_id || ''),
      started_at_ms: Number.isFinite(started) ? started : null,
      elapsed_ms: Number.isFinite(started) ? Math.max(0, now - started) : null,
    };
  });
  const grunts = Number.isFinite(Number(status.grunts)) ? Number(status.grunts) : null;
  let daemonPid = null;
  try { daemonPid = Number(fs.readFileSync(path.join(sidecarDir, 'daemon.pid'), 'utf-8').trim()) || null; } catch {}
  const pool = {
    grunts,
    busy: inFlight.length,
    idle: grunts === null ? null : Math.max(0, grunts - inFlight.length),
    daemon_pid: daemonPid,
    status_generated_at_ms: Number.isFinite(statusGeneratedAt) ? statusGeneratedAt : null,
    status_age_ms: Number.isFinite(statusGeneratedAt) ? Math.max(0, now - statusGeneratedAt) : null,
    last_seen_cycle: Number.isFinite(Number(cursor.last_seen_cycle)) ? Number(cursor.last_seen_cycle) : null,
  };

  // ---- export header (kernel candidates.json) ----
  const exportGeneratedAt = Number(candidates.generated_at_ms);
  const exportInfo = {
    present: Object.keys(candidates).length > 0,
    schema: Number.isFinite(Number(candidates.schema)) ? Number(candidates.schema) : null,
    cycle: Number.isFinite(Number(candidates.cycle)) ? Number(candidates.cycle) : null,
    phase: candidates.phase ? String(candidates.phase) : null,
    snapshot_sha: candidates.snapshot_sha ? String(candidates.snapshot_sha) : null,
    window_open: !!candidates.sidecar_window_open,
    generated_at_ms: Number.isFinite(exportGeneratedAt) ? exportGeneratedAt : null,
    age_ms: Number.isFinite(exportGeneratedAt) ? Math.max(0, now - exportGeneratedAt) : null,
    eligible_now: asArray(candidates.eligible_now).length,
  };

  // ---- queue: kernel `sidecar_queue` is authoritative (order =
  // reviewer submission order); the export supplies per-entry status and
  // the blocked reason. Fall back to the export's own rows when kernel
  // state is unreadable.
  const exportQueueByKey = new Map();
  for (const row of asArray(candidates.queue)) {
    const r = asObject(row);
    exportQueueByKey.set(`${r.node}|${Number(r.entry_seq) || 0}`, r);
  }
  const inFlightByKey = new Map(inFlight.map((r) => [`${r.node}|${r.entry_seq}`, r]));
  const kernelQueue = asArray(state.sidecar_queue);
  const queueSource = Array.isArray(state.sidecar_queue) ? 'kernel' : 'export';
  const rawQueue = queueSource === 'kernel' ? kernelQueue : asArray(candidates.queue);
  const queue = rawQueue.map((row) => {
    const r = asObject(row);
    const node = String(r.node || '');
    const entrySeq = Number(r.entry_seq) || 0;
    const key = `${node}|${entrySeq}`;
    const exported = exportQueueByKey.get(key) || (queueSource === 'export' ? r : null);
    const { status: qStatus, blocked_reason } = splitQueueStatus(exported ? exported.status : null);
    const assigned = inFlightByKey.get(key) || null;
    return {
      node,
      entry_seq: entrySeq,
      queued_at_cycle: Number.isFinite(Number(r.queued_at_cycle)) ? Number(r.queued_at_cycle) : null,
      status: qStatus,
      blocked_reason,
      in_export: !!exportQueueByKey.get(key),
      in_flight: !!assigned,
      grunt: assigned ? assigned.grunt : null,
    };
  });

  // ---- recent closures: durable kernel map preferred, export mirror as
  // the fallback (a fresh run has the export's window but no state key).
  //
  // The durable map is `closure_provenance`. It SUBSUMED an earlier
  // `sidecar_closures` map (kernel model.rs) — which this reader went on
  // asking for long after it stopped existing, so the "preferred" branch
  // never once ran and every request silently took the export fallback.
  // That fallback carries no attempt_id, which is what emptied
  // `closureByAttempt` below and left every ledger row's model unresolved.
  //
  // Two shape differences from the map it replaced, both load-bearing:
  //   * it records EVERY closure, worker and grunt alike, tagged by
  //     `closed_by` — so the grunt surfaces must filter on it, or worker
  //     closures would show up as grunt work;
  //   * the attempt identity (attempt_id/provider/model/wall_ms) is nested
  //     under `sidecar`, present iff `closed_by == "sidecar"`. `cycle`
  //     stays on the enclosing entry: one source of truth for "when".
  const closureMap = asObject(state.closure_provenance);
  const sidecarClosureEntries = Object.entries(closureMap)
    .filter(([, meta]) => String(asObject(meta).closed_by || '') === 'sidecar');
  let recentClosures;
  if (sidecarClosureEntries.length) {
    recentClosures = sidecarClosureEntries.map(([node, meta]) => {
      const m = asObject(meta);
      const sc = asObject(m.sidecar);
      return {
        node: String(node),
        cycle: Number.isFinite(Number(m.cycle)) ? Number(m.cycle) : null,
        model: sc.model ? String(sc.model) : null,
        provider: sc.provider ? String(sc.provider) : null,
        attempt_id: sc.attempt_id ? String(sc.attempt_id) : null,
        wall_ms: Number.isFinite(Number(sc.wall_ms)) ? Number(sc.wall_ms) : null,
      };
    });
    recentClosures.sort((a, b) => (b.cycle || 0) - (a.cycle || 0) || a.node.localeCompare(b.node));
  } else {
    recentClosures = asArray(candidates.recent_closures).map((row) => {
      const r = asObject(row);
      return {
        node: String(r.node || ''),
        cycle: Number.isFinite(Number(r.cycle)) ? Number(r.cycle) : null,
        model: r.model ? String(r.model) : null,
        provider: null,
        attempt_id: null,
        wall_ms: null,
      };
    });
  }
  const closuresTotal = recentClosures.length;
  // Attribution backfill for the ledger: the daemon's ledger rows carry
  // no provider/model (those live on the spool attempt record), but the
  // landed-closure map keys the same attempt ids.
  const closureByAttempt = new Map();
  for (const c of recentClosures) {
    if (c.attempt_id) closureByAttempt.set(c.attempt_id, c);
  }
  recentClosures = recentClosures.slice(0, GRUNT_CLOSURES_MAX);

  // ---- prunes: export mirrors the state log oldest-last; show newest first.
  const prunedAll = asArray(candidates.pruned_recent).map((row) => {
    const r = asObject(row);
    return {
      node: String(r.node || ''),
      entry_seq: Number(r.entry_seq) || 0,
      cycle: Number.isFinite(Number(r.cycle)) ? Number(r.cycle) : null,
      reason: r.reason ? String(r.reason) : '',
    };
  }).reverse();
  const pruned = prunedAll.slice(0, GRUNT_PRUNES_MAX);

  // ---- Lean LOC: the proof body lives on the spool record only, joined
  // onto the attempt rows below by attempt id.
  const bodyByAttempt = gruntSpoolBodies(sidecarDir, errors);
  const noBody = { body_lines: null, body_bytes: null };

  // ---- per-node attempt digests (status.json) ----
  const attemptsByNode = {};
  let attemptTotal = 0;
  for (const [node, rows] of Object.entries(asObject(status.attempts))) {
    attemptsByNode[String(node)] = asArray(rows).map((row) => {
      const r = asObject(row);
      attemptTotal += 1;
      const body = bodyByAttempt.get(String(r.attempt_id || '')) || noBody;
      return {
        entry_seq: Number(r.entry_seq) || 0,
        attempt_id: String(r.attempt_id || ''),
        body_lines: body.body_lines,
        body_bytes: body.body_bytes,
        status: String(r.status || 'unknown'),
        detail: r.detail ? String(r.detail) : '',
        iterations: Number(r.iterations) || 0,
        prompt_tokens: Number(r.prompt_tokens) || 0,
        completion_tokens: Number(r.completion_tokens) || 0,
        wall_secs: Number(r.wall_secs) || 0,
        ts: Number(r.ts) || null,
      };
    });
  }

  // ---- ledger tail: newest first, capped. Malformed rows are dropped
  // (readJsonlSync already skips them) rather than failing the read.
  const ledgerPath = path.join(sidecarDir, 'ledger.jsonl');
  let ledgerTotal = 0;
  let history = [];
  if (fs.existsSync(ledgerPath)) {
    const rows = readJsonlSync(ledgerPath);
    ledgerTotal = rows.length;
    history = rows.slice(-historyLimit).reverse().map((row) => {
      const r = asObject(row);
      const prov = asObject(r.provenance);
      const landed = closureByAttempt.get(String(r.attempt_id || '')) || {};
      const body = bodyByAttempt.get(String(r.attempt_id || '')) || noBody;
      return {
        ts: Number(r.ts) || null,
        node: String(r.node || ''),
        entry_seq: Number(r.entry_seq) || 0,
        attempt_id: String(r.attempt_id || ''),
        body_lines: body.body_lines,
        body_bytes: body.body_bytes,
        grunt: Number.isFinite(Number(r.grunt)) ? Number(r.grunt) : null,
        status: String(r.status || 'unknown'),
        detail: r.detail ? String(r.detail) : '',
        iterations: Number(r.iterations) || 0,
        prompt_tokens: Number(r.prompt_tokens) || 0,
        completion_tokens: Number(r.completion_tokens) || 0,
        wall_secs: Number(r.wall_secs) || 0,
        model: r.model ? String(r.model) : (prov.model ? String(prov.model) : (landed.model || null)),
        provider: r.provider ? String(r.provider) : (prov.provider ? String(prov.provider) : (landed.provider || null)),
        landed_cycle: Number.isFinite(landed.cycle) ? landed.cycle : null,
        compactions: Number.isFinite(Number(r.compactions)) ? Number(r.compactions) : null,
        transport_retries: Number.isFinite(Number(r.transport_retries)) ? Number(r.transport_retries) : null,
        reasoning_effort: r.reasoning_effort ? String(r.reasoning_effort) : null,
        timings: asObject(r.timings),
      };
    });
  }

  return {
    enabled: true,
    sidecar_dir: sidecarDir,
    now_ms: now,
    pool,
    export: exportInfo,
    in_flight: inFlight,
    queue,
    queue_source: queueSource,
    recent_closures: recentClosures,
    recent_closures_total: closuresTotal,
    pruned_recent: pruned,
    pruned_recent_total: prunedAll.length,
    attempts: attemptsByNode,
    attempts_total: attemptTotal,
    history,
    history_total: ledgerTotal,
    history_limit: historyLimit,
    spool: gruntSpoolCounts(sidecarDir),
    errors,
  };
}

function gruntsState(projectInfo, historyLimit) {
  return gruntsStateForRuntimeRoot(runtimeRootForProject(projectInfo), { historyLimit });
}
app.get(`${BASE}/api/grunts.json`, (req, res) => {
  try { res.json(gruntsState(resolveRepoPath(defaultProjectSlug()), Number(req.query.limit))); }
  catch (e) { res.status(500).json({ error: e.message }); }
});
app.get(`${BASE}/:project/api/grunts.json`, (req, res) => {
  try { res.json(gruntsState(resolveRepoPath(projectFromRequest(req)), Number(req.query.limit))); }
  catch (e) { res.status(500).json({ error: e.message }); }
});

// Unified Chats tab: list of calls for a cycle + per-call structured events.
// (Replaces the old live-panes.json / burst-log.json endpoints. Those were
// removed after the frontend migrated to chat-calls.json / chat-events.json.)
app.get(`${BASE}/api/chat-calls.json`, (req, res) => {
  handleChatCalls(req, res, defaultProjectSlug());
});

app.get(`${BASE}/:project/api/chat-calls.json`, (req, res) => {
  handleChatCalls(req, res, projectFromRequest(req));
});

app.get(`${BASE}/api/chat-events.json`, (req, res) => {
  handleChatEvents(req, res, defaultProjectSlug());
});

app.get(`${BASE}/:project/api/chat-events.json`, (req, res) => {
  handleChatEvents(req, res, projectFromRequest(req));
});

// API: submit human feedback
app.use(express.json());

registerControlRoute('feedback', [], (req, res) => {
  try {
    handleFeedbackPost(req, res, projectFromRequest(req));
  } catch (e) {
    res.status(500).json({ error: e.message });
  }
});

// API: get current human feedback status
app.get(`${BASE}/api/feedback`, (req, res) => {
  try {
    handleFeedbackGet(res, defaultProjectSlug());
  } catch (e) {
    res.status(500).json({ error: e.message });
  }
});

app.get(`${BASE}/:project/api/feedback`, (req, res) => {
  try {
    handleFeedbackGet(res, projectFromRequest(req));
  } catch (e) {
    res.status(500).json({ error: e.message });
  }
});

// =====================================================================
// /api/usage — provider+role rollups + check ledger + per-stage walltime
// + latest quota snapshot per provider. Mirrors trellis/usage_report.py.
// =====================================================================

function readJsonlSync(p) {
  if (!p || !fs.existsSync(p)) return [];
  try {
    return fs.readFileSync(p, 'utf8').split('\n').filter(Boolean).map((l) => {
      try { return JSON.parse(l); } catch { return null; }
    }).filter(Boolean);
  } catch { return []; }
}

// Per-stage wall-clock, folded straight out of the per-cycle event-log files.
//
// This used to materialize every record in global index order and then walk
// adjacent pairs. On one live run that array is 3.01 GB of JSONL — about
// 1.45x that as live V8 objects — so /api/usage.json took the viewer to a
// fatal heap OOM at ~4 GB. The pairwise walk reads two scalars per record, so
// carry the previous record's {ts_ms, stage} across the fold and retain
// nothing else. Also returns the record count for `counts.event_rows`.
//
// Records are skipped exactly where readJsonlSync skipped them (empty line,
// parse failure, falsy parse), so adjacency — and therefore the totals — match
// the materialized walk.
//
// Streaming made the walk survivable, not cheap: it still JSON.parses all
// 5.7 GB on every cache miss (37s measured live, 1358 cycle files) to
// learn what the one live file changed. So each file's contribution is now
// memoized and only files whose identity changed are re-read. See
// `_eventLogFoldPartials` / `eventLogFilePartial` below.

// Per-cycle-file memo for `foldEventLogStageWalltime`, keyed
// event-log dir → (cycle file path → { key, partial }). The outer level exists
// so folding project B does not evict project A's partials — the viewer serves
// every project under PROJECTS_ROOT and the Usage tab is per project.
//
// The key is (size, mtimeMs, ino), NOT the path alone. Cycle files are
// append-only under normal operation, but a LastClean rewind rewrites history:
// `git reset --hard` to an older tag restores older, SHORTER cycle files at
// paths that already exist, and the newest ones disappear. Keying on path
// alone would serve totals derived from a timeline that no longer exists,
// silently and forever. Size and mtime both change under any such rewrite, and
// `ino` additionally catches replace-by-rename (checkout writes a temp file and
// renames it over the old one, which can preserve size and coarse mtime).
// Stale entries for vanished paths are dropped by rebuilding the map from the
// current file list on every fold.
const _eventLogFoldPartials = new Map();

// Fold one cycle file in isolation. `byStage[s].duration_ms` accumulates the
// pair durations wholly inside this file; `first`/`last` carry the file's
// boundary records so partials compose into exactly the totals a single
// uninterrupted pass produces.
//
// `complete` is false when the read threw partway. The caller still uses that
// partial (the pre-change walk also kept whatever it had folded before the
// throw) but must not cache it: the failure is usually transient, and the same
// (size, mtime, ino) would then pin the truncated answer.
function eventLogFilePartial(filePath) {
  const byStage = {};
  let rows = 0;
  let first = null;
  let last = null;
  let complete = true;
  try {
    forEachFileLine(filePath, (line) => {
      if (!line) return;
      let rec;
      try { rec = JSON.parse(line); } catch { return; }
      if (!rec) return;
      rows++;
      const cur = { ts_ms: rec.ts_ms, stage: rec.stage };
      if (last) addStageInterval(byStage, last, cur);
      if (!first) first = cur;
      last = cur;
    });
  } catch {
    // unreadable cycle file — readJsonlSync ignored these too
    complete = false;
  }
  return { byStage, rows, first, last, complete };
}

// Accumulate the interval between two adjacent records into `byStage`, under
// the earlier record's stage. Same predicate as the pre-change pairwise walk.
//
// Durations accumulate as integer milliseconds and are divided by 1000 once,
// at the end. Float addition is not associative, so summing per-file and then
// summing the files would drift from the single global accumulator in the last
// ulp — the incremental result would be *nearly* the full-walk result rather
// than identical to it. Integer addition is associative, so any grouping of
// the same intervals yields the same total, exactly.
function addStageInterval(byStage, a, b) {
  const ta = parseInt(a.ts_ms || 0), tb = parseInt(b.ts_ms || 0);
  if (!(ta > 0 && tb > ta)) return;
  const stage = String(a.stage || '?');
  if (!byStage[stage]) byStage[stage] = { intervals: 0, duration_ms: 0 };
  byStage[stage].intervals++;
  byStage[stage].duration_ms += (tb - ta);
}

function foldEventLogStageWalltime(projectInfo) {
  const dir = eventLogDirForProject(projectInfo);
  const files = eventLogCycleFiles(dir);
  const memo = _eventLogFoldPartials.get(dir) || new Map();
  const keep = new Map();
  const byStage = {};
  let rows = 0;
  let prev = null;
  for (const f of files) {
    let st;
    try { st = fs.statSync(f); } catch { continue; }
    const key = `${st.size}:${st.mtimeMs}:${st.ino}`;
    const cached = memo.get(f);
    let p;
    if (cached && cached.key === key) {
      p = cached.partial;
      keep.set(f, cached);
    } else {
      p = eventLogFilePartial(f);
      if (p.complete) keep.set(f, { key, partial: p });
    }
    // The pair straddling the file boundary is the one a per-file memo would
    // lose; recompute it from the retained boundary records.
    if (prev && p.first) addStageInterval(byStage, prev, p.first);
    for (const stage of Object.keys(p.byStage)) {
      const v = p.byStage[stage];
      if (!byStage[stage]) byStage[stage] = { intervals: 0, duration_ms: 0 };
      byStage[stage].intervals += v.intervals;
      byStage[stage].duration_ms += v.duration_ms;
    }
    rows += p.rows;
    // A file with no parsed records leaves `prev` alone, exactly as the
    // single pass did — adjacency skips over it to the next record.
    if (p.last) prev = p.last;
  }
  // `keep` holds exactly the files that exist now, so replacing the memo with
  // it drops entries for cycle files a rewind deleted.
  if (dir) _eventLogFoldPartials.set(dir, keep);
  const out = {};
  for (const stage of Object.keys(byStage)) {
    out[stage] = { intervals: byStage[stage].intervals, duration_s: byStage[stage].duration_ms / 1000 };
  }
  return { byStage: out, rows };
}

// Monthly subscription prices for the Usage tab's "effective USD/mo"
// estimate. These are the user's stated personal-plan figures; if you
// upgrade/downgrade, edit here.
const MONTHLY_SUBSCRIPTION_USD = {
  claude: 200,
  gemini: 250,
  codex: 200,
};

// The weekly remaining-budget headline for one raw quota snapshot, or null.
//
// This is deliberately narrow. The surrounding policy suppresses per-window
// BURN RATES and quota-derived USD (the β model owns cost); how much
// subscription quota is left is not a cost estimate but plain operational
// state, and it is the first thing an operator wants to see.
function weeklyBudgetFromSnapshot(snap) {
  const weekly = Array.isArray(snap && snap.windows)
    ? snap.windows.find((w) => w && w.name === 'weekly')
    : null;
  if (!weekly || !Number.isFinite(Number(weekly.pct_used))) return null;
  const used = Number(weekly.pct_used);
  return {
    pct_used: used,
    pct_left: Math.max(0, Math.min(100, 100 - used)),
    pct_used_kind: weekly.pct_used_kind || null,
    resets_at: weekly.resets_at ?? null,
    resets_at_repr: weekly.resets_at_repr || null,
    resets_in_seconds: weekly.resets_in_seconds ?? null,
  };
}

// Latest quota snapshot per provider, weekly budget attached.
//
// Reads the JSONL directly rather than going through `buildUsageRollup`,
// because the budget floor is evaluated on a timer once a minute and must
// not drag the whole cost/check/event rollup along with it.
function latestQuotaSnapshotsForProject(projectInfo) {
  const rows = readJsonlSync(
    path.join(projectInfo.repoPath, '.trellis', 'logs', 'quota-snapshots.jsonl'));
  const latest = {};
  for (const r of rows) {
    const provider = r && r.provider;
    if (!provider) continue;
    if (!latest[provider] || Number(r.ts) > Number(latest[provider].ts)) latest[provider] = r;
  }
  for (const provider of Object.keys(latest)) {
    const budget = weeklyBudgetFromSnapshot(latest[provider]);
    if (budget) latest[provider] = { ...latest[provider], weekly_budget: budget };
  }
  return latest;
}

function buildUsageRollup(projectInfo) {
  const repo = projectInfo.repoPath;
  const runtimeRoot = runtimeRootForProject(projectInfo);
  const cost = readJsonlSync(path.join(repo, '.trellis', 'logs', 'cost-ledger.jsonl'));
  const check = readJsonlSync(path.join(repo, '.trellis', 'logs', 'check-ledger.jsonl'));
  const quota = readJsonlSync(path.join(repo, '.trellis', 'logs', 'quota-snapshots.jsonl'));
  const { byStage, rows: eventRows } = foldEventLogStageWalltime(projectInfo);

  function emptyAgg() {
    return { bursts: 0, ok: 0, duration_s: 0, input: 0, output: 0,
             cache_read: 0, cache_write: 0, messages: 0, bursts_with_msgs: 0 };
  }

  function addCostRow(map, key, r) {
    if (!map[key]) map[key] = emptyAgg();
    const a = map[key];
    a.bursts++;
    if (r.ok) a.ok++;
    a.duration_s += parseFloat(r.duration_seconds || 0) || 0;
    const u = r.usage || {};
    a.input += parseInt(u.input_tokens || u.input || 0) || 0;
    a.output += parseInt(u.output_tokens || u.output || 0) || 0;
    a.cache_read += parseInt(u.cache_read_input_tokens || u.cached_input_tokens || u.cached || 0) || 0;
    a.cache_write += parseInt(u.cache_creation_input_tokens || 0) || 0;
    if (typeof r.message_count === 'number' && r.message_count >= 0) {
      a.messages += r.message_count;
      a.bursts_with_msgs++;
    }
  }

  // role in the cost ledger is just "worker" or "reviewer", but the
  // reviewer slot covers four distinct callers — paper, corr, sound (the
  // three verifier kinds) and the actual reviewer (`review`). The
  // verifier kind is encoded in the scope, e.g.
  //   proof_formalization:reviewer:paper:135:v1:claude:claude-opus-4-6:max
  //   proof_formalization:reviewer:review:claude:claude-opus-4-6:max
  // Split them out for the per-(provider, category) breakdown so cost
  // attribution actually reflects which kind of caller spent the budget.
  function categoryFor(r) {
    const role = r.role || '?';
    if (role === 'worker') return 'worker';
    const scope = String(r.scope || '');
    for (const k of ['paper', 'corr', 'sound', 'review']) {
      if (scope.includes(`:${k}:`)) return k;
    }
    return role;
  }

  const byProvider = {};
  const byProviderCategory = {};
  for (const r of cost) {
    const prov = r.provider || '?';
    const cat = categoryFor(r);
    addCostRow(byProvider, prov, r);
    addCostRow(byProviderCategory, `${prov}::${cat}`, r);
  }

  // Check ledger split by kind
  const byCheckSub = {};
  const byGitSub = {};
  for (const r of check) {
    const kind = r.kind || 'check';
    const sub = r.subcommand || '?';
    const dur = parseFloat(r.duration_seconds || 0) || 0;
    const map = (kind === 'git') ? byGitSub : byCheckSub;
    if (!map[sub]) map[sub] = { count: 0, ok: 0, duration_s: 0 };
    map[sub].count++;
    if (r.ok) map[sub].ok++;
    map[sub].duration_s += dur;
  }

  // Latest quota snapshot per provider (success or failure)
  const latestQuota = {};
  for (const r of quota) {
    const p = r.provider;
    if (!p) continue;
    if (!latestQuota[p] || (Number(r.ts) > Number(latestQuota[p].ts))) {
      latestQuota[p] = r;
    }
  }

  // 5h burn data is hidden here per the model-based cost-reporting
  // policy. The β model (built below as `codexCostRollup`) is the
  // canonical source for cost USD; the latest snapshot's per-window
  // burn rates and any quota-derived effective_monthly_usd estimate
  // are no longer surfaced in the API response. We continue to write
  // 5h data into quota-snapshots.jsonl going forward — that record
  // stays available on disk for re-calibrating the β model when more
  // data accumulates — but it is intentionally not exposed here.
  //
  // Subscription metadata (account, plan_tier, credits, ts, ok) is
  // preserved on each snapshot as operational state.
  for (const prov of Object.keys(latestQuota)) {
    const snap = latestQuota[prov];
    if (!snap) continue;
    const price = MONTHLY_SUBSCRIPTION_USD[prov];
    if (price) snap.subscription_usd = price;
    // Keep the WEEKLY remaining-budget headline before dropping `windows`.
    // Same extraction the budget-floor policy uses, so the number the
    // operator reads and the number that stops the run are the same number.
    const budget = weeklyBudgetFromSnapshot(snap);
    if (budget) snap.weekly_budget = budget;
    delete snap.windows;
    delete snap.models;
    delete snap.effective_monthly_usd;
    delete snap.effective_monthly_usd_basis;
  }

  // Build per-provider and per-(provider, category) arrays first so we
  // can annotate them with attributed monthly_burn USD/% (replacing the
  // old API-equivalent USD column, which was fictitious for subscription
  // accounts). `category` ∈ {worker, paper, corr, sound, review}.
  const byProviderArr = Object.entries(byProvider)
    .map(([k, v]) => ({ provider: k, ...v }))
    .sort((x, y) => x.provider.localeCompare(y.provider));
  const byProviderCategoryArr = Object.entries(byProviderCategory)
    .map(([k, v]) => { const [p, c] = k.split('::'); return { provider: p, category: c, ...v }; })
    .sort((x, y) => `${x.provider}::${x.category}`.localeCompare(`${y.provider}::${y.category}`));

  function totalTokens(r) {
    return (r.input || 0) + (r.output || 0) + (r.cache_read || 0) + (r.cache_write || 0);
  }

  // Per-provider total tokens (kept for token_share reporting only — the
  // USD/burn columns no longer use it). Total tokens were the old
  // attribution-share metric; we now use per-burst quota deltas instead.
  const providerTotalTokens = {};
  for (const r of byProviderArr) providerTotalTokens[r.provider] = totalTokens(r);

  // ===== per-burst quota-delta USD attribution =============================
  //
  // For each cost-ledger row that has bracketing probes (quota_pre +
  // quota_post), compute the "weekly_pct" (what fraction of one week's quota
  // this single burst consumed) by diffing pct_used. Reset-aware:
  //   pre_pct=80, post_pct=5 with post_resets_at > pre_resets_at  →
  //     burst spanned a quota reset; consumption = (100 - 80) + 5 = 25.
  //
  // For codex/claude we PREFER the 5h delta (×1/7 to project to weekly)
  // because the 5h window resets often enough that mid-burst noise from
  // OTHER agents is bounded. Fall back to the weekly delta when no 5h
  // signal is present.
  //
  // For gemini there is no 5h vs weekly distinction — only per-category
  // daily windows. Take the max delta across categories ×1/7 as the
  // weekly_pct contribution.
  //
  // Sum weekly_pct over all bursts in a (provider) or (provider, category)
  // bucket, then convert to monthly_burn_usd via the configured monthly
  // subscription price. Coverage = (rows with both probes) / (rows total),
  // surfaced so a low coverage flags an underestimate.

  const PROVIDER_WEEKLY_TO_MONTHLY = { claude: 1/4, codex: 1/4 };
  // codex: empirical calibration history —
  //   1/7 (initial naïve assumption) underestimated by ~27% over a
  //   24h dedicated-account check (predicted 6.57 weekly-pct, observed 9.0).
  //   1/6 (interim correction) still underestimated; the β-model+1/6
  //   total ($7.79/mo) was ~22% below the direct sum of per-burst
  //   weekly_pct deltas ($10.00/mo) over cycles 1-77 of example-run.
  //   1/5 was overshooting the codex meter by ~8% over a clean 19.4h
  //   verifier-complete window: model 19.5 weekly_pct vs meter 18.0.
  //   1/5.5 (current) trims the over-prediction; lands within ~1% of
  //   the meter's reading on that window.
  // Revisit once a token-based cost proxy lands or per-phase β
  // calibration tightens the lane breakdown.
  const PROVIDER_5H_TO_WEEKLY = { claude: 1/7, codex: 1/5.5 };

  function deltaPctReset(prePct, postPct, preResetsAt, postResetsAt) {
    if (prePct == null || postPct == null) return null;
    const preNum = Number(prePct), postNum = Number(postPct);
    if (!Number.isFinite(preNum) || !Number.isFinite(postNum)) return null;
    if (preResetsAt != null && postResetsAt != null
        && Number(postResetsAt) > Number(preResetsAt) + 60) {
      // Burst spanned a reset boundary. Pre-reset depletion is invisible
      // (the meter zeroed out), so estimate total consumption as 2× the
      // post-reset depletion under the assumption of constant burst rate.
      // Cap at 100 since one window can't deplete more than itself.
      return Math.min(100, 2 * Math.max(0, postNum));
    }
    return Math.max(0, postNum - preNum);
  }

  function weeklyPctForBurstCodexLike(qpre, qpost, prov) {
    if (!qpre || !qpost) return null;
    const fhDelta = deltaPctReset(
      qpre.five_hour_pct, qpost.five_hour_pct,
      qpre.five_hour_resets_at, qpost.five_hour_resets_at,
    );
    if (fhDelta != null) return fhDelta * (PROVIDER_5H_TO_WEEKLY[prov] || 1/7);
    const wDelta = deltaPctReset(
      qpre.weekly_pct, qpost.weekly_pct,
      qpre.weekly_resets_at, qpost.weekly_resets_at,
    );
    return wDelta;
  }

  function weeklyPctForBurstGemini(qpre, qpost) {
    if (!qpre || !qpost) return null;
    const preCats = (qpre.models || []).reduce((m, x) => {
      if (x && x.category) m[x.category] = x;
      return m;
    }, {});
    let maxWeekly = 0;
    let saw = false;
    for (const post of (qpost.models || [])) {
      if (!post || !post.category) continue;
      const pre = preCats[post.category];
      if (!pre) continue;
      const d = deltaPctReset(pre.pct_used, post.pct_used, pre.resets_at, post.resets_at);
      if (d != null) {
        saw = true;
        const weekly = d * (1/7);
        if (weekly > maxWeekly) maxWeekly = weekly;
      }
    }
    return saw ? maxWeekly : null;
  }

  function weeklyPctToUsd(weeklyPct, prov) {
    const sub = MONTHLY_SUBSCRIPTION_USD[prov];
    const w2m = PROVIDER_WEEKLY_TO_MONTHLY[prov] || 1/4;
    if (!sub) return null;
    return Number((weeklyPct * w2m * sub / 100).toFixed(3));
  }

  // Aggregation: per-(provider) and per-(provider, category), sum the
  // weekly_pct contributions. probe_coverage tracks how many rows had usable
  // bracketing probes vs total rows.
  const burnPctByProv = {};
  const burnPctByProvCat = {};
  const probeCoverageByProv = {};
  const probeCoverageByProvCat = {};

  for (const r of cost) {
    const prov = r.provider || '?';
    const cat = categoryFor(r);
    const k = `${prov}::${cat}`;
    if (!probeCoverageByProv[prov]) probeCoverageByProv[prov] = { withProbes: 0, total: 0 };
    probeCoverageByProv[prov].total++;
    if (!probeCoverageByProvCat[k]) probeCoverageByProvCat[k] = { withProbes: 0, total: 0 };
    probeCoverageByProvCat[k].total++;

    let weeklyPct = null;
    let estimated = false;
    if (prov === 'codex' || prov === 'claude') {
      weeklyPct = weeklyPctForBurstCodexLike(r.quota_pre, r.quota_post, prov);
    } else if (prov === 'gemini') {
      weeklyPct = weeklyPctForBurstGemini(r.quota_pre, r.quota_post);
    }
    // Backfill: when a row has no usable bracket (pre-deploy bursts, probe
    // failures), estimate weekly_pct from burst duration alone, treating it
    // as if it ran at the rate that would saturate the weekly quota over a
    // full week. duration_seconds / weekly_window_seconds × 100. Coarse but
    // self-documenting; over a full run the over/under-estimates average out.
    if (weeklyPct == null || !Number.isFinite(weeklyPct)) {
      const dur = parseFloat(r.duration_seconds || 0) || 0;
      if (dur > 0 && (prov === 'codex' || prov === 'claude' || prov === 'gemini')) {
        const WEEKLY_WINDOW_SECONDS = 7 * 24 * 3600;
        weeklyPct = (dur / WEEKLY_WINDOW_SECONDS) * 100;
        estimated = true;
      }
    }
    if (weeklyPct == null || !Number.isFinite(weeklyPct)) continue;
    if (!estimated) {
      probeCoverageByProv[prov].withProbes++;
      probeCoverageByProvCat[k].withProbes++;
    }
    burnPctByProv[prov] = (burnPctByProv[prov] || 0) + weeklyPct;
    burnPctByProvCat[k] = (burnPctByProvCat[k] || 0) + weeklyPct;
  }

  // For codex, replace the quota-delta-derived weekly_pct with a
  // model-derived prediction: model_burn_5h_pct = β(phase, lane) ·
  // llm_seconds, where llm_seconds comes from the rollout files. β is
  // calibrated empirically per (phase, lane) — see
  // trellis.codex_timing.BETA_BY_PHASE_LANE for the constants and the
  // β-stability analysis under cycles 1-72 of the live run for the
  // methodology. We pin a single β model across phases initially; the
  // dict structure leaves room for per-phase calibration as evidence
  // accumulates.
  //
  // The 5h-pct → weekly conversion (× 1/6 for codex) and the
  // weekly → monthly USD conversion are unchanged — those are
  // subscription-policy constants, not part of the β fit.
  //
  // Non-codex providers (claude, gemini) don't have rollouts to feed
  // the β model, so they fall back to the legacy per-burst quota-delta
  // path. When/if rollouts exist for them too, lift them into the
  // model side here.
  const codexCostRollup = (() => {
    try {
      const ledgerPath = path.join(repo, '.trellis', 'logs', 'cost-ledger.jsonl');
      const env = {
        ...process.env,
        PYTHONPATH: process.env.PYTHONPATH
          ? `${TRELLIS_ROOT}:${process.env.PYTHONPATH}`
          : TRELLIS_ROOT,
      };
      const raw = execFileSync('python3', [
        '-m', 'trellis.codex_timing', 'cost-rollup',
        '--ledger', ledgerPath,
      ], {
        cwd: TRELLIS_ROOT, env, encoding: 'utf-8',
        timeout: 30000, maxBuffer: 16 * 1024 * 1024,
      });
      return JSON.parse(raw);
    } catch (e) {
      return null;
    }
  })();
  const modelByProv = {};
  const modelByProvCat = {};
  if (codexCostRollup) {
    for (const r of codexCostRollup.by_provider || []) {
      modelByProv[r.provider] = r;
    }
    for (const r of codexCostRollup.by_provider_category || []) {
      modelByProvCat[`${r.provider}::${r.category}`] = r;
    }
  }

  function applyCostFields(rec, model5h, fallbackWeekly) {
    // All `total_*` fields are RUN-CUMULATIVE: a sum of per-burst
    // contributions since the start of the cost ledger. They are NOT
    // rates, despite the legacy labels (`monthly_burn_pct`,
    // `monthly_burn_usd`) suggesting otherwise. Multiply through by the
    // unit conversions the original `monthly_burn_*` chain used:
    //
    //     total_5h_pct ─×fhToW─→ total_weekly_pct ─×wToM─→ total_monthly_pct
    //                          (1/5 for codex)         (1/4 for codex/claude)
    //     total_monthly_pct ─×subscription_usd/100─→ total_usd
    //
    // The fhToW and wToM factors are calibration constants; they don't
    // turn the cumulative sum into a rate. A run that takes 30 days
    // would have total_monthly_pct ~= 100 if its monthly burn rate
    // matched its monthly subscription. For shorter runs
    // total_monthly_pct < 100 just because the run hasn't been long
    // enough to consume a month's quota; that's not a "low burn rate".
    //
    // To compute an actual rate, divide total_usd by the run duration.
    const prov = rec.provider;
    const sub = MONTHLY_SUBSCRIPTION_USD[prov] || null;
    const wToM = PROVIDER_WEEKLY_TO_MONTHLY[prov] || 1/4;
    const fhToW = PROVIDER_5H_TO_WEEKLY[prov] || 1/7;
    if (model5h != null) {
      const burn5h = Number(model5h.model_burn_5h_pct);
      const wpct = burn5h * fhToW;
      const mpct = wpct * wToM;
      const usd = sub != null ? Number((mpct * sub / 100).toFixed(3)) : null;
      return {
        total_5h_pct: Number(burn5h.toFixed(3)),
        total_weekly_pct: Number(wpct.toFixed(3)),
        total_monthly_pct: Number(mpct.toFixed(3)),
        total_usd: usd,
        // Legacy aliases — same values as the total_* fields above; kept so
        // any existing consumer (e.g. an external dashboard polling
        // /api/usage.json) that hard-coded the old names doesn't show
        // blank cells. New consumers should prefer the total_* names which
        // accurately convey that these are run-cumulative sums, not rates.
        weekly_pct: Number(wpct.toFixed(3)),
        monthly_burn_pct: Number(mpct.toFixed(3)),
        monthly_burn_usd: usd,
        subscription_usd: sub,
        attribution_basis: 'beta_model',
        rollout_coverage: model5h.n != null
          ? `${model5h.with_rollout}/${model5h.n}`
          : null,
      };
    }
    // Fallback (non-codex or codex with no rollouts at all).
    if (fallbackWeekly != null) {
      const wpct = fallbackWeekly;
      const mpct = wpct * wToM;
      const usd = weeklyPctToUsd(wpct, prov);
      return {
        total_5h_pct: null,
        total_weekly_pct: Number(wpct.toFixed(3)),
        total_monthly_pct: Number(mpct.toFixed(3)),
        total_usd: usd,
        // Legacy aliases (same values).
        weekly_pct: Number(wpct.toFixed(3)),
        monthly_burn_pct: Number(mpct.toFixed(3)),
        monthly_burn_usd: usd,
        subscription_usd: sub,
        attribution_basis: 'per_burst_quota_delta',
        rollout_coverage: null,
      };
    }
    return {
      total_5h_pct: null, total_weekly_pct: null,
      total_monthly_pct: null, total_usd: null,
      // Legacy aliases.
      weekly_pct: null, monthly_burn_pct: null, monthly_burn_usd: null,
      subscription_usd: sub, attribution_basis: 'unavailable',
      rollout_coverage: null,
    };
  }

  for (const r of byProviderArr) {
    const fields = applyCostFields(
      r,
      r.provider === 'codex' ? modelByProv[r.provider] : null,
      burnPctByProv[r.provider],
    );
    Object.assign(r, fields);
  }
  for (const r of byProviderCategoryArr) {
    const k = `${r.provider}::${r.category}`;
    const fields = applyCostFields(
      r,
      r.provider === 'codex' ? modelByProvCat[k] : null,
      burnPctByProvCat[k],
    );
    Object.assign(r, fields);
    const provTokens = providerTotalTokens[r.provider] || 0;
    if (provTokens > 0) {
      r.token_share = Number((totalTokens(r) / provTokens).toFixed(4));
    }
  }

  // Merge stage wall-clock + check-subcommand totals into one
  // wall-clock breakdown table. Stages come from event_log ts_ms
  // deltas (Worker/Reviewer/...); check rows come from
  // .trellis/logs/check-ledger.jsonl (lake compile, sync, etc.). Both
  // are real supervisor wall-clock; presenting them in one table makes
  // the picture coherent. Drop the per-call "average" — only totals
  // matter when comparing time spent across categories.
  const wallClockArr = [];
  for (const [name, v] of Object.entries(byStage)) {
    wallClockArr.push({ kind: 'stage', name, duration_s: v.duration_s, intervals: v.intervals });
  }
  for (const [name, v] of Object.entries(byCheckSub)) {
    wallClockArr.push({ kind: 'check', name, duration_s: v.duration_s, count: v.count });
  }
  for (const [name, v] of Object.entries(byGitSub)) {
    wallClockArr.push({ kind: 'git', name, duration_s: v.duration_s, count: v.count });
  }
  wallClockArr.sort((a, b) => b.duration_s - a.duration_s);

  // Codex wall-clock decomposition (tool_exec / file_change / llm) computed
  // ON DEMAND from the codex rollout files under the burst user's
  // ~/.codex/sessions/. Spawn the python module once per
  // page load and pass it the same ledger this rollup is reading.
  // Best-effort: any failure (timeout, missing python module, etc.) just
  // omits the table.
  let codexTimingByProvider = [];
  try {
    const ledgerPath = path.join(repo, '.trellis', 'logs', 'cost-ledger.jsonl');
    const env = {
      ...process.env,
      PYTHONPATH: process.env.PYTHONPATH
        ? `${TRELLIS_ROOT}:${process.env.PYTHONPATH}`
        : TRELLIS_ROOT,
    };
    const raw = execFileSync('python3', [
      '-m', 'trellis.codex_timing', 'aggregate',
      '--ledger', ledgerPath, '--by', 'provider', '--codex-only',
    ], {
      cwd: TRELLIS_ROOT,
      env,
      encoding: 'utf-8',
      timeout: 15000,
      maxBuffer: 16 * 1024 * 1024,
    });
    const parsed = JSON.parse(raw);
    if (Array.isArray(parsed)) codexTimingByProvider = parsed;
  } catch (e) {
    codexTimingByProvider = [];
  }

  return {
    runtime_root: runtimeRoot || null,
    counts: { cost_rows: cost.length, check_rows: check.length, quota_rows: quota.length, event_rows: eventRows },
    by_provider: byProviderArr,
    by_provider_category: byProviderCategoryArr,
    wall_clock: wallClockArr,
    codex_timing_by_provider: codexTimingByProvider,
    quota: latestQuota,
  };
}

// In-process TTL cache for the usage rollup. buildUsageRollup spawns two
// Python subprocesses (aggregate + cost-rollup) — ~5s of subprocess time
// on top of the event-log fold. The cache makes every page load after the
// first cheap; the run state evolves on a ~minute scale so a 30s TTL is
// plenty fresh.
//
// This TTL is load-bearing only because the miss is now fast. Before
// `foldEventLogStageWalltime` became incremental the whole build took 37-45s
// on a large run — LONGER than the TTL — so the entry was always stale by the
// time the next request arrived and every single request paid full price.
// Measured on a large run (1358 cycle files, 5.7 GB): 43-45s per request
// before, 5.3s on a miss and ~0s on a hit after. If the build ever creeps
// back past 30s the cache silently stops working again.
const USAGE_ROLLUP_TTL_MS = 30 * 1000;
const _usageRollupCache = new Map();  // key: project slug → { ts, value }

function buildUsageRollupCached(projectInfo) {
  const key = projectInfo.slug || projectInfo.repoPath;
  const entry = _usageRollupCache.get(key);
  const now = Date.now();
  if (entry && (now - entry.ts) < USAGE_ROLLUP_TTL_MS) {
    return entry.value;
  }
  const value = buildUsageRollup(projectInfo);
  _usageRollupCache.set(key, { ts: now, value });
  return value;
}

app.get(`${BASE}/api/usage.json`, (req, res) => {
  try {
    const projectInfo = resolveRepoPath(defaultProjectSlug());
    res.json(buildUsageRollupCached(projectInfo));
  } catch (e) { res.status(500).json({ error: e.message }); }
});
app.get(`${BASE}/:project/api/usage.json`, (req, res) => {
  try {
    const projectInfo = resolveRepoPath(projectFromRequest(req));
    res.json(buildUsageRollupCached(projectInfo));
  } catch (e) { res.status(500).json({ error: e.message }); }
});

// =====================================================================
// /api/kernel-pane — capture-pane the supervisor's tmux session.
// Falls back to tailing the most recent run-resume-*.log under the
// runtime root if no tmux session exists yet (transitional case).
// =====================================================================

function findSupervisorTmuxSession(projectInfo) {
  // Convention: trellis-run-<project_slug>. The slug is derived from the
  // project entry; fall back to scanning live tmux sessions for any
  // trellis-run-* and picking the one whose name matches the project.
  let lsOut = '';
  try {
    lsOut = execFileSync('tmux', tmuxArgs('ls', '-F', '#{session_name}'), {
      encoding: 'utf8', stdio: ['ignore', 'pipe', 'ignore'],
    });
  } catch { return null; }
  const candidates = lsOut.split('\n').map((s) => s.trim())
    .filter((s) => s.startsWith('trellis-run-'));
  if (candidates.length === 0) return null;
  // If only one, use it. If multiple, prefer one whose suffix matches
  // the project slug or the repoPath basename.
  const slug = projectInfo.slug || path.basename(projectInfo.repoPath);
  const exact = candidates.find((n) => n === `trellis-run-${slug}`);
  if (exact) return exact;
  const partial = candidates.find((n) => n.includes(slug));
  return partial || candidates[0];
}

function findLatestRunResumeLog(runtimeRoot) {
  if (!runtimeRoot) return null;
  const logsDir = path.join(runtimeRoot, 'logs');
  if (!fs.existsSync(logsDir)) return null;
  let best = null;
  for (const name of fs.readdirSync(logsDir)) {
    if (!/^run-resume-\d+\.log$/.test(name)) continue;
    const p = path.join(logsDir, name);
    try {
      const mt = fs.statSync(p).mtimeMs;
      if (!best || mt > best.mtime) best = { path: p, mtime: mt };
    } catch {}
  }
  return best ? best.path : null;
}

function readKernelPane(projectInfo, opts) {
  const tail = (opts && opts.tail) || 200;
  const out = { events: [], pane: { source: 'none', text: '' } };

  // Structured event-log tail. This is the supervisor's actual activity
  // signal: cycle starts, wrapper responses, commit_checkpoints, etc. The
  // recent records live in the highest-numbered per-cycle files; walk them
  // backward, accumulating lines until we have at least `tail` of them.
  const eventLogDir = eventLogDirForProject(projectInfo);
  const cycleFiles = eventLogCycleFiles(eventLogDir);
  if (cycleFiles.length) {
    try {
      let lines = [];
      for (let i = cycleFiles.length - 1; i >= 0 && lines.length < tail; i--) {
        const fileLines = fs.readFileSync(cycleFiles[i], 'utf8').split('\n').filter(Boolean);
        lines = fileLines.concat(lines);
      }
      const recent = lines.slice(-tail);
      for (const line of recent) {
        try {
          const r = JSON.parse(line);
          const ev = r.event || {};
          const kind = String(ev.event || '?');
          const summary = summarizeEvent(r, ev);
          const cmds = (r.commands || []).map((c) => (c && c.command) || '?');
          out.events.push({
            index: r.index, kind, summary, commands: cmds,
          });
        } catch {}
      }
    } catch (e) {
      out.event_error = String(e.message || e);
    }
  }

  // Live tmux pane. Blank in normal operation — catches errors / stack
  // traces that the kernel writes to stdout/stderr when something
  // unexpected happens.
  const session = findSupervisorTmuxSession(projectInfo);
  if (session) {
    try {
      const text = execFileSync('tmux',
        tmuxArgs('capture-pane', '-t', session, '-p', '-S', `-${tail * 4}`),
        { encoding: 'utf8', stdio: ['ignore', 'pipe', 'ignore'], maxBuffer: 4 * 1024 * 1024 });
      out.pane = { source: 'tmux', session, text: (text || '').trimEnd() };
    } catch (e) {
      out.pane = { source: 'tmux', session, text: '', error: String(e.message || e) };
    }
  } else {
    const logPath = findLatestRunResumeLog(runtimeRoot);
    if (logPath) {
      try {
        const buf = fs.readFileSync(logPath, 'utf8');
        const lines = buf.split('\n');
        const slice = lines.slice(Math.max(0, lines.length - tail)).join('\n');
        out.pane = { source: 'logfile', logfile: logPath, text: slice };
      } catch (e) {
        out.pane = { source: 'logfile', logfile: logPath, text: '', error: String(e.message || e) };
      }
    } else {
      out.pane = {
        source: 'none', text: '',
        note: 'no trellis-run-* tmux session and no run-resume-*.log',
      };
    }
  }
  return out;
}

// Compact one-line summary of a kernel event for the activity tail.
function summarizeEvent(record, ev) {
  const kind = String(ev.event || '?');
  if (kind === 'start_cycle') {
    const req = (record.commands || [])
      .map((c) => (c && c.request) || null).find(Boolean);
    if (req) {
      return `cycle=${req.cycle} ${req.kind || '?'}#${req.id || '?'} active=${req.active_node || '—'} mode=${req.mode || '—'}`;
    }
    return '';
  }
  if (kind === 'wrapper_response') {
    const resp = ev.response || {};
    if (!resp || typeof resp !== 'object') return '';
    // Response is a flat dict with `kind` discriminating the variant.
    const parts = [];
    if (resp.kind) parts.push(`kind=${resp.kind}`);
    if (resp.request_id != null) parts.push(`req=${resp.request_id}`);
    if (resp.cycle != null) parts.push(`cycle=${resp.cycle}`);
    if (resp.status && resp.status !== 'Ok') parts.push(`status=${resp.status}`);
    if (resp.outcome) parts.push(`outcome=${resp.outcome}`);
    if (resp.decision) {
      const dec = String(resp.decision);
      const next = resp.next_active ? ` next=${resp.next_active}` : '';
      const reset = resp.reset && resp.reset !== 'None' ? ` reset=${resp.reset}` : '';
      parts.push(`decision=${dec}${next}${reset}`);
    }
    return parts.join(' ');
  }
  return '';
}

app.get(`${BASE}/api/kernel-pane.json`, (req, res) => {
  try {
    const projectInfo = resolveRepoPath(defaultProjectSlug());
    res.json(readKernelPane(projectInfo, { tail: parseInt(req.query.tail || 200, 10) || 200 }));
  } catch (e) { res.status(500).json({ error: e.message }); }
});
app.get(`${BASE}/:project/api/kernel-pane.json`, (req, res) => {
  try {
    const projectInfo = resolveRepoPath(projectFromRequest(req));
    res.json(readKernelPane(projectInfo, { tail: parseInt(req.query.tail || 200, 10) || 200 }));
  } catch (e) { res.status(500).json({ error: e.message }); }
});

// =====================================================================
// /api/progress — time series of {total, corr_passing, sound_or_waived,
// sound_unknown, lean_closed, lean_closed_transitive} for all nodes across all
// supervisor2 checkpoint commits in the live repo.
// "lean_closed" is the kernel's shallow committed closed predicate. Cached
// per-sha (the data at a given sha never changes).
// =====================================================================

const PROGRESS_CACHE = new Map();  // sha -> {ts, data}
const PROGRESS_CACHE_MAX = 1024;
// v9: currentlyFailing no longer requires an approved-fingerprint match
// (first-time fails were uncountable), so cached per-commit series computed
// under v8 undercount the fail edge and must be recomputed.
// v10: attachSoundVerifierFailCounts ran on a truncated event log (see there),
// so every v9 series is missing sound_verifier_fail past the truncation point.
// The series cache is keyed on headSha, so a run whose head has stopped moving
// would keep serving the truncated edge forever; the version bump forces it.
const PROGRESS_DISK_CACHE_VERSION = 10;
// Persist the per-sha caches every this-many freshly-computed commits DURING a
// rebuild (not only at the end), so a worker that is interrupted — or that
// can't outrun a run committing a new checkpoint every ~minute — still leaves
// warm caches for the next worker to resume from. On a ~1157-checkpoint run
// this is ~8 flushes/build; each writes only the compact metrics/projection
// objects (not the 58 MB blobs), so the write cost is negligible against the
// git I/O it makes resumable. Env-overridable for ops tuning / deterministic
// tests; a non-positive or non-numeric value falls back to the default.
const PROGRESS_FLUSH_EVERY = (() => {
  const n = parseInt(process.env.PROGRESS_FLUSH_EVERY || '', 10);
  return Number.isFinite(n) && n > 0 ? n : 150;
})();
const PROGRESS_DISK_CACHE_LOADED = new Set();
const PROGRESS_SERIES_CACHE = new Map();  // repoPath -> {headSha, ts, data}
const PROGRESS_WORKERS = new Map();  // repoPath -> {headSha, child}

function progressCacheKey(repoPath, sha) {
  return `${repoPath}\0${sha}`;
}

function trimDisabledProgressBuckets(data) {
  if (!data || typeof data !== 'object') return data;
  delete data.coarse;
  delete data.coarse_proofs_only;
  delete data.coarse_fallback;
  delete data.lean_proof_words;
  for (const bucketName of ['all', 'all_proofs_only']) {
    if (data[bucketName] && typeof data[bucketName] === 'object') {
      delete data[bucketName].lean_proof_words;
    }
  }
  if (Array.isArray(data.checkpoints)) {
    for (const checkpoint of data.checkpoints) trimDisabledProgressBuckets(checkpoint);
  }
  return data;
}

function progressDiskCachePath(projectInfo) {
  return path.join(viewerApiDir(projectInfo), `progress-cache-v${PROGRESS_DISK_CACHE_VERSION}.json`);
}

function progressSeriesDiskCachePath(projectInfo) {
  return path.join(viewerApiDir(projectInfo), `progress-series-cache-v${PROGRESS_DISK_CACHE_VERSION}.json`);
}

function isProofNodeKind(kind) {
  const k = String(kind || '').toLowerCase();
  return k !== 'definition' && k !== 'preamble';
}

// Find the FIRST byte of the `-- BODY` marker line, or -1 if no such
// line exists / there's more than one. The marker line is the FILESPEC
// v2 statement/proof boundary; mirrors the kernel's
// `filespec_split::find_marker_line` in viewer-display semantics.
function findBodyMarkerStart(text) {
  if (!text) return -1;
  let offset = 0;
  let first = -1;
  for (const line of text.split('\n')) {
    if (line.trim() === '-- BODY') {
      if (first !== -1) return -1;  // multiple markers → ambiguous
      first = offset;
    }
    offset += line.length + 1;  // +1 for the consumed '\n'
  }
  return first;
}

function extractLeanProofTextForMetrics(leanText) {
  const text = String(leanText || '');
  const markerStart = findBodyMarkerStart(text);
  if (markerStart >= 0) {
    // Body = everything after the marker line (skip past its newline).
    const eol = text.indexOf('\n', markerStart);
    return text.slice(eol < 0 ? markerStart : eol + 1).trim();
  }
  // Pre-migration fallback: strip imports/comments, then split at the
  // last `:=`. Wrong on let-in-type signatures and (k := k) named args,
  // but no longer load-bearing for current HEAD content — only
  // relevant for historical commits shown in the viewer's git-history
  // pane.
  const stripped = text.replace(/^(?:\s*--[^\n]*\n|\s*import[^\n]*\n)+/g, '');
  const idx = stripped.lastIndexOf(':=');
  if (idx < 0) return '';
  return stripped.slice(idx + 2).trim();
}

function extractTexProofTextForMetrics(texText) {
  const m = String(texText || '').match(/\\begin\{proof\}[\s\S]*?\\end\{proof\}/);
  return m ? m[0] : '';
}

function texNaturalLanguageWordCount(texText) {
  const prose = String(texText || '')
    .replace(/\$\$[\s\S]*?\$\$/g, ' ')
    .replace(/\\\[[\s\S]*?\\\]/g, ' ')
    .replace(/\\\([\s\S]*?\\\)/g, ' ')
    .replace(/\$[^$\n]*\$/g, ' ')
    .replace(/%[^\n]*/g, ' ')
    .replace(/\\(?:begin|end)\{[^}]*\}/g, ' ')
    .replace(/\\[A-Za-z]+\*?(?:\[[^\]]*\])?(?:\{[^{}]*\})?/g, ' ')
    .replace(/[{}\\^_&~#]/g, ' ');
  const words = prose.match(/[A-Za-z]+(?:[-'][A-Za-z]+)*/g);
  return words ? words.length : 0;
}

// Truthful "compiled-proof uses sorryAx" would require running lean per node;
// this is the cheap text-only approximation: strip comments, then look for a
// literal `sorry` — but treat the file as sorry-free if it contains a
// `local macro_rules` rule that rewrites the `sorry` tactic to a real proof
// (the literal token is in the source but the compiled term contains no
// sorryAx). Mirrors trellis/viewer_adapter.py.
const macroRulesSorryRe = /macro_rules\s*\|\s*`\(\s*(?:tactic|term)\|\s*sorry\s*\)\s*=>/;
const sketchProofRe = /\\begin\{proof\}([\s\S]*?)\\end\{proof\}/;

// Mirrors `_proof_starts_with_sketch_marker` in trellis/agent_wrapper/executor.py:
// first non-blank line of the \begin{proof}…\end{proof} block is exactly
// "SKETCH:". Such nodes would be auto-failed by the supervisor's
// `_maybe_synthesize_sketch_soundness_artifact` if Sound-dispatched, even
// without a real verifier round-trip.
function texProofStartsWithSketch(texContent) {
  const m = sketchProofRe.exec(texContent || '');
  if (!m) return false;
  for (const line of m[1].split('\n')) {
    const s = line.trim();
    if (!s) continue;
    return s === 'SKETCH:';
  }
  return false;
}

// ---------------------------------------------------------------------------
// Tablet blob layer (content-addressed).
//
// The progress walk used to run one `git show <sha>:Tablet/<node>.<ext>` per
// node per checkpoint — a shell plus a git process each, ~1,600 spawns per
// checkpoint late in a run, 1.0M across a measured 1,028-checkpoint walk, and
// ~90% of its cost (4.6 s of the ~5 s per checkpoint). Tablet churn between
// consecutive checkpoints is a handful of files, so those 1.0M reads carry
// only ~3,000 DISTINCT blob shas. So: one `git ls-tree` per checkpoint for the
// node -> blob-sha map, blob-sha-keyed projection caches, and one batched
// `git cat-file --batch` for the shas not yet projected — 1,918 git spawns and
// ~10 s for the same walk. A blob's content is immutable, so a projection is
// never invalidated: not by a rewind, not by a series-cache version bump.
const TABLET_BLOB_DISK_VERSION = 1;
const LEAN_BLOB_CACHE = new Map();  // `${repoPath}\0${blobSha}` -> {sorry, chars}
const TEX_BLOB_CACHE = new Map();   // `${repoPath}\0${blobSha}` -> {words, sketch}
const TABLET_BLOB_DISK_LOADED = new Set();
// Sized for the one checkpoint that misses on everything: the first of a cold
// walk fetches the whole tablet in a single batch.
const TABLET_BATCH_MAX_BUFFER = 512 * 1024 * 1024;

// Cross-check gate for the enumeration risk: the old walk asked git for one
// path per `node_kinds` entry and read a failure as "file absent", whereas
// ls-tree enumerates what the tree actually holds. Set
// PROGRESS_TABLET_ASSERT_CHECKPOINTS=N to compare the two file sets over the
// first N checkpoints this process projects and log any disagreement.
const PROGRESS_TABLET_ASSERT_CHECKPOINTS = (() => {
  const n = parseInt(process.env.PROGRESS_TABLET_ASSERT_CHECKPOINTS || '', 10);
  return Number.isFinite(n) && n > 0 ? n : 0;
})();
let tabletAssertBudget = PROGRESS_TABLET_ASSERT_CHECKPOINTS;

function tabletBlobDiskPath(projectInfo) {
  return path.join(viewerApiDir(projectInfo), `tablet-blob-cache-v${TABLET_BLOB_DISK_VERSION}.json`);
}

function loadTabletBlobDiskCache(projectInfo) {
  const repoPath = projectInfo.repoPath;
  if (TABLET_BLOB_DISK_LOADED.has(repoPath)) return;
  TABLET_BLOB_DISK_LOADED.add(repoPath);
  const p = tabletBlobDiskPath(projectInfo);
  if (!fs.existsSync(p)) return;
  let parsed;
  try { parsed = JSON.parse(fs.readFileSync(p, 'utf8')); } catch { return; }
  if (!parsed || parsed.version !== TABLET_BLOB_DISK_VERSION) return;
  for (const [field, cache] of [['lean', LEAN_BLOB_CACHE], ['tex', TEX_BLOB_CACHE]]) {
    for (const [blobSha, value] of Object.entries(parsed[field] || {})) {
      if (!blobSha || !value) continue;
      cache.set(progressCacheKey(repoPath, blobSha), value);
    }
  }
}

function saveTabletBlobDiskCache(projectInfo) {
  const repoPath = projectInfo.repoPath;
  const prefix = `${repoPath}\0`;
  const p = tabletBlobDiskPath(projectInfo);
  const out = { version: TABLET_BLOB_DISK_VERSION, generated_at: new Date().toISOString(), lean: {}, tex: {} };
  // Union with the existing file for the same reasons as saveProgressDiskCache
  // (resumable periodic flushes, and this cache outliving any single walk).
  try {
    const existing = JSON.parse(fs.readFileSync(p, 'utf8'));
    if (existing && existing.version === TABLET_BLOB_DISK_VERSION) {
      Object.assign(out.lean, existing.lean || {});
      Object.assign(out.tex, existing.tex || {});
    }
  } catch { /* no prior file, or unreadable — start fresh */ }
  for (const [field, cache] of [['lean', LEAN_BLOB_CACHE], ['tex', TEX_BLOB_CACHE]]) {
    for (const [key, value] of cache.entries()) {
      if (!key.startsWith(prefix)) continue;
      out[field][key.slice(prefix.length)] = value;
    }
  }
  fs.mkdirSync(path.dirname(p), { recursive: true });
  const tmp = `${p}.tmp`;
  fs.writeFileSync(tmp, JSON.stringify(out));
  fs.renameSync(tmp, p);
}

// {lean: {node -> blobSha}, tex: {node -> blobSha}} for one commit. `-z` emits
// raw paths, so node names that git would otherwise quote survive intact.
function tabletBlobShasForCommit(repoPath, sha, nodeExt) {
  const out = { lean: {}, tex: {} };
  const leanSuffix = `.${nodeExt}`;
  let raw;
  try {
    raw = execFileSync('git', ['-C', repoPath, 'ls-tree', '-r', '-z', sha, '--', 'Tablet/'],
      { encoding: 'utf8', stdio: ['ignore', 'pipe', 'ignore'], maxBuffer: TABLET_BATCH_MAX_BUFFER });
  } catch {
    return out;
  }
  for (const record of raw.split('\0')) {
    if (!record) continue;
    const tab = record.indexOf('\t');
    if (tab < 0) continue;
    const fields = record.slice(0, tab).split(' ');
    if (fields.length < 3 || fields[1] !== 'blob') continue;
    const filePath = record.slice(tab + 1);
    if (!filePath.startsWith('Tablet/')) continue;
    const name = filePath.slice('Tablet/'.length);
    if (name.includes('/')) continue;  // Tablet/paper/… and friends are not nodes
    if (name.endsWith(leanSuffix)) out.lean[name.slice(0, -leanSuffix.length)] = fields[2];
    else if (name.endsWith('.tex')) out.tex[name.slice(0, -'.tex'.length)] = fields[2];
  }
  return out;
}

// Parse `git cat-file --batch` output into blobSha -> content. Framing is
// `<sha> <type> <size>\n<content>\n`; an unresolvable object emits a bodiless
// `<sha> missing\n`. Slice by the declared byte length — proof text is full of
// newlines, so hunting for the next record boundary would desync the stream —
// and stop on anything unparseable rather than mis-attributing bytes to a sha.
function parseCatFileBatch(buf) {
  const out = new Map();
  let pos = 0;
  while (pos < buf.length) {
    const nl = buf.indexOf(0x0a, pos);
    if (nl < 0) break;
    const fields = buf.toString('utf8', pos, nl).split(' ');
    pos = nl + 1;
    if (fields.length < 3) continue;  // `<sha> missing` and other bodiless errors
    const size = Number(fields[2]);
    if (!Number.isInteger(size) || size < 0 || pos + size > buf.length) break;
    out.set(fields[0], buf.toString('utf8', pos, pos + size));
    pos += size;
    if (buf[pos] === 0x0a) pos++;
  }
  return out;
}

function catFileBatchBlobs(repoPath, shas) {
  if (!shas.length) return new Map();
  const res = spawnSync('git', ['-C', repoPath, 'cat-file', '--batch'], {
    input: `${shas.join('\n')}\n`,
    maxBuffer: TABLET_BATCH_MAX_BUFFER,
    stdio: ['pipe', 'pipe', 'ignore'],
  });
  if (res.error || !res.stdout) return new Map();
  return parseCatFileBatch(res.stdout);
}

function leanBlobProjection(leanText) {
  const text = String(leanText || '');
  const cleaned = text
    .replace(/\/-[\s\S]*?-\//g, '')
    .replace(/--[^\n]*/g, '');
  const sorry = /\bsorry\b/.test(cleaned) && !macroRulesSorryRe.test(cleaned);
  return { sorry, chars: extractLeanProofTextForMetrics(text).length };
}

function texBlobProjection(texText) {
  return {
    words: texNaturalLanguageWordCount(extractTexProofTextForMetrics(texText)),
    sketch: texProofStartsWithSketch(texText),
  };
}

// Per-node Tablet metrics for one checkpoint, given its node -> blob-sha map.
// A node listed in `node_kinds` with no Tablet file gets hasSorry=false and
// contributes no proof metrics — the pre-content-addressing behavior, which
// the historical series depends on.
function tabletBlobProjections(repoPath, presentNodes, nodeKinds, blobShas,
                               fetchBlobs = (shas) => catFileBatchBlobs(repoPath, shas)) {
  const hasSorry = {};
  const hasSketch = {};
  const leanProofMetrics = {};
  const nlProofWordCounts = {};
  const wanted = new Set();
  const missing = (cache, blobSha) => blobSha && !cache.has(progressCacheKey(repoPath, blobSha));
  for (const node of presentNodes) {
    if (node === 'Preamble') continue;
    const leanSha = blobShas.lean[node];
    if (!leanSha) continue;
    if (missing(LEAN_BLOB_CACHE, leanSha)) wanted.add(leanSha);
    if (isProofNodeKind(nodeKinds[node]) && missing(TEX_BLOB_CACHE, blobShas.tex[node])) {
      wanted.add(blobShas.tex[node]);
    }
  }
  const fetched = wanted.size ? fetchBlobs([...wanted]) : new Map();
  const project = (cache, blobSha, projector) => {
    const key = progressCacheKey(repoPath, blobSha);
    const cached = cache.get(key);
    if (cached) return cached;
    const content = fetched.get(blobSha);
    if (content === undefined) return null;
    const value = projector(content);
    cache.set(key, value);
    return value;
  };
  for (const node of presentNodes) {
    if (node === 'Preamble') { hasSorry[node] = false; continue; }
    const leanSha = blobShas.lean[node];
    const lean = leanSha ? project(LEAN_BLOB_CACHE, leanSha, leanBlobProjection) : null;
    if (!lean) { hasSorry[node] = false; continue; }
    if (isProofNodeKind(nodeKinds[node])) {
      leanProofMetrics[node] = { chars: lean.chars };
      const texSha = blobShas.tex[node];
      const tex = texSha ? project(TEX_BLOB_CACHE, texSha, texBlobProjection) : null;
      nlProofWordCounts[node] = tex ? tex.words : 0;
      if (tex && tex.sketch) hasSketch[node] = true;
    }
    hasSorry[node] = lean.sorry;
  }
  return { hasSorry, hasSketch, leanProofMetrics, nlProofWordCounts };
}

function assertTabletBlobSets(repoPath, sha, nodeExt, presentNodes, blobShas) {
  if (tabletAssertBudget <= 0) return;
  tabletAssertBudget--;
  let mismatches = 0;
  for (const node of presentNodes) {
    if (node === 'Preamble') continue;
    for (const [ext, map] of [[nodeExt, blobShas.lean], ['tex', blobShas.tex]]) {
      let exists = true;
      try {
        execFileSync('git', ['-C', repoPath, 'cat-file', '-e', `${sha}:Tablet/${node}.${ext}`], { stdio: 'ignore' });
      } catch { exists = false; }
      if (exists !== !!map[node]) {
        mismatches++;
        console.error(`[progress] tablet file-set mismatch at ${sha} ${node}.${ext}: ls-tree=${!!map[node]} per-node=${exists}`);
      }
    }
  }
  console.error(`[progress] tablet file-set check ${sha}: ${presentNodes.length} nodes, ${mismatches} mismatches`);
}

function blockerKindKey(kind) {
  const k = String(kind || '').toLowerCase();
  if (k === 'paperfaithfulness') return 'paper';
  if (k === 'nodecorr') return 'corr';
  if (k === 'substantiveness') return 'subst';
  if (k === 'soundness') return 'sound';
  return null;
}

function taskBlockerMetrics(state) {
  const metrics = { total: 0, paper: 0, corr: 0, subst: 0, sound: 0 };
  const task = state && state.pending_task;
  const blockers = task && Array.isArray(task.task_blockers) ? task.task_blockers : [];
  for (const blocker of blockers) {
    metrics.total++;
    const key = blockerKindKey(blocker && blocker.kind);
    if (key && Object.prototype.hasOwnProperty.call(metrics, key)) metrics[key]++;
  }
  return metrics;
}

// --- trellis-shared-state/1 decoder ----------------------------------------
// Reader half of the structural-sharing container format for
// `.trellis-history/supervisor_state.json`. The normative spec is
// `SHARED_STATE_SPEC` in trellis/history_artifacts.py (which holds the
// reference encoder + decoder); the cross-language agreement gate is the
// golden vector set in tests/fixtures/shared_state_vectors/, driven from
// test_shared_state.js.
//
// Only the reading direction matters here: `decodeSharedState` is the identity
// on every plain snapshot ever committed (detection is structural — a JSON
// object carrying a `$format` member — so no flag, env var or SHA-ancestry
// test is involved) and expands the envelope the day the writer flips.
// Everything malformed throws SharedStateError; the decoder never substitutes
// a default, because a silently-empty projection here renders as a plausible
// but wrong chart.
const SHARED_STATE_FORMAT = 'trellis-shared-state/1';
// Only these top-level values are encoded; every other member passes through.
const SHARED_STATE_ENCODED_KEYS = ['checkpoint', 'state'];
const SHARED_STATE_ENVELOPE_KEYS = ['$format', '$strings', '$pool'];

class SharedStateError extends Error {
  constructor(message) {
    super(message);
    this.name = 'SharedStateError';
  }
}

// JSON-value type name, for error messages that read like the Python ones.
function sharedStateTypeName(value) {
  if (value === null) return 'null';
  if (Array.isArray(value)) return 'array';
  return typeof value;
}

function isPlainJsonObject(value) {
  return typeof value === 'object' && value !== null && !Array.isArray(value);
}

function hasOwn(obj, key) {
  return Object.prototype.hasOwnProperty.call(obj, key);
}

// `out[key] = value` invokes the inherited setter for the one key JSON can
// carry that is not an ordinary data property on an object literal. JSON.parse
// itself defines it as an own property, so the decoder must too or a
// `"__proto__"` member would vanish from the output.
function setJsonKey(out, key, value) {
  if (key === '__proto__') {
    Object.defineProperty(out, key, {
      value, writable: true, enumerable: true, configurable: true,
    });
  } else {
    out[key] = value;
  }
}

// Structural detection, per spec: a document is in this format iff it is a
// JSON object with a `$format` member.
function isSharedState(document) {
  return isPlainJsonObject(document) && hasOwn(document, '$format');
}

function deepCopyJson(value) {
  if (Array.isArray(value)) return value.map(deepCopyJson);
  if (!isPlainJsonObject(value)) return value;
  const out = {};
  for (const key of Object.keys(value)) setJsonKey(out, key, deepCopyJson(value[key]));
  return out;
}

// Expansion state: the two tables plus the active `$r` stack that turns a
// corrupt or hostile pool cycle into an error instead of an infinite descent.
class SharedStateExpander {
  constructor(strings, pool) {
    this.strings = strings;
    this.pool = pool;
    this.activeList = [];
    this.activeSet = new Set();
  }

  node(value) {
    if (Array.isArray(value)) return value.map((item) => this.node(item));
    if (!isPlainJsonObject(value)) return value;
    const keys = Object.keys(value);
    if (keys.length === 1) {
      const key = keys[0];
      if (key === '$r') return this.expand(value[key]);
      if (key === '$s') return this.string(value[key]);
      if (key === '$e') {
        const inner = value[key];
        if (!isPlainJsonObject(inner)) {
          throw new SharedStateError(`"$e" must hold an object, got ${sharedStateTypeName(inner)}`);
        }
        // The escaped object is a literal: its members are decoded, but its own
        // single-member sigil shape is NOT re-interpreted.
        const unwrapped = {};
        for (const k of Object.keys(inner)) setJsonKey(unwrapped, k, this.node(inner[k]));
        return unwrapped;
      }
    }
    const out = {};
    for (const k of keys) setJsonKey(out, k, this.node(value[k]));
    return out;
  }

  string(key) {
    if (typeof key !== 'string') {
      throw new SharedStateError(`"$s" reference must be a string, got ${sharedStateTypeName(key)}`);
    }
    if (!hasOwn(this.strings, key)) {
      throw new SharedStateError(`"$s" reference ${JSON.stringify(key)} is absent from $strings`);
    }
    const text = this.strings[key];
    if (typeof text !== 'string') {
      throw new SharedStateError(`$strings[${JSON.stringify(key)}] is not a string`);
    }
    return text;
  }

  // Every occurrence re-walks the stored body, so each expansion yields a fresh
  // object graph: two positions naming the same key never share a mutable
  // object, and a caller mutating one alias cannot affect another.
  expand(key) {
    if (typeof key !== 'string') {
      throw new SharedStateError(`"$r" reference must be a string, got ${sharedStateTypeName(key)}`);
    }
    if (!hasOwn(this.pool, key)) {
      throw new SharedStateError(`"$r" reference ${JSON.stringify(key)} is absent from $pool`);
    }
    if (this.activeSet.has(key)) {
      throw new SharedStateError(`$pool reference cycle: ${this.activeList.concat([key]).join(' -> ')}`);
    }
    this.activeList.push(key);
    this.activeSet.add(key);
    try {
      return this.node(this.pool[key]);
    } finally {
      this.activeList.pop();
      this.activeSet.delete(key);
    }
  }
}

// Return `document` expanded, or `document` itself (same reference) if it is a
// plain snapshot. Throws SharedStateError on anything malformed.
function decodeSharedState(document) {
  if (!isSharedState(document)) {
    if (isPlainJsonObject(document)) {
      for (const key of ['$strings', '$pool']) {
        if (hasOwn(document, key)) {
          throw new SharedStateError(
            `document carries ${key} but no $format member; it is neither a plain `
            + 'snapshot nor a well-formed shared-state document');
        }
      }
    }
    return document;
  }
  const fmt = document.$format;
  if (fmt !== SHARED_STATE_FORMAT) {
    throw new SharedStateError(`unsupported supervisor-state format: ${JSON.stringify(fmt)}`);
  }
  const strings = document.$strings;
  const pool = document.$pool;
  if (!isPlainJsonObject(strings)) {
    throw new SharedStateError('shared-state document has no $strings object');
  }
  if (!isPlainJsonObject(pool)) {
    throw new SharedStateError('shared-state document has no $pool object');
  }
  const expander = new SharedStateExpander(strings, pool);
  const out = {};
  for (const key of Object.keys(document)) {
    if (SHARED_STATE_ENVELOPE_KEYS.includes(key)) continue;
    if (key.startsWith('$')) {
      throw new SharedStateError(
        `unrecognised envelope member ${JSON.stringify(key)} for ${SHARED_STATE_FORMAT}; `
        + 'an extension must bump $format');
    }
    if (SHARED_STATE_ENCODED_KEYS.includes(key)) {
      setJsonKey(out, key, expander.node(document[key]));
    } else {
      setJsonKey(out, key, deepCopyJson(document[key]));
    }
  }
  return out;
}

// The committed supervisor_state.json grows with the run (local-closure
// record mirrors + progress_history dominate): ~33 MB around cycle 920,
// ~48 MB by cycle 945 and still climbing. The old 32 MiB `maxBuffer` made
// `git show` throw ENOBUFS for every checkpoint past that threshold, and the
// catch below silently dropped them — freezing the progress series at the
// last sub-32 MiB cycle (921). Use a cap far above any realistic state size
// so valid checkpoints are never silently truncated, and surface any failure
// (missing file vs. an actual size overflow) instead of swallowing it.
const SUPERVISOR_STATE_MAX_BUFFER = 1024 * 1024 * 1024; // 1 GiB
// Read + parse a commit's whole supervisor_state.json blob (the OUTER object:
// `{ event_count, state: {...}, ... }`). Both the main progress walk (which
// wants `.state`) and the sound-cpinfo sidecar (which additionally wants the
// outer `event_count`) project from this same ~58 MB blob, so exposing the
// full parse lets a single `git show` feed both — see progressForCommit, which
// populates the sound-cpinfo sidecar from the same read.
function readSupervisorParsedForCommit(repoPath, sha) {
  let stateRaw;
  try {
    stateRaw = execFileSync('git', ['-C', repoPath, 'show', `${sha}:.trellis-history/supervisor_state.json`],
      { encoding: 'utf8', stdio: ['ignore', 'pipe', 'ignore'], maxBuffer: SUPERVISOR_STATE_MAX_BUFFER });
  } catch (e) {
    // ENOBUFS here means a valid-but-oversized state was dropped (the bug this
    // cap was raised to prevent); log loudly rather than silently truncate.
    if (e && (e.code === 'ENOBUFS' || /maxBuffer/i.test(String(e.message || '')))) {
      console.error(`[progress] readSupervisorStateForCommit(${sha}) exceeded maxBuffer (${SUPERVISOR_STATE_MAX_BUFFER} bytes); checkpoint dropped — raise SUPERVISOR_STATE_MAX_BUFFER`);
    }
    return null;
  }
  let parsed;
  try {
    parsed = JSON.parse(stateRaw);
  } catch {
    return null;
  }
  // The single point where this artifact enters the viewer, so decoding sits
  // immediately after the parse and every downstream projection
  // (progressForCommit, soundCpInfoFromParsed, readSupervisorStateForCommit)
  // sees the plain shape it has always seen. Deliberately outside the catch
  // above: a shared-state decode failure means the file is corrupt, and it must
  // propagate rather than become a `null` that silently drops the checkpoint
  // from the chart.
  return decodeSharedState(parsed);
}

function readSupervisorStateForCommit(repoPath, sha) {
  const parsed = readSupervisorParsedForCommit(repoPath, sha);
  if (!parsed) return null;
  return parsed.state || {};
}

function ensureTaskBlockerMetrics(repoPath, sha, data) {
  if (!data || data.task_blockers !== undefined) return false;
  const state = readSupervisorStateForCommit(repoPath, sha);
  data.task_blockers = taskBlockerMetrics(state || {});
  return true;
}

function progressSeriesHasTaskBlockers(data) {
  return !!(data && Array.isArray(data.checkpoints)
    && data.checkpoints.every((c) => c && c.task_blockers !== undefined));
}

function repoHeadSha(repoPath) {
  try {
    return execFileSync('git', ['-C', repoPath, 'rev-parse', 'HEAD'], {
      encoding: 'utf8',
      stdio: ['ignore', 'pipe', 'ignore'],
    }).trim();
  } catch {
    return '';
  }
}

function loadProgressDiskCache(projectInfo) {
  const repoPath = projectInfo.repoPath;
  if (PROGRESS_DISK_CACHE_LOADED.has(repoPath)) return;
  PROGRESS_DISK_CACHE_LOADED.add(repoPath);
  const p = progressDiskCachePath(projectInfo);
  if (!fs.existsSync(p)) return;
  let parsed;
  try {
    parsed = JSON.parse(fs.readFileSync(p, 'utf8'));
  } catch {
    return;
  }
  if (!parsed || parsed.version !== PROGRESS_DISK_CACHE_VERSION || !parsed.entries) return;
  for (const [sha, data] of Object.entries(parsed.entries)) {
    if (!sha || !data) continue;
    PROGRESS_CACHE.set(progressCacheKey(repoPath, sha), { ts: Date.now(), data });
  }
}

function saveProgressDiskCache(projectInfo) {
  const repoPath = projectInfo.repoPath;
  const prefix = `${repoPath}\0`;
  const p = progressDiskCachePath(projectInfo);
  // Merge with any existing on-disk entries before writing. Two reasons this
  // must be a union rather than a snapshot of the in-memory cache:
  //   1. Periodic mid-walk flushes: a later flush must not drop commits an
  //      earlier flush already persisted but that have since been LRU-evicted.
  //   2. The in-memory PROGRESS_CACHE is bounded (PROGRESS_CACHE_MAX); on a run
  //      with more checkpoints than that bound, early commits are evicted
  //      during the walk and a snapshot save would silently lose them.
  // A sha's committed data is immutable and the cache version is part of the
  // filename, so the union is well-defined; in-memory wins on collision (an
  // identical value, or a task_blockers backfill on the cache-hit path).
  const entries = {};
  try {
    const existing = JSON.parse(fs.readFileSync(p, 'utf8'));
    if (existing && existing.version === PROGRESS_DISK_CACHE_VERSION && existing.entries) {
      Object.assign(entries, existing.entries);
    }
  } catch { /* no prior file, or unreadable — start fresh */ }
  for (const [key, value] of PROGRESS_CACHE.entries()) {
    if (!key.startsWith(prefix)) continue;
    entries[key.slice(prefix.length)] = value.data;
  }
  fs.mkdirSync(path.dirname(p), { recursive: true });
  const tmp = `${p}.tmp`;
  fs.writeFileSync(tmp, JSON.stringify({
    version: PROGRESS_DISK_CACHE_VERSION,
    generated_at: new Date().toISOString(),
    entries,
  }));
  fs.renameSync(tmp, p);
}

function readProgressSeriesDiskCache(projectInfo, headSha) {
  const p = progressSeriesDiskCachePath(projectInfo);
  if (!headSha || !fs.existsSync(p)) return null;
  let parsed;
  try {
    parsed = JSON.parse(fs.readFileSync(p, 'utf8'));
  } catch {
    return null;
  }
  if (!parsed || parsed.version !== PROGRESS_DISK_CACHE_VERSION || parsed.headSha !== headSha) return null;
  if (!parsed.data || !Array.isArray(parsed.data.checkpoints)) return null;
  if (!progressSeriesHasTaskBlockers(parsed.data)) return null;
  return trimDisabledProgressBuckets(parsed.data);
}

// Read the on-disk series regardless of which head it was built for. Used as a
// stale placeholder after a restart: in-memory PROGRESS_SERIES_CACHE is empty,
// so without this the tab shows nothing until a full (minutes-long) rebuild
// completes, even though the last completed series is sitting on disk.
function readProgressSeriesDiskCacheStale(projectInfo) {
  const p = progressSeriesDiskCachePath(projectInfo);
  if (!fs.existsSync(p)) return null;
  let parsed;
  try {
    parsed = JSON.parse(fs.readFileSync(p, 'utf8'));
  } catch {
    return null;
  }
  if (!parsed || parsed.version !== PROGRESS_DISK_CACHE_VERSION) return null;
  if (!parsed.data || !Array.isArray(parsed.data.checkpoints)) return null;
  return trimDisabledProgressBuckets(parsed.data);
}

function saveProgressSeriesDiskCache(projectInfo, headSha, data) {
  if (!headSha || !data) return;
  const p = progressSeriesDiskCachePath(projectInfo);
  fs.mkdirSync(path.dirname(p), { recursive: true });
  const tmp = `${p}.tmp`;
  fs.writeFileSync(tmp, JSON.stringify({
    version: PROGRESS_DISK_CACHE_VERSION,
    headSha,
    generated_at: new Date().toISOString(),
    data,
  }));
  fs.renameSync(tmp, p);
}

function progressForCommit(repoPath, sha) {
  const cacheKey = progressCacheKey(repoPath, sha);
  if (PROGRESS_CACHE.has(cacheKey)) {
    const data = trimDisabledProgressBuckets(PROGRESS_CACHE.get(cacheKey).data);
    ensureTaskBlockerMetrics(repoPath, sha, data);
    return data;
  }
  const parsed = readSupervisorParsedForCommit(repoPath, sha);
  if (!parsed) return null;
  const state = parsed.state || {};
  // Populate the sound-cpinfo sidecar from this same blob so
  // attachSoundVerifierFailCounts' second pass is a cache hit rather than a
  // redundant 58 MB re-read of the identical commit. Same cache key scheme
  // (repoPath\0sha). Skip if already present (warmed from disk or a prior read).
  if (!SOUND_CPINFO_CACHE.has(cacheKey)) {
    const cpInfo = soundCpInfoFromParsed(parsed);
    if (cpInfo) SOUND_CPINFO_CACHE.set(cacheKey, cpInfo);
  }
  const nodeExt = backendNodeExtForRepo(repoPath);
  const nodeKinds = state.node_kinds || {};
  const presentNodes = Object.keys(nodeKinds);
  // The historical snapshot writes `deps` (and `committed_deps`); the live
  // protocol_state.json calls the same field `current_deps`. Accept any.
  const deps = state.deps || state.committed_deps || state.current_deps || {};
  const corrStatus = state.corr_status || {};
  const soundStatus = state.sound_status || {};
  // Fingerprint maps so we can mirror the kernel's `current_*_state` logic:
  // a Fail/Structural status only counts as currently failing when the
  // approved fingerprint matches the current one. If they drift the kernel
  // treats it as Unknown (re-needs verification), and so does this chart.
  const corrApprovedFp = state.corr_approved_fingerprints || {};
  const corrCurrentFp = (state.live && state.live.corr_current_fingerprints) || {};
  const soundApprovedFp = state.sound_approved_fingerprints || {};
  const soundCurrentFp = (state.live && state.live.sound_current_fingerprints) || {};
  // Substantiveness lane was added in kernel commit 7198d6c (April 2026).
  // Older checkpoints predate it and omit these fields. Detect presence
  // at the lane level (any of the three field families present) so the
  // metrics emit `null` for pre-lane checkpoints — the chart's path()
  // helper breaks the line at nulls, rendering as a gap rather than a
  // misleading zero.
  const substantivenessStatusRaw = state.substantiveness_status;
  const substantivenessPresent = substantivenessStatusRaw !== undefined
    || state.substantiveness_approved_fingerprints !== undefined
    || (state.live && state.live.substantiveness_current_fingerprints !== undefined);
  const substantivenessStatus = substantivenessStatusRaw || {};
  const substantivenessApprovedFp = state.substantiveness_approved_fingerprints || {};
  const substantivenessCurrentFp =
    (state.live && state.live.substantiveness_current_fingerprints) || {};

  // Per-lane waiver, mirroring the kernel's `correspondence_waived` and
  // `substantiveness_waived` predicates (model.rs, D6 re-keyed via
  // `challenge_claim_waives_lanes`): a node is waived on BOTH lanes iff it
  // claims >=1 challenge target AND no claimed target's spec has
  // `statement_provenance: "worker_authored"` (absent spec / absent field
  // default to seed_pinned — the field is skip-serialized at its default).
  // Both kernel predicates share the one helper, so a single function
  // covers corr and subst. Empty / absent claims for non-challenge runs
  // (the field is `skip_serializing_if = is_empty`), so every callsite is
  // a no-op there — the non-challenge path is byte-identical. This is the
  // corr/subst analogue of the soundness "or waived" treatment below
  // (which waives on own-Lean-closed).
  const challengeClaims = state.challenge_claims || {};
  const challengeRegistry = state.configured_challenge_targets || {};
  function laneWaived(n) {
    const claims = challengeClaims[n];
    if (!Array.isArray(claims) || claims.length === 0) return false;
    return claims.every((target) => {
      const spec = challengeRegistry[String(target)];
      const provenance = (spec && spec.statement_provenance) || 'seed_pinned';
      return provenance !== 'worker_authored';
    });
  }

  function currentlyPassing(status, approvedFp, currentFp) {
    // Mirror the kernel's `current_*_state` predicate exactly (model.rs
    // ~line 1648). A node passes iff status=Pass AND both fingerprint
    // entries are PRESENT in the map AND equal. Use a strict
    // `=== undefined` check (not `!fp`) so an empty-string fingerprint
    // counts as present: Preamble has no .tex content to fingerprint, so
    // the kernel records both `corr_approved_fingerprints["Preamble"]` and
    // `live.corr_current_fingerprints["Preamble"]` as `Some("")`, which
    // satisfies the kernel's `Some(current) == Some(approved)` check.
    // The previous `!approvedFp` truthiness check incorrectly treated the
    // empty-string fingerprint as missing and excluded Preamble forever,
    // showing a permanent +1 gap between `total` and `corr_passing`.
    if (status !== 'Pass') return false;
    if (approvedFp === undefined || currentFp === undefined) return false;
    return approvedFp === currentFp;
  }

  // Mirror of currentlyPassing for the rejection side: Fail and Structural
  // mean "kernel currently considers this node not sound". Unlike the
  // passing side, NO fingerprint agreement is required: approved
  // fingerprints exist only for nodes that once PASSED, so requiring
  // approvedFp === currentFp made every first-time fail uncountable and
  // the fail series structurally zero (the dec2flt example had real fails at cycles
  // 282-293 never appeared in the band). Kernel sound_status stays
  // Fail through repair-edit drift until a new verdict lands, which is
  // exactly the operator-facing series this band should show.
  function currentlyFailing(status, approvedFp, currentFp) {
    return status === 'Fail' || status === 'Structural';
  }

  // Kernel-tracked closed-ness — what the DAG view colors blue/green.
  // A node is "closed" iff it's in committed.present_nodes AND NOT in
  // committed.open_nodes. This is the kernel's authoritative view: it
  // reflects the latest deterministic-checker outcome AND the kernel's
  // structural classification, not just .lean text grep. Use this for
  // chart progress so the chart matches what the DAG view shows the user.
  //
  // The hasSorry map below stays — it's still needed for the
  // sound_or_waived calculation (sound waives when own .lean is sorry-free,
  // independent of the kernel's lifecycle on the node).
  const committed = state.committed || {};
  const committedPresent = new Set(committed.present_nodes || []);
  const committedOpen = new Set(committed.open_nodes || []);
  // Patch C local-closure unverified set (per LOCAL_CLOSURE_IMPL_PLAN.md §9).
  // A sorry-free node not in `open_nodes` may still be `local_closure_unverified`:
  // the chart counts these in a parallel `lean_unverified` series so the operator
  // can see the gap between "kernel-closed but not yet locally verified" and
  // "fully verified." When the new fields are absent (pre-Patch-C / pre-migration
  // checkpoints) the set is empty and the series renders as zero.
  //
  // Tier alignment: this chart is committed-DAG-based (uses `committed.open_nodes`
  // above), so we prefer the committed mirror over the live-tier field. Fall back
  // to the live field for backward compat with pre-Patch-C-A snapshots.
  const localClosureUnverified = new Set(
    Array.isArray(state.committed_local_closure_unverified_nodes)
      ? state.committed_local_closure_unverified_nodes
      : (Array.isArray(state.local_closure_unverified_nodes)
          ? state.local_closure_unverified_nodes // fallback
          : [])
  );
  function isKernelClosed(n) {
    return committedPresent.has(n) && !committedOpen.has(n);
  }

  // Per-node .lean/.tex projections for this commit, via the content-addressed
  // Tablet blob layer (see tabletBlobProjections).
  const blobShas = tabletBlobShasForCommit(repoPath, sha, nodeExt);
  assertTabletBlobSets(repoPath, sha, nodeExt, presentNodes, blobShas);
  const { hasSorry, hasSketch, leanProofMetrics, nlProofWordCounts } =
    tabletBlobProjections(repoPath, presentNodes, nodeKinds, blobShas);

  // Recursively-closed memo: a node is "transitively kernel-closed" iff
  // IT is kernel-closed AND every direct dep is recursively kernel-closed.
  // Mirrors viewer/public/index.html `isRecursivelyClosed`. Cycle-safe.
  const transClosedMemo = {};
  function isRecursivelyKernelClosed(n, stack) {
    if (n in transClosedMemo) return transClosedMemo[n];
    if (stack.has(n)) return true;  // cycle guard
    if (!isKernelClosed(n)) { transClosedMemo[n] = false; return false; }
    stack.add(n);
    const childList = Array.isArray(deps[n]) ? deps[n] : [];
    for (const c of childList) {
      if (!isRecursivelyKernelClosed(c, stack)) {
        stack.delete(n);
        transClosedMemo[n] = false;
        return false;
      }
    }
    stack.delete(n);
    transClosedMemo[n] = true;
    return true;
  }

  // Coarse-DAG shallow closure: a node x is "coarse-shallowly closed" iff
  // IT is kernel-closed AND every non-coarse-DAG descendant of x (descending
  // through `deps`) is closed. Recursion stops AT another coarse-DAG node —
  // that node is taken as an opaque leaf, its closure is reported as its own
  // coarse-DAG entry. So a coarse-DAG island can be "shallowly complete" even
  // if its sibling coarse-DAG dependencies are still open, telling us "this
  // cluster of work is structurally finished."
  //
  // Cycle-safe via the `stack` set; memoized at every node (including
  // non-coarse helpers) so a helper subtree that's reused under multiple
  // coarse parents is computed once.
  const coarseSet = new Set(state.coarse_dag_nodes || []);
  const coarseShallowMemo = {};
  function isCoarseShallowlyClosed(n, stack) {
    if (n in coarseShallowMemo) return coarseShallowMemo[n];
    if (stack && stack.has(n)) return true;  // cycle guard
    if (!isKernelClosed(n)) { coarseShallowMemo[n] = false; return false; }
    const childStack = stack || new Set();
    childStack.add(n);
    const childList = Array.isArray(deps[n]) ? deps[n] : [];
    for (const c of childList) {
      if (coarseSet.has(c)) continue;  // STOP — coarse-DAG opaque leaf
      if (!isCoarseShallowlyClosed(c, childStack)) {
        childStack.delete(n);
        coarseShallowMemo[n] = false;
        return false;
      }
    }
    childStack.delete(n);
    coarseShallowMemo[n] = true;
    return true;
  }

  function metricsFor(nodeSet, opts = {}) {
    let total = 0, corrPass = 0, substPass = 0, soundOrWaived = 0, soundUnknown = 0, closedShallow = 0, closedTrans = 0;
    let leanProofChars = 0, nlProofWords = 0, nlProofUnclosedWords = 0;
    let leanUnverified = 0;
    const shallowClosed = opts.shallowClosed || isKernelClosed;
    for (const n of presentNodes) {
      if (!nodeSet.has(n)) continue;
      total++;
      const leanProofMetric = leanProofMetrics[n];
      if (leanProofMetric) {
        leanProofChars += leanProofMetric.chars || 0;
      }
      nlProofWords += nlProofWordCounts[n] || 0;
      if (!shallowClosed(n)) {
        nlProofUnclosedWords += nlProofWordCounts[n] || 0;
      }
      // Use kernel-strict "actually verified" semantics: status=Pass AND
      // current_fp == approved_fp. Missing entries, Unknown status, and
      // fingerprint drift all mean "not yet verified" and don't count.
      // Count as passing when explicitly verified OR waived on this lane —
      // mirrors the soundness "or waived" treatment below. A corr-waived node
      // (challenge target) has no worker-authored proposition for the lane to
      // check, so it should not understate the curve.
      if (currentlyPassing(corrStatus[n], corrApprovedFp[n], corrCurrentFp[n]) || laneWaived(n)) corrPass++;
      // Substantiveness mirrors the kernel's `current_substantiveness_state`
      // predicate: Preamble is exempt (model.rs:2380 filters PREAMBLE_NAME
      // out of the verifier frontier and the state accessor returns Pass
      // unconditionally for it), so count it as auto-pass here. Other
      // nodes pass iff status=Pass AND fingerprints match.
      if (substantivenessPresent) {
        // "or waived" mirrors the soundness/corr treatment: a
        // substantiveness-waived node (challenge target) counts toward the
        // curve even without an explicit Pass.
        if (n === 'Preamble') {
          substPass++;
        } else if (laneWaived(n) || currentlyPassing(
          substantivenessStatus[n],
          substantivenessApprovedFp[n],
          substantivenessCurrentFp[n],
        )) {
          substPass++;
        }
      }
      // For soundness we additionally waive when the node's lean is closed
      // (no real `sorry`): if the Lean proof is verified, the kernel's view
      // of the NL proof's soundness doesn't matter for our progress
      // tracking — the node is verified by the Lean side.
      const soundPass = currentlyPassing(soundStatus[n], soundApprovedFp[n], soundCurrentFp[n]);
      const soundFail = currentlyFailing(soundStatus[n], soundApprovedFp[n], soundCurrentFp[n]);
      const myFree = !hasSorry[n];
      if (soundPass || myFree) soundOrWaived++;
      else if (!soundFail) soundUnknown++;
      // Use kernel `committed.open_nodes` as the source of truth for
      // closed-ness — same as the DAG view. Previously this used a .lean
      // text grep for `sorry`, which silently disagreed with the kernel:
      // a node whose .lean had been edited sorry-free but the kernel
      // hadn't yet re-blessed (waiting on a deterministic check, or a
      // structural classification update) would be counted closed by the
      // chart but shown open by the DAG view. The kernel's view is what
      // determines whether downstream proofs can rely on it, so it's the
      // honest progress signal.
      if (shallowClosed(n)) closedShallow++;
      if (isRecursivelyKernelClosed(n, new Set())) closedTrans++;
      // Patch C unverified count: kernel-closed (not in open_nodes) but the
      // local-closure probe has not produced a fresh record. Counted on top of
      // the closed series so the operator sees both "shallowly closed by the
      // kernel" and "the subset of those that are still pending local-closure
      // verification."
      if (localClosureUnverified.has(n)) leanUnverified++;
    }
    return {
      total, corr_passing: corrPass, sound_or_waived: soundOrWaived,
      sound_unknown: soundUnknown,
      // null for pre-substantiveness-lane checkpoints — chart renders as a gap.
      substantiveness_passing: substantivenessPresent ? substPass : null,
      lean_closed: closedShallow,
      lean_closed_transitive: closedTrans,
      lean_unverified: leanUnverified,
      lean_proof_chars: leanProofChars,
      nl_proof_words: nlProofWords,
      nl_proof_unclosed_words: nlProofUnclosedWords,
    };
  }

  const allSet = new Set(presentNodes);
  // Proof-only filter: definition + preamble nodes are excluded so the
  // totals reflect just the proof-bearing work. The viewer renders both
  // views and toggles client-side.
  const proofOnlySet = new Set(presentNodes.filter((n) => {
    return isProofNodeKind(nodeKinds[n]);
  }));
  // List of nodes whose .tex proof block opens with the literal `SKETCH:`
  // marker — used by `attachSoundVerifierFailCounts` to widen the chart's
  // "definitive Fail" set (verifier-Fail ∪ SKETCH-marked) since SKETCH nodes
  // are auto-failed by the supervisor whenever they get Sound-dispatched.
  const sketchNodes = presentNodes.filter((n) => hasSketch[n]);
  const data = {
    sha,
    cycle: parseInt(state.cycle || 0, 10),
    phase: state.phase || '',
    sketch_nodes: sketchNodes,
    all: metricsFor(allSet),
    all_proofs_only: metricsFor(proofOnlySet),
    // Coarse-DAG shallow closure — see `isCoarseShallowlyClosed` above.
    // Empty bucket (`total: 0`) when the state predates the coarse_dag_nodes
    // field (theorem-stating phase, legacy runs); chart renders as a gap.
    //
    // Two parallel buckets so the chart can honor the "include definitions"
    // toggle the same way the Nodes / Proofs charts do:
    //   - `coarse_shallow`              — all coarse-DAG nodes (Definition + Proof + Preamble)
    //   - `coarse_shallow_proofs_only`  — only proof-bearing coarse-DAG nodes
    // Closure semantics are identical (the predicate walks the same deps);
    // only the *counted* set differs. metricsFor's iteration over
    // presentNodes does the filtering naturally.
    coarse_shallow: metricsFor(coarseSet, { shallowClosed: (n) => isCoarseShallowlyClosed(n) }),
    coarse_shallow_proofs_only: metricsFor(
      new Set([...coarseSet].filter((n) => isProofNodeKind(nodeKinds[n]))),
      { shallowClosed: (n) => isCoarseShallowlyClosed(n) },
    ),
    task_blockers: taskBlockerMetrics(state),
  };
  // LRU-ish bound
  if (PROGRESS_CACHE.size >= PROGRESS_CACHE_MAX) {
    const oldest = [...PROGRESS_CACHE.entries()].sort((a, b) => a[1].ts - b[1].ts)[0];
    if (oldest) PROGRESS_CACHE.delete(oldest[0]);
  }
  PROGRESS_CACHE.set(cacheKey, { ts: Date.now(), data });
  return data;
}

// Walk event_log.jsonl once and attach soundness-Fail counts to each
// checkpoint's metrics buckets. Two counts get attached per nodeset:
//   - `sound_verifier_fail`: nodes whose most recent Sound verifier verdict
//     was Fail/Structural AND that verdict's fingerprint matches the node's
//     current sound fingerprint at the checkpoint (no worker drift since).
//   - `sound_definitive_fail`: the union of verifier-Fail with SKETCH-marked
//     nodes (top-level `sketch_nodes` on the data bucket). A Sound dispatch
//     on a SKETCH-marked node is auto-failed by the supervisor's
//     `_maybe_synthesize_sketch_soundness_artifact` in agent_wrapper/executor.py,
//     so SKETCHes are "Fail-by-construction" even without a verifier
//     round-trip.
//
// Both counts are stable under reviewer task-adjudication and reset-blocker
// relabeling, unlike the kernel-derived `current_sound_state == Fail` count
// which oscillates as the reviewer alternates Fail↔Unknown labels each
// cycle without verifier evidence.
//
// Cost: one linear pass over event_log.jsonl per series-rebuild (~30MB at
// 459 events on a live run; sub-second). Cached implicitly via
// PROGRESS_SERIES_CACHE — same lifecycle as the rest of the series.
// Per-sha sidecar cache for attachSoundVerifierFailCounts. The main progress
// walk (progressForCommit) reads each commit's supervisor_state.json; this
// pass needs a DIFFERENT projection of the same blob (event_count +
// sound_current_fingerprints + node sets), so it used to re-`git show` all
// ~1157 checkpoints on every rebuild. At ~58 MB/blob that is minutes of pure
// git I/O and — with the old 32 MiB maxBuffer, smaller than the blob — every
// read overflowed and was silently dropped, so the metric never populated and
// the whole rebuild was wasted work. Cache the extracted projection per sha
// (a sha's committed state is immutable) so only genuinely-new commits are read.
const SOUND_CPINFO_CACHE = new Map();   // `${repoPath}\0${sha}` -> {event_count, currentFps, nodeSets:{all:[],...}}
const SOUND_CPINFO_DISK_VERSION = 1;
const SOUND_CPINFO_DISK_LOADED = new Set();

function soundCpInfoDiskPath(projectInfo) {
  return path.join(viewerApiDir(projectInfo), `sound-cpinfo-cache-v${SOUND_CPINFO_DISK_VERSION}.json`);
}

function loadSoundCpInfoDiskCache(projectInfo) {
  const repoPath = projectInfo.repoPath;
  if (SOUND_CPINFO_DISK_LOADED.has(repoPath)) return;
  SOUND_CPINFO_DISK_LOADED.add(repoPath);
  const p = soundCpInfoDiskPath(projectInfo);
  if (!fs.existsSync(p)) return;
  let parsed;
  try { parsed = JSON.parse(fs.readFileSync(p, 'utf8')); } catch { return; }
  if (!parsed || parsed.version !== SOUND_CPINFO_DISK_VERSION || !parsed.entries) return;
  for (const [sha, data] of Object.entries(parsed.entries)) {
    if (!sha || !data) continue;
    SOUND_CPINFO_CACHE.set(progressCacheKey(repoPath, sha), data);
  }
}

function saveSoundCpInfoDiskCache(projectInfo) {
  const repoPath = projectInfo.repoPath;
  const prefix = `${repoPath}\0`;
  const p = soundCpInfoDiskPath(projectInfo);
  // Union with the existing file for the same reasons as saveProgressDiskCache
  // (resumable periodic flushes + bounded-in-memory eviction); the projection
  // for a given sha is immutable, so in-memory wins on collision harmlessly.
  const entries = {};
  try {
    const existing = JSON.parse(fs.readFileSync(p, 'utf8'));
    if (existing && existing.version === SOUND_CPINFO_DISK_VERSION && existing.entries) {
      Object.assign(entries, existing.entries);
    }
  } catch { /* no prior file, or unreadable — start fresh */ }
  for (const [key, value] of SOUND_CPINFO_CACHE.entries()) {
    if (!key.startsWith(prefix)) continue;
    entries[key.slice(prefix.length)] = value;
  }
  fs.mkdirSync(path.dirname(p), { recursive: true });
  const tmp = `${p}.tmp`;
  fs.writeFileSync(tmp, JSON.stringify({
    version: SOUND_CPINFO_DISK_VERSION,
    generated_at: new Date().toISOString(),
    entries,
  }));
  fs.renameSync(tmp, p);
}

// Project a parsed supervisor_state blob down to the sound-cpinfo shape
// {event_count, currentFps, nodeSets} (nodeSets values as arrays), or null when
// the blob lacks a numeric outer `event_count`. Pure function of the parse, so
// the main walk (progressForCommit) and the second pass (soundCpInfoForCommit)
// produce byte-identical projections from the same blob.
function soundCpInfoFromParsed(parsed) {
  if (!parsed) return null;
  const evCount = parsed.event_count;
  if (typeof evCount !== 'number') return null;
  const state = parsed.state || {};
  const nodeKinds = state.node_kinds || {};
  const present = Object.keys(nodeKinds);
  const currentFps = (state.live && state.live.sound_current_fingerprints) || {};
  const coarseList = state.coarse_dag_nodes || [];
  return {
    event_count: evCount,
    currentFps,
    nodeSets: {
      all: present,
      all_proofs_only: present.filter((n) => isProofNodeKind(nodeKinds[n])),
      coarse_shallow: coarseList,
      coarse_shallow_proofs_only: coarseList.filter((n) => isProofNodeKind(nodeKinds[n])),
    },
  };
}

// Returns {event_count, currentFps, nodeSets} for a checkpoint sha, or null.
// Cached per sha; reads the committed blob at most once ever. On a cold rebuild
// the main progress walk populates this cache from the SAME blob it reads (see
// progressForCommit), so this second-pass lookup is normally a cache hit and
// the 58 MB blob is read once, not twice.
function soundCpInfoForCommit(repoPath, sha) {
  const key = progressCacheKey(repoPath, sha);
  const cached = SOUND_CPINFO_CACHE.get(key);
  if (cached) return cached;
  const data = soundCpInfoFromParsed(readSupervisorParsedForCommit(repoPath, sha));
  if (!data) return null;
  SOUND_CPINFO_CACHE.set(key, data);
  return data;
}

function attachSoundVerifierFailCounts(projectInfo, checkpoints) {
  if (!Array.isArray(checkpoints) || !checkpoints.length) return;
  const cycleFiles = eventLogCycleFiles(eventLogDirForProject(projectInfo));
  if (!cycleFiles.length) return;
  const repo = projectInfo.repoPath;

  // For each checkpoint sha, project its supervisor_state.json down to
  // {event_count, sound_current_fingerprints, node_sets}. Index by event_count
  // for O(1) snapshot lookup during the event-log walk. Cached per sha, so a
  // warm rebuild reads only commits added since the last build.
  loadSoundCpInfoDiskCache(projectInfo);
  const cpInfo = new Map(); // event_count -> { sha, currentFps, nodeSets }
  let cpInfoDirty = false;
  let newSinceFlush = 0;
  for (const cp of checkpoints) {
    const wasCached = SOUND_CPINFO_CACHE.has(progressCacheKey(repo, cp.sha));
    const info = soundCpInfoForCommit(repo, cp.sha);
    if (!info) continue;
    if (!wasCached) { cpInfoDirty = true; newSinceFlush++; }
    cpInfo.set(info.event_count, {
      sha: cp.sha,
      currentFps: info.currentFps,
      nodeSets: {
        all: new Set(info.nodeSets.all),
        all_proofs_only: new Set(info.nodeSets.all_proofs_only),
        coarse_shallow: new Set(info.nodeSets.coarse_shallow),
        coarse_shallow_proofs_only: new Set(info.nodeSets.coarse_shallow_proofs_only),
      },
    });
    // Durably flush mid-loop as well: on the normal path the main walk already
    // warmed the sidecar so this loop is all cache hits, but if it isn't (e.g.
    // attach ran with a cold sidecar) a periodic flush keeps this pass resumable
    // too rather than re-reading every uncached blob on the next invocation.
    if (newSinceFlush >= PROGRESS_FLUSH_EVERY) {
      try { saveSoundCpInfoDiskCache(projectInfo); } catch {}
      newSinceFlush = 0;
    }
  }
  if (cpInfoDirty) { try { saveSoundCpInfoDiskCache(projectInfo); } catch {} }
  if (!cpInfo.size) return;

  // Build a sha → checkpoint object index for fast attach.
  const cpBySha = new Map();
  for (const cp of checkpoints) cpBySha.set(cp.sha, cp);

  // Maps maintained during the event-log walk.
  const pendingReq = new Map(); // request_id -> { node: fingerprint }
  const lastVerdict = new Map(); // node -> { status, fingerprint }

  // Stream-parse the event log line-by-line, walking the per-cycle files in
  // global index order. This used to concatenate every file into one string
  // first; past ~274 files that string crossed node's MAX_STRING_LENGTH
  // (536,870,888) and the per-file catch swallowed the RangeError, so the walk
  // silently ran on a truncated log — sound_verifier_fail stopped populating
  // at cycle 362 of a run that had reached 972. The walk itself is positional
  // only in that it needs global order; checkpoints are matched on the
  // record's own `index` field, so per-file streaming is equivalent.
  const onEventLine = (line) => {
    if (!line) return;
    let ev;
    try { ev = JSON.parse(line); } catch { return; }

    // Cache per-node fingerprints from issued Sound requests, keyed by request id.
    const commands = ev.commands || [];
    for (const cmd of commands) {
      if (cmd && cmd.command === 'issue_request') {
        const req = cmd.request;
        if (req && req.kind === 'Sound') {
          const fps = {};
          for (const b of (req.blockers || [])) {
            if (b && b.kind === 'Soundness' && b.object && b.object.node && b.fingerprint) {
              fps[b.object.node] = b.fingerprint;
            }
          }
          pendingReq.set(req.id, fps);
        }
      }
    }

    // Apply Sound responses to per-node verdict map.
    const evt = ev.event || {};
    if (evt.event === 'wrapper_response') {
      const resp = evt.response;
      if (resp && resp.kind === 'sound') {
        const fps = pendingReq.get(resp.request_id) || {};
        const lanes = resp.lane_updates || {};
        for (const laneKey of Object.keys(lanes)) {
          const laneMap = lanes[laneKey] || {};
          for (const node of Object.keys(laneMap)) {
            const update = laneMap[node];
            if (update && typeof update === 'object' && Object.prototype.hasOwnProperty.call(update, 'Set')) {
              const fp = fps[node];
              if (fp !== undefined) {
                lastVerdict.set(node, { status: update.Set, fingerprint: fp });
              }
            }
            // Update::Same (string "Same") means no change to the verdict.
          }
        }
        pendingReq.delete(resp.request_id);
      }
    }

    // At each checkpoint's event_count, snapshot per-nodeset counts onto
    // the corresponding checkpoint object.
    const idx = ev.index;
    if (typeof idx === 'number' && cpInfo.has(idx)) {
      const info = cpInfo.get(idx);
      const cp = cpBySha.get(info.sha);
      if (cp) {
        const sketchSet = new Set(Array.isArray(cp.sketch_nodes) ? cp.sketch_nodes : []);
        for (const setKey of Object.keys(info.nodeSets)) {
          const nodes = info.nodeSets[setKey];
          let verifierFail = 0;
          let definitiveFail = 0;
          for (const node of nodes) {
            const v = lastVerdict.get(node);
            // The kernel treats `Structural` as Fail (model.rs:6163 includes
            // both in `current_sound_state == Fail`), so a Sound verdict of
            // `Structural` also counts here.
            const isVerifierFail = v
              && (v.status === 'Fail' || v.status === 'Structural')
              && v.fingerprint === info.currentFps[node];
            if (isVerifierFail) verifierFail++;
            if (isVerifierFail || sketchSet.has(node)) definitiveFail++;
          }
          if (cp[setKey] && typeof cp[setKey] === 'object') {
            cp[setKey].sound_verifier_fail = verifierFail;
            cp[setKey].sound_definitive_fail = definitiveFail;
          }
        }
      }
    }
  };

  for (const f of cycleFiles) {
    try { forEachFileLine(f, onEventLine); } catch {}
  }
}

function computeProgressSeriesSync(projectInfo, headSha = repoHeadSha(projectInfo.repoPath)) {
  const repo = projectInfo.repoPath;
  loadProgressDiskCache(projectInfo);
  // Warm the sidecar too, so the sidecar the main walk populates below unions
  // with (not restarts from scratch) whatever a prior interrupted worker
  // already persisted.
  loadSoundCpInfoDiskCache(projectInfo);
  // Blob projections survive every rewind and version bump (content-addressed),
  // so this warms even when no progress-cache entry does.
  loadTabletBlobDiskCache(projectInfo);
  // Walk only HEAD-reachable checkpoint commits. This gives a consistent
  // single-timeline progress view: cycles are monotone in time, totals
  // and counts only move forward (never regress across cycles).
  //
  // After a rewind across cycle boundaries, pre-rewind checkpoints become
  // orphaned (still tagged, but unreachable from HEAD). Including them
  // would mix two timelines — e.g. pre-rewind c50 sitting between
  // post-rewind c49 and c51 — and create non-monotonic cycles + apparent
  // regressions in closed-counts. The chart trades historical visibility
  // for honest current-timeline coherence.
  //
  // The DAG view's `_build_historical_viewer_state` (in
  // trellis/viewer_adapter.py) walks git TAGS and picks the highest
  // event-count per cycle — that rule is fine for a single-cycle inspector
  // (it surfaces the latest snapshot of that cycle, even from an orphaned
  // timeline) but it's the wrong rule for a time-series chart.
  let logOut = '';
  try {
    logOut = execSync(`git -C ${JSON.stringify(repo)} log --reverse --format='%H %ct' --grep='supervisor2 checkpoint'`,
      { encoding: 'utf8', stdio: ['ignore', 'pipe', 'ignore'], maxBuffer: 4 * 1024 * 1024 });
  } catch (e) {
    return { error: String(e.message || e), checkpoints: [] };
  }
  const lines = logOut.split('\n').filter(Boolean);
  const checkpoints = [];
  let diskCacheDirty = false;
  let newSinceFlush = 0;
  for (const line of lines) {
    const sp = line.indexOf(' ');
    if (sp < 0) continue;
    const sha = line.slice(0, sp).replace(/^'/, '').replace(/'$/, '');
    const ts = parseInt(line.slice(sp + 1).replace(/'$/, ''), 10);
    const cachedEntry = PROGRESS_CACHE.get(progressCacheKey(repo, sha));
    const hadCached = !!cachedEntry;
    const hadTaskBlockers = cachedEntry && cachedEntry.data && cachedEntry.data.task_blockers !== undefined;
    let data;
    try {
      data = progressForCommit(repo, sha);
    } catch (e) {
      if (!(e instanceof SharedStateError)) throw e;
      // A corrupt shared-state blob must not read as "this run has no
      // checkpoints". This walk normally runs in a detached worker whose stderr
      // goes nowhere, so publish the failure as the series itself: the API
      // hands the client `{error, checkpoints:[...]}` and the chart says why it
      // stopped instead of sitting at "building" forever.
      const message = `shared-state decode failed at ${sha}: ${e.message}`;
      console.error(`[progress] ${message}`);
      const failed = { checkpoints, error: message };
      if (headSha) {
        PROGRESS_SERIES_CACHE.set(repo, { headSha, ts: Date.now(), data: failed });
        try { saveProgressSeriesDiskCache(projectInfo, headSha, failed); } catch {}
      }
      return failed;
    }
    if (!data) continue;
    if (!hadCached || !hadTaskBlockers) { diskCacheDirty = true; newSinceFlush++; }
    checkpoints.push({ ts: ts * 1000, ...data });
    // Durably flush every PROGRESS_FLUSH_EVERY freshly-computed commits so an
    // interrupted worker leaves warm caches and the next worker resumes from
    // here instead of re-reading every blob. progressForCommit populated the
    // sidecar from the same reads, so both caches advance together. Writes are
    // atomic (temp+rename inside the save fns).
    if (newSinceFlush >= PROGRESS_FLUSH_EVERY) {
      try { saveProgressDiskCache(projectInfo); } catch {}
      try { saveSoundCpInfoDiskCache(projectInfo); } catch {}
      try { saveTabletBlobDiskCache(projectInfo); } catch {}
      newSinceFlush = 0;
    }
  }
  try {
    attachSoundVerifierFailCounts(projectInfo, checkpoints);
  } catch (e) {
    // Non-fatal: chart falls back to upper edge = sound_or_waived + sound_unknown
    // when sound_verifier_fail is missing from the bucket.
    console.error(`[progress] attachSoundVerifierFailCounts failed: ${e && e.message || e}`);
  }
  const out = { checkpoints };
  if (headSha) {
    PROGRESS_SERIES_CACHE.set(repo, { headSha, ts: Date.now(), data: out });
    try { saveProgressSeriesDiskCache(projectInfo, headSha, out); } catch {}
  }
  if (diskCacheDirty) {
    try { saveProgressDiskCache(projectInfo); } catch {}
    // Symmetric final sidecar save: the main walk populated SOUND_CPINFO_CACHE
    // from the same reads, so attachSoundVerifierFailCounts saw every commit as
    // already-cached and skipped its own save. Persist the tail (< PROGRESS_FLUSH_EVERY
    // commits computed since the last periodic flush) here, or they'd be lost.
    try { saveSoundCpInfoDiskCache(projectInfo); } catch {}
    try { saveTabletBlobDiskCache(projectInfo); } catch {}
  }
  return out;
}

function startProgressWorker(projectInfo, headSha) {
  if (!headSha) return;
  const repo = projectInfo.repoPath;
  const running = PROGRESS_WORKERS.get(repo);
  // At most one worker per repo. A full rebuild can take longer than the run's
  // commit cadence, so spawning one per new head piles up dozens of concurrent
  // doomed builds. Serialize: while a build is in flight, requests serve the
  // stale on-disk series (buildProgressSeries) instead of racing a new worker.
  if (running && running.child && running.child.exitCode === null) return;
  const child = spawn(process.execPath, [__filename, '--progress-worker', projectInfo.slug, headSha], {
    cwd: __dirname,
    env: process.env,
    stdio: 'ignore',
    detached: true,
  });
  child.unref();
  PROGRESS_WORKERS.set(repo, { headSha, child });
  const clear = () => {
    const current = PROGRESS_WORKERS.get(repo);
    if (current && current.child === child) PROGRESS_WORKERS.delete(repo);
  };
  child.on('exit', clear);
  // A spawn failure (EAGAIN/EMFILE/ENOMEM under load) emits 'error'; without a
  // listener Node raises it as an uncaughtException. Handle it locally so a
  // transient spawn failure just frees the slot for the next request.
  child.on('error', (err) => {
    console.error(`[viewer] progress worker spawn error: ${(err && err.message) || err}`);
    clear();
  });
}

function buildProgressSeries(projectInfo) {
  const repo = projectInfo.repoPath;
  const headSha = repoHeadSha(repo);
  const seriesCache = PROGRESS_SERIES_CACHE.get(repo);
  if (headSha && seriesCache && seriesCache.headSha === headSha) {
    return seriesCache.data;
  }
  const diskSeries = readProgressSeriesDiskCache(projectInfo, headSha);
  if (diskSeries) {
    PROGRESS_SERIES_CACHE.set(repo, { headSha, ts: Date.now(), data: diskSeries });
    return diskSeries;
  }

  // Do not compute progress synchronously in the request path: it can take
  // tens of seconds and Node would block DAG/state requests. Return stale data
  // if we have it, otherwise an empty building response, and let a detached
  // worker populate the disk cache for the next refresh.
  startProgressWorker(projectInfo, headSha);
  // Stale fallback while the worker rebuilds: the in-memory series if warm,
  // else the on-disk series regardless of head (empty in-memory cache after a
  // restart). Seed it into the in-memory cache so later requests serve it
  // without re-reading disk, until the worker publishes a fresh head.
  let staleData = seriesCache && seriesCache.data;
  if (!staleData) {
    staleData = readProgressSeriesDiskCacheStale(projectInfo);
    if (staleData) {
      PROGRESS_SERIES_CACHE.set(repo, { headSha: null, ts: Date.now(), data: staleData });
    }
  }
  const stale = staleData
    ? { ...staleData, stale: true, building: true }
    : { checkpoints: [], building: true };
  return stale;
}

function runProgressWorker() {
  const slug = process.argv[3] || defaultProjectSlug();
  const expectedHead = process.argv[4] || '';
  const projectInfo = resolveRepoPath(slug);
  const headSha = repoHeadSha(projectInfo.repoPath);
  if (expectedHead && headSha && expectedHead !== headSha) {
    // HEAD moved after this worker was spawned. The next request will spawn a
    // fresh worker for the new head; avoid writing immediately-stale series.
    return;
  }
  computeProgressSeriesSync(projectInfo, headSha || expectedHead);
}

app.get(`${BASE}/api/progress.json`, (req, res) => {
  try {
    const projectInfo = resolveRepoPath(defaultProjectSlug());
    res.json(buildProgressSeries(projectInfo));
  } catch (e) { res.status(500).json({ error: e.message }); }
});
app.get(`${BASE}/:project/api/progress.json`, (req, res) => {
  try {
    const projectInfo = resolveRepoPath(projectFromRequest(req));
    res.json(buildProgressSeries(projectInfo));
  } catch (e) { res.status(500).json({ error: e.message }); }
});

// =====================================================================
// LANDING PAGE — cross-project index and host/run monitoring.
//
// Everything here reuses a collector that already exists and is already
// authoritative: `pauseStatusCached` (which is `scripts/trellis_pause.sh
// status`, the script that owns the pause state machine), the halt-marker
// reader, the quota-snapshot reader, and `trellis/sidecar/health.py`. The
// only genuinely new readers are the host ones (/proc, statfs), because the
// repo had none.
// =====================================================================

// Phase/stage/cycle for a card, read straight off disk.
//
// `readLiveViewerState` would be more complete but shells out to the Python
// adapter, and the landing page reads EVERY project at once. protocol_state
// can be 25 MB+, so this reads only the head of the file — the kernel writes
// `phase`, `stage` and `cycle` as its first three fields — and falls back to
// null rather than parsing the document.
const PROTOCOL_HEAD_BYTES = 64 * 1024;

function readProtocolHeadCheap(runtimeRoot) {
  if (!runtimeRoot) return null;
  const statePath = path.join(runtimeRoot, 'protocol_state.json');
  let fd;
  try { fd = fs.openSync(statePath, 'r'); } catch { return null; }
  try {
    const buf = Buffer.alloc(PROTOCOL_HEAD_BYTES);
    const n = fs.readSync(fd, buf, 0, PROTOCOL_HEAD_BYTES, 0);
    if (n <= 0) return null;
    const text = buf.toString('utf-8', 0, n);
    const phase = text.match(/"phase"\s*:\s*"([^"]*)"/);
    const stage = text.match(/"stage"\s*:\s*"([^"]*)"/);
    const cycle = text.match(/"cycle"\s*:\s*(\d+)/);
    if (!phase && !stage && !cycle) return null;
    return {
      phase: phase ? phase[1] : null,
      stage: stage ? stage[1] : null,
      cycle: cycle ? Number(cycle[1]) : null,
    };
  } catch {
    return null;
  } finally {
    try { fs.closeSync(fd); } catch {}
  }
}

// The lifecycle buckets in PRESENTATION order — the sequence `projects.json`
// sorts cards into and `landing.html` renders sections in. This is a ranking of
// how much the operator needs to look at a run, NOT the classifier's precedence
// (which state wins when several signals hold at once). Precedence lives in
// `classifyLifecycle`'s if-chain below and is deliberately independent: a
// Complete run still outranks every other signal when classifying, but once
// classified it belongs near the bottom of the page.
//
// Attention first:
//
//   halted         a halt marker on disk. The run stopped on something that
//                  needs a human, and `trellis_pause.sh resume` refuses while
//                  the marker is present.
//   paused         a pause request on disk. Covers the script's `arming`
//                  (request written, supervisor still running) as well as
//                  `paused`; `pause_state` carries the distinction. Someone
//                  parked this on purpose and means to come back to it.
//   live           supervisor process up, nothing requested. The run the
//                  operator is actually watching.
//   initialized    runtime exists but the run never actually ran — a launch
//                  that needs finishing.
//
// Then the inert buckets, which exist to be found rather than acted on:
//
//   finished       Phase::Complete. Done, and consulted for its results.
//   abandoned      down, no pause request, no halt, not complete. Nobody
//                  recorded why it stopped.
//   never_started  no runtime sibling at all — setup ran, the run never did.
//                  Pure placeholder; last so a host full of scaffolded
//                  directories cannot bury the run that is executing.
//
// The previous order was classifier precedence reused verbatim as display
// order, which put `never_started` and `finished` above `live` — on this host
// that pushed a newly launched run below eight cards and the operator reported
// it as "doesn't show up".
//
// `initialized` deliberately does NOT key on launch_env.json alone, which is
// what the plan's table proposed. launch_env.json is written by trellis.sh at
// every start, so runs that predate env capture have none — trellis_pause.sh
// says so in as many words when it refuses to resume one — and keying on it
// alone labelled a 1419-cycle run on this host "initialized". A run that has
// advanced a cycle has demonstrably launched, whatever its env file says.
const LIFECYCLE_ORDER = [
  'halted', 'paused', 'live', 'initialized', 'finished', 'abandoned', 'never_started',
];

const LIFECYCLE_LABELS = {
  never_started: 'never started',
  finished: 'finished',
  halted: 'halted',
  paused: 'paused',
  live: 'live',
  initialized: 'initialized',
  abandoned: 'abandoned',
};

function classifyLifecycle(signals) {
  const s = signals || {};
  const pauseState = s.pause_state || 'unknown';
  if (!s.has_runtime) return { lifecycle: 'never_started', reason: 'no runtime sibling with runtime_metadata.json' };
  if (s.phase_complete) return { lifecycle: 'finished', reason: 'protocol_state phase is Complete' };
  if (s.halted) return { lifecycle: 'halted', reason: 'halt marker on disk' };
  if (pauseState === 'paused' || pauseState === 'arming' || s.pause_requested) {
    return {
      lifecycle: 'paused',
      reason: pauseState === 'arming'
        ? 'pause armed; the run stops at the next checkpoint'
        : 'pause request on disk',
    };
  }
  if (pauseState === 'running') return { lifecycle: 'live', reason: 'supervisor process is up' };
  if (!s.has_launch_env && !s.has_run_evidence) {
    return { lifecycle: 'initialized', reason: 'runtime exists but the run has never advanced a cycle' };
  }
  return { lifecycle: 'abandoned', reason: 'supervisor is down and nothing on disk says why' };
}

// Checker liveness. The pid file at `<runtime>/checker-state/server.pid` is
// held under an exclusive flock for the server's whole lifetime, so the lock
// is the real oracle — but Node has no flock, so this corroborates the
// recorded pid against /proc instead. That closes the stale-pid-file failure
// mode in the direction that matters (a dead checker never reads up); a
// recycled pid running something else is caught by the cmdline check.
function readProcCmdline(pid, procRoot) {
  try {
    return fs.readFileSync(path.join(procRoot || '/proc', String(pid), 'cmdline'), 'utf-8')
      .replace(/\0/g, ' ')
      .trim();
  } catch {
    return null;
  }
}

function checkerLiveness(runtimeRoot, options) {
  const procRoot = (options && options.procRoot) || '/proc';
  if (!runtimeRoot) return { state: 'unknown', detail: 'no runtime root' };
  const socketPath = path.join(runtimeRoot, 'sockets', 'checker.sock');
  const socket_present = fs.existsSync(socketPath);
  let pidText = null;
  try { pidText = fs.readFileSync(path.join(runtimeRoot, 'checker-state', 'server.pid'), 'utf-8').trim(); } catch {}
  if (!pidText) {
    return { state: 'not_running', socket_present, pid: null, detail: 'no checker-state/server.pid' };
  }
  const pid = Number.parseInt(pidText, 10);
  if (!Number.isInteger(pid) || pid <= 0) {
    return { state: 'unknown', socket_present, pid: null, detail: 'unparseable pid file' };
  }
  const cmdline = readProcCmdline(pid, procRoot);
  if (cmdline === null) {
    return { state: 'not_running', socket_present, pid, detail: 'pid is not in /proc (stale pid file)' };
  }
  if (!cmdline.includes('trellis.checker.server')) {
    return { state: 'not_running', socket_present, pid, detail: 'pid was recycled by another process' };
  }
  return { state: 'running', socket_present, pid, detail: 'pid file corroborated against /proc' };
}

// Sidecar liveness. `trellis/sidecar/health.py` is the ONE authority — its
// module docstring lists the seven incidents caused by consumers inventing
// their own rule, and its oracle is an flock Node cannot take. So this
// shells out to that module rather than guessing, and caches the answer so
// it is not a per-poll subprocess.
const SIDECAR_HEALTH_TTL_MS = 15000;
const SIDECAR_HEALTH_CACHE = new Map();

function sidecarHealth(runtimeRoot, options) {
  if (!runtimeRoot) return { state: 'unknown', detail: 'no runtime root' };
  // The sidecar is off by default, so most runs have no `sidecar/` directory
  // at all — and that is the one answer we can reach the same conclusion on
  // without the subprocess (health.py returns exactly this for the case).
  // Skipping it here is what keeps a 14-project sweep from spawning 14
  // Python interpreters.
  if (!fs.existsSync(path.join(runtimeRoot, 'sidecar'))) {
    return {
      state: 'not_running',
      detail: `no sidecar directory at ${path.join(runtimeRoot, 'sidecar')}`,
      pid: null,
      configured: false,
      status_stale: false,
      pool_size: null,
      in_flight: null,
    };
  }
  const now = Date.now();
  const cached = SIDECAR_HEALTH_CACHE.get(runtimeRoot);
  if (cached && (now - cached.ts) < SIDECAR_HEALTH_TTL_MS) return cached.value;
  let value;
  try {
    const out = execFileSync(
      'python3',
      ['-m', 'trellis.sidecar.health', runtimeRoot, '--json'],
      {
        cwd: TRELLIS_ROOT,
        env: { ...process.env, PYTHONPATH: [TRELLIS_ROOT, process.env.PYTHONPATH].filter(Boolean).join(':') },
        encoding: 'utf-8',
        timeout: (options && options.timeoutMs) || 8000,
        stdio: ['ignore', 'pipe', 'ignore'],
      },
    );
    const health = JSON.parse(out);
    const liveness = health.liveness || {};
    value = {
      state: liveness.state || 'unknown',
      detail: liveness.detail || '',
      pid: liveness.pid == null ? null : liveness.pid,
      configured: Boolean(health.sidecar_dir_present),
      status_stale: Boolean(health.status_stale),
      pool_size: health.pool_size == null ? null : health.pool_size,
      in_flight: Array.isArray(health.in_flight) ? health.in_flight.length : null,
    };
  } catch (e) {
    // A non-zero exit is normal here (1 = not running, 3 = no sidecar dir),
    // and execFileSync throws on those, so read the payload off the error.
    let parsed = null;
    try { parsed = JSON.parse(String(e.stdout || '')); } catch {}
    if (parsed) {
      const liveness = parsed.liveness || {};
      value = {
        state: liveness.state || 'unknown',
        detail: liveness.detail || '',
        pid: liveness.pid == null ? null : liveness.pid,
        configured: Boolean(parsed.sidecar_dir_present),
        status_stale: Boolean(parsed.status_stale),
        pool_size: parsed.pool_size == null ? null : parsed.pool_size,
        in_flight: Array.isArray(parsed.in_flight) ? parsed.in_flight.length : null,
      };
    } else {
      value = { state: 'unknown', detail: `health probe failed: ${e.message}`, configured: false };
    }
  }
  SIDECAR_HEALTH_CACHE.set(runtimeRoot, { ts: now, value });
  return value;
}

// --- host metrics -------------------------------------------------------
// INSTALLATION.md's system requirements: 32 GB RAM minimum, 48 GB
// recommended, and swap on the SSD. Report them as they are — an operator
// looking at a landing page after an OOM-killed burst deserves the real
// number next to the requirement, not a green tick.
const HOST_RAM_MIN_GB = 32;
const HOST_RAM_RECOMMENDED_GB = 48;

function parseMeminfo(text) {
  const out = {};
  for (const line of String(text).split('\n')) {
    const m = line.match(/^([A-Za-z()_]+):\s+(\d+)\s*kB/);
    if (m) out[m[1]] = Number(m[2]) * 1024;
  }
  return out;
}

// /proc/swaps names WHERE swap lives — a partition, or a file on some
// filesystem. That is the actionable half of "put swap on the SSD"; this
// does not claim to know a device's media type, it just says what is in use.
function parseSwaps(text) {
  const rows = [];
  const lines = String(text).split('\n').slice(1);
  for (const line of lines) {
    const parts = line.trim().split(/\s+/);
    if (parts.length < 4 || !parts[0]) continue;
    rows.push({
      name: parts[0],
      type: parts[1],
      size_bytes: Number(parts[2]) * 1024,
      used_bytes: Number(parts[3]) * 1024,
    });
  }
  return rows;
}

function diskUsage(target) {
  try {
    const s = fs.statfsSync(target);
    const total = s.blocks * s.bsize;
    const avail = s.bavail * s.bsize;
    const used = total - (s.bfree * s.bsize);
    return {
      path: target,
      total_bytes: total,
      avail_bytes: avail,
      used_bytes: used,
      pct_used: total > 0 ? Math.round((used / total) * 1000) / 10 : null,
    };
  } catch (e) {
    return { path: target, error: e.code || e.message };
  }
}

// Presence and age only. These files are provider OAuth credentials; the
// viewer must never read, echo, or proxy their contents.
const PROVIDER_CRED_FILES = [
  { provider: 'codex', rel: ['.codex', 'auth.json'] },
  { provider: 'claude', rel: ['.claude', '.credentials.json'] },
  { provider: 'gemini', rel: ['.gemini', 'oauth_creds.json'] },
];

function providerAuthPresence(home) {
  const base = home || os.homedir();
  return PROVIDER_CRED_FILES.map(({ provider, rel }) => {
    const file = path.join(base, ...rel);
    try {
      const stat = fs.statSync(file);
      return { provider, present: true, mtime_ms: stat.mtimeMs };
    } catch {
      return { provider, present: false, mtime_ms: null };
    }
  });
}

function hostSnapshot(options) {
  const opts = options || {};
  const procRoot = opts.procRoot || '/proc';
  let mem = {};
  let load = null;
  let swaps = [];
  try { mem = parseMeminfo(fs.readFileSync(path.join(procRoot, 'meminfo'), 'utf-8')); } catch {}
  try {
    const parts = fs.readFileSync(path.join(procRoot, 'loadavg'), 'utf-8').trim().split(/\s+/);
    load = { '1m': Number(parts[0]), '5m': Number(parts[1]), '15m': Number(parts[2]) };
  } catch {}
  try { swaps = parseSwaps(fs.readFileSync(path.join(procRoot, 'swaps'), 'utf-8')); } catch {}

  const memTotal = mem.MemTotal || 0;
  const memTotalGb = memTotal / (1024 ** 3);
  let ramVerdict = 'unknown';
  if (memTotal > 0) {
    // Vendors ship "32 GB" as slightly less usable RAM, so compare with a
    // small tolerance rather than calling a 32 GB machine under-spec.
    if (memTotalGb >= HOST_RAM_RECOMMENDED_GB * 0.95) ramVerdict = 'meets_recommended';
    else if (memTotalGb >= HOST_RAM_MIN_GB * 0.95) ramVerdict = 'meets_minimum';
    else ramVerdict = 'below_minimum';
  }
  const swapTotal = mem.SwapTotal || 0;

  const diskPaths = opts.diskPaths || [PROJECTS_ROOT, TRELLIS_ROOT, os.tmpdir()];
  const seen = new Set();
  const disks = [];
  for (const p of diskPaths) {
    if (!p || seen.has(p)) continue;
    seen.add(p);
    disks.push(diskUsage(p));
  }

  return {
    hostname: os.hostname(),
    generated_ts: Date.now(),
    memory: {
      total_bytes: memTotal,
      available_bytes: mem.MemAvailable == null ? null : mem.MemAvailable,
      free_bytes: mem.MemFree == null ? null : mem.MemFree,
      cached_bytes: mem.Cached == null ? null : mem.Cached,
      requirement_min_gb: HOST_RAM_MIN_GB,
      requirement_recommended_gb: HOST_RAM_RECOMMENDED_GB,
      verdict: ramVerdict,
    },
    swap: {
      total_bytes: swapTotal,
      free_bytes: mem.SwapFree == null ? null : mem.SwapFree,
      used_bytes: swapTotal ? swapTotal - (mem.SwapFree || 0) : 0,
      present: swapTotal > 0,
      devices: swaps,
      note: swapTotal > 0
        ? 'INSTALLATION.md asks for swap on the SSD, with room to spare.'
        : 'No swap configured. INSTALLATION.md asks for swap on the SSD — a burst that peaks over RAM gets OOM-killed instead.',
    },
    load,
    cpus: os.cpus().length,
    disks,
    provider_auth: providerAuthPresence(opts.home),
    control: {
      enabled: CONTROL_ENABLED,
      bind_host: BIND_HOST,
      bind_is_loopback: bindIsLoopback(BIND_HOST),
      header: CONTROL_HEADER,
    },
  };
}

// Provider budget for a card.
//
// `latestQuotaSnapshotsForProject` is the existing collector and is used
// unchanged by the usage panel, but it parses the WHOLE snapshot log — up to
// 8.6 MB on a long-lived run here — and the landing page reads every project
// on the host at once. Same file, same `weeklyBudgetFromSnapshot` extractor,
// but only the tail: snapshots are append-only and strictly increasing in
// time, so the newest row per provider is always near the end.
const QUOTA_TAIL_BYTES = 128 * 1024;

function quotaSummaryForProject(projectInfo, options) {
  const file = path.join(projectInfo.repoPath, '.trellis', 'logs', 'quota-snapshots.jsonl');
  const tailBytes = (options && options.tailBytes) || QUOTA_TAIL_BYTES;
  let text;
  try {
    const stat = fs.statSync(file);
    const start = Math.max(0, stat.size - tailBytes);
    const fd = fs.openSync(file, 'r');
    try {
      const len = stat.size - start;
      const buf = Buffer.alloc(len);
      fs.readSync(fd, buf, 0, len, start);
      text = buf.toString('utf-8');
    } finally {
      try { fs.closeSync(fd); } catch {}
    }
    // A non-zero start almost certainly lands mid-line; drop that fragment.
    if (start > 0) text = text.slice(text.indexOf('\n') + 1);
  } catch {
    return {};
  }
  const latest = {};
  for (const line of text.split('\n')) {
    if (!line.trim()) continue;
    let row;
    try { row = JSON.parse(line); } catch { continue; }
    const provider = row && row.provider;
    if (!provider) continue;
    if (!latest[provider] || Number(row.ts) > Number(latest[provider].ts)) latest[provider] = row;
  }
  const out = {};
  for (const [provider, row] of Object.entries(latest)) {
    out[provider] = {
      ts: row.ts ?? null,
      ok: row.ok !== false,
      plan_tier: row.plan_tier || null,
      account: row.account || null,
      weekly_budget: weeklyBudgetFromSnapshot(row),
    };
  }
  return out;
}

// --- the project index --------------------------------------------------
const PROJECTS_INDEX_TTL_MS = 15000;
let PROJECTS_INDEX_CACHE = null;

// Does this project OWN the runtime root it resolved to?
//
// `runtimeRootForProject` falls back to scanning the parent directory for any
// `*-runtime` when the `<repo>-runtime` sibling is absent. On a host with
// several projects that fallback hands a repo somebody else's runtime: two
// archived sibling repos here resolved to the LIVE run's
// runtime, which made the landing page report four live runs when
// two were live and show one run's cycle and checker on another's card.
//
// The run viewer's own behaviour is deliberately left alone — narrowing a
// shared helper would change every per-project page. Instead the index
// resolves ownership across projects, which is information only a
// cross-project view has: a runtime named `<repo>-runtime` for this repo's
// real path is owned outright, and any other claim loses to that owner.
function runtimeRootOwnedBy(runtimeRoot, projectInfo) {
  if (!runtimeRoot) return false;
  let real = projectInfo.repoPath;
  try { real = fs.realpathSync(projectInfo.repoPath); } catch {}
  return path.basename(runtimeRoot) === `${path.basename(real)}-runtime`;
}

// Does some OTHER directory in the parent look like this runtime's owner?
//
// The cross-project contest below only fires when two DISCOVERED projects
// claim the same runtime. That left a hole the audit caught: if the owning
// repo is not itself a discovered project — no trellis.config.json yet, or
// renamed away — nothing contests the claim and an archived repo silently
// adopts a live run's runtime again. `<X>-runtime` names its owner, so ask
// the filesystem directly instead of only asking the project list.
function runtimeRootHasOtherOwnerOnDisk(runtimeRoot, projectInfo) {
  const base = path.basename(runtimeRoot);
  if (!base.endsWith('-runtime')) return false;
  const ownerDir = path.join(path.dirname(runtimeRoot), base.slice(0, -'-runtime'.length));
  let ownerReal;
  try {
    if (!fs.statSync(ownerDir).isDirectory()) return false;
    ownerReal = fs.realpathSync(ownerDir);
  } catch {
    return false;
  }
  let selfReal = projectInfo.repoPath;
  try { selfReal = fs.realpathSync(projectInfo.repoPath); } catch {}
  return ownerReal !== selfReal;
}

function resolveRuntimeRoots(projects) {
  // Pass 1: what each project resolves to, and whether an on-disk owner
  // already disqualifies an inherited claim.
  const claim = new Map();
  for (const projectInfo of projects) {
    let root = null;
    try { root = runtimeRootForProject(projectInfo); } catch {}
    const inherited = Boolean(root) && !runtimeRootOwnedBy(root, projectInfo);
    if (inherited && runtimeRootHasOtherOwnerOnDisk(root, projectInfo)) {
      claim.set(projectInfo.slug, { runtimeRoot: null, disowned: root });
    } else {
      claim.set(projectInfo.slug, { runtimeRoot: root, disowned: null });
    }
  }

  // Pass 2: two DISTINCT repos cannot share one runtime. Several slugs for
  // the same repo legitimately can — `~/math/current` is a symlink to a real
  // project — so contention is counted over resolved repo paths.
  const claimants = new Map();
  for (const projectInfo of projects) {
    const { runtimeRoot } = claim.get(projectInfo.slug);
    if (!runtimeRoot) continue;
    let real = projectInfo.repoPath;
    try { real = fs.realpathSync(projectInfo.repoPath); } catch {}
    if (!claimants.has(runtimeRoot)) claimants.set(runtimeRoot, new Set());
    claimants.get(runtimeRoot).add(real);
  }

  const out = new Map();
  for (const projectInfo of projects) {
    const current = claim.get(projectInfo.slug);
    const { runtimeRoot } = current;
    const contested = runtimeRoot && claimants.get(runtimeRoot).size > 1;
    if (contested && !runtimeRootOwnedBy(runtimeRoot, projectInfo)) {
      out.set(projectInfo.slug, { runtimeRoot: null, disowned: runtimeRoot });
    } else {
      out.set(projectInfo.slug, current);
    }
  }
  return out;
}

function projectCard(projectInfo, resolved) {
  const card = {
    slug: projectInfo.slug,
    repo_path: projectInfo.repoPath,
    repo_type: projectInfo.repoType,
    href: `${BASE}/${projectInfo.slug}/`,
  };
  let runtimeRoot;
  let disowned = null;
  if (resolved && 'runtimeRoot' in resolved) {
    runtimeRoot = resolved.runtimeRoot;
    disowned = resolved.disowned || null;
  } else {
    runtimeRoot = null;
    try { runtimeRoot = runtimeRootForProject(projectInfo); } catch {}
  }
  card.runtime_root = runtimeRoot;
  card.runtime_root_disowned = disowned;

  // `trellis_pause.sh status` is the authority on supervisor state, but it is
  // a bash+python subprocess pair (~70ms) and this runs over every project on
  // the host. A repo with no runtime sibling has no supervisor to ask about
  // and classifies as never_started regardless, so it never pays for the call.
  let pause = { state: 'unknown' };
  if (runtimeRoot) {
    try { pause = pauseStatusCached(projectInfo) || pause; }
    catch (e) { pause = { state: 'unknown', error: e.message }; }
  } else {
    pause = { state: 'no_runtime' };
  }
  // Read markers off the RESOLVED runtime root, not via
  // `haltStateForProject` — that re-resolves through the fallback scan and
  // would report a disowned sibling's halt on this card.
  let halt = { halted: false, markers: [] };
  try { halt = haltStateForRuntimeRoot(runtimeRoot) || halt; } catch {}
  const protocol = readProtocolHeadCheap(runtimeRoot);

  const signals = {
    has_runtime: Boolean(runtimeRoot),
    has_launch_env: Boolean(pause && pause.launch_env && pause.launch_env.present),
    pause_state: pause && pause.state,
    pause_requested: Boolean(pause && pause.request),
    halted: Boolean(halt && halt.halted),
    phase_complete: /complete/i.test(String((protocol && protocol.phase) || '')),
    has_run_evidence: Boolean(protocol && Number(protocol.cycle) > 0),
  };
  let { lifecycle, reason } = classifyLifecycle(signals);
  if (disowned) {
    reason = `${reason} (a sibling ${path.basename(disowned)} belongs to another repo)`;
  }

  card.lifecycle = lifecycle;
  card.lifecycle_label = LIFECYCLE_LABELS[lifecycle] || lifecycle;
  card.lifecycle_reason = reason;
  card.phase = protocol ? protocol.phase : null;
  card.stage = protocol ? protocol.stage : null;
  card.cycle = protocol ? protocol.cycle : null;
  card.pause_state = pause && pause.state;
  card.sentinel_present = Boolean(pause && pause.sentinel_present);
  card.resumable = Boolean(pause && pause.resumable);
  card.halt = {
    halted: Boolean(halt && halt.halted),
    markers: (halt && halt.markers ? halt.markers : []).map(m => (m && m.marker_kind) || 'unreadable'),
  };
  card.supervisor = {
    state: (pause && pause.state) || 'unknown',
    pid: (pause && pause.wrapper_pid) || null,
    launch_env_present: signals.has_launch_env,
  };
  card.checker = checkerLiveness(runtimeRoot);
  card.sidecar = sidecarHealth(runtimeRoot);
  try {
    card.quota = quotaSummaryForProject(projectInfo);
  } catch { card.quota = {}; }
  return card;
}

function projectsIndex(options) {
  const now = Date.now();
  if (!(options && options.fresh) && PROJECTS_INDEX_CACHE && (now - PROJECTS_INDEX_CACHE.ts) < PROJECTS_INDEX_TTL_MS) {
    return PROJECTS_INDEX_CACHE.value;
  }
  const discovered = discoverProjects();
  const runtimeRoots = resolveRuntimeRoots(discovered);
  let createJobs = [];
  try { createJobs = createJobsIndex(); } catch { /* jobs are additive; never blank the page */ }
  const claimedRepos = new Set(createJobs
    .filter(job => job && job.state !== 'done' && job.repo_path)
    .map(job => path.resolve(job.repo_path)));
  const projects = [];
  for (const projectInfo of discovered) {
    // A repo bearing `.trellis-creating` is a create job's half-built output,
    // not a run: setup writes trellis.config.json mid-way, so without this a
    // failed creation masquerades as an `abandoned` project. It renders as a
    // creating/create_failed card instead (createJobsIndex below), and only
    // graduates into this list when a verified launch removes the marker.
    if (fs.existsSync(path.join(projectInfo.repoPath, '.trellis-creating'))
        || claimedRepos.has(path.resolve(projectInfo.repoPath))) continue;
    try {
      projects.push(projectCard(projectInfo, runtimeRoots.get(projectInfo.slug)));
    } catch (e) {
      // One broken project must never blank the landing page.
      projects.push({
        slug: projectInfo.slug,
        repo_path: projectInfo.repoPath,
        href: `${BASE}/${projectInfo.slug}/`,
        lifecycle: 'unknown',
        lifecycle_label: 'unreadable',
        lifecycle_reason: e.message,
        error: e.message,
      });
    }
  }
  const rank = slug => {
    const i = LIFECYCLE_ORDER.indexOf(slug);
    return i < 0 ? LIFECYCLE_ORDER.length : i;
  };
  projects.sort((a, b) => (rank(a.lifecycle) - rank(b.lifecycle)) || a.slug.localeCompare(b.slug));
  const value = {
    generated_ts: now,
    projects_root: PROJECTS_ROOT,
    default_project: defaultProjectSlug(),
    base_path: BASE,
    lifecycle_order: LIFECYCLE_ORDER,
    lifecycle_labels: LIFECYCLE_LABELS,
    projects,
    create_jobs: createJobs,
  };
  PROJECTS_INDEX_CACHE = { ts: now, value };
  return value;
}

registerReadRoute('projects.json', (_req, res) => {
  try { res.json(projectsIndex()); }
  catch (e) { res.status(500).json({ error: e.message }); }
});

registerReadRoute('host.json', (_req, res) => {
  try { res.json(hostSnapshot()); }
  catch (e) { res.status(500).json({ error: e.message }); }
});

// =====================================================================
// RUN CREATION — the browser flow over scripts/trellis_create_run.sh.
//
// The script owns the state machine and every on-disk format (the
// trellis_pause.sh precedent); the viewer's share of the work is exactly
// three things: intake uploads, relay mutations to the script, and read the
// job dirs back for polling. All interpretation lives in the exported pure
// functions below so the wizard page can stay dumb rendering + polling
// (design §9) and the tests can hold the logic without an HTTP server.
//
// Every endpoint here — reads included — is token-gated and lives in the
// control namespace: a create job's status carries its log and paper
// metadata, which belong behind the same door as the writes.
// =====================================================================

const CREATE_RUN_SCRIPT = path.join(TRELLIS_ROOT, 'scripts', 'trellis_create_run.sh');
// The script's own slug rule (plan §1.5): strict, dot-free — this string
// becomes a repo path, tmux session names and a URL segment.
const CREATE_SLUG_RE = /^[A-Za-z][A-Za-z0-9_-]{0,63}$/;
const CREATE_UPLOAD_ID_RE = /^[a-f0-9]{16}$/;
const CREATE_REFERENCE_ID_RE = /^[A-Za-z0-9][A-Za-z0-9._-]*$/;
const CREATE_UPLOAD_LIMIT = '20mb';
const CREATE_UPLOAD_LIMIT_BYTES = 20 * 1024 * 1024;
const CREATE_STALE_SECS = Number(process.env.TRELLIS_CREATE_STALE_SECS || 60);

// Product-pinned choices lead every model dropdown. Keep one model per line:
// adding a newly served model (Astra or otherwise) is one line here. The
// create-options pricing-table rows are appended after these in the browser,
// and "other…" remains the final escape hatch.
const CREATE_PINNED_MODELS = [
  'gpt-5.6-luna',
  'gpt-5.6-terra',
  'gpt-5.6-sol',
];
const CREATE_DEFAULT_MODEL = 'gpt-5.6-sol';
const CREATE_DEFAULT_EFFORT = 'xhigh';

const CREATE_RUNNING_STATES = [
  'intake', 'resolving', 'building', 'initing', 'launching', 'verifying',
];
const CREATE_FAILED_STATES = [
  // resolve_stale is row 10's dedicated state: the confirmed selection no
  // longer matches the current resolution (paper changed between confirm and
  // build). Its recovery is re-resolve -> re-confirm, never a plain retry of
  // phase B — the script routes its retry back through phase A.
  'resolve_failed', 'resolve_stale', 'build_failed', 'init_failed', 'launch_failed',
];

function createJobsRoot(root) {
  return path.join(root === undefined ? PROJECTS_ROOT : root, '.trellis-viewer', 'create-jobs');
}

function createUploadsRoot(root) {
  return path.join(root === undefined ? PROJECTS_ROOT : root, '.trellis-viewer', 'uploads');
}

function readJsonSafe(file) {
  try {
    const parsed = JSON.parse(fs.readFileSync(file, 'utf-8'));
    return parsed && typeof parsed === 'object' ? parsed : null;
  } catch {
    return null;
  }
}

// --- upload intake (design §4 POST uploads; §7 rows 1-3) ----------------
//
// Raw body, no multipart (plan §3). The server names the file; the client
// name never becomes a path. Non-UTF-8 input gets the add_reference_paper.sh
// transcode treatment — windows-1252 first, latin-1 when cp1252's five
// undefined slots appear — closing the R7 asymmetry for primary papers.

// The 27 cp1252 codepoints that differ from latin-1 (0x80-0x9F range).
const CP1252_HIGH = {
  0x80: 0x20AC, 0x82: 0x201A, 0x83: 0x0192, 0x84: 0x201E, 0x85: 0x2026,
  0x86: 0x2020, 0x87: 0x2021, 0x88: 0x02C6, 0x89: 0x2030, 0x8A: 0x0160,
  0x8B: 0x2039, 0x8C: 0x0152, 0x8E: 0x017D, 0x91: 0x2018, 0x92: 0x2019,
  0x93: 0x201C, 0x94: 0x201D, 0x95: 0x2022, 0x96: 0x2013, 0x97: 0x2014,
  0x98: 0x02DC, 0x99: 0x2122, 0x9A: 0x0161, 0x9B: 0x203A, 0x9C: 0x0153,
  0x9E: 0x017E, 0x9F: 0x0178,
};

function decodeCp1252(buf) {
  // Python's cp1252 codec fails on the five undefined slots; mirroring that
  // failure is what makes the latin-1 fallback order match
  // add_reference_paper.sh exactly.
  const undefinedSlots = new Set([0x81, 0x8D, 0x8F, 0x90, 0x9D]);
  let out = '';
  for (const byte of buf) {
    if (undefinedSlots.has(byte)) return null;
    if (byte >= 0x80 && byte <= 0x9F) out += String.fromCharCode(CP1252_HIGH[byte]);
    else out += String.fromCharCode(byte);
  }
  return out;
}

function intakeUpload(buf, options) {
  const opts = options || {};
  const label = opts.label || '.tex text file';
  if (!Buffer.isBuffer(buf) || buf.length === 0) {
    return { ok: false, error: 'empty upload — send the file bytes as the raw request body' };
  }
  if (buf.includes(0)) {
    return { ok: false, error: `binary content (NUL bytes) — upload a ${label}` };
  }
  let text = null;
  let transcoded = null;
  try {
    text = new TextDecoder('utf-8', { fatal: true }).decode(buf);
  } catch {
    text = decodeCp1252(buf);
    transcoded = 'windows-1252';
    if (text === null) {
      text = buf.toString('latin1');
      transcoded = 'latin-1';
    }
  }
  const warnings = [];
  if (transcoded) {
    warnings.push({
      code: 'not_utf8',
      message: `transcoded from ${transcoded} to UTF-8 (the kernel reads papers as UTF-8 only)`,
    });
  }
  // Plausibility is a warning, never a block (row 2): the operator may know
  // better than this heuristic.
  if (opts.latexPlausibility !== false && !text.includes('\\begin')) {
    warnings.push({
      code: 'no_begin',
      message: 'no \\begin{...} found — this does not look like LaTeX. You can proceed anyway.',
    });
  }
  return { ok: true, text, transcoded_from: transcoded, warnings };
}

function createUploadFile(id, root) {
  if (!CREATE_UPLOAD_ID_RE.test(String(id || ''))) return null;
  return path.join(createUploadsRoot(root), id, 'paper.tex');
}

function mathCreateJobConflict(slug) {
  const job = readJsonSafe(path.join(createJobsRoot(), slug, 'job.json'));
  return job && job.create_flow && job.create_flow !== 'math'
    ? { error: 'create_flow_mismatch', detail: `create job ${slug} belongs to ${job.create_flow}, not the math flow` }
    : null;
}

// Optional create surfaces reuse the same raw-body endpoint and its single
// size limit. A handler returns {file_name, content, response}; the core owns
// the random id, private directory, write, cleanup, and response shape. When
// no optional module is installed, only the historical paper kind exists.
const CREATE_UPLOAD_KIND_HANDLERS = new Map();

function registerCreateUploadKind(kind, handler) {
  if (!/^[a-z][a-z0-9_-]{0,31}$/.test(String(kind || '')) || typeof handler !== 'function') {
    throw new Error('create upload kind requires a short lowercase name and a handler');
  }
  if (kind === 'paper' || CREATE_UPLOAD_KIND_HANDLERS.has(kind)) {
    throw new Error(`create upload kind already registered: ${kind}`);
  }
  CREATE_UPLOAD_KIND_HANDLERS.set(kind, handler);
}

// --- pure interpretation (exported; viewer/test_create_job.js) ----------

// The design's heartbeat rule (§4, rows 22-24): a running state whose
// updated_ts is stale beyond the threshold with the tmux session absent is
// `interrupted`. awaiting_targets has no process BY DESIGN and never decays.
function classifyCreateJob({ state, updated_ts, now, tmux_alive, stale_secs }) {
  const stale = stale_secs === undefined ? CREATE_STALE_SECS : stale_secs;
  const running = CREATE_RUNNING_STATES.includes(state);
  const age = Number.isFinite(Number(updated_ts)) && updated_ts
    ? Math.max(0, Number(now) - Number(updated_ts))
    : null;
  let effective = state;
  let reason = '';
  if (running && !tmux_alive && (age === null || age > stale)) {
    effective = 'interrupted';
    reason = age === null
      ? 'phase recorded as running, but no heartbeat was ever written and the tmux session is gone'
      : `heartbeat is ${age}s stale (threshold ${stale}s) and the tmux session is gone`;
  } else if (state === 'awaiting_targets') {
    reason = 'parked with no process — target selection can wait indefinitely';
  } else if (running) {
    reason = tmux_alive ? 'phase running in tmux' : 'phase recorded as running; heartbeat still fresh';
  } else if (CREATE_FAILED_STATES.includes(state)) {
    reason = 'a stage failed; see the log tail';
  }
  const bucket = state === 'done'
    ? 'done'
    : (effective === 'interrupted' || CREATE_FAILED_STATES.includes(state))
      ? 'create_failed'
      : 'creating';
  return { effective_state: effective, bucket, heartbeat_age_secs: age, reason };
}

// What the target page renders (design §5). All decisions are made HERE:
// the page checks what `checked` says, disables confirm when `confirm_allowed`
// is false, and shows the general R1-R9 guidance when `show_general_rules`
// says so — the §1.3 honesty boundary: when no rejected block explains an
// empty candidate list, we state the rules rather than invent a reason.
function interpretTargetsResolution(resolution) {
  if (!resolution || typeof resolution !== 'object') return null;
  const candidates = (Array.isArray(resolution.candidates) ? resolution.candidates : [])
    .map(c => ({ ...c, checked: Boolean(c && c.preselected) }));
  const rejected = Array.isArray(resolution.rejected_blocks) ? resolution.rejected_blocks : [];
  const fileWarnings = Array.isArray(resolution.file_warnings) ? resolution.file_warnings : [];
  const zero = candidates.length === 0;
  const mainEnvs = Array.isArray(resolution.main_result_envs) && resolution.main_result_envs.length
    ? resolution.main_result_envs
    : ['theorem', 'corollary'];
  // Row 29: a rejected block whose env IS in the main-result set would be a
  // candidate were it not swallowed by nesting inside another matched block —
  // exactly what env-set widening produces (§1.4's non-additive direction).
  // Warned inline, naming both blocks (the scanner's message already carries
  // the enclosing block's env and lines); confirm STAYS enabled, because the
  // outer block may be intended.
  const nestingWarnings = rejected
    .filter(b => b && (b.reason_code === 'nested_in_candidate' || b.reason_code === 'nested_in_dropped_block')
      && mainEnvs.includes(b.env))
    .map(b => ({
      env: b.env,
      label: b.label || null,
      start_line: b.start_line,
      end_line: b.end_line,
      message: `a \\begin{${b.env}}${b.label ? ` (${b.label})` : ''} at lines `
        + `${b.start_line}-${b.end_line} is NOT a candidate: ${b.message || 'it is nested inside another matched block.'}`
        + ' Narrow the environment set back or fix the nesting to restore it; confirming keeps the outer block.',
    }));
  return {
    paper_sha256: resolution.paper_sha256 || null,
    paper_name: resolution.paper_name || null,
    main_result_envs: mainEnvs,
    normalization: resolution.normalization || {},
    candidates,
    envs_present: Array.isArray(resolution.envs_present) ? resolution.envs_present : [],
    rejected_blocks: rejected,
    file_warnings: fileWarnings,
    scan_truncated: resolution.scan_truncated || null,
    available_labels: Array.isArray(resolution.available_labels) ? resolution.available_labels : [],
    zero_candidates: zero,
    nesting_warnings: nestingWarnings,
    // Kernel init_from_config fails a run with zero targets; the page never
    // offers a confirm that cannot succeed (row 7).
    confirm_allowed: !zero,
    show_general_rules: zero && rejected.length === 0,
  };
}

// What changed across a re-upload/re-resolve (§5.5 item 3): the page says
// "+2 candidates, thm:aux disappeared" instead of silently re-rendering.
// `swallowed` singles out the row-29 case — a candidate that vanished AND is
// now explained by a nesting rejection, i.e. the widened set ate it.
function diffTargetsResolutions(prev, next) {
  if (!prev || !next || typeof prev !== 'object' || typeof next !== 'object') return null;
  const brief = c => ({
    key: c.key, tex_label: c.tex_label || null, env: c.env || null,
    start_line: c.start_line, end_line: c.end_line,
  });
  const prevC = Array.isArray(prev.candidates) ? prev.candidates : [];
  const nextC = Array.isArray(next.candidates) ? next.candidates : [];
  const prevByKey = new Map(prevC.map(c => [c.key, c]));
  const nextByKey = new Map(nextC.map(c => [c.key, c]));
  const added = nextC.filter(c => !prevByKey.has(c.key)).map(brief);
  const removed = prevC.filter(c => !nextByKey.has(c.key)).map(brief);
  const moved = nextC.filter(c => {
    const p = prevByKey.get(c.key);
    return p && (p.start_line !== c.start_line || p.end_line !== c.end_line
      || String(p.text || '') !== String(c.text || ''));
  }).map(brief);
  const rejected = Array.isArray(next.rejected_blocks) ? next.rejected_blocks : [];
  const swallowed = removed.flatMap(r => {
    const b = rejected.find(x => x
      && (x.reason_code === 'nested_in_candidate' || x.reason_code === 'nested_in_dropped_block')
      && ((r.tex_label && x.label === r.tex_label) || x.start_line === r.start_line));
    if (!b) return [];
    return [{
      ...r,
      message: `candidate ${r.tex_label || r.key} disappeared: ${b.message || 'it is now nested inside another matched block.'}`,
    }];
  });
  return {
    added,
    removed,
    moved,
    swallowed,
    unchanged: nextC.length - added.length - moved.length,
    changed: Boolean(added.length || removed.length || moved.length),
  };
}

// One reference spec in the script's normalized `<id>=<file>:<source_id>`
// shape, split for display. The file path is job-dir-internal and carries no
// colons; the split-at-last-colon mirrors the script's own parse.
function parseReferenceSpec(spec) {
  const s = String(spec || '');
  const eq = s.indexOf('=');
  if (eq <= 0) return null;
  const id = s.slice(0, eq);
  const rest = s.slice(eq + 1);
  const colon = rest.lastIndexOf(':');
  if (colon < 0) return { id, file: rest, source_id: id };
  return { id, file: rest.slice(0, colon), source_id: rest.slice(colon + 1) || id };
}

// Recovery hints mined from a failed job's error + log tail — the page
// renders these verbatim next to the failure instead of asking the operator
// to recognize the signature themselves.
const CREATE_FAILURE_HINT_RULES = [
  {
    code: 'reference_immutable',
    re: /reference (papers|registrations) are immutable/i,
    message: 'Row 26: a reference file already exists under this id with different '
      + 'content, and reference ids are immutable (REFERENCE_PAPERS.md). Give the '
      + 'changed file a NEW id — edit the references, re-resolve, and re-confirm. '
      + 'Re-uploading the same content under the old id is fine; changed content is not.',
  },
  {
    code: 'disk_full',
    re: /ENOSPC|No space left on device/i,
    message: 'Row 14: the disk filled up. Free space and retry — every stage is '
      + 'idempotent under re-run, and completed work (the mathlib prewarm included) is kept.',
  },
];

function createFailureHints({ error, log_lines }) {
  const haystack = [String(error || ''), ...(Array.isArray(log_lines) ? log_lines : [])].join('\n');
  return CREATE_FAILURE_HINT_RULES
    .filter(rule => rule.re.test(haystack))
    .map(rule => ({ code: rule.code, message: rule.message }));
}

// Row 14 preflight: free-space verdict for the projects root, computed
// server-side and rendered before confirm. Warn, never block.
const CREATE_DISK_WARN_GB = Number(process.env.TRELLIS_CREATE_DISK_WARN_GB || 10);

function createDiskPreflight(disk, warnGb) {
  const gb = 1024 ** 3;
  const threshold = (warnGb === undefined ? CREATE_DISK_WARN_GB : warnGb) * gb;
  if (!disk || disk.error || !Number.isFinite(disk.avail_bytes)) {
    return { known: false, warn: false, message: null };
  }
  const availGb = disk.avail_bytes / gb;
  return {
    known: true,
    warn: disk.avail_bytes < threshold,
    avail_bytes: disk.avail_bytes,
    threshold_bytes: threshold,
    path: disk.path,
    message: disk.avail_bytes < threshold
      ? `only ${availGb.toFixed(1)} GB free at ${disk.path} (below ${Math.round(threshold / gb)} GB): `
        + 'the mathlib prewarm alone can need more. The build may fail with ENOSPC; '
        + 'freeing space first is strongly advised. You can proceed — this warns, it does not block.'
      : null,
  };
}

// --- provider auth preflight (row 16) -----------------------------------
//
// Layer-1 in the plan-§0 sense: what a browser can honestly check without a
// repo or an API call — do the OAuth credential files the configured lanes
// need exist? The three logins are browser-impossible (interactive OAuth in
// a terminal), so the wizard's job is to refuse a launch that MUST fail and
// print exactly what to run. The full provider_check (sandbox probe, real
// bursts) needs a built repo and stays where it is: inside setup (S10).

// The exact commands, per INSTALLATION.md §2b: each CLI, run interactively
// once as the operator, writes its own credential file.
const PROVIDER_LOGIN_COMMANDS = {
  codex: 'codex',
  claude: 'claude',
  gemini: 'gemini',
};

// Every lane in a config template that names a provider, by dotted path.
// Generic walk rather than a hardcoded lane list so template evolution
// (new lanes, verification agent arrays) cannot silently escape preflight.
function lanesFromConfigTemplate(cfg, prefix, depth) {
  const out = [];
  const d = depth || 0;
  if (!cfg || typeof cfg !== 'object' || d > 6) return out;
  if (typeof cfg.provider === 'string' && cfg.provider.trim()) {
    out.push({ lane: prefix || '(root)', provider: cfg.provider.trim().toLowerCase() });
  }
  for (const [key, value] of Object.entries(cfg)) {
    if (key === 'provider') continue;
    const p = prefix ? `${prefix}.${key}` : key;
    if (Array.isArray(value)) {
      value.forEach((entry, i) => {
        out.push(...lanesFromConfigTemplate(entry, `${p}[${i}]`, d + 1));
      });
    } else if (value && typeof value === 'object') {
      out.push(...lanesFromConfigTemplate(value, p, d + 1));
    }
  }
  return out;
}

// The preflight verdict for one template. Missing auth for a configured lane
// makes ok=false and the confirm endpoint refuses launch (the operator logs
// in in a terminal and re-checks). An unreadable template or an unknown
// provider FAILS OPEN with a note — this gate exists to stop a launch that
// cannot work, never to block one our own check cannot understand.
function createAuthPreflight(options) {
  const opts = options || {};
  const templatePath = opts.template || path.join(TRELLIS_ROOT, 'examples', 'trellis.config.json');
  let cfg = null;
  try {
    cfg = JSON.parse(fs.readFileSync(templatePath, 'utf-8'));
  } catch (e) {
    return {
      ok: true,
      template: templatePath,
      note: `template unreadable (${e.code || e.message}) — auth preflight skipped`,
      providers: [],
      missing: [],
    };
  }
  const lanes = lanesFromConfigTemplate(cfg);
  const byProvider = new Map();
  for (const { lane, provider } of lanes) {
    if (!byProvider.has(provider)) byProvider.set(provider, []);
    byProvider.get(provider).push(lane);
  }
  const presence = providerAuthPresence(opts.home);
  const presentByProvider = new Map(presence.map(a => [a.provider, a]));
  const credFileByProvider = new Map(
    PROVIDER_CRED_FILES.map(({ provider, rel }) => [provider, path.join('~', ...rel)]));
  const providers = [...byProvider.entries()].map(([provider, laneList]) => {
    const known = presentByProvider.get(provider);
    const loginCommand = PROVIDER_LOGIN_COMMANDS[provider] || null;
    return {
      provider,
      lanes: laneList.sort(),
      present: known ? known.present : null,
      cred_file: credFileByProvider.get(provider) || null,
      login_command: loginCommand,
      message: known
        ? (known.present
          ? null
          : `no ${credFileByProvider.get(provider)} — the ${laneList.length} lane(s) using `
            + `${provider} cannot burst. Run \`${loginCommand}\` in a terminal on this host, `
            + 'complete its login once, then re-check. (A browser cannot do the OAuth flow.)')
        : `provider "${provider}" is not one this preflight knows how to check; launch is not blocked on it`,
    };
  });
  const missing = providers.filter(p => p.present === false);
  return {
    ok: missing.length === 0,
    template: templatePath,
    providers,
    missing,
  };
}

function createRetryEligible(effectiveState) {
  return effectiveState === 'interrupted' || CREATE_FAILED_STATES.includes(effectiveState);
}

// Delete's client-side eligibility mirror. The script re-checks all of this
// and is authoritative; this only decides whether the card offers the button.
function createDeleteEligible({ effective_state, tmux_alive }) {
  if (tmux_alive) return false;
  return effective_state !== 'done' && effective_state !== 'missing';
}

// Why a requested slug cannot be claimed, or null. The script's job-dir
// mkdir stays the atomic authority (rows 4-5); this exists to answer with a
// 409 that names the existing path instead of a subprocess stderr.
function createSlugIssue(slug, opts) {
  const root = (opts && opts.root) === undefined ? PROJECTS_ROOT : opts.root;
  if (!CREATE_SLUG_RE.test(String(slug || ''))) {
    return {
      code: 'invalid_slug',
      detail: `slug must match ${CREATE_SLUG_RE} (letters, digits, - and _; starts with a letter; no dots)`,
    };
  }
  const repoPath = path.join(root, slug);
  if (fs.existsSync(repoPath)) {
    return { code: 'slug_taken', detail: `a project already exists at ${repoPath}`, path: repoPath };
  }
  const jobDir = path.join(createJobsRoot(root), slug);
  if (fs.existsSync(jobDir)) {
    return { code: 'job_exists', detail: `a create job already holds this slug (${jobDir}) — open it, retry it, or delete it`, path: jobDir };
  }
  let token = null;
  if (opts && 'token' in opts) token = opts.token;
  else { try { token = controlToken(); } catch { token = null; } }
  if (token && slug === token) {
    // A project dir equal to the control token would flip `${BASE}/<slug>/`
    // into control mode and make the project unreachable (see
    // mintControlToken) — the same collision, approached from the other side.
    return { code: 'slug_reserved', detail: 'this name is reserved; pick another' };
  }
  return null;
}

// --- impure readers ------------------------------------------------------

// One `tmux list-sessions` covers every job on the page; briefly cached so a
// poll sweep is one subprocess, not one per card. Exact-name matching:
// has-session does prefix matching.
const TMUX_SESSIONS_TTL_MS = 4000;
let TMUX_SESSIONS_CACHE = null;

function listTmuxSessionsCached() {
  const now = Date.now();
  if (TMUX_SESSIONS_CACHE && (now - TMUX_SESSIONS_CACHE.ts) < TMUX_SESSIONS_TTL_MS) {
    return TMUX_SESSIONS_CACHE.value;
  }
  let value = [];
  try {
    value = execFileSync(
      'tmux',
      ['-L', process.env.TRELLIS_TMUX_SOCKET || 'trellis', 'list-sessions', '-F', '#S'],
      { encoding: 'utf-8', timeout: 3000, stdio: ['ignore', 'pipe', 'ignore'] },
    ).split('\n').filter(Boolean);
  } catch {
    value = [];  // no server on the socket = no sessions
  }
  TMUX_SESSIONS_CACHE = { ts: now, value };
  return value;
}

// A landing card for one create job. `opts.{root,now,tmuxSessions}` exist for
// the tests; production passes nothing.
function createJobCard(slug, opts) {
  const root = (opts && opts.root) === undefined ? PROJECTS_ROOT : opts.root;
  const jobDir = path.join(createJobsRoot(root), slug);
  const status = readJsonSafe(path.join(jobDir, 'status.json')) || {};
  const job = readJsonSafe(path.join(jobDir, 'job.json')) || {};
  const repoPath = job.repo || path.join(root, slug);
  const runtimeRoot = job.runtime_root || `${repoPath}-runtime`;
  const sessions = (opts && opts.tmuxSessions) || listTmuxSessionsCached();
  const tmuxAlive = sessions.includes(`trellis-create-${slug}`);
  const now = (opts && opts.now) !== undefined ? opts.now : Math.floor(Date.now() / 1000);
  const state = status.state || (fs.existsSync(jobDir) ? 'unknown' : 'missing');
  const cls = classifyCreateJob({
    state,
    updated_ts: status.updated_ts,
    now,
    tmux_alive: tmuxAlive,
    stale_secs: (opts && opts.stale_secs),
  });
  const markerPresent = fs.existsSync(path.join(repoPath, '.trellis-creating'));
  const authorityRoot = job.authority_root || null;
  const freshOnlyBlocked = job.retry_policy === 'fresh-only'
    && (fs.existsSync(repoPath) || fs.existsSync(runtimeRoot)
      || (authorityRoot && fs.existsSync(authorityRoot)));
  // Abandoned-job visibility (row 9): a job parked at awaiting_targets is
  // stable forever BY DESIGN, so the card says how long it has waited rather
  // than letting "updated 3w ago" read as something being wrong.
  const parkedSecs = cls.effective_state === 'awaiting_targets' && status.updated_ts
    ? Math.max(0, now - Number(status.updated_ts))
    : null;
  return {
    kind: 'create_job',
    slug,
    state,
    effective_state: cls.effective_state,
    bucket: cls.bucket,
    reason: cls.reason,
    stage: status.stage || null,
    phase: status.phase || null,
    error: status.error || null,
    started_ts: status.started_ts || null,
    updated_ts: status.updated_ts || null,
    heartbeat_age_secs: cls.heartbeat_age_secs,
    parked_secs: parkedSecs,
    tmux_alive: tmuxAlive,
    create_flow: job.create_flow || 'math',
    paper_name: job.paper_name || null,
    goal_name: job.goal_name || null,
    crate_name: job.crate_name || null,
    selected: job.selected || null,
    references: (Array.isArray(job.references) ? job.references : [])
      .map(parseReferenceSpec).filter(Boolean).map(r => ({ id: r.id, source_id: r.source_id })),
    has_resolution: fs.existsSync(path.join(jobDir, 'targets_resolution.json')),
    repo_path: repoPath,
    repo_exists: fs.existsSync(repoPath),
    marker_present: markerPresent,
    runtime_root: runtimeRoot,
    retry_eligible: createRetryEligible(cls.effective_state) && !freshOnlyBlocked,
    delete_eligible: createDeleteEligible({
      effective_state: cls.effective_state,
      tmux_alive: tmuxAlive,
    }),
  };
}

// Every create job the landing page should show. Two sources: job dirs, and
// marker-bearing repos with NO job record (a crash between claim and first
// write, or a hand-deleted job dir) — those synthesize an `interrupted` card
// so the repo is never invisible. `done` jobs whose marker is gone have
// graduated into the ordinary project list and are suppressed here.
function createJobsIndex(opts) {
  const root = (opts && opts.root) === undefined ? PROJECTS_ROOT : opts.root;
  const jobsRoot = createJobsRoot(root);
  let slugs = [];
  try {
    slugs = fs.readdirSync(jobsRoot, { withFileTypes: true })
      .filter(e => e.isDirectory() && CREATE_SLUG_RE.test(e.name))
      .map(e => e.name);
  } catch { /* no jobs dir yet */ }
  const cards = [];
  const seen = new Set();
  for (const slug of slugs) {
    seen.add(slug);
    const card = createJobCard(slug, opts);
    if (card.state === 'done' && !card.marker_present) continue;
    cards.push(card);
  }
  let entries = [];
  try { entries = fs.readdirSync(root, { withFileTypes: true }); } catch {}
  for (const entry of entries) {
    if (!entry.isDirectory() || seen.has(entry.name) || !CREATE_SLUG_RE.test(entry.name)) continue;
    if (!fs.existsSync(path.join(root, entry.name, '.trellis-creating'))) continue;
    cards.push({
      kind: 'create_job',
      slug: entry.name,
      state: 'unknown',
      effective_state: 'interrupted',
      bucket: 'create_failed',
      reason: 'repo bears .trellis-creating but no create-job record exists',
      stage: null,
      phase: null,
      error: null,
      started_ts: null,
      updated_ts: null,
      heartbeat_age_secs: null,
      parked_secs: null,
      tmux_alive: false,
      paper_name: null,
      selected: null,
      references: [],
      has_resolution: false,
      repo_path: path.join(root, entry.name),
      repo_exists: true,
      marker_present: true,
      runtime_root: path.join(root, `${entry.name}-runtime`),
      retry_eligible: false,
      delete_eligible: true,
    });
  }
  cards.sort((a, b) => a.slug.localeCompare(b.slug));
  return cards;
}

// Rolling 50-line log tail, read incrementally from the cached offset (the
// forEachFileLineFromOffset idiom the plan names).
const CREATE_LOG_TAIL_LIMIT = 50;
const CREATE_LOG_TAILS = new Map();

function createLogTail(slug, root) {
  const file = path.join(createJobsRoot(root), slug, 'create.log');
  let stat;
  try { stat = fs.statSync(file); } catch { return { lines: [], bytes: 0 }; }
  let cached = CREATE_LOG_TAILS.get(slug);
  // A shrunken file means the log was replaced (delete + recreate); restart.
  if (!cached || cached.offset > stat.size) cached = { offset: 0, lines: [] };
  try {
    cached.offset = forEachFileLineFromOffset(file, cached.offset, (line) => {
      cached.lines.push(line);
      if (cached.lines.length > CREATE_LOG_TAIL_LIMIT) {
        cached.lines.splice(0, cached.lines.length - CREATE_LOG_TAIL_LIMIT);
      }
    });
  } catch { /* mid-rotation read; next poll catches up */ }
  CREATE_LOG_TAILS.set(slug, cached);
  return { lines: [...cached.lines], bytes: stat.size };
}

// The full polling payload for one job (GET create-status/:slug).
function createStatusPayload(slug, opts) {
  const root = (opts && opts.root) === undefined ? PROJECTS_ROOT : opts.root;
  const jobDir = path.join(createJobsRoot(root), slug);
  if (!fs.existsSync(jobDir)) return { slug, exists: false };
  const card = createJobCard(slug, opts);
  const job = readJsonSafe(path.join(jobDir, 'job.json')) || {};
  const resolution = readJsonSafe(path.join(jobDir, 'targets_resolution.json'));
  const prevResolution = readJsonSafe(path.join(jobDir, 'targets_resolution.prev.json'));
  const log = createLogTail(slug, root);
  // Row 28: the script rotates create.log between phases; the payload says
  // so, so a suddenly-short log reads as rotation, not loss.
  log.rotated = fs.existsSync(path.join(jobDir, 'create.log.1'));
  log.notice = log.rotated
    ? 'log rotated at the 10 MB cap — earlier output is kept at create.log.1 in the job dir'
    : null;
  const disk = (opts && opts.disk) !== undefined ? opts.disk : diskUsage(root);
  return {
    ...card,
    exists: true,
    job_dir: jobDir,
    resolution: interpretTargetsResolution(resolution),
    // "What changed since I last looked": present only after a re-resolve
    // (phase A keeps the previous resolution beside the current one).
    resolution_diff: diffTargetsResolutions(prevResolution, resolution),
    settings: createJobSettings(job),
    log,
    hints: createFailureHints({ error: card.error, log_lines: log.lines }),
    disk_preflight: createDiskPreflight(disk),
    auth_preflight: createAuthPreflight({
      template: job.template || undefined,
      home: opts && opts.home,
    }),
  };
}

function createJobSettings(job) {
  if (!job || typeof job !== 'object') return null;
  const rawRoles = job.role_overrides && typeof job.role_overrides === 'object'
    && !Array.isArray(job.role_overrides) ? job.role_overrides : null;
  const roleOverrides = rawRoles && Object.keys(rawRoles).length ? rawRoles : null;
  if (!roleOverrides && job.grunts === undefined && job.grunt_wall === undefined
      && !job.remote_url && job.allow_same_model_lanes === undefined) return null;
  return {
    role_overrides: roleOverrides || {},
    grunts: job.grunts || null,
    grunt_wall: job.grunt_wall || null,
    remote_url: job.remote_url || null,
    ...(job.allow_same_model_lanes === undefined ? {}
      : { allow_same_model_lanes: job.allow_same_model_lanes === true }),
  };
}

// Config templates the wizard may offer. Enumerated server-side — the
// browser never names a path — and validated against on submit.
function listCreateTemplates() {
  const dir = path.join(TRELLIS_ROOT, 'examples');
  let entries = [];
  try { entries = fs.readdirSync(dir); } catch { return []; }
  const out = [];
  for (const name of entries.sort()) {
    if (!name.endsWith('.config.json')) continue;
    const config = path.join(dir, name);
    const policy = config.replace(/\.config\.json$/, '.policy.json');
    if (!fs.existsSync(policy)) continue;
    out.push({
      path: config,
      label: name.replace(/\.config\.json$/, ''),
      default: name === 'trellis.config.json',
    });
  }
  return out;
}

// Every model-bearing role in one config template, expressed as the config
// path the role occupies. Arrays deliberately do not contribute an index:
// all entries in a verifier pool are one operator-facing role and receive the
// same launch selection. This is a schema walk, not a role-name list; adding
// another provider+model block to a shipped template makes it appear here.
function launcherRolesFromConfigTemplate(cfg, prefix, depth) {
  const out = [];
  const d = depth || 0;
  if (!cfg || typeof cfg !== 'object' || d > 12) return out;
  if (Array.isArray(cfg)) {
    for (const entry of cfg) out.push(...launcherRolesFromConfigTemplate(entry, prefix, d + 1));
    return [...new Set(out)];
  }
  if (Object.prototype.hasOwnProperty.call(cfg, 'provider')
      && Object.prototype.hasOwnProperty.call(cfg, 'model') && prefix) {
    out.push(prefix);
  }
  for (const [key, value] of Object.entries(cfg)) {
    if (!value || typeof value !== 'object') continue;
    const child = prefix ? `${prefix}.${key}` : key;
    out.push(...launcherRolesFromConfigTemplate(value, child, d + 1));
  }
  return [...new Set(out)];
}

function launcherRoleLabel(key) {
  const leaf = String(key || '').split('.').pop().replace(/_agents$/, '');
  const readable = leaf.replace(/_/g, ' ');
  return String(key || '').startsWith('verification.') ? `${readable} verifier` : readable;
}

// Role paths plus the template values the shared settings component should
// select initially. Arrays deliberately collapse to one path, exactly like
// launcherRolesFromConfigTemplate: one control updates the whole lane pool.
function launcherRoleSettingsFromConfigTemplate(cfg, prefix, depth, seen) {
  const out = [];
  const d = depth || 0;
  const emitted = seen || new Set();
  if (!cfg || typeof cfg !== 'object' || d > 12) return out;
  if (Array.isArray(cfg)) {
    for (const entry of cfg) {
      out.push(...launcherRoleSettingsFromConfigTemplate(entry, prefix, d + 1, emitted));
    }
    return out;
  }
  if (Object.prototype.hasOwnProperty.call(cfg, 'provider')
      && Object.prototype.hasOwnProperty.call(cfg, 'model') && prefix
      && !emitted.has(prefix)) {
    emitted.add(prefix);
    out.push({
      key: prefix,
      label: launcherRoleLabel(prefix),
      provider: String(cfg.provider || ''),
      model: String(cfg.model || ''),
      effort: String(cfg.effort || ''),
    });
  }
  for (const [key, value] of Object.entries(cfg)) {
    if (!value || typeof value !== 'object') continue;
    const child = prefix ? `${prefix}.${key}` : key;
    out.push(...launcherRoleSettingsFromConfigTemplate(value, child, d + 1, emitted));
  }
  return out;
}

// The launcher writes one of listCreateTemplates() plus its paired policy.
// Union their discovered config roles in template/path order. Policy carries
// verifier selectors but no provider/model/effort binding, so it contributes
// no additional role control.
function launcherRoleOptions(templates) {
  const seen = new Set();
  const roles = [];
  for (const template of templates || listCreateTemplates()) {
    let cfg;
    try { cfg = JSON.parse(fs.readFileSync(template.path, 'utf-8')); } catch { continue; }
    for (const key of launcherRolesFromConfigTemplate(cfg)) {
      if (seen.has(key)) continue;
      seen.add(key);
      roles.push({ key, label: launcherRoleLabel(key) });
    }
  }
  return roles;
}

// --- the endpoints -------------------------------------------------------

function runCreateScript(args, timeoutMs, cb) {
  execFile('bash', [CREATE_RUN_SCRIPT, ...args], {
    env: { ...process.env, TRELLIS_PROJECTS_ROOT: PROJECTS_ROOT },
    timeout: timeoutMs,
    maxBuffer: 8 * 1024 * 1024,
    encoding: 'utf-8',
  }, (err, stdout, stderr) => cb(err, String(stdout || ''), String(stderr || '')));
}

function respondScriptResult(res, okBody) {
  return (err, stdout, stderr) => {
    if (err) {
      res.status(400).json({
        error: 'create_script_failed',
        detail: (stderr || stdout || err.message || '').trim().slice(-2000),
        exit_code: typeof err.code === 'number' ? err.code : null,
      });
      return;
    }
    res.json({ ok: true, output: stdout.trim(), ...(okBody || {}) });
  };
}

// express.raw throws PayloadTooLargeError past the cap; answer it as JSON
// with the limit named rather than Express's default HTML page.
function uploadErrorMiddleware(err, _req, res, next) {
  if (err && err.type === 'entity.too.large') {
    res.status(413).json({ error: 'upload_too_large', detail: `uploads are capped at ${CREATE_UPLOAD_LIMIT}` });
    return;
  }
  next(err);
}

registerControlRoute('uploads',
  [express.raw({ type: () => true, limit: CREATE_UPLOAD_LIMIT }), uploadErrorMiddleware],
  (req, res) => {
    let dir = null;
    try {
      const id = crypto.randomBytes(8).toString('hex');
      dir = path.join(createUploadsRoot(), id);
      fs.mkdirSync(dir, { recursive: true, mode: 0o700 });
      const kind = String((req.query && req.query.kind) || 'paper');
      if (kind === 'paper') {
        const intake = intakeUpload(req.body);
        if (!intake.ok) {
          fs.rmSync(dir, { recursive: true, force: true });
          res.status(400).json({ error: 'upload_rejected', detail: intake.error });
          return;
        }
        // Stored as UTF-8 text: the transcode (when any) happened at intake,
        // and everything downstream — resolver, kernel, setup — reads UTF-8.
        fs.writeFileSync(path.join(dir, 'paper.tex'), intake.text, 'utf-8');
        res.json({
          upload_id: id,
          bytes: Buffer.byteLength(intake.text, 'utf-8'),
          sha256: crypto.createHash('sha256').update(intake.text, 'utf-8').digest('hex'),
          transcoded_from: intake.transcoded_from,
          warnings: intake.warnings,
        });
        return;
      }
      const handler = CREATE_UPLOAD_KIND_HANDLERS.get(kind);
      if (!handler) {
        fs.rmSync(dir, { recursive: true, force: true });
        res.status(400).json({ error: 'upload_kind_unknown', detail: `unknown upload kind: ${kind}` });
        return;
      }
      const stored = handler(req.body, {
        id, dir, limit: CREATE_UPLOAD_LIMIT, limit_bytes: CREATE_UPLOAD_LIMIT_BYTES,
        query: Object.freeze({ ...(req.query || {}) }),
      });
      if (!stored || stored.ok === false) {
        fs.rmSync(dir, { recursive: true, force: true });
        res.status(400).json({
          error: 'upload_rejected',
          detail: stored && stored.error ? stored.error : 'upload handler rejected the file',
        });
        return;
      }
      if (!stored.file_name || !/^[A-Za-z0-9._-]+$/.test(stored.file_name)) {
        throw new Error('upload handler returned an unsafe file name');
      }
      const content = stored.content;
      fs.writeFileSync(path.join(dir, stored.file_name), content,
        typeof content === 'string' ? 'utf-8' : undefined);
      const bytes = typeof content === 'string' ? Buffer.byteLength(content, 'utf-8') : content.length;
      const digest = crypto.createHash('sha256').update(content,
        typeof content === 'string' ? 'utf-8' : undefined).digest('hex');
      res.json({ upload_id: id, kind, bytes, sha256: digest, ...(stored.response || {}) });
    } catch (e) {
      if (dir) {
        try { fs.rmSync(dir, { recursive: true, force: true }); } catch {}
      }
      res.status(500).json({ error: e.message });
    }
  });

function validatedEnvMap(raw) {
  if (raw === undefined || raw === null) return [];
  if (!Array.isArray(raw)) throw new Error('env_map must be an array of "alias=canonical" strings');
  const out = [];
  for (const entry of raw) {
    const s = String(entry || '').trim();
    // Shape only — normalize_paper_envs.py owns canonical-set validation and
    // refuses loudly; this stops shell-hostile junk from reaching argv.
    if (!/^[A-Za-z0-9@*+-]+=[A-Za-z]+$/.test(s)) {
      throw new Error(`env_map entry ${JSON.stringify(entry)} is not ALIAS=canonical`);
    }
    out.push(s);
  }
  return out;
}

function validatedMainResultEnvs(raw) {
  if (raw === undefined || raw === null || raw === '') return null;
  const s = String(raw).trim().toLowerCase();
  if (!/^[a-z]+(,[a-z]+)*$/.test(s)) {
    throw new Error('main_result_envs must be a comma-separated env list');
  }
  return s;
}

// Template overrides: per-role model/effort, grunts, and remote_url. Each is
// optional at the API boundary; the browser sends all displayed roles with
// its explicit defaults. Validation mirrors `validate_overrides` in
// trellis_create_run.sh so a bad value is a 4xx while the form is still on
// screen, not a phase-B failure twenty minutes into a mathlib fetch. The
// legacy global model/effort pair remains accepted for non-browser callers.
//
// Effort is a charset check rather than an allowlist on purpose: efforts are
// provider-specific (codex `xhigh`, claude `max`, ...) and a list hardcoded
// here would rot the first time a provider adds a tier.
function validatedOverrides(body, options) {
  const out = {};
  const rawRoles = body.role_overrides;
  if (rawRoles !== undefined && rawRoles !== null) {
    if (!rawRoles || typeof rawRoles !== 'object' || Array.isArray(rawRoles)) {
      throw new Error('role_overrides must be an object keyed by launcher role');
    }
    const configuredRoles = options && Array.isArray(options.roles)
      ? options.roles : launcherRoleOptions();
    const allowedRoles = new Set(configuredRoles.map(role => (
      typeof role === 'string' ? role : role.key
    )));
    const roles = {};
    for (const [role, raw] of Object.entries(rawRoles)) {
      if (!allowedRoles.has(role)) {
        throw new Error(`role_overrides contains unknown launcher role ${JSON.stringify(role)}`);
      }
      if (!raw || typeof raw !== 'object' || Array.isArray(raw)) {
        throw new Error(`role_overrides.${role} must be an object`);
      }
      const roleModel = raw.model === undefined || raw.model === null ? '' : String(raw.model).trim();
      const roleEffort = raw.effort === undefined || raw.effort === null ? '' : String(raw.effort).trim();
      const selected = {};
      if (roleModel) {
        if (!/^[A-Za-z0-9][A-Za-z0-9._-]*$/.test(roleModel)) {
          throw new Error(`role_overrides.${role}.model must match [A-Za-z0-9][A-Za-z0-9._-]*`);
        }
        selected.model = roleModel;
      }
      if (roleEffort) {
        if (!/^[a-z][a-z0-9_-]*$/.test(roleEffort)) {
          throw new Error(`role_overrides.${role}.effort must match [a-z][a-z0-9_-]*`);
        }
        selected.effort = roleEffort;
      }
      if (Object.keys(selected).length) roles[role] = selected;
    }
    if (Object.keys(roles).length) out.role_overrides = roles;
  }
  const model = body.model === undefined || body.model === null ? '' : String(body.model).trim();
  if (model) {
    if (!/^[A-Za-z0-9][A-Za-z0-9._-]*$/.test(model)) {
      throw new Error('model must match [A-Za-z0-9][A-Za-z0-9._-]*');
    }
    out.model = model;
  }
  const effort = body.effort === undefined || body.effort === null ? '' : String(body.effort).trim();
  if (effort) {
    if (!/^[a-z][a-z0-9_-]*$/.test(effort)) {
      throw new Error('effort must match [a-z][a-z0-9_-]*');
    }
    out.effort = effort;
  }
  // `grunts` accepts false/'off' (disable) or a positive integer (enable
  // with that pool size). Absent leaves the template's own choice alone,
  // which for the shipped default is off.
  if (body.grunts !== undefined && body.grunts !== null && body.grunts !== '') {
    const g = body.grunts;
    if (g === false || g === 'off' || g === 0 || g === '0') {
      out.grunts = 'off';
    } else {
      const n = Number(g);
      if (!Number.isInteger(n) || n < 1 || n > 99) {
        throw new Error("grunts must be 'off' or an integer 1-99");
      }
      out.grunts = String(n);
    }
  }
  // Per-attempt wall-clock cap for a grunt, seconds. The wall is a hard
  // kill — hitting it destroys that attempt's work — so the floor is the
  // observed cost of a single compile round-trip: below it a grunt cannot
  // finish anything and only burns pool slots.
  if (body.grunt_wall !== undefined && body.grunt_wall !== null && body.grunt_wall !== '') {
    const w = Number(body.grunt_wall);
    if (!Number.isInteger(w) || w < 300 || w > 21600) {
      throw new Error('grunt_wall must be an integer 300-21600 seconds');
    }
    out.grunt_wall = String(w);
  }
  const remote = body.remote_url === undefined || body.remote_url === null ? '' : String(body.remote_url).trim();
  if (remote) {
    // The two shapes setup writes into config.git.remote_url. Anything
    // carrying shell or newline payload is refused: this string ends up in
    // a config the supervisor hands to git.
    if (!/^(git@[A-Za-z0-9._-]+:[A-Za-z0-9._/-]+(\.git)?|https:\/\/[A-Za-z0-9._-]+\/[A-Za-z0-9._/-]+(\.git)?)$/.test(remote)) {
      throw new Error('remote_url must be git@host:owner/repo(.git) or https://host/owner/repo(.git)');
    }
    out.remote_url = remote;
  }
  return out;
}

// The wizard's references array validated into script `--reference` specs.
// Two entry shapes: {id, upload_id, source_id?} names a fresh upload;
// {id, keep: true, source_id?} re-points at the job's already-ingested
// refs/<id>.tex, so editing the set (adding one, renaming another — row
// 26's recovery) never forces re-uploading files the browser no longer
// holds. Row 25: a bad id, a duplicate id, or a missing upload answers 4xx
// HERE — at request time, before any job state changes (bad encoding was
// already refused by POST uploads).
function referenceSpecsFromBody(raw, opts) {
  const references = Array.isArray(raw) ? raw : [];
  const specs = [];
  const seen = new Set();
  for (const ref of references) {
    const id = String((ref && ref.id) || '');
    if (!CREATE_REFERENCE_ID_RE.test(id)) {
      return { error: { status: 400, body: { error: 'bad_reference_id', detail: `reference id ${JSON.stringify(id)} must match ${CREATE_REFERENCE_ID_RE}` } } };
    }
    if (seen.has(id)) {
      return { error: { status: 400, body: { error: 'duplicate_reference_id', detail: `reference id "${id}" appears twice — reference ids are unique per run` } } };
    }
    seen.add(id);
    let refFile;
    if (ref && ref.keep === true) {
      const jobDir = opts && opts.jobDir;
      refFile = jobDir ? path.join(jobDir, 'refs', `${id}.tex`) : null;
      if (!refFile || !fs.existsSync(refFile)) {
        return { error: { status: 400, body: { error: 'reference_not_kept', detail: `reference ${id}: no already-ingested file to keep — upload it` } } };
      }
    } else {
      refFile = createUploadFile(ref && ref.upload_id);
      if (!refFile || !fs.existsSync(refFile)) {
        return { error: { status: 400, body: { error: 'upload_missing', detail: `reference ${id}: upload_id does not name a stored upload` } } };
      }
    }
    const sourceId = String((ref && ref.source_id) || id).replace(/[:\n\t]/g, ' ').trim() || id;
    specs.push(`${id}=${refFile}:${sourceId}`);
  }
  return { specs };
}

registerControlRoute('create-jobs', [express.json()], (req, res) => {
  const body = req.body || {};
  const slug = String(body.slug || '');
  const issue = createSlugIssue(slug);
  if (issue) {
    res.status(issue.code === 'invalid_slug' ? 400 : 409).json({ error: issue.code, ...issue });
    return;
  }
  const loogle = body.loogle === 'on' ? 'on' : body.loogle === 'off' ? 'off' : null;
  if (!loogle) {
    res.status(400).json({ error: 'loogle_required', detail: 'loogle must be "on" or "off" (does this host run a local Loogle server?)' });
    return;
  }
  const paperFile = createUploadFile(body.paper_upload_id);
  if (!paperFile || !fs.existsSync(paperFile)) {
    res.status(400).json({ error: 'upload_missing', detail: 'paper_upload_id does not name a stored upload — upload the paper first' });
    return;
  }
  const args = ['start', slug, '--paper', paperFile, '--loogle', loogle];
  if (body.template !== undefined && body.template !== null && body.template !== '') {
    const allowed = listCreateTemplates().find(t => t.path === body.template);
    if (!allowed) {
      res.status(400).json({ error: 'template_unknown', detail: 'template must be one of the enumerated template paths (GET create-jobs.json lists them)' });
      return;
    }
    args.push('--template', allowed.path);
  }
  let envMap;
  let mainEnvs;
  let overrides;
  try {
    envMap = validatedEnvMap(body.env_map);
    mainEnvs = validatedMainResultEnvs(body.main_result_envs);
    overrides = validatedOverrides(body);
  } catch (e) {
    res.status(400).json({ error: 'bad_request', detail: e.message });
    return;
  }
  for (const spec of envMap) args.push('--env-map', spec);
  if (mainEnvs) args.push('--main-result-envs', mainEnvs);
  if (overrides.model) args.push('--model', overrides.model);
  if (overrides.effort) args.push('--effort', overrides.effort);
  for (const [role, selected] of Object.entries(overrides.role_overrides || {})) {
    if (selected.model) args.push('--role-model', `${role}=${selected.model}`);
    if (selected.effort) args.push('--role-effort', `${role}=${selected.effort}`);
  }
  if (overrides.grunts) args.push('--grunts', overrides.grunts);
  if (overrides.grunt_wall) args.push('--grunt-wall', overrides.grunt_wall);
  if (overrides.remote_url) args.push('--remote-url', overrides.remote_url);
  const refs = referenceSpecsFromBody(body.references);
  if (refs.error) {
    res.status(refs.error.status).json(refs.error.body);
    return;
  }
  for (const spec of refs.specs) args.push('--reference', spec);
  runCreateScript(args, 30000, respondScriptResult(res, { slug }));
});

registerControlReadRoute('create-jobs.json', (_req, res) => {
  try {
    res.json({
      generated_ts: Date.now(),
      projects_root: PROJECTS_ROOT,
      jobs: createJobsIndex(),
      templates: listCreateTemplates(),
      roles: launcherRoleOptions(),
      pinned_models: CREATE_PINNED_MODELS,
      default_model: CREATE_DEFAULT_MODEL,
      default_effort: CREATE_DEFAULT_EFFORT,
      slug_rule: String(CREATE_SLUG_RE),
    });
  } catch (e) { res.status(500).json({ error: e.message }); }
});

// --- create-options.json: derived model/effort suggestion lists ----------
//
// The wizard appends these model and effort rows to its pinned <select>
// choices; this is where the derived choices come from. Everything is
// derived per request by
// `python3 -m trellis.create_options`, which reads the pricing tables in
// trellis/agents/tmux_backend.py and the KNOWN_EFFORTS registry in
// trellis/config.py — so a CLI-side change to either source reaches the
// browser with no edit here. The lists INFORM and never GATE:
// validatedOverrides stays a charset check, and a well-formed value absent
// from these lists must keep working (a pricing table is the set the system
// can COST, not the set it can RUN — same stance as the loogle probe:
// evidence beside the choice, not control over it). Token-gated read like
// the rest of the create surface; TRELLIS_CREATE_OPTIONS_CMD (read per
// request) substitutes the derivation for tests, the loogle-probe trick.
const CREATE_OPTIONS_MODULE = 'trellis.create_options';

function createOptionsArgv() {
  const override = process.env.TRELLIS_CREATE_OPTIONS_CMD;
  if (override) return override.split(' ').filter(Boolean);
  return ['python3', '-m', CREATE_OPTIONS_MODULE];
}

// Pure shaping of the module's stdout: structural normalization only — rows
// ride through verbatim (value / providers / label; a single `provider`
// string is folded into `providers` so the page renders one shape), rows
// with no usable value are dropped, an unparseable document throws for the
// route to answer as a 502. Deliberately NO value-level filtering here:
// this layer relaying the source faithfully is exactly what the
// tests/test_create_options.py drift guard pins.
function interpretCreateOptions(stdout) {
  const raw = JSON.parse(String(stdout || ''));
  if (!raw || typeof raw !== 'object' || Array.isArray(raw)) {
    throw new Error('create-options output is not an object');
  }
  const str = v => (typeof v === 'string' ? v : '');
  const rows = list => (Array.isArray(list) ? list : [])
    .filter(r => r && typeof r === 'object' && typeof r.value === 'string' && r.value)
    .map(r => ({
      value: r.value,
      providers: Array.isArray(r.providers)
        ? r.providers.map(p => str(p)).filter(Boolean)
        : (str(r.provider) ? [str(r.provider)] : []),
      label: str(r.label),
    }));
  const sources = (raw.sources && typeof raw.sources === 'object') ? raw.sources : {};
  return {
    models: rows(raw.models),
    efforts: rows(raw.efforts),
    sources: { models: str(sources.models), efforts: str(sources.efforts) },
  };
}

registerControlReadRoute('create-options.json', (_req, res) => {
  const argv = createOptionsArgv();
  execFile(argv[0], argv.slice(1), {
    cwd: TRELLIS_ROOT,
    timeout: 15000,
    maxBuffer: 4 * 1024 * 1024,
    encoding: 'utf-8',
  }, (err, stdout, stderr) => {
    if (err) {
      res.status(502).json({
        error: 'create_options_failed',
        detail: (String(stderr || '').trim() || err.message || '').slice(-2000),
      });
      return;
    }
    try {
      res.json({ ...interpretCreateOptions(stdout), generated_ts: Date.now() });
    } catch (e) {
      res.status(502).json({ error: 'create_options_unparseable', detail: e.message });
    }
  });
});

registerControlReadRoute('create-status/:slug', (req, res) => {
  const slug = String(req.params.slug || '');
  if (!CREATE_SLUG_RE.test(slug)) {
    res.status(400).json({ error: 'invalid_slug' });
    return;
  }
  try { res.json(createStatusPayload(slug)); }
  catch (e) { res.status(500).json({ error: e.message }); }
});

registerControlRoute('create-jobs/:slug/resolve', [express.json()], (req, res) => {
  const slug = String(req.params.slug || '');
  if (!CREATE_SLUG_RE.test(slug)) { res.status(400).json({ error: 'invalid_slug' }); return; }
  const flowConflict = mathCreateJobConflict(slug);
  if (flowConflict) { res.status(409).json(flowConflict); return; }
  const body = req.body || {};
  const args = ['resolve', slug];
  let envMap;
  let mainEnvs;
  try {
    envMap = validatedEnvMap(body.env_map);
    mainEnvs = body.main_result_envs === undefined ? undefined : validatedMainResultEnvs(body.main_result_envs);
  } catch (e) {
    res.status(400).json({ error: 'bad_request', detail: e.message });
    return;
  }
  for (const spec of envMap) args.push('--env-map', spec);
  // An explicit empty string means "back to the default set"; undefined means
  // "keep what the job recorded".
  if (mainEnvs !== undefined) args.push('--main-result-envs', mainEnvs === null ? '' : mainEnvs);
  if (body.paper_upload_id !== undefined && body.paper_upload_id !== null && body.paper_upload_id !== '') {
    const paperFile = createUploadFile(body.paper_upload_id);
    if (!paperFile || !fs.existsSync(paperFile)) {
      res.status(400).json({ error: 'upload_missing', detail: 'paper_upload_id does not name a stored upload' });
      return;
    }
    args.push('--paper', paperFile);
  }
  // References stay editable until confirm (row 26's recovery needs this: a
  // changed file gets a NEW id). An array REPLACES the recorded set
  // wholesale — empty array clears it; absent field keeps it. `keep`
  // entries resolve against this job's already-ingested files.
  if (body.references !== undefined) {
    const refs = referenceSpecsFromBody(body.references,
      { jobDir: path.join(createJobsRoot(), slug) });
    if (refs.error) {
      res.status(refs.error.status).json(refs.error.body);
      return;
    }
    if (refs.specs.length === 0) args.push('--clear-references');
    for (const spec of refs.specs) args.push('--reference', spec);
  }
  runCreateScript(args, 30000, respondScriptResult(res, { slug }));
});

registerControlRoute('create-jobs/:slug/confirm', [express.json()], (req, res) => {
  const slug = String(req.params.slug || '');
  if (!CREATE_SLUG_RE.test(slug)) { res.status(400).json({ error: 'invalid_slug' }); return; }
  const flowConflict = mathCreateJobConflict(slug);
  if (flowConflict) { res.status(409).json(flowConflict); return; }
  const selected = Array.isArray(req.body && req.body.selected) ? req.body.selected : [];
  if (!selected.length) {
    res.status(400).json({ error: 'selection_required', detail: 'select at least one target — a run with zero targets cannot init' });
    return;
  }
  // Row 16: refuse to launch lanes that cannot burst. The three provider
  // logins are interactive OAuth in a terminal (plan §0 — a browser cannot
  // do them), so a missing credential file makes phase B a guaranteed
  // failure; the answer names the exact command per lane. The wizard's poll
  // recomputes auth_preflight continuously, so logging in and re-checking
  // needs no page reload. TRELLIS_CREATE_SKIP_AUTH_PREFLIGHT=1 is the
  // escape hatch for a deliberately unusual setup; the preflight itself
  // fails OPEN on unreadable templates and unknown providers.
  if (process.env.TRELLIS_CREATE_SKIP_AUTH_PREFLIGHT !== '1') {
    const job = readJsonSafe(path.join(createJobsRoot(), slug, 'job.json')) || {};
    const preflight = createAuthPreflight({ template: job.template || undefined });
    if (!preflight.ok) {
      res.status(409).json({
        error: 'provider_auth_missing',
        detail: preflight.missing.map(m => m.message).join(' '),
        auth_preflight: preflight,
      });
      return;
    }
  }
  const args = ['confirm', slug];
  for (const key of selected) {
    const s = String(key || '').trim();
    if (!s || s.length > 512 || /[\n\t]/.test(s)) {
      res.status(400).json({ error: 'bad_selection', detail: `selection key ${JSON.stringify(key)} is not a candidate key` });
      return;
    }
    args.push('--select', s);
  }
  runCreateScript(args, 30000, respondScriptResult(res, { slug, selected }));
});

registerControlRoute('create-jobs/:slug/retry', [express.json()], (req, res) => {
  const slug = String(req.params.slug || '');
  if (!CREATE_SLUG_RE.test(slug)) { res.status(400).json({ error: 'invalid_slug' }); return; }
  const flowConflict = mathCreateJobConflict(slug);
  if (flowConflict) { res.status(409).json(flowConflict); return; }
  runCreateScript(['retry', slug], 30000, respondScriptResult(res, { slug }));
});

registerControlRoute('create-jobs/:slug/delete', [express.json()], (req, res) => {
  const slug = String(req.params.slug || '');
  if (!CREATE_SLUG_RE.test(slug)) { res.status(400).json({ error: 'invalid_slug' }); return; }
  const flowConflict = mathCreateJobConflict(slug);
  if (flowConflict) { res.status(409).json(flowConflict); return; }
  const args = ['delete', slug];
  const paths = Array.isArray(req.body && req.body.paths) ? req.body.paths : [];
  for (const p of paths) args.push('--confirm', String(p));
  runCreateScript(args, 60000, (err, stdout, stderr) => {
    if (err && err.code === 3) {
      // The script's confirmation gate: it answered with the doomed paths.
      // The browser shows them and posts again with `paths` echoed back —
      // the explicit confirmation NAMING THE PATHS the design requires.
      let parsed = null;
      try { parsed = JSON.parse(stdout); } catch {}
      res.status(409).json(parsed || { error: 'confirmation_required', detail: stdout.trim() });
      return;
    }
    if (err) {
      res.status(400).json({
        error: 'create_script_failed',
        detail: (stderr || stdout || err.message || '').trim().slice(-2000),
      });
      return;
    }
    CREATE_LOG_TAILS.delete(slug);
    res.json({ ok: true, output: stdout.trim(), slug });
  });
});

// --- the loogle probe ----------------------------------------------------
//
// The wizard's loogle on/off choice sits next to EVIDENCE: the result of an
// actual attempt to reach the local Loogle server, so the operator is not
// guessing. scripts/loogle_json.sh is the single source of truth for the
// endpoint URL (127.0.0.1:8088, hardcoded at loogle_json.sh:73) and for the
// failure taxonomy (curl exit 7 = nothing listening; any other failure =
// query failed or timed out) — so the probe RUNS that script rather than
// keeping a second copy of the URL or the curl handling here. Short timeout:
// the wizard must never hang on this, and it never gates form submission.
// The probe informs; the choice stays the operator's — selecting `on` for a
// server they are about to start is legitimate.

const LOOGLE_PROBE_SCRIPT = path.join(TRELLIS_ROOT, 'scripts', 'loogle_json.sh');
const LOOGLE_PROBE_QUERY = 'Nat';
const LOOGLE_PROBE_TIMEOUT_SECS = Number(process.env.TRELLIS_LOOGLE_PROBE_TIMEOUT_SECS || 4);

// Pure: (exit code, stdout, stderr) -> the verdict the page renders. Three
// server-side states — 'answering' (reachable, valid JSON, no error field),
// 'erroring' (reachable but the service reported an error, answered
// non-JSON, or the query failed/timed out), 'unreachable' (curl exit 7:
// nothing listens on the port) — plus the page's own fourth state,
// not-checked-yet, which exists before any response arrives.
function interpretLoogleProbe({ code, stdout, stderr }) {
  const firstLine = (s, fallback) => (String(s || '').trim().split('\n')[0] || fallback);
  if (code === 7) {
    return {
      state: 'unreachable',
      detail: firstLine(stderr, 'Loogle is not reachable (connection refused) — no local server is listening.'),
    };
  }
  if (code !== 0 && code !== null && code !== undefined) {
    return { state: 'erroring', detail: firstLine(stderr, `probe failed (exit ${code})`) };
  }
  let payload;
  try { payload = JSON.parse(String(stdout || '')); } catch {
    return { state: 'erroring', detail: 'Loogle answered with output that is not valid JSON.' };
  }
  if (payload && typeof payload === 'object' && !Array.isArray(payload) && payload.error) {
    return { state: 'erroring', detail: `Loogle answered but reported an error: ${String(payload.error).slice(0, 300)}` };
  }
  const hits = payload && typeof payload === 'object' && Array.isArray(payload.hits)
    ? payload.hits.length
    : (payload && typeof payload === 'object' && Number.isFinite(payload.count) ? payload.count : null);
  return {
    state: 'answering',
    detail: hits === null
      ? 'Loogle answered the probe query.'
      : `Loogle answered the probe query (${hits} hit${hits === 1 ? '' : 's'}).`,
  };
}

// Token-gated read: the probe reveals host-local service state, which
// belongs behind the same door as the rest of the create surface. GET is
// honest — probing is idempotent and mutates nothing. TRELLIS_LOOGLE_PROBE_CMD
// (read per request) substitutes the script for the tests, the same trick as
// the TRELLIS_CREATE_*_CMD overrides.
registerControlReadRoute('loogle-probe.json', (_req, res) => {
  const started = Date.now();
  const override = process.env.TRELLIS_LOOGLE_PROBE_CMD;
  const argv = override
    ? override.split(' ').filter(Boolean)
    : ['bash', LOOGLE_PROBE_SCRIPT, '--timeout', String(LOOGLE_PROBE_TIMEOUT_SECS), '--raw', LOOGLE_PROBE_QUERY];
  execFile(argv[0], argv.slice(1), {
    timeout: (LOOGLE_PROBE_TIMEOUT_SECS + 4) * 1000,
    maxBuffer: 4 * 1024 * 1024,
    encoding: 'utf-8',
  }, (err, stdout, stderr) => {
    const verdict = (err && err.killed)
      ? { state: 'erroring', detail: `probe timed out after ${LOOGLE_PROBE_TIMEOUT_SECS}s (killed)` }
      : interpretLoogleProbe({
        code: err ? (typeof err.code === 'number' ? err.code : 1) : 0,
        stdout: String(stdout || ''),
        stderr: String(stderr || ''),
      });
    res.json({
      ...verdict,
      query: LOOGLE_PROBE_QUERY,
      timeout_secs: LOOGLE_PROBE_TIMEOUT_SECS,
      elapsed_ms: Date.now() - started,
      checked_ts: Date.now(),
      source: 'scripts/loogle_json.sh',
    });
  });
});

// This module is intentionally optional: PV-excluded public releases delete
// it (and its client chunk), leaving no routes or browser controls behind.
// Only absence of this exact top-level module is tolerated; a missing nested
// dependency is a broken private build and must still stop startup.
try {
  require('./pv_create').install({
    TRELLIS_ROOT,
    PROJECTS_ROOT,
    CREATE_SLUG_RE,
    CREATE_UPLOAD_ID_RE,
    CREATE_UPLOAD_LIMIT_BYTES,
    CREATE_PINNED_MODELS,
    CREATE_DEFAULT_MODEL,
    CREATE_DEFAULT_EFFORT,
    express,
    fs,
    path,
    crypto,
    execFile,
    intakeUpload,
    createSlugIssue,
    createJobsRoot,
    createUploadsRoot,
    readJsonSafe,
    createStatusPayload,
    createJobsIndex,
    launcherRoleSettingsFromConfigTemplate,
    validatedOverrides,
    registerControlRoute,
    registerControlReadRoute,
    registerCreateUploadKind,
    registerLandingHtmlTransform,
  });
} catch (error) {
  const missingOptionalModule = error && error.code === 'MODULE_NOT_FOUND'
    && /^Cannot find module ['"]\.\/pv_create(?:\.js)?['"](?:\r?\n|$)/
      .test(String(error.message || ''));
  if (!missingOptionalModule) throw error;
}

// Liveness only, and deliberately empty of run state: this is the one path
// `readTokenMiddleware` lets through unauthenticated, because a process
// supervisor has to be able to ask whether the port is answering without
// being handed a secret. `scripts/start_viewer.sh` polls it after launch.
// The Host check still applies — a probe comes from the host itself, so it
// arrives as a loopback literal.
app.get(healthProbePath(), (_req, res) => {
  res.set('Cache-Control', 'no-store');
  res.json({ ok: true, base: BASE, readsRequireToken: READS_REQUIRE_TOKEN });
});

function startServer() {
  // Express 5's app.listen registers this callback as BOTH the 'listening'
  // handler and the server 'error' handler (server.once('error', done)).
  // With the old zero-arg callback, EADDRINUSE printed the startup banner
  // and exited 0 with no error — a silent death that looks like success.
  //
  // The host argument is the point of this call. Without it Node binds
  // 0.0.0.0, which is how an unauthenticated operator control surface ended
  // up on every interface of a host with no firewall.
  return app.listen(PORT, BIND_HOST, (err) => {
    if (err) {
      console.error(`[viewer] listen failed on ${BIND_HOST}:${PORT}: ${err.code || err.message}`);
      process.exit(1);
    }
    writeStatic();
    console.log(`Tablet viewer at http://localhost:${PORT}${BASE}/`);
    console.log(`Projects root: ${PROJECTS_ROOT}`);
    if (CONTROL_ENABLED) {
      try {
        // The control URL IS the delivery mechanism: browsing under this
        // prefix is what turns the controls on. Everything below it behaves
        // exactly like the plain viewer.
        const tok = controlToken();
        console.log(`[viewer] control plane ON — controls live under the token path:`);
        console.log(`            ${BASE}/${tok}/            (landing, controls on)`);
        console.log(`            ${BASE}/${tok}/<project>   (a run, controls on)`);
        console.log(`[viewer] plain ${BASE}/… keeps working unchanged, without controls.`);
        console.log(`[viewer] token file: ${controlTokenPath()}`);
      } catch (e) {
        console.error(`[viewer] control plane ON but the token file is unusable (${e.message}) — every mutating request will fail.`);
      }
    } else {
      console.log('[viewer] control plane OFF (TRELLIS_VIEWER_CONTROL=0) — read-only; pause/resume/feedback answer 503.');
    }
    // Which read regime is in force, always, so it is never a guess.
    if (READS_REQUIRE_TOKEN) {
      console.log(`[viewer] reads REQUIRE the token path (TRELLIS_VIEWER_READ_TOKEN=${READ_TOKEN_MODE}${READ_TOKEN_MODE === 'auto' ? `, bind ${BIND_HOST} is not loopback` : ''}).`);
      console.log(`[viewer]   plain ${BASE}/… answers 403; ${BASE}/api/health.json stays open for liveness probes.`);
      console.log('[viewer]   over plain HTTP the token is on the wire — this stops scanners, not an on-path observer.');
    } else {
      console.log(`[viewer] reads are OPEN, no token (TRELLIS_VIEWER_READ_TOKEN=${READ_TOKEN_MODE}${READ_TOKEN_MODE === 'auto' ? `, bind ${BIND_HOST} is loopback` : ''}).`);
    }
    if (HOST_CHECK_DISABLED) {
      console.warn(`[viewer] Host check DISABLED (${HOST_ENV_VAR}=*) — DNS-rebinding protection is off.`);
    } else {
      console.log(`[viewer] Host check ON, answering to: ${[...ALLOWED_HOSTS].sort().join(', ')} (+ 127.x.x.x / ::1). Extend with ${HOST_ENV_VAR}.`);
    }
    if (!bindIsLoopback(BIND_HOST)) {
      // Loud on purpose. This is a deliberate choice with real consequences
      // and it should never happen by accident or go unnoticed in a log.
      console.warn('');
      console.warn('  ============================================================');
      console.warn(`  [viewer] BOUND TO ${BIND_HOST}:${PORT} — NOT loopback.`);
      console.warn('  This was requested explicitly via TRELLIS_VIEWER_BIND.');
      console.warn('  The viewer is an operator control surface running as your');
      console.warn('  user, next to provider credentials. Anyone who can reach');
      console.warn(`  ${BIND_HOST}:${PORT} can read every run.`);
      console.warn(`  Mutating routes still require the ${CONTROL_HEADER} header,`);
      console.warn('  and controls appear only under the secret token path —');
      console.warn('  but set TRELLIS_VIEWER_CONTROL=0 before any public');
      console.warn('  exposure, and see SECURITY.md.');
      console.warn('  ============================================================');
      console.warn('');
    }
  });
}

if (require.main === module) {
  if (process.argv[2] === '--progress-worker') {
    runProgressWorker();
  } else {
    startServer();
  }
}

module.exports = {
  app,
  // control plane
  BIND_HOST,
  CONTROL_ENABLED,
  CONTROL_HEADER,
  CONTROL_PREFIX,
  CONTROL_ROUTE_TAILS,
  CONTROL_ROUTE_METHODS,
  CONTROL_TOKEN_MARKER,
  bindIsLoopback,
  pathIsPrivateToUs,
  // Host validation (DNS-rebinding defence) + token-gated reads
  HOST_ENV_VAR,
  HOST_CHECK_DISABLED,
  ALLOWED_HOSTS,
  READ_TOKEN_MODE,
  READS_REQUIRE_TOKEN,
  normalizeHostName,
  isLoopbackLiteral,
  machineHostNames,
  buildAllowedHosts,
  hostIsAllowed,
  hostRefusal,
  hostGuardMiddleware,
  readTokenMiddleware,
  healthProbePath,
  CONTROL_TOKEN_ALPHABET,
  CONTROL_TOKEN_LENGTH,
  CONTROL_TOKEN_RE,
  mintControlToken,
  slugsInRoot,
  controlPrefixMiddleware,
  basePathFor,
  controlTokenPath,
  ensureControlToken,
  controlAuthFailure,
  requireControlToken,
  controlDeliveryDecision,
  controlBootstrapScript,
  readPublicHtml,
  // landing page / monitoring
  LIFECYCLE_ORDER,
  LIFECYCLE_LABELS,
  classifyLifecycle,
  readProtocolHeadCheap,
  checkerLiveness,
  sidecarHealth,
  parseMeminfo,
  parseSwaps,
  diskUsage,
  providerAuthPresence,
  hostSnapshot,
  quotaSummaryForProject,
  projectCard,
  projectsIndex,
  // run creation (create jobs)
  CREATE_SLUG_RE,
  CREATE_RUNNING_STATES,
  CREATE_FAILED_STATES,
  CREATE_STALE_SECS,
  CREATE_UPLOAD_LIMIT,
  CREATE_UPLOAD_LIMIT_BYTES,
  intakeUpload,
  decodeCp1252,
  classifyCreateJob,
  interpretTargetsResolution,
  createRetryEligible,
  createDeleteEligible,
  createSlugIssue,
  createJobCard,
  createJobsIndex,
  createStatusPayload,
  createJobSettings,
  createLogTail,
  listCreateTemplates,
  launcherRolesFromConfigTemplate,
  launcherRoleSettingsFromConfigTemplate,
  launcherRoleOptions,
  CREATE_PINNED_MODELS,
  CREATE_DEFAULT_MODEL,
  CREATE_DEFAULT_EFFORT,
  createJobsRoot,
  createUploadsRoot,
  registerCreateUploadKind,
  registerLandingHtmlTransform,
  applyLandingHtmlTransforms,
  interpretLoogleProbe,
  interpretCreateOptions,
  createOptionsArgv,
  CREATE_OPTIONS_MODULE,
  diffTargetsResolutions,
  parseReferenceSpec,
  referenceSpecsFromBody,
  validatedOverrides,
  createFailureHints,
  createDiskPreflight,
  lanesFromConfigTemplate,
  createAuthPreflight,
  ATTENTION_STATES,
  GATE_ROWS,
  HALT_ROWS,
  attachSoundVerifierFailCounts,
  augmentViewerStateAttention,
  buildArtifactChatData,
  // chat dropdown: burst -> call rows, and the pure naming helpers the
  // dedup between chat-dir names and tmux session names depends on
  discoverProjects,
  buildChatCalls,
  canonicalLaneKey,
  inferCallKind,
  foldEventLogStageWalltime,
  // Exposed so tests can force a cold fold and inspect what got memoized;
  // nothing else should touch it.
  _eventLogFoldPartials,
  gateAttention,
  gruntsStateForRuntimeRoot,
  haltAttention,
  haltMarkersAreViewerLiftable,
  haltStateForRuntimeRoot,
  liftHaltForResume,
  pauseAttention,
  supervisorContextFromStatus,
  recentSystemFeedbackForRuntimeRoot,
  loadTabletBlobDiskCache,
  saveTabletBlobDiskCache,
  parseCatFileBatch,
  SHARED_STATE_FORMAT,
  SHARED_STATE_ENCODED_KEYS,
  SharedStateError,
  isSharedState,
  decodeSharedState,
  progressForCommit,
  tabletBlobProjections,
  tabletBlobShasForCommit,
  leanBlobProjection,
  texBlobProjection,
  TABLET_BLOB_CACHES: { lean: LEAN_BLOB_CACHE, tex: TEX_BLOB_CACHE },
  parseChangelogRelease,
  versionIsNewer,
  computeUpdateAvailable,
  localTrellisInstall,
  parseCodexOutputEntries,
  parseJsonlTranscriptEntries,
  parseJsonTranscriptEntries,
  readHistoricalViewerState,
  readHistoricalChats,
  readLiveViewerState,
  readLiveChats,
  startServer,
  writeStatic,
};
