const assert = require('assert');

const {
  parseCatFileBatch,
  tabletBlobProjections,
  leanBlobProjection,
  texBlobProjection,
  TABLET_BLOB_CACHES,
} = require('./server');

const REPO = '/repo';
const key = (sha) => `${REPO}\0${sha}`;

function resetCaches() {
  TABLET_BLOB_CACHES.lean.clear();
  TABLET_BLOB_CACHES.tex.clear();
}

// --- cat-file --batch framing ----------------------------------------------
// Records are `<sha> <type> <size>\n<content>\n`, sliced by declared byte
// length: proof text carries newlines, and a line inside it can look exactly
// like a record header, so a scanning parser would desync.
{
  const multiline = 'theorem t : True := by\n  sorry\n\ndeadbeef blob 7\n';
  const tex = '\\begin{proof}\nSKETCH:\ntext\n\\end{proof}';
  const buf = Buffer.concat([
    Buffer.from(`aaa1 blob ${Buffer.byteLength(multiline)}\n`), Buffer.from(multiline), Buffer.from('\n'),
    Buffer.from('bbb2 missing\n'),
    Buffer.from(`ccc3 blob ${Buffer.byteLength(tex)}\n`), Buffer.from(tex), Buffer.from('\n'),
  ]);
  const parsed = parseCatFileBatch(buf);
  assert.strictEqual(parsed.size, 2);
  assert.strictEqual(parsed.get('aaa1'), multiline);
  assert.strictEqual(parsed.get('ccc3'), tex);
  // A missing object mid-stream must not consume the following record.
  assert.strictEqual(parsed.has('bbb2'), false);
}

// Multi-byte content: the size is in bytes, the slice must land on it.
{
  const content = 'λ — ∀ x, x = x\nsecond line\n';
  const buf = Buffer.concat([
    Buffer.from(`aaa1 blob ${Buffer.byteLength(content)}\n`), Buffer.from(content), Buffer.from('\n'),
    Buffer.from('bbb2 blob 3\nxyz\n'),
  ]);
  const parsed = parseCatFileBatch(buf);
  assert.strictEqual(parsed.get('aaa1'), content);
  assert.strictEqual(parsed.get('bbb2'), 'xyz');
}

// A truncated / malformed record stops the walk instead of mis-attributing
// bytes to a sha.
{
  const parsed = parseCatFileBatch(Buffer.from('aaa1 blob 4\nab'));
  assert.strictEqual(parsed.size, 0);
  const bad = parseCatFileBatch(Buffer.from('aaa1 blob notanumber\nabcd\nbbb2 blob 3\nxyz\n'));
  assert.strictEqual(bad.size, 0);
}
assert.strictEqual(parseCatFileBatch(Buffer.alloc(0)).size, 0);

// --- blob projections -------------------------------------------------------
{
  const withMarker = 'import X\ntheorem t : True\n-- BODY\n:= by\n  trivial\n';
  assert.deepStrictEqual(leanBlobProjection(withMarker), { sorry: false, chars: ':= by\n  trivial'.length });
  assert.strictEqual(leanBlobProjection('-- BODY\n:= by sorry\n').sorry, true);
  // Commented-out `sorry` is not a sorry.
  assert.strictEqual(leanBlobProjection('-- BODY\n:= by trivial -- sorry\n').sorry, false);
  // The macro_rules rewrite makes the literal token compile to a real proof.
  assert.strictEqual(leanBlobProjection(
    'local macro_rules | `(tactic| sorry) => `(tactic| trivial)\n-- BODY\n:= by sorry\n').sorry, false);

  assert.deepStrictEqual(
    texBlobProjection('\\begin{proof}\nSKETCH:\nfill this in\n\\end{proof}'),
    { words: 4, sketch: true });
  assert.strictEqual(texBlobProjection('\\begin{proof}\nLet $x$ be given.\n\\end{proof}').sketch, false);
  assert.deepStrictEqual(texBlobProjection(''), { words: 0, sketch: false });
}

