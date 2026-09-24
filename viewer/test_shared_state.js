// Tests for the viewer's `trellis-shared-state/1` decoder.
//
// The contract is not this file: it is `SHARED_STATE_SPEC` +
// `decode_shared_state` in trellis/history_artifacts.py, and the golden vector
// set in tests/fixtures/shared_state_vectors/ that the Python, Rust and
// JavaScript readers all replay. Those vectors are driven verbatim below —
// every roundtrip vector must decode to its pinned `decoded` value, every
// failure vector must throw a message containing its pinned `error_contains`
// — so a divergence between languages fails here rather than in production.
//
// Two properties beyond the vectors matter to the viewer specifically:
//   * both forms in real history. Old-format checkpoints must come back by
//     identity; new-format checkpoints must round-trip through the reference
//     encoder after JavaScript decodes them. Checked against real committed
//     blobs when a run repo is present.
//   * deep copy on `$r` expansion. Downstream projections mutate what they are
//     handed (progressForCommit spreads and deletes buckets), so two positions
//     sharing a pool key must never share a mutable object.

const assert = require('assert');
const crypto = require('crypto');
const fs = require('fs');
const path = require('path');
const { execFileSync } = require('child_process');

const {
  SHARED_STATE_FORMAT,
  SHARED_STATE_ENCODED_KEYS,
  SharedStateError,
  isSharedState,
  decodeSharedState,
} = require('./server');

const VECTOR_DIR = path.join(__dirname, '..', 'tests', 'fixtures', 'shared_state_vectors');

// --- golden vectors ---------------------------------------------------------

const vectorFiles = fs.readdirSync(VECTOR_DIR).filter((f) => f.endsWith('.json')).sort();
assert.ok(vectorFiles.length >= 18, `expected the committed vector set, found ${vectorFiles.length}`);

let roundtripSeen = 0;
let failureSeen = 0;
let digestSeen = 0;

for (const file of vectorFiles) {
  const raw = fs.readFileSync(path.join(VECTOR_DIR, file), 'utf8');
  const vector = JSON.parse(raw);
  const label = `${file} (${vector.name})`;

  if (vector.kind === 'roundtrip') {
    roundtripSeen++;
    // The pinned encoding decodes to the pinned plain value...
    const decoded = decodeSharedState(vector.encoded);
    assert.deepStrictEqual(decoded, vector.decoded, `${label}: decode(encoded) != decoded`);
    // ...including member order, which the format pins (passthrough members in
    // input order, encoded subtrees in UTF-8 byte order of their keys). Both
    // sides went through JSON.parse, so a stringify comparison is an order
    // check and not a number-formatting one.
    assert.strictEqual(
      JSON.stringify(decoded), JSON.stringify(vector.decoded),
      `${label}: decode(encoded) member order differs from decoded`);
    // The old format is passed through untouched, by identity.
    assert.strictEqual(isSharedState(vector.plain), false, `${label}: plain must not look encoded`);
    assert.strictEqual(decodeSharedState(vector.plain), vector.plain,
      `${label}: decode(plain) must return the very same object`);
    // `decoded` is `plain` re-ordered, never re-valued.
    assert.deepStrictEqual(vector.decoded, vector.plain, `${label}: decoded != plain (as values)`);
    assert.strictEqual(isSharedState(vector.encoded), true, `${label}: encoded must be detected`);
    assert.strictEqual(vector.encoded.$format, SHARED_STATE_FORMAT, `${label}: pinned $format`);
    console.log(`  vector ${label}: roundtrip OK`);
    continue;
  }

  if (vector.kind === 'failure') {
    failureSeen++;
    let thrown = null;
    try {
      decodeSharedState(vector.document);
    } catch (e) {
      thrown = e;
    }
    assert.ok(thrown, `${label}: expected a throw, got a value`);
    assert.ok(thrown instanceof SharedStateError,
      `${label}: expected SharedStateError, got ${thrown && thrown.name}: ${thrown && thrown.message}`);
    assert.ok(String(thrown.message).includes(vector.error_contains),
      `${label}: message ${JSON.stringify(thrown.message)} lacks ${JSON.stringify(vector.error_contains)}`);
    console.log(`  vector ${label}: throws "${vector.error_contains}" OK`);
    continue;
  }

  if (vector.kind === 'digests') {
    digestSeen++;
    // A decoder never computes digests -- table keys are read, not derived --
    // so this is not exercising viewer code. It is the cross-language check
    // that JavaScript CAN reproduce Python's Merkle hash, which is what makes
    // the table keys in the other vectors meaningful here. The number typing
    // JSON.parse destroys (int vs float, and integers past 2^53) is recovered
    // by re-reading the fixture text with a type-preserving parser.
    const entries = parseDigestVector(raw).digests;
    for (const entry of entries) {
      const got = merkleDigestHex(entry.value);
      assert.strictEqual(got, entry.digest, `${label}: H(${entry.label}) mismatch`);
    }
    console.log(`  vector ${label}: ${entries.length} digests OK`);
    continue;
  }

  assert.fail(`${label}: unknown vector kind ${vector.kind}`);
}

