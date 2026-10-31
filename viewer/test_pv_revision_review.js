'use strict';
// A small DOM contract test; the full browser smoke is run separately.
const assert = require('assert');
const fs = require('fs');
const vm = require('vm');
class Element {
  constructor(tag, text = '') { this.tagName = tag; this._text = text; this.children = []; this.attrs = {}; this.dataset = {}; this.style = {}; this.events = {}; this.hidden = false; this.scrollTop = 0; }
  get textContent() { return this._text + this.children.map(c => c.textContent).join(''); }
  set textContent(value) { this._text = String(value); this.children = []; }
  get value() { return this._value || ''; }
  set value(value) { this._value = this.tagName === 'textarea' ? String(value).replace(/\r\n?/g, '\n') : String(value); }
  set innerHTML(_) { throw new Error('Package data must never enter innerHTML'); }
  append(...items) { this.children.push(...items); }
  replaceChildren(...items) { this._text = ''; this.children = items; }
  insertBefore(item, before) { this.children.splice(this.children.indexOf(before), 0, item); }
  setAttribute(k, v) { this.attrs[k] = v; }
  addEventListener(k, fn) { (this.events[k] ||= []).push(fn); }
  async fire(k) { for (const fn of this.events[k] || []) await fn({ preventDefault() {} }); }
  querySelectorAll(selector) { return walk(this).filter(n => selector === 'button' ? n.tagName === 'button' : selector === '[data-goal]' ? n.dataset.goal : false); }
  focus() { this.focused = true; }
  scrollIntoView() { this.scrolled = true; }
}
function walk(root) { return root.children.flatMap(child => [child, ...walk(child)]); }
const digest = 'a'.repeat(64), changed = 'b'.repeat(64);
const pkg = { digest, candidate_digest: 'c'.repeat(64), schema: 'trellis-pv-revision-review/v2', evidence_digest: 'e'.repeat(64),
  original_collection: { context: 'Definition <script>source</script>', goals: [{ id: 'retained', text: '<img src=x onerror=bad()> source meaning' }, { id: 'old', text: 'before changed' }, { id: 'retired', text: 'old obligation' }] },
  proposed_collection: { context: 'Definition <script>source</script>', goals: [{ id: 'retained', text: '<img src=x onerror=bad()> source meaning' }, { id: 'old', text: 'after changed' }, { id: 'survivor', text: 'conditional obligation' }] },
  plan: { summary: 'Shared audit rationale', alternatives: [{ remedy: 'retire', rationale: 'Keep obligation', evidence: ['prior evidence'] }], tasks: [{ goal_ids: ['old'] }],
    dispositions: [{ goal_id: 'old', remedy: 'modify', reason: 'Different requirement', replacement_goals: [{ id: 'old', text: 'after changed' }] }, { goal_id: 'retired', remedy: 'retire', reason: 'Conditional remains', surviving_obligations: [{ id: 'survivor', text: 'conditional obligation' }] }],
    goal_notes: [{ goal_ids: ['retained'], characterization: 'source-dependent interpretation change', reason: 'Changed referent', author_role: 'audit' }] },
  aggregate_scope: { reason: 'Final scope prose', alternatives_assessment: 'Final alternatives', code_notes: [{ path: 'code.rs', goal_ids: ['retained'], reason: 'Exact code note', author_role: 'aggregate_scope', range: { side: 'after', start: 1, end: 2 } }] },
  files: [{ path: 'code.rs', operation: 'modify', group: 'production' }, { path: 'added.rs', operation: 'add', group: 'evidence' }, { path: 'deleted.rs', operation: 'delete', group: 'production' }],
  attempts: [{ id: 'attempt-1', task_id: 'fix', accepted: true, authorization: { title: 'Bounded repair', goal_ids: ['old'], authorization: 'Task scope', paths: ['code.rs', 'unchanged.rs'] }, response: { summary: 'Implementation summary', alternatives: [] }, scope: { reason: 'Task scope reason', alternatives_assessment: 'Task alternatives' } }],
  task_diffs: [{ attempt_id: 'attempt-1', task_id: 'fix', accepted: true, paths: ['code.rs'] }] };