// --- per-checkpoint projection: cache hit/miss + absent files ---------------
const nodeKinds = { Preamble: 'preamble', Alpha: 'theorem', Beta: 'theorem', Gamma: 'definition', Ghost: 'theorem' };
const presentNodes = Object.keys(nodeKinds);
const blobShas = {
  // Ghost is in node_kinds with no Tablet file; Gamma is a definition, so its
  // .tex is never read; Beta shares Alpha's .lean content (same blob sha).
  lean: { Alpha: 'lean1', Beta: 'lean1', Gamma: 'lean2' },
  tex: { Alpha: 'tex1', Beta: 'tex2', Gamma: 'tex3' },
};
const BLOBS = new Map([
  ['lean1', '-- BODY\n:= by\n  sorry\n'],
  ['lean2', 'def f := 1\n'],
  ['tex1', '\\begin{proof}\nOne two three.\n\\end{proof}'],
  ['tex2', '\\begin{proof}\nSKETCH:\nlater\n\\end{proof}'],
  ['tex3', '\\begin{proof}\nunread definition proof\n\\end{proof}'],
]);
let fetchCalls = 0;
let fetchedShas = [];
function fakeFetch(shas) {
  fetchCalls++;
  fetchedShas = fetchedShas.concat(shas);
  const out = new Map();
  for (const sha of shas) if (BLOBS.has(sha)) out.set(sha, BLOBS.get(sha));
  return out;
}

resetCaches();
const first = tabletBlobProjections(REPO, presentNodes, nodeKinds, blobShas, fakeFetch);
assert.strictEqual(fetchCalls, 1);
// One request per DISTINCT sha: Alpha and Beta share lean1, Gamma's .tex is
// never wanted (definitions carry no proof metrics).
assert.deepStrictEqual(fetchedShas.slice().sort(), ['lean1', 'lean2', 'tex1', 'tex2']);
assert.deepStrictEqual(first.hasSorry, { Preamble: false, Alpha: true, Beta: true, Gamma: false, Ghost: false });
assert.deepStrictEqual(first.hasSketch, { Beta: true });
assert.deepStrictEqual(first.leanProofMetrics, { Alpha: { chars: ':= by\n  sorry'.length }, Beta: { chars: ':= by\n  sorry'.length } });
assert.deepStrictEqual(first.nlProofWordCounts, { Alpha: 3, Beta: 2 });
// A node with no Tablet file contributes no proof metrics at all.
assert.strictEqual('Ghost' in first.leanProofMetrics, false);
assert.strictEqual('Ghost' in first.nlProofWordCounts, false);
assert.strictEqual(TABLET_BLOB_CACHES.lean.size, 2);
assert.strictEqual(TABLET_BLOB_CACHES.tex.size, 2);

// Same checkpoint again: every sha is cached, so no batch runs at all.
fetchCalls = 0;
fetchedShas = [];
const second = tabletBlobProjections(REPO, presentNodes, nodeKinds, blobShas, fakeFetch);
assert.strictEqual(fetchCalls, 0);
assert.deepStrictEqual(second, first);

// Next checkpoint with one file changed: only the changed blob is fetched —
// the 1.24M-read / 4,703-blob collapse this layer exists for.
fetchCalls = 0;
fetchedShas = [];
BLOBS.set('lean3', '-- BODY\n:= by\n  trivial\n');
const churned = { lean: { ...blobShas.lean, Alpha: 'lean3' }, tex: blobShas.tex };
const third = tabletBlobProjections(REPO, presentNodes, nodeKinds, churned, fakeFetch);
assert.strictEqual(fetchCalls, 1);
assert.deepStrictEqual(fetchedShas, ['lean3']);
assert.strictEqual(third.hasSorry.Alpha, false);
assert.strictEqual(third.hasSorry.Beta, true);
assert.deepStrictEqual(third.leanProofMetrics.Alpha, { chars: ':= by\n  trivial'.length });

// A blob the batch cannot resolve (git reported `missing`) falls back to the
// no-Tablet-file behavior rather than throwing or caching a bogus projection.
resetCaches();
fetchCalls = 0;
const vanished = tabletBlobProjections(
  REPO, ['Alpha'], { Alpha: 'theorem' }, { lean: { Alpha: 'gone' }, tex: {} }, fakeFetch);
assert.deepStrictEqual(vanished.hasSorry, { Alpha: false });
assert.deepStrictEqual(vanished.leanProofMetrics, {});
assert.strictEqual(TABLET_BLOB_CACHES.lean.has(key('gone')), false);

// A proof-bearing node whose .tex is absent counts zero NL words, keeps its
// lean metrics, and is not sketch-marked.
resetCaches();
const noTex = tabletBlobProjections(
  REPO, ['Alpha'], { Alpha: 'theorem' }, { lean: { Alpha: 'lean2' }, tex: {} }, fakeFetch);
assert.deepStrictEqual(noTex.nlProofWordCounts, { Alpha: 0 });
assert.deepStrictEqual(noTex.hasSketch, {});
assert.ok('Alpha' in noTex.leanProofMetrics);

resetCaches();
console.log('test_tablet_blob: OK');