assert.strictEqual(roundtripSeen, 7, 'expected 7 roundtrip vectors');
assert.strictEqual(failureSeen, 10, 'expected 10 failure vectors');
assert.strictEqual(digestSeen, 1, 'expected 1 digest vector');

// --- identity on the old format --------------------------------------------

// Everything committed to date is plain, and every plain shape must come back
// as the identical object -- no copy, no reordering, no reconstruction.
for (const plain of [
  {},
  { event_count: 0, state: {} },
  { state: { a: [1, 2, { b: null }] }, checkpoint: { c: 'x' } },
  { metadata: { '$notformat': 1 } },      // a `$` name that is not top-level
  { state: { '$r': 'looks-like-a-ref' } }, // an unescaped sigil in a plain doc
  { format: SHARED_STATE_FORMAT },         // the value, but not under `$format`
]) {
  assert.strictEqual(decodeSharedState(plain), plain);
}
// Non-object documents are not this format and are returned as-is.
for (const other of [null, 3, 'text', true, [1, 2, 3]]) {
  assert.strictEqual(decodeSharedState(other), other);
}
assert.strictEqual(isSharedState(null), false);
assert.strictEqual(isSharedState([1]), false);
assert.deepStrictEqual(SHARED_STATE_ENCODED_KEYS, ['checkpoint', 'state']);

// --- deep copy on expansion -------------------------------------------------

{
  const doc = {
    $format: SHARED_STATE_FORMAT,
    $strings: { '1111111111111111': 'shared-text' },
    $pool: {
      '2222222222222222': { inner: { deep: [1, 2] } },
      '3333333333333333': { child: { $r: '2222222222222222' }, tag: { $s: '1111111111111111' } },
    },
    checkpoint: { a: { $r: '3333333333333333' } },
    state: { b: { $r: '3333333333333333' }, c: { $r: '2222222222222222' } },
  };
  const out = decodeSharedState(doc);
  const a = out.checkpoint.a;
  const b = out.state.b;
  const c = out.state.c;
  assert.deepStrictEqual(a, b);
  assert.deepStrictEqual(c, { inner: { deep: [1, 2] } });
  assert.notStrictEqual(a, b, 'two $r expansions must not share the container');
  assert.notStrictEqual(a.child, b.child, 'nested expansions must not be shared either');
  assert.notStrictEqual(a.child.inner, b.child.inner, 'deep members must not be shared');
  assert.notStrictEqual(a.child.inner.deep, b.child.inner.deep, 'arrays must not be shared');
  assert.notStrictEqual(a.child, c, 'a nested and a top-level expansion of one key must differ');
  // The decisive property: mutating one alias leaves the others intact.
  a.child.inner.deep.push(3);
  a.tag = 'rewritten';
  assert.deepStrictEqual(b.child.inner.deep, [1, 2]);
  assert.strictEqual(b.tag, 'shared-text');
  assert.deepStrictEqual(c.inner.deep, [1, 2]);
  // ...and the pool itself is never handed out, so decoding twice is stable.
  const again = decodeSharedState(doc);
  assert.deepStrictEqual(again.state.c, { inner: { deep: [1, 2] } });
  assert.notStrictEqual(again.state.c, c);
}

// Passthrough members are deep-copied too, so a caller mutating the decoded
// document cannot reach back into the parse it came from.
{
  const doc = {
    $format: SHARED_STATE_FORMAT,
    event_count: 7,
    metadata: { run: 'alpha', nested: { list: [1] } },
    $strings: {},
    $pool: {},
  };
  const out = decodeSharedState(doc);
  assert.deepStrictEqual(out, { event_count: 7, metadata: { run: 'alpha', nested: { list: [1] } } });
  assert.notStrictEqual(out.metadata, doc.metadata);
  out.metadata.nested.list.push(2);
  assert.deepStrictEqual(doc.metadata.nested.list, [1]);
}