const document = { head: new Element('head'), body: new Element('body'), createElement: tag => new Element(tag), createTextNode: text => new Element('#text', text) };
const storage = new Map(), calls = [];
const sandbox = { document, URLSearchParams, location: { pathname: '/base/project', search: '?control=secret', hash: '' },
  history: { replaceState() {} }, sessionStorage: { getItem: k => storage.get(k), setItem: (k, v) => storage.set(k, v) },
  window: { addEventListener() {} }, fetch: async (url, options) => {
    calls.push({ url, options });
    if (options) return { ok: true, json: async () => ({ scheduled: false }) };
    if (url.includes('/review?')) return { ok: true, json: async () => pkg };
    const q = new URL(url, 'http://localhost').searchParams, mode = q.get('mode'), path = q.get('path');
    return { ok: true, json: async () => ({ digest, path, mode, before_exists: path !== 'added.rs', after_exists: path !== 'deleted.rs',
      total: 301, rows: mode === 'diff' ? [{ kind: 'hunk', text: '@@ -1,1 +1,2 @@' }, { kind: 'delete', before: 1, text: 'let old = "<script>";\n' }, { kind: 'add', after: 1, text: 'let new = "<img>";\n' }, { kind: 'add', after: 2, text: 'x'.repeat(20000) }] : [{ kind: 'context', [mode]: 1, text: 'full text\n' }] }) };
  } };
vm.runInNewContext(fs.readFileSync(require.resolve('./public/pv_revision_review.js'), 'utf8'), sandbox);
const controller = sandbox.window.PVRevisionReview({ apiPath: x => '/base/project/api/' + x, controlPath: x => '/base/project/control/' + x, controlHeaders: x => ({ ...x, csrf: 'existing' }), controlAvailable: () => true, refresh: async () => {} });
const page = document.body.children[0];
const find = (tag, text) => walk(page).find(n => n.tagName === tag && n.textContent === text);
const click = async text => { const n = find('button', text); assert(n, `Missing button: ${text}`); await n.fire('click'); await new Promise(resolve => setImmediate(resolve)); };
async function testReview() {
  controller.update({ digest, state: 'awaiting_human' }); await controller.open();
  assert(page.textContent.includes('Harness files and audit evidence remain attached, not approved.'));
  assert(walk(page).some(n => n.tagName === 'a' && n.textContent === 'evidence patch' && n.href.includes('format=evidence-patch')));
  assert(page.textContent.includes('text unchanged'));
  assert(page.textContent.includes('Changed referent'));
  assert(page.textContent.includes('Replaces: old'));
  assert(page.textContent.includes('Survives retirement of: retired'));
  assert(page.textContent.includes('<img src=x onerror=bad()>'));
  assert(walk(page).some(n => n.tagName === 'mark' && n.textContent.includes('before')));
  assert(find('summary', 'Shared context and definitions · text unchanged'));
  const reason = walk(page).find(n => n.name === 'reason'), amendment = walk(page).find(n => n.name === 'amendment');
  reason.value = 'Unsent reason'; amendment.value = 'Unsent amendment'; await reason.fire('input'); await amendment.fire('input');
  await click('Code');
  assert(find('h4', 'Production changes (2)'));
  assert(find('h4', 'Evidence and harness (not ratified) (1)'));
  assert(page.textContent.includes('@@ -1,1 +1,2 @@'));
  assert(page.textContent.includes('Exact code note'));
  assert(page.textContent.includes('Authorized paths (may include unchanged files): code.rs, unchanged.rs'));
  assert(!find('button', '~ unchanged.rs'));
  assert(walk(page).some(n => n.tagName === 'span' && n.className === 'pvr-keyword'));
  await click('Side-by-side view'); assert(walk(page).some(n => (n.className || '').includes('pvr-split')));
  await click('Expand context (3 lines)'); assert(calls.at(-1).url.includes('context=13'));
  page.scrollTop = 817; await page.fire('scroll');
  const priorChildren = page.children;
  controller.update({ digest, state: 'awaiting_human', feedback: 'poll changed' });
  assert.strictEqual(page.children, priorChildren); assert.strictEqual(page.scrollTop, 817); assert.strictEqual(reason.value, 'Unsent reason');
  await click('+ added.rs');
  assert(page.textContent.includes('Attached evidence; excluded from production and ratification.'));
  await click('Full file · before'); assert(page.textContent.includes('No before file exists'));
  await click('− deleted.rs'); await click('Full file · after'); assert(page.textContent.includes('No after file exists'));
  controller.update({ digest: changed, state: 'awaiting_human' });
  assert(find('button', 'Ratify goals and code').disabled);
  await click('Ratify goals and code'); assert(!calls.some(c => c.options), 'Stale displayed package must not submit');
  controller.update({ digest, state: 'awaiting_human' }); await click('Ratify goals and code');
  const post = calls.find(c => c.options); assert.strictEqual(JSON.parse(post.options.body).digest, digest); assert.strictEqual(post.options.headers.csrf, 'existing');
  assert.strictEqual(calls.filter(c => c.url.includes('/review?')).length, 1, 'Polling and file navigation must not reload full package');
  console.log('PV review renderer, drafts, snapshot navigation and exact decision binding passed');
}