// --- structural edges -------------------------------------------------------

{
  // `$e` is unwrapped exactly one level: the inner object is a literal and its
  // own sigil shape is not re-read, but its VALUES are still decoded.
  const doc = {
    $format: SHARED_STATE_FORMAT,
    $strings: { '4444444444444444': 'interned' },
    $pool: {},
    state: {
      literal: { $e: { $r: 'not-a-reference' } },
      nested: { $e: { $e: { $e: { $r: 'x' } } } },
      valued: { $e: { $s: { $s: '4444444444444444' } } },
      multi: { $r: 'x', extra: 1 },
    },
  };
  assert.deepStrictEqual(decodeSharedState(doc).state, {
    literal: { $r: 'not-a-reference' },
    // One wrapper is stripped; the remaining ones are decoded as ordinary
    // members, which strips a second. This is the exact inverse of the
    // encoder's "escaping nests" rule (vector 06).
    nested: { $e: { $r: 'x' } },
    valued: { $s: 'interned' },
    multi: { $r: 'x', extra: 1 },
  });
}

{
  // Table lookups must be own-property lookups: an inherited name is not an
  // entry, and must dangle rather than resolve to something off Object.prototype.
  for (const key of ['constructor', 'toString', '__proto__', 'hasOwnProperty']) {
    assert.throws(
      () => decodeSharedState({
        $format: SHARED_STATE_FORMAT, $strings: {}, $pool: {}, state: { x: { $r: key } },
      }),
      (e) => e instanceof SharedStateError && /absent from \$pool/.test(e.message),
      `$r ${key} must dangle`);
    assert.throws(
      () => decodeSharedState({
        $format: SHARED_STATE_FORMAT, $strings: {}, $pool: {}, state: { x: { $s: key } },
      }),
      (e) => e instanceof SharedStateError && /absent from \$strings/.test(e.message),
      `$s ${key} must dangle`);
  }
  // And a `__proto__` member of the payload survives as data, not as a
  // prototype assignment.
  // Written as text: an object LITERAL's `__proto__:` sets the prototype,
  // where JSON.parse defines a real own property — which is what a decoder
  // reading a file has to preserve.
  const out = decodeSharedState(JSON.parse(JSON.stringify({
    $format: SHARED_STATE_FORMAT,
    $strings: {},
    $pool: { '5555555555555555': JSON.parse('{"__proto__":{"polluted":true}}') },
    state: { x: { $r: '5555555555555555' } },
  })));
  assert.strictEqual(Object.prototype.hasOwnProperty.call(out.state.x, '__proto__'), true);
  assert.strictEqual(Object.getPrototypeOf(out.state.x), Object.prototype);
  assert.strictEqual({}.polluted, undefined);
}

{
  // A table whose entry is the wrong type is an error, not a coercion.
  assert.throws(
    () => decodeSharedState({
      $format: SHARED_STATE_FORMAT, $strings: { aaaa: 5 }, $pool: {}, state: { x: { $s: 'aaaa' } },
    }),
    (e) => e instanceof SharedStateError && /is not a string/.test(e.message));
  // `$strings` / `$pool` present but not objects.
  for (const bad of [[], 'x', 3, null]) {
    assert.throws(
      () => decodeSharedState({ $format: SHARED_STATE_FORMAT, $strings: bad, $pool: {}, state: {} }),
      (e) => e instanceof SharedStateError && /no \$strings object/.test(e.message));
    assert.throws(
      () => decodeSharedState({ $format: SHARED_STATE_FORMAT, $strings: {}, $pool: bad, state: {} }),
      (e) => e instanceof SharedStateError && /no \$pool object/.test(e.message));
  }
  // A cycle reached only through a deeply nested position still terminates.
  assert.throws(
    () => decodeSharedState({
      $format: SHARED_STATE_FORMAT,
      $strings: {},
      $pool: { aaaa: { deep: [{ nested: { $r: 'aaaa' } }] } },
      state: { x: { $r: 'aaaa' } },
    }),
    (e) => e instanceof SharedStateError && /reference cycle/.test(e.message));
  // ...while the same key used twice in SEQUENCE (not nested) is legal.
  const diamond = decodeSharedState({
    $format: SHARED_STATE_FORMAT,
    $strings: {},
    $pool: { aaaa: { v: 1 } },
    state: { p: { $r: 'aaaa' }, q: { $r: 'aaaa' } },
  });
  assert.deepStrictEqual(diamond.state, { p: { v: 1 }, q: { v: 1 } });
}

// --- real historical blobs --------------------------------------------------

// Run against real old- and new-format checkpoints when a run repo is
// reachable; skip (loudly) when it is not, so the suite stays portable.
function historicalRepoCandidates() {
  const out = [];
  if (process.env.TRELLIS_STATE_REPO) out.push(process.env.TRELLIS_STATE_REPO);
  const home = process.env.HOME || '';
  if (home) {
    out.push(path.join(home, 'math', 'current'));
    out.push(path.join(home, 'math', 'alpha'));
  }
  return out.filter((p) => {
    try { return fs.existsSync(path.join(p, '.git')) || fs.existsSync(p); } catch { return false; }
  });
}

function checkpointShas(repo) {
  try {
    const out = execFileSync('git', ['-C', repo, 'log', '--reverse', '--format=%H',
      '--grep=supervisor2 checkpoint'],
    { encoding: 'utf8', stdio: ['ignore', 'pipe', 'ignore'], maxBuffer: 64 * 1024 * 1024 });
    return out.split('\n').filter(Boolean);
  } catch {
    return [];
  }
}

function assertReferenceRoundtrip(document, decoded, sha) {
  const script = [
    'import json, sys',
    'from trellis.history_artifacts import encode_shared_state',
    'with open(sys.argv[1], encoding="utf-8") as fh:',
    '    encoded = json.load(fh)',
    'with open(sys.argv[2], encoding="utf-8") as fh:',
    '    plain = json.load(fh)',
    'if encode_shared_state(plain) != encoded:',
    '    raise SystemExit("reference codec did not reproduce the historical envelope")',
    'print("OK")',
  ].join('\n');
  const root = fs.mkdtempSync(path.join(require('os').tmpdir(), 'trellis-sharedstate-history-'));
  try {
    const encodedPath = path.join(root, 'encoded.json');
    const decodedPath = path.join(root, 'decoded.json');
    // Re-serialize both through JavaScript so the reference check sees exactly
    // the number representation the viewer sees, including 1 vs 1.0.
    fs.writeFileSync(encodedPath, JSON.stringify(document));
    fs.writeFileSync(decodedPath, JSON.stringify(decoded));
    const result = execFileSync('python3', ['-c', script, encodedPath, decodedPath], {
      cwd: path.join(__dirname, '..'), encoding: 'utf8',
      stdio: ['ignore', 'pipe', 'pipe'], maxBuffer: 1024 * 1024,
    }).trim();
    assert.strictEqual(result, 'OK', `${sha}: reference roundtrip did not complete`);
  } finally {
    fs.rmSync(root, { recursive: true, force: true });
  }
}

let historicalChecked = 0;
for (const repo of historicalRepoCandidates()) {
  const shas = checkpointShas(repo);
  if (shas.length < 3) continue;
  // Oldest, middle and newest: this spans the old plain era and, in an active
  // run, the newer shared-state era, at both size extremes.
  const picks = [shas[0], shas[Math.floor(shas.length / 2)], shas[shas.length - 1]];
  for (const sha of picks) {
    let raw;
    try {
      raw = execFileSync('git', ['-C', repo, 'show', `${sha}:.trellis-history/supervisor_state.json`],
        { encoding: 'utf8', stdio: ['ignore', 'pipe', 'ignore'], maxBuffer: 1024 * 1024 * 1024 });
    } catch {
      continue;
    }
    const parsed = JSON.parse(raw);
    if (isSharedState(parsed)) {
      const decoded = decodeSharedState(parsed);
      assert.strictEqual(isSharedState(decoded), false,
        `${sha}: decoded shared-state must be a plain document`);
      assertReferenceRoundtrip(parsed, decoded, sha);
      console.log(`  historical ${sha.slice(0, 12)} (${(raw.length / 1e6).toFixed(1)} MB): roundtrip OK`);
    } else {
      assert.strictEqual(decodeSharedState(parsed), parsed,
        `${sha}: decode(plain) must return the input unchanged, by identity`);
      console.log(`  historical ${sha.slice(0, 12)} (${(raw.length / 1e6).toFixed(1)} MB): identity OK`);
    }
    historicalChecked++;
  }
  if (historicalChecked) break;
}
if (!historicalChecked) {
  console.log('  historical blobs: SKIPPED (no run repo with checkpoint commits found;'
    + ' set TRELLIS_STATE_REPO to enable)');
}