const source = fs.readFileSync(require.resolve('./public/pv_revision_review.js'), 'utf8');
const hash = text => require('crypto').createHash('sha256').update(text).digest('hex');
const originals = { 'code.rs': 'fn original() {\r\n    1\r\n}\r\n', 'added.rs': null, 'deleted.rs': 'restore\r\nexact\r\n',
  'unchanged.rs': 'untouched\n', 'mixed.rs': 'first\r\nsecond\nlast\r', 'empty.rs': '', 'binary.bin': null };
const proposals = { ...originals, 'code.rs': 'fn proposed() {\r\n    2\r\n}\r\n', 'added.rs': 'new file\n', 'deleted.rs': null, 'binary.bin': 'binary' };
const manifest = text => text === null ? null : { sha256_of_raw_bytes: hash(text), size: Buffer.byteLength(text) };
const sourceFiles = Object.keys(proposals).map(path => ({ path, before: manifest(originals[path]), after: manifest(proposals[path]),
  operation: originals[path] === proposals[path] ? 'unchanged' : originals[path] === null ? 'add' : proposals[path] === null ? 'delete' : 'modify' }));
function editorHarness(storage = new Map(), options = {}) {
  const doc = { head: new Element('head'), body: new Element('body'), createElement: tag => new Element(tag), createTextNode: text => new Element('#text', text) };
  let currentDigest = digest;
  const requests = [];
  const env = { document: doc, URLSearchParams, TextEncoder,
    location: { pathname: '/base/project', search: '', hash: '' }, history: { replaceState() {} },
    sessionStorage: { getItem: key => storage.get(key), setItem: (key, value) => { if (options.storageFails) throw new Error('quota'); storage.set(key, value); } },
    window: { addEventListener() {} }, fetch: async (url, request) => {
      requests.push({ url, request });
      if (request) return options.post ? options.post(request) : { ok: true, json: async () => ({ scheduled: true }) };
      if (url.includes('/review?')) return { ok: true, json: async () => ({ ...pkg, schema: undefined, digest: currentDigest,
        ...(options.commentReview ? { comment_amendment: { review: options.commentReview } } : {}),
        files: sourceFiles.filter(f => f.operation !== 'unchanged'), source_files: sourceFiles }) };
      const params = new URL(url, 'http://localhost').searchParams, file = params.get('path'), mode = params.get('mode');
      const data = { digest: currentDigest, path: file, mode,
        before_hash: originals[file] === null ? null : hash(originals[file]), after_hash: proposals[file] === null ? null : hash(proposals[file]),
        before_exists: originals[file] !== null, after_exists: proposals[file] !== null,
        binary: file === 'binary.bin', text: proposals[file] || '', original_text: originals[file] || '',
        rows: [{ kind: 'context', before: 1, after: 1, text: proposals[file] || '' }], total: 1 };
      if (options.fileResponse) return options.fileResponse(data);
      return { ok: true, json: async () => data };
    },
  };
  vm.runInNewContext(source, env);
  const ctrl = env.window.PVRevisionReview({ apiPath: x => '/base/project/api/' + x, controlPath: x => '/base/project/control/' + x,
    controlHeaders: headers => ({ ...headers, token: 'existing-auth' }), controlAvailable: () => true, refresh: async () => {} });
  const page = doc.body.children[0];
  const find = (tag, text) => walk(page).find(n => n.tagName === tag && n.textContent === text);
  const settle = () => new Promise(resolve => setImmediate(resolve));
  const click = async text => { const target = find('button', text); assert(target, `Missing button ${text}`); await target.fire('click'); await settle(); };
  const editor = () => walk(page).find(n => n.attrs['aria-label']?.startsWith('Desired code for '));
  const codeFile = async path => {
    const target = walk(page).find(n => n.tagName === 'button' && n.textContent.split(' · ')[0].slice(2) === path);
    assert(target, `Missing file ${path}`); await target.fire('click'); await settle();
  };
  const input = async text => { const e = editor(); assert(e && !e.disabled, 'editable textarea required'); e.value = text; await e.fire('input'); };
  const allFiles = async () => { const c = walk(page).find(n => n.tagName === 'input' && n.type === 'checkbox'); c.checked = true; await c.fire('change'); await settle(); };
  const open = async value => { currentDigest = value || digest; ctrl.update({ digest: currentDigest, state: 'awaiting_human' }); await ctrl.open(); await settle(); };
  return { page, requests, ctrl, find, click, editor, input, allFiles, codeFile, open, storage, settle,
    drafts: () => JSON.parse(storage.get(`pv-review:/base/project:${currentDigest}`) || '{}').codeDrafts || {},
    posts: () => requests.filter(r => r.request),
    setReason: async text => { const r = walk(page).find(n => n.name === 'reason'); r.value = text; await r.fire('input'); } };
}
async function testCodeDrafts() {
  const h = editorHarness(); await h.open(); await h.click('Code'); await h.click('Edit code');
  assert.strictEqual(h.editor().value, proposals['code.rs'].replace(/\r\n/g, '\n'));
  assert.deepStrictEqual(h.drafts(), {}, 'Opening CRLF text must not create a dirty edit');
  assert(!h.find('button', 'Ratify goals and code').disabled);
  await h.input(h.editor().value.replace('proposed', 'desired'));
  const desired = proposals['code.rs'].replace('proposed', 'desired');
  assert.strictEqual(h.drafts()['code.rs'].text, desired, 'Only the changed span is replaced; CRLF and EOF survive');
  assert(h.find('button', 'Ratify goals and code').disabled); assert(h.find('button', 'Deny').disabled);
  await h.click('Ratify goals and code'); await h.click('Deny'); assert.strictEqual(h.posts().length, 0, 'No decision silently discards a code draft');
  await h.click('Request amendment'); assert(h.page.textContent.includes('Enter a reason.')); assert.strictEqual(h.posts().length, 0);
  await h.click('Diff');
  assert(h.page.textContent.includes('Original → Proposed'));
  assert(h.page.textContent.includes(proposals['code.rs'])); assert(!h.page.textContent.includes(desired), 'Original/proposed views exclude unsent editor bytes');
  await h.click('Edit code'); assert.strictEqual(h.editor().value, desired.replace(/\r\n/g, '\n'));
  await h.allFiles(); await h.codeFile('unchanged.rs'); await h.input('changed without final newline');
  assert.strictEqual(h.drafts()['unchanged.rs'].text, 'changed without final newline');
  await h.codeFile('mixed.rs'); await h.input(h.editor().value.replace('second', 'replaced'));
  assert.strictEqual(h.drafts()['mixed.rs'].text, 'first\r\nreplaced\nlast\r', 'Untouched mixed line endings survive local edits');
  await h.click('Reset draft'); assert(!h.drafts()['mixed.rs']);
  await h.codeFile('code.rs'); assert.strictEqual(h.editor().value, desired.replace(/\r\n/g, '\n'), 'Each file owns its draft');
  await h.click('Goals'); await h.click('Code'); assert.strictEqual(h.editor().value, desired.replace(/\r\n/g, '\n'));
  await h.setReason('Exact desired code.');
  const reloaded = editorHarness(h.storage); await reloaded.open();
  assert.strictEqual(reloaded.editor().value, desired.replace(/\r\n/g, '\n'), 'Digest-scoped code draft survives refresh');
  assert.strictEqual(walk(reloaded.page).find(n => n.name === 'reason').value, 'Exact desired code.');
  await reloaded.codeFile('added.rs'); await reloaded.click('Restore original');
  assert.strictEqual(reloaded.drafts()['added.rs'].text, null, 'Restoring an original absence requests deletion');
  assert(reloaded.editor().disabled);
  await reloaded.codeFile('deleted.rs'); await reloaded.click('Restore original');
  assert.strictEqual(reloaded.drafts()['deleted.rs'].text, originals['deleted.rs']);
  assert.strictEqual(reloaded.drafts()['deleted.rs'].base_sha256, null, 'Restored deletions are based on absent proposed bytes');
  await reloaded.codeFile('empty.rs'); await reloaded.input('x'); await reloaded.input('');
  assert(!reloaded.drafts()['empty.rs'], 'An empty file is distinct from a missing file, and exact no-ops are removed');
  await reloaded.codeFile('binary.bin'); assert(!reloaded.editor()); assert(reloaded.page.textContent.includes('binary'));
  reloaded.ctrl.update({ digest: changed, state: 'awaiting_human' });
  assert(reloaded.find('button', 'Request amendment').disabled); await reloaded.click('Request amendment');
  assert.strictEqual(reloaded.posts().length, 0, 'Stale digest never submits code');
  reloaded.ctrl.update({ digest, state: 'awaiting_human' }); await reloaded.click('Request amendment');
  const post = reloaded.posts()[0]; assert(post); const body = JSON.parse(post.request.body);
  assert.strictEqual(body.action, 'amend'); assert.strictEqual(body.digest, digest); assert.strictEqual(body.reason, 'Exact desired code.');
  assert.strictEqual(body.amendment, '', 'Code-only amendments need no redundant prose');
  assert.strictEqual(post.request.headers.token, 'existing-auth');
  assert.deepStrictEqual(body.code_edits, [
    { path: 'added.rs', base_sha256: hash(proposals['added.rs']), text: null },
    { path: 'code.rs', base_sha256: hash(proposals['code.rs']), text: desired },
    { path: 'deleted.rs', base_sha256: null, text: originals['deleted.rs'] },
    { path: 'unchanged.rs', base_sha256: hash(proposals['unchanged.rs']), text: 'changed without final newline' },
  ]);
  await reloaded.click('Request amendment'); assert.strictEqual(reloaded.posts().length, 1, 'A saved decision cannot be submitted twice while status refresh catches up');
  const next = editorHarness(h.storage); await next.open(changed);
  assert.deepStrictEqual(next.drafts(), {}, 'New approval digest does not inherit the old code drafts');
  assert(next.find('button', 'Ratify goals and code').disabled === false);
  assert(JSON.parse(h.storage.get(`pv-review:/base/project:${digest}`)).codeDrafts['code.rs'], 'Old digest draft remains separately retained');
  console.log('PV editor exact text, newline preservation, file/digest isolation, restore/delete and amendment payload passed');
}
async function testEditorFailureStates() {
  const h = editorHarness(); await h.open(); await h.click('Code'); await h.click('Edit code'); await h.setReason('Fix source.');
  await h.input('has\0nul'); assert(h.find('button', 'Request amendment').disabled); assert(h.page.textContent.includes('NUL character'));
  await h.click('Reset draft'); assert.deepStrictEqual(h.drafts(), {});
  await h.input('é'.repeat(530000)); assert(h.find('button', 'Request amendment').disabled); assert(h.page.textContent.includes('1 MiB'));
  await h.click('Reset draft'); await h.input('x\n'.repeat(20001)); assert(h.find('button', 'Request amendment').disabled);
  await h.click('Reset all code drafts'); assert(!h.find('button', 'Ratify goals and code').disabled);
  await h.click('Request amendment'); assert.strictEqual(h.posts().length, 0, 'An amendment without code or prose stays invalid');
  const fail = editorHarness(new Map(), { storageFails: true }); await fail.open(); await fail.click('Code'); await fail.click('Edit code'); await fail.input('keep this exact text');
  assert(fail.page.textContent.includes('Browser storage could not save this draft for reload.'));
  assert(fail.find('button', 'Ratify goals and code').disabled, 'Storage failure does not lose the in-memory draft');
  const mismatch = editorHarness(new Map(), { fileResponse: async data => ({ ok: true, json: async () => ({ ...data, after_hash: 'f'.repeat(64) }) }) });
  await mismatch.open(); await mismatch.click('Code'); await mismatch.click('Edit code');
  assert(!mismatch.editor()); assert(mismatch.page.textContent.includes('Proposed file identity changed'));
  let release;
  const pending = editorHarness(new Map(), { post: () => new Promise(resolve => { release = resolve; }) });
  await pending.open(); await pending.click('Code'); await pending.click('Edit code'); await pending.input('desired\n'); await pending.setReason('Exact change.');
  const posting = pending.click('Request amendment'); await pending.settle();
  assert(pending.editor().disabled, 'The submitted edit cannot change while its request is in flight');
  release({ ok: false, json: async () => ({ error: 'Package changed on server' }) }); await posting;
  assert(!pending.editor().disabled); assert(pending.drafts()['code.rs'], 'A failed submission keeps the desired text for correction');
  assert(pending.page.textContent.includes('Package changed on server'));
  let finishOldFile;
  const raced = editorHarness(new Map(), { fileResponse: data => data.path === 'code.rs' && data.mode === 'edit'
    ? new Promise(resolve => { finishOldFile = () => resolve({ ok: true, json: async () => data }); })
    : Promise.resolve({ ok: true, json: async () => data }) });
  await raced.open(); await raced.click('Code'); await raced.click('Edit code');
  await raced.codeFile('added.rs'); await raced.input('new desired file\n');
  finishOldFile(); await raced.settle();
  assert.strictEqual(raced.editor().attrs['aria-label'], 'Desired code for added.rs');
  assert.strictEqual(raced.editor().value, 'new desired file\n', 'A late file response must not overwrite another file’s editor');
  assert(!raced.drafts()['code.rs']);
  console.log('PV editor bounds, binary/identity gates, storage warnings and failed-submit retention passed');
}
async function testOptionalApprovalReason() {
  const h = editorHarness(); await h.open();
  await h.click('Deny'); assert.strictEqual(h.posts().length, 0);
  assert(h.page.textContent.includes('Enter a reason.'));
  await h.click('Request amendment'); assert.strictEqual(h.posts().length, 0);
  await h.click('Ratify goals and code'); assert.strictEqual(h.posts().length, 1);
  assert.deepStrictEqual(JSON.parse(h.posts()[0].request.body), { action: 'accept', digest, reason: '', amendment: '' });
  assert.strictEqual(h.posts()[0].request.headers.token, 'existing-auth');
  console.log('PV blank approval reason and required denial/amendment reasons passed');
}
async function testCommentOnlySelection() {
  const h = editorHarness(new Map(), { commentReview: { decision: 'pass', reason: 'Accurate comments.',
    editorial_notes: ['Optional wording <script>suggestion</script>.'] } });
  await h.open(); await h.setReason('Comment wording.');
  assert(h.page.textContent.includes('Editorial note: Optional wording <script>suggestion</script>.'));
  assert(!walk(h.page).find(n => n.tagName === 'script'));
  const checkbox = walk(h.page).find(n => n.name === 'comment_only');
  assert(checkbox && !checkbox.checked);
  checkbox.checked = true; await checkbox.fire('change');
  await h.click('Request amendment'); assert.strictEqual(h.posts().length, 0);
  assert(h.page.textContent.includes('Edit the exact comment text'));
  await h.click('Code'); await h.click('Edit code'); await h.input('// revised comment\n');
  const restored = editorHarness(h.storage); await restored.open();
  assert(walk(restored.page).find(n => n.name === 'comment_only').checked);
  await restored.click('Request amendment');
  assert.strictEqual(JSON.parse(restored.posts()[0].request.body).comment_only, true);
  console.log('PV comment-only selection, persistence and exact-edit requirement passed');
}
(async () => { await testReview(); await testCodeDrafts(); await testEditorFailureStates(); await testCommentOnlySelection(); await testOptionalApprovalReason(); })().catch(error => { console.error(error); process.exitCode = 1; });