// --- the real encoder's output, end to end ----------------------------------
// The vectors above are hand-sized. This is the writer-flip gate: a real
// supervisor state encoded by trellis/history_artifacts.py must decode here to
// the byte-for-byte original document. Point TRELLIS_SHARED_STATE_PLAIN at the
// plain snapshot and TRELLIS_SHARED_STATE_ENCODED at its encoding (the same
// pair the kernel's `python_encoded_state_decodes_back_to_the_plain_document`
// consumes); skipped otherwise. Compared with a canonicalizing stringify,
// since the encoder emits object members inside `checkpoint`/`state` in UTF-8
// byte order rather than the input document's order.
{
  const plainPath = process.env.TRELLIS_SHARED_STATE_PLAIN;
  const encodedPath = process.env.TRELLIS_SHARED_STATE_ENCODED;
  if (plainPath && encodedPath) {
    const canonical = (value) => {
      if (Array.isArray(value)) return `[${value.map(canonical).join(',')}]`;
      if (value && typeof value === 'object') {
        const keys = Object.keys(value).sort(
          (a, b) => Buffer.compare(Buffer.from(a, 'utf8'), Buffer.from(b, 'utf8')));
        return `{${keys.map((k) => `${JSON.stringify(k)}:${canonical(value[k])}`).join(',')}}`;
      }
      return JSON.stringify(value);
    };
    const plain = JSON.parse(fs.readFileSync(plainPath, 'utf8'));
    const encoded = JSON.parse(fs.readFileSync(encodedPath, 'utf8'));
    assert.strictEqual(isSharedState(plain), false, 'the plain fixture carries $format');
    assert.strictEqual(isSharedState(encoded), true, 'the encoded fixture has no $format');
    assert.strictEqual(canonical(decodeSharedState(encoded)), canonical(plain),
      'decode(python_encode(plain)) != plain');
    console.log('  real encoded state: decode == plain OK'
      + ` (${(fs.statSync(plainPath).size / 1e6).toFixed(1)} MB ->`
      + ` ${(fs.statSync(encodedPath).size / 1e6).toFixed(1)} MB)`);
  } else {
    console.log('  real encoded state: SKIPPED (set TRELLIS_SHARED_STATE_PLAIN/_ENCODED)');
  }
}

// --- wiring: the one reader that feeds the charts ---------------------------

// `readSupervisorParsedForCommit` decodes immediately after JSON.parse, so
// `progressForCommit` (and the sound-cpinfo projection it warms) see the plain
// shape whether the commit holds the old or the new format. Exercised through
// the public entry point against a scratch git repo — never a live one.
{
  const os = require('os');
  const { progressForCommit } = require('./server');
  let root = null;
  try {
    root = fs.mkdtempSync(path.join(os.tmpdir(), 'trellis-viewer-sharedstate-'));
    execFileSync('git', ['-C', root, 'init', '-q'], { stdio: 'ignore' });
    execFileSync('git', ['-C', root, 'config', 'user.email', 'test@example.com'], { stdio: 'ignore' });
    execFileSync('git', ['-C', root, 'config', 'user.name', 'test'], { stdio: 'ignore' });
  } catch {
    root = null;
    console.log('  wiring: SKIPPED (git unavailable)');
  }
  if (root) {
    process.on('exit', () => { try { fs.rmSync(root, { recursive: true, force: true }); } catch {} });
    const commit = (document, message) => {
      fs.mkdirSync(path.join(root, '.trellis-history'), { recursive: true });
      fs.writeFileSync(path.join(root, '.trellis-history', 'supervisor_state.json'), JSON.stringify(document));
      execFileSync('git', ['-C', root, 'add', '-A'], { stdio: 'ignore' });
      execFileSync('git', ['-C', root, 'commit', '-q', '-m', message], { stdio: 'ignore' });
      return execFileSync('git', ['-C', root, 'rev-parse', 'HEAD'], { encoding: 'utf8' }).trim();
    };
    const KIND = '1111111111111111';
    const KINDS = '2222222222222222';
    // A checkpoint in the NEW format whose node_kinds arrive only through a
    // pool reference and interned strings.
    const encodedSha = commit({
      $format: SHARED_STATE_FORMAT,
      event_count: 11,
      $strings: { [KIND]: 'theorem' },
      $pool: { [KINDS]: { Alpha: { $s: KIND }, Beta: { $s: KIND } } },
      state: { cycle: 7, phase: 'ProofFormalization', node_kinds: { $r: KINDS } },
    }, 'supervisor2 checkpoint c7');
    const data = progressForCommit(root, encodedSha);
    assert.ok(data, 'an encoded checkpoint must project like a plain one');
    assert.strictEqual(data.cycle, 7);
    assert.strictEqual(data.phase, 'ProofFormalization');
    assert.strictEqual(data.all.total, 2, 'node_kinds behind a $r must reach the projection');

    // The same checkpoint in the OLD format projects identically.
    const plainSha = commit({
      event_count: 11,
      state: { cycle: 7, phase: 'ProofFormalization', node_kinds: { Alpha: 'theorem', Beta: 'theorem' } },
    }, 'supervisor2 checkpoint c8');
    const plainData = progressForCommit(root, plainSha);
    assert.deepStrictEqual({ ...plainData, sha: null }, { ...data, sha: null },
      'old and new format must project to the same metrics');

    // A corrupt blob must reach the caller as a throw. Returning null here is
    // what would silently drop the checkpoint and render as an empty chart.
    const brokenSha = commit({
      $format: SHARED_STATE_FORMAT,
      event_count: 12,
      $strings: {},
      $pool: {},
      state: { node_kinds: { $r: 'deadbeefdeadbeef' } },
    }, 'supervisor2 checkpoint c9');
    assert.throws(
      () => progressForCommit(root, brokenSha),
      (e) => e instanceof SharedStateError && /absent from \$pool/.test(e.message),
      'a corrupt checkpoint must surface, not become an empty projection');
    console.log('  wiring: encoded + plain project identically, corrupt throws OK');
  }
}

// --- Merkle digest, for the digest vector only ------------------------------
// Not part of the viewer: a decoder reads table keys and never derives them.

// Declared as a function, not a class, so it is hoisted above the vector loop
// that runs it.
function JsonNumber(isInt, raw, value) {
  this.isInt = isInt;
  this.raw = raw;
  this.value = value;
}

// JSON.parse collapses `1` and `1.0` and rounds integers past 2^53, both of
// which change the digest. Re-read the text keeping the distinction, and
// return the digest vector's records with their numbers still typed.
function parseDigestVector(text) {
  let i = 0;
  const fail = (msg) => { throw new Error(`bad JSON at ${i}: ${msg}`); };
  const ws = () => { while (i < text.length && ' \t\n\r'.includes(text[i])) i++; };
  function parseString() {
    if (text[i] !== '"') fail('expected string');
    i++;
    let out = '';
    while (i < text.length) {
      const ch = text[i];
      if (ch === '"') { i++; return out; }
      if (ch === '\\') {
        const esc = text[++i];
        i++;
        if (esc === 'u') { out += String.fromCharCode(parseInt(text.slice(i, i + 4), 16)); i += 4; }
        else if (esc === 'n') out += '\n';
        else if (esc === 't') out += '\t';
        else if (esc === 'r') out += '\r';
        else if (esc === 'b') out += '\b';
        else if (esc === 'f') out += '\f';
        else out += esc; // " \ /
        continue;
      }
      out += ch;
      i++;
    }
    return fail('unterminated string');
  }
  function parseValue() {
    ws();
    const ch = text[i];
    if (ch === '{') {
      i++;
      const obj = { __keys: [], __values: new Map() };
      ws();
      if (text[i] === '}') { i++; return obj; }
      for (;;) {
        ws();
        const k = parseString();
        ws();
        if (text[i] !== ':') fail('expected :');
        i++;
        const v = parseValue();
        obj.__keys.push(k);
        obj.__values.set(k, v);
        ws();
        if (text[i] === ',') { i++; continue; }
        if (text[i] === '}') { i++; return obj; }
        return fail('expected , or }');
      }
    }
    if (ch === '[') {
      i++;
      const arr = [];
      ws();
      if (text[i] === ']') { i++; return arr; }
      for (;;) {
        arr.push(parseValue());
        ws();
        if (text[i] === ',') { i++; continue; }
        if (text[i] === ']') { i++; return arr; }
        return fail('expected , or ]');
      }
    }
    if (ch === '"') return parseString();
    if (text.startsWith('true', i)) { i += 4; return true; }
    if (text.startsWith('false', i)) { i += 5; return false; }
    if (text.startsWith('null', i)) { i += 4; return null; }
    const m = /^-?(?:0|[1-9]\d*)(?:\.\d+)?(?:[eE][+-]?\d+)?/.exec(text.slice(i));
    if (!m) return fail(`unexpected ${JSON.stringify(text.slice(i, i + 12))}`);
    i += m[0].length;
    const isInt = !/[.eE]/.test(m[0]);
    return new JsonNumber(isInt, m[0], isInt ? null : Number(m[0]));
  }
  const root = parseValue();
  ws();
  // Convenience view for the digest vector: `.digests` is an array of records.
  return {
    digests: root.__values.get('digests').map((rec) => ({
      label: rec.__values.get('label'),
      value: rec.__values.get('value'),
      digest: rec.__values.get('digest'),
    })),
  };
}

// `shortest_roundtrip(f)` as Python's repr renders it. NOT `String(f)`: that
// gives "1e-9" where Python gives "1e-09", and "0" for -0.0.
function pythonFloatRepr(x) {
  if (!Number.isFinite(x)) throw new Error(`non-finite float: ${x}`);
  const negative = x < 0 || Object.is(x, -0);
  const magnitude = Math.abs(x);
  let body;
  if (magnitude === 0) {
    body = '0.0';
  } else {
    const m = /^(\d)(?:\.(\d+))?e([+-]\d+)$/.exec(magnitude.toExponential());
    const digits = m[1] + (m[2] || '');
    const exp = parseInt(m[3], 10);
    if (exp < -4 || exp >= 16) {
      const mantissa = digits.length > 1 ? `${digits[0]}.${digits.slice(1)}` : digits;
      const sign = exp < 0 ? '-' : '+';
      body = `${mantissa}e${sign}${String(Math.abs(exp)).padStart(2, '0')}`;
    } else if (exp >= 0) {
      body = digits.length > exp + 1
        ? `${digits.slice(0, exp + 1)}.${digits.slice(exp + 1)}`
        : `${digits}${'0'.repeat(exp + 1 - digits.length)}.0`;
    } else {
      body = `0.${'0'.repeat(-exp - 1)}${digits}`;
    }
  }
  return negative ? `-${body}` : body;
}

function sha256(...parts) {
  const h = crypto.createHash('sha256');
  for (const p of parts) h.update(p);
  return h.digest();
}

function merkleDigest(value) {
  if (value === null) return sha256(Buffer.from('n'));
  if (value === true) return sha256(Buffer.from('t'));
  if (value === false) return sha256(Buffer.from('f'));
  if (value instanceof JsonNumber) {
    return value.isInt
      ? sha256(Buffer.from('i'), Buffer.from(BigInt(value.raw).toString(), 'ascii'))
      : sha256(Buffer.from('d'), Buffer.from(pythonFloatRepr(value.value), 'ascii'));
  }
  if (typeof value === 'string') return sha256(Buffer.from('s'), Buffer.from(value, 'utf8'));
  if (Array.isArray(value)) {
    const parts = [Buffer.from('a')];
    for (const item of value) parts.push(Buffer.from(merkleDigest(item).toString('hex'), 'ascii'));
    return sha256(...parts);
  }
  // Object: members in ascending UTF-8 BYTE order of their keys (not the
  // UTF-16 code-unit order a bare Array#sort would use).
  const keys = value.__keys.slice().sort((a, b) => Buffer.compare(Buffer.from(a, 'utf8'), Buffer.from(b, 'utf8')));
  const parts = [Buffer.from('o')];
  for (const k of keys) {
    parts.push(Buffer.from(merkleDigest(k).toString('hex'), 'ascii'));
    parts.push(Buffer.from(merkleDigest(value.__values.get(k)).toString('hex'), 'ascii'));
  }
  return sha256(...parts);
}

function merkleDigestHex(value) {
  return merkleDigest(value).toString('hex');
}

console.log('test_shared_state: OK');
