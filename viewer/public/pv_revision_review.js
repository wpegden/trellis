/* Full-width PV review. All package text is rendered as text, never markup. */
(() => {
  const node = (tag, text = '', cls = '') => { const n = document.createElement(tag); n.textContent = text; if (cls) n.className = cls; return n; };
  const button = (text, action) => { const n = node('button', text); n.type = 'button'; n.addEventListener('click', action); return n; };
  const paragraph = text => node('p', text || 'None recorded.');
  const details = (label, ...contents) => { const d = node('details'); d.append(node('summary', label), ...contents); return d; };
  const unique = values => [...new Set(values)];
  function textComparison(before, after) {
    const box = node('div', '', 'pvr-compare');
    let prefix = 0, suffix = 0;
    while (prefix < before.length && prefix < after.length && before[prefix] === after[prefix]) prefix++;
    while (suffix < before.length - prefix && suffix < after.length - prefix && before[before.length - suffix - 1] === after[after.length - suffix - 1]) suffix++;
    for (const [title, value] of [['Original', before], ['Proposed', after]]) {
      const column = node('div'); column.append(node('h4', title));
      const text = node('div', '', 'pvr-prose');
      if (before === after) text.textContent = value || '(empty)';
      else {
        text.append(document.createTextNode(value.slice(0, prefix)));
        text.append(node('mark', value.slice(prefix, value.length - suffix)));
        text.append(document.createTextNode(suffix ? value.slice(-suffix) : ''));
        if (!value) text.append(document.createTextNode('(absent)'));
      }
      column.append(text); box.append(column);
    }
    return box;
  }
  function alternatives(rows) {
    const list = node('ul');
    for (const row of rows || []) list.append(node('li', `${row.remedy || row.approach}${row.status ? ` (${row.status})` : ''}: ${row.rationale}\n${(row.evidence || []).join('\n')}`));
    return list;
  }
  function syntax(text, path) {
    const code = node('code');
    if (!/\.(rs|py|js|ts|c|h|cpp|lean|sh|json)$/.test(path) || text.length > 16000) { code.textContent = text; return code; }
    const tokens = /(\/\/[^\n]*|#[^\n]*|"(?:\\.|[^"\\])*"|'(?:\\.|[^'\\])*'|\b(?:fn|pub|let|mut|const|return|if|else|match|for|while|in|impl|struct|enum|use|mod|def|class|import|from|assert|try|except|True|False|None|true|false|null|async|await)\b|\b\d+(?:\.\d+)?\b)/g;
    let last = 0;
    for (const match of text.matchAll(tokens)) {
      code.append(document.createTextNode(text.slice(last, match.index)));
      const t = match[0]; code.append(node('span', t, t.startsWith('//') || t.startsWith('#') ? 'pvr-comment' : /^['"]/.test(t) ? 'pvr-string' : /^\d/.test(t) ? 'pvr-number' : 'pvr-keyword'));
      last = match.index + t.length;
    }
    code.append(document.createTextNode(text.slice(last))); return code;
  }
  // Textareas display CRLF/CR as LF. Preserve unchanged bytes and map only the
  // edited span back to the source's newline convention, including its EOF.
  function editorText(previous, next) {
    const view = previous.replace(/\r\n?/g, '\n');
    if (view === next) return previous;
    let start = 0, suffix = 0;
    while (start < view.length && start < next.length && view[start] === next[start]) start++;
    while (suffix < view.length - start && suffix < next.length - start
      && view[view.length - suffix - 1] === next[next.length - suffix - 1]) suffix++;
    const exactOffset = position => {
      let exact = 0, displayed = 0;
      while (displayed < position && exact < previous.length) {
        if (previous[exact] === '\r' && previous[exact + 1] === '\n') exact++;
        exact++; displayed++;
      }
      return exact;
    };
    const newline = previous.match(/\r\n|\r|\n/)?.[0] || '\n';
    return previous.slice(0, exactOffset(start))
      + next.slice(start, next.length - suffix).replace(/\n/g, newline)
      + previous.slice(exactOffset(view.length - suffix));
  }
  window.PVRevisionReview = function ({ apiPath, controlPath, controlHeaders, controlAvailable, refresh }) {
    const style = node('style'); style.textContent = `
      .pvr-page{position:fixed;inset:0;z-index:10050;overflow:auto;background:#1a1b20;color:#ddd;padding:24px 32px;font:14px/1.55 system-ui,sans-serif;box-sizing:border-box}
      .pvr-page[hidden]{display:none}.pvr-page *{box-sizing:border-box}.pvr-page h1{font-size:22px}.pvr-page h2{font-size:18px}.pvr-page h3{font-size:16px}.pvr-page h4{margin:8px 0;color:#aab0bf}
      .pvr-page button,.pvr-page select{background:#292b33;color:#e5e7ec;border:1px solid #4d515d;border-radius:4px;padding:6px 10px;cursor:pointer;font:inherit}.pvr-page button:disabled{opacity:.5;cursor:default}.pvr-page button[aria-pressed=true]{border-color:#8be9fd;color:#8be9fd}
      .pvr-page a{color:#8be9fd}.pvr-page textarea,.pvr-page input{background:#202128;color:#eee;border:1px solid #555963;border-radius:4px;font:inherit;padding:8px}.pvr-page textarea{width:100%;min-height:70px;margin:6px 0}
      .pvr-toolbar{display:flex;gap:10px;align-items:center;flex-wrap:wrap;margin:12px 0}.pvr-tabs{position:sticky;top:0;z-index:3;background:#1a1b20;padding:10px 0;border-bottom:1px solid #393d48}.pvr-muted{color:#aab0bf;font-size:12px}.pvr-digest{overflow-wrap:anywhere;font:12px monospace}.pvr-prose{white-space:pre-wrap;overflow-wrap:anywhere}.pvr-page li{white-space:pre-wrap;overflow-wrap:anywhere}
      .pvr-compare{display:grid;grid-template-columns:1fr 1fr;gap:20px}.pvr-compare>div{min-width:0}.pvr-page mark{background:#574922;color:#ffe9a2}.pvr-card{border-top:1px solid #3a3d46;padding:16px 0}.pvr-page details{margin:10px 0}.pvr-page summary{cursor:pointer;color:#d6d9e0}.pvr-note{border-left:2px solid #8b80b8;padding:2px 12px;margin:12px 0}
      .pvr-code-layout{display:grid;grid-template-columns:240px minmax(0,1fr);gap:22px}.pvr-files{align-self:start;position:sticky;top:64px;max-height:75vh;overflow:auto}.pvr-files button{display:block;text-align:left;width:100%;overflow-wrap:anywhere;margin-bottom:5px;font:12px monospace}.pvr-code-main{min-width:0}.pvr-diff{overflow:auto;border:1px solid #393d48;max-height:70vh}.pvr-diff table{border-collapse:collapse;min-width:100%;font:12px/1.6 ui-monospace,monospace}.pvr-diff td{padding:0 8px;vertical-align:top}.pvr-line{user-select:none;color:#8a919e;text-align:right;min-width:36px;border-right:1px solid #333842}.pvr-source{white-space:pre;min-width:200px}.pvr-add{background:#183529}.pvr-delete{background:#40262d}.pvr-hunk{background:#252d41;color:#b8c5e4}.pvr-keyword{color:#c7a9ef}.pvr-string{color:#dac486}.pvr-comment{color:#939daf}.pvr-number{color:#99c9dd}.pvr-split .pvr-source{min-width:300px}.pvr-status{color:#f2d58a}.pvr-ratify{border-top:1px solid #4e445f;margin-top:24px;padding-top:16px}
      .pvr-page .pvr-editor{font:13px/1.5 ui-monospace,Menlo,Consolas,monospace;min-height:50vh;white-space:pre;overflow:auto;tab-size:4;resize:vertical}.pvr-draft-summary{border-left:3px solid #d9b268;padding:8px 12px;white-space:pre-wrap}.pvr-editor-status{white-space:pre-wrap;color:#aab0bf;font-size:12px}
      @media(max-width:800px){.pvr-page{padding:16px}.pvr-code-layout{grid-template-columns:1fr}.pvr-files{position:static;max-height:180px}.pvr-compare{grid-template-columns:1fr}}
    `; document.head.append(style);
    const page = node('section', '', 'pvr-page'); page.hidden = true; page.setAttribute('aria-label', 'Program verification revision review'); document.body.append(page);
    let latest = null, pkg = null, loadedDigest = '', loading = false, fileRequest = 0, busy = false;
    let tab = 'goals', file = '', mode = 'diff', split = false, context = 3, offset = 0, showAllFiles = false;
    let codeDrafts = Object.create(null), storageError = '', draftSummary, submitted = false, updateEditorControls = null;
    const editFiles = new Map();
    let reason, amendment, commentOnly, status, decisionForm, content, goalsTab, codeTab, stale, openTrigger, savedHash = '';
    const key = () => `pv-review:${location.pathname}:${loadedDigest}`;
    const persist = () => {
      if (!loadedDigest) return;
      try {
        sessionStorage.setItem(key(), JSON.stringify({ tab, file, mode, split, context, offset, showAllFiles, codeDrafts,
          reason: reason?.value || '', amendment: amendment?.value || '', commentOnly: commentOnly?.checked || false, scroll: page.scrollTop }));
        storageError = '';
      } catch { storageError = 'Browser storage could not save this draft for reload. Keep this page open until you submit or reset it.'; }
      updateDraftSummary();
    };
    function restore() { try { return JSON.parse(sessionStorage.getItem(key()) || '{}'); } catch { return {}; } }
    const sourceFiles = () => pkg.source_files || pkg.files;
    const edits = () => Object.entries(codeDrafts).sort(([a], [b]) => a.localeCompare(b))
      .map(([path, draft]) => ({ path, base_sha256: draft?.base_sha256, text: draft?.text }));
    function editError() {
      const rows = edits();
      if (rows.length > 64) return 'An amendment can include up to 64 edited files.';
      let total = 0;
      for (const edit of rows) {
        const entry = sourceFiles().find(f => f.path === edit.path);
        if (!entry || edit.base_sha256 !== (entry.after?.sha256_of_raw_bytes ?? null)) return `Reload or reset the draft for ${edit.path}; its proposed-file identity does not match this package.`;
        if (edit.text !== null && typeof edit.text !== 'string') return `Reset the invalid draft for ${edit.path}.`;
        if (edit.text === null) continue;
        const bytes = new TextEncoder().encode(edit.text).length;
        const lines = edit.text ? edit.text.split(/\r\n|\r|\n/).length - Number(/[\r\n]$/.test(edit.text)) : 0;
        if (edit.text.includes('\0')) return `${edit.path} contains a NUL character and cannot be edited as text.`;
        if (bytes > 1024 * 1024 || lines > 20000) return `${edit.path} exceeds the limit of 1 MiB or 20,000 lines.`;
        total += bytes;
      }
      return total > 4 * 1024 * 1024 ? 'The edited files exceed the 4 MiB amendment limit.' : '';
    }
    function updateDraftSummary() {
      if (!draftSummary) return;
      const count = Object.keys(codeDrafts).length;
      draftSummary.hidden = !count && !storageError;
      draftSummary.textContent = [count ? submitted ? 'Code edits sent with the amendment. The browser retains this draft for reference.' : `${count} code file${count === 1 ? '' : 's'} edited. Use Request amendment to send these edits; ratification and denial are disabled until you send or reset them.` : '',
        count ? editError() : '', storageError].filter(Boolean).join('\n');
    }
    function setCodeDraft(path, data, text) {
      const proposed = data.after_exists ? data.text : null;
      if (text === proposed) delete codeDrafts[path];
      else codeDrafts[path] = { base_sha256: data.after_hash, text };
      persist(); updateDecision();
    }
    function renderEditor(data, holder, path) {
      holder.replaceChildren();
      if (data.binary || typeof data.text !== 'string' || typeof data.original_text !== 'string') {
        holder.append(paragraph('This file is binary, non-UTF-8, or unavailable for text editing.')); return;
      }
      editFiles.set(path, data);
      const restored = codeDrafts[path];
      if (restored && restored.base_sha256 === data.after_hash
        && restored.text === (data.after_exists ? data.text : null)) { delete codeDrafts[path]; persist(); updateDecision(); }
      holder.append(node('h4', 'Requested code edit'), paragraph('Edits are sent with your amendment for review and checking.'));
      const editorStatus = node('p', '', 'pvr-editor-status'); editorStatus.setAttribute('role', 'status');
      const editor = node('textarea', '', 'pvr-editor'); editor.setAttribute('aria-label', `Desired code for ${path}`);
      editor.spellcheck = false; editor.wrap = 'off'; editor.autocomplete = 'off'; editor.autocapitalize = 'off';
      let exact = '';
      const toolbar = node('div', '', 'pvr-toolbar');
      const reset = button('Reset draft', () => { delete codeDrafts[path]; persist(); updateDecision(); display(); });
      const original = button('Restore original', () => { setCodeDraft(path, data, data.before_exists ? data.original_text : null); display(); });
      const remove = button('Delete file', () => { setCodeDraft(path, data, null); display(); });
      const create = button('Create empty file', () => { setCodeDraft(path, data, ''); display(); });
      toolbar.append(reset, original, remove, create);
      updateEditorControls = () => {
        const draft = codeDrafts[path], value = draft ? draft.text : data.after_exists ? data.text : null;
        editor.disabled = value === null || busy || submitted;
        reset.disabled = !draft || busy || submitted;
        original.disabled = busy || submitted || value === (data.before_exists ? data.original_text : null);
        remove.disabled = busy || submitted || value === null; create.hidden = value !== null; create.disabled = busy || submitted;
      };
      function display() {
        const draft = codeDrafts[path];
        const value = draft ? draft.text : data.after_exists ? data.text : null;
        exact = value ?? '';
        editor.value = exact; updateEditorControls();
        editorStatus.textContent = value === null ? (draft ? 'Requested change: delete this file.' : 'This file is absent from the proposal. Restore original or create an empty file to edit it.')
          : `${draft ? 'Requested code edit' : 'Proposed code · no unsent edit'} · ${new TextEncoder().encode(exact).length} bytes · ${/[\r\n]$/.test(exact) ? 'ends with a newline' : 'no final newline'}${editError() ? '\n' + editError() : ''}`;
      }
      editor.addEventListener('input', () => {
        if (busy || submitted) { editor.value = exact; return; }
        exact = editorText(exact, editor.value);
        setCodeDraft(path, data, exact);
        reset.disabled = !codeDrafts[path];
        original.disabled = exact === (data.before_exists ? data.original_text : null);
        editorStatus.textContent = `${codeDrafts[path] ? 'Requested code edit' : 'Proposed code · no unsent edit'} · ${new TextEncoder().encode(exact).length} bytes · ${/[\r\n]$/.test(exact) ? 'ends with a newline' : 'no final newline'}${editError() ? '\n' + editError() : ''}`;
      });
      holder.append(editorStatus, toolbar, editor); display();
    }
    function close() { persist(); page.hidden = true; if (location.hash === '#pv-review') history.replaceState(null, '', location.pathname + location.search + savedHash); openTrigger?.focus(); }
    function goalLink(id) { return button(id, () => { tab = 'goals'; showTab(); const card = [...content.querySelectorAll('[data-goal]')].find(n => n.dataset.goal === id); if (card) { card.open = true; card.scrollIntoView({ block: 'start' }); } persist(); }); }
    function fileLink(path) { return button(path, () => { if (!pkg.files.some(f => f.path === path)) showAllFiles = true; file = path; tab = 'code'; mode = 'diff'; offset = 0; showTab(); persist(); }); }
    function noteView(note, label) { const b = node('div', '', 'pvr-note'); if (note.path) b.append(node('div', note.path, 'pvr-digest')); b.append(node('div', `${label} · ${note.author_role || 'role not recorded'}${note.characterization ? ` · ${note.characterization}` : ''}`, 'pvr-muted'), paragraph(note.reason)); if (note.range) b.append(paragraph(`${note.range.side} lines ${note.range.start}–${note.range.end} in this note’s review snapshot`)); const links = node('div', '', 'pvr-toolbar'); for (const id of note.goal_ids || []) links.append(goalLink(id)); b.append(links); return b; }
    function taskView(attempt) {
      const auth = attempt.authorization || {};
      const box = details(`Task context: ${auth.title || attempt.task_id} · ${attempt.id} · ${attempt.accepted ? 'accepted' : 'not accepted'}`);
      box.append(paragraph(`Authorization: ${auth.authorization || 'Not recorded'}`), paragraph(`Authorized paths (may include unchanged files): ${(auth.paths || auth.allowed_paths || []).join(', ') || 'semantic scope only'}`));
      box.append(paragraph(`Implementation summary: ${attempt.response?.summary || 'Not recorded'}`), alternatives(attempt.response?.alternatives));
      box.append(paragraph(`Task scope review: ${attempt.scope?.reason || 'Not recorded'}`), paragraph(attempt.scope?.alternatives_assessment));
      const links = node('div', '', 'pvr-toolbar'); for (const id of auth.goal_ids || []) links.append(goalLink(id)); box.append(links);
      return box;
    }
    function relatedAttempts(path) { const ids = new Set(pkg.task_diffs.filter(d => d.paths.includes(path)).map(d => d.attempt_id)); return pkg.attempts.filter(a => ids.has(a.id)); }
    function allNotes() { return [...(pkg.aggregate_scope?.code_notes || []).map(note => ({ note, label: 'Final scope note' })), ...pkg.attempts.flatMap(a => (a.scope?.code_notes || []).map(note => ({ note, label: `Task scope note · ${a.id} · ${a.accepted ? 'accepted' : 'not accepted'}` })))]; }
    function renderGoals() {
      const old = pkg.original_collection, next = pkg.proposed_collection;
      content.append(paragraph('Identical requirement text does not establish identical meaning. Definitions that refer to source can change interpretation when the implementation changes.'));
      content.append(details('Shared context and definitions · ' + (old.context === next.context ? 'text unchanged' : 'text changed'), textComparison(old.context || '', next.context || '')));
      content.append(details('Complete audit rationale and remedy alternatives · includes the rationale for retained goals', paragraph(pkg.plan?.summary), alternatives(pkg.plan?.alternatives)));
      const originals = new Map(old.goals.map(g => [g.id, g])), proposed = new Map(next.goals.map(g => [g.id, g]));
      const dispositions = pkg.plan?.dispositions || [], notes = pkg.plan?.goal_notes || [], codeNotes = allNotes();
      const affected = new Set([...dispositions.map(d => d.goal_id), ...notes.flatMap(n => n.goal_ids), ...(pkg.plan?.tasks || []).flatMap(t => t.goal_ids), ...codeNotes.flatMap(n => n.note.goal_ids), ...dispositions.flatMap(d => [...(d.replacement_goals || []), ...(d.surviving_obligations || [])].map(g => g.id))]);
      const ids = unique([...originals.keys(), ...proposed.keys()]).sort((a, b) => Number(affected.has(b)) - Number(affected.has(a)));
      const inventory = node('div');
      const filter = node('input'); filter.placeholder = 'Find any requirement by ID or text'; filter.setAttribute('aria-label', 'Find requirement'); filter.style.width = '100%';
      content.append(node('h2', `Complete requirement inventory (${ids.length})`), paragraph('Goals with structured audit, task or note relationships appear first. Every retained requirement remains below; “text unchanged” describes wording only.'), filter, inventory);
      const added = ids.filter(id => !originals.has(id)).length, retired = ids.filter(id => !proposed.has(id)).length;
      content.insertBefore(paragraph(`${added} added · ${retired} retired · ${ids.length - added - retired} retained IDs`), filter);
      for (const id of ids) {
        const before = originals.get(id), after = proposed.get(id), disposition = dispositions.find(d => d.goal_id === id);
        const label = !before ? 'added' : !after ? 'retired' : before.text === after.text ? 'retained · text unchanged' : 'retained ID · text changed';
        const card = details(`${id} — ${label}`); card.dataset.goal = id; card.open = affected.has(id);
        card.append(textComparison(before?.text || '', after?.text || ''));
        if (disposition) { card.append(node('h4', `Audit disposition: ${disposition.remedy}`), paragraph(disposition.reason)); for (const [key, label] of [['replacement_goals', 'Replacement'], ['surviving_obligations', 'Surviving obligation']]) for (const goal of disposition[key] || []) { const relation = paragraph(`${label}: `); relation.append(goalLink(goal.id)); card.append(relation); } }
        for (const d of dispositions) for (const k of ['replacement_goals', 'surviving_obligations']) if ((d[k] || []).some(g => g.id === id)) { const relation = paragraph(k === 'replacement_goals' ? 'Replaces: ' : 'Survives retirement of: '); relation.append(goalLink(d.goal_id)); card.append(relation); }
        for (const n of notes.filter(n => n.goal_ids.includes(id))) card.append(noteView(n, 'Audit note'));
        const tasks = pkg.attempts.filter(a => (a.authorization?.goal_ids || []).includes(id));
        for (const a of tasks) card.append(taskView(a));
        const paths = unique([...pkg.task_diffs.filter(d => tasks.some(a => a.id === d.attempt_id)).flatMap(d => d.paths), ...codeNotes.filter(n => n.note.goal_ids.includes(id)).map(n => n.note.path)]).filter(p => pkg.files.some(f => f.path === p));
        if (paths.length) { const links = node('div', '', 'pvr-toolbar'); links.append(document.createTextNode('Related changed files: ')); for (const p of paths) links.append(fileLink(p)); card.append(links); }
        for (const n of codeNotes.filter(n => n.note.goal_ids.includes(id))) card.append(noteView(n.note, n.label));
        if (!disposition && !notes.some(n => n.goal_ids.includes(id))) card.append(node('p', 'No goal-specific audit reason recorded. See the shared context, audit rationale and alternatives above.', 'pvr-muted'));
        inventory.append(card);
      }
      filter.addEventListener('input', () => { const q = filter.value.toLowerCase(); for (const c of inventory.children) { c.hidden = !c.textContent.toLowerCase().includes(q); if (q && !c.hidden) c.open = true; } });
    }
    function renderRows(data, holder) {
      holder.replaceChildren();
      if (data.binary) { holder.append(paragraph('Binary or non-UTF-8 file. Exact bytes are available in the complete review archive.')); return; }
      if (mode !== 'diff' && !data[mode + '_exists']) { holder.append(paragraph(`No ${mode} file exists for this ${mode === 'before' ? 'addition' : 'deletion'}.`)); return; }
      const frame = node('div', '', 'pvr-diff' + (split && mode === 'diff' ? ' pvr-split' : ''));
      const table = node('table'); table.setAttribute('aria-label', `${file} ${mode === 'diff' ? (split ? 'side by side diff' : 'unified diff') : mode + ' file'}`);
      const head = node('tr'); for (const title of mode === 'diff' ? (split ? ['Old', 'Original', 'New', 'Proposed'] : ['Old', 'New', 'Change', 'Source']) : ['Line', 'Source']) head.append(node('th', title)); table.append(head);
      for (const row of data.rows) {
        const tr = node('tr', '', 'pvr-' + row.kind);
        if (row.kind === 'hunk') { const td = node('td', row.text); td.colSpan = 4; tr.append(td); }
        else if (split && mode === 'diff') {
          for (const side of ['before', 'after']) { tr.append(node('td', row[side] || '', 'pvr-line')); const td = node('td', '', 'pvr-source'); if (row[side]) td.append(syntax(row.text, file)); tr.append(td); }
        } else {
          for (const side of mode === 'diff' ? ['before', 'after'] : [mode]) tr.append(node('td', row[side] || '', 'pvr-line'));
          if (mode === 'diff') tr.append(node('td', row.kind === 'add' ? '+' : row.kind === 'delete' ? '−' : ' '));
          const td = node('td', '', 'pvr-source'); td.append(syntax(row.text, file)); tr.append(td);
        }
        if (row.kind !== 'hunk' && !row.text.endsWith('\n')) tr.title = 'No newline at end of file';
        table.append(tr);
      }
      frame.append(table); holder.append(frame);
      if (!data.total) holder.append(paragraph('No text diff (the manifest may record a file metadata change).'));
      const pagination = node('div', '', 'pvr-toolbar');
      const previous = button('Previous lines', () => { offset = Math.max(0, offset - 300); renderCode(); persist(); }); previous.disabled = offset === 0;
      const next = button('Next lines', () => { offset += 300; renderCode(); persist(); }); next.disabled = offset + data.rows.length >= data.total;
      pagination.append(previous, node('span', `${data.total ? offset + 1 : 0}–${offset + data.rows.length} of ${data.total} displayed rows`), next); holder.append(pagination);
    }
    async function renderCode() {
      content.replaceChildren(); updateEditorControls = null;
      const files = showAllFiles ? sourceFiles() : pkg.files;
      if (!files.some(f => f.path === file)) file = files[0]?.path || '';
      content.append(details('Final scope review and alternatives assessment', paragraph(pkg.aggregate_scope?.reason), paragraph(pkg.aggregate_scope?.alternatives_assessment)));
      const allLabel = node('label'), allToggle = node('input'); allToggle.type = 'checkbox'; allToggle.checked = showAllFiles;
      allToggle.addEventListener('change', () => { showAllFiles = allToggle.checked; offset = 0; renderCode(); persist(); });
      allLabel.append(allToggle, document.createTextNode(' Show all source files')); content.append(allLabel);
      if (!files.length) { content.append(paragraph(showAllFiles ? 'No source files are available in this package.' : 'No source files changed. Show all source files to request an edit.')); return; }
      const layout = node('div', '', 'pvr-code-layout'), nav = node('nav', '', 'pvr-files'), main = node('div', '', 'pvr-code-main'); nav.setAttribute('aria-label', showAllFiles ? 'All source files' : 'Changed files');
      const separated = pkg.schema === 'trellis-pv-revision-review/v2';
      for (const group of separated ? ['production', 'evidence'] : ['production']) {
        const members = separated ? files.filter(f => (f.group || 'production') === group) : files;
        if (separated) nav.append(node('h4', `${group === 'evidence' ? 'Evidence and harness (not ratified)' : showAllFiles ? 'Production files' : 'Production changes'} (${members.length})`));
        for (const f of members) { const b = button(`${f.operation === 'add' ? '+' : f.operation === 'delete' ? '−' : f.operation === 'unchanged' ? '=' : '~'} ${f.path}${codeDrafts[f.path] ? ' · edited' : ''}`, () => { file = f.path; offset = 0; renderCode(); persist(); }); b.setAttribute('aria-pressed', String(f.path === file)); nav.append(b); }
      }
      main.append(node('h3', file), node('p', 'Original → Proposed · review snapshot; unsent edits do not change these views.', 'pvr-muted'));
      if (separated) {
        const selected = files.find(f => f.path === file);
        main.append(paragraph(selected?.group === 'evidence' ? 'Attached evidence; excluded from production and ratification.' : 'Production code; included in ratification.'));
        if (selected?.after) main.append(node('p', `SHA-256: ${selected.after.sha256_of_raw_bytes}`, 'pvr-digest'));
      }
      const attempts = relatedAttempts(file), notes = allNotes().filter(n => n.note.path === file);
      const ids = unique([...attempts.flatMap(a => a.authorization?.goal_ids || []), ...notes.flatMap(n => n.note.goal_ids)]);
      const links = node('div', '', 'pvr-toolbar'); links.append(document.createTextNode('Related goals: ')); for (const id of ids) links.append(goalLink(id)); if (!ids.length) links.append(document.createTextNode('none recorded')); main.append(links);
      for (const n of notes) main.append(noteView(n.note, n.label));
      if (!notes.length) main.append(node('p', 'No file-specific notes recorded. The following authorizations, summaries and scope reviews are task-level context.', 'pvr-muted'));
      for (const a of attempts) main.append(taskView(a));
      const toolbar = node('div', '', 'pvr-toolbar');
      for (const [v, label] of [['diff', 'Diff'], ['before', 'Full file · before'], ['after', 'Full file · after'], ['edit', 'Edit code']]) { const b = button(label, () => { mode = v; offset = 0; renderCode(); persist(); }); b.setAttribute('aria-pressed', String(mode === v)); toolbar.append(b); }
      if (mode === 'diff') {
        const toggle = button(split ? 'Unified view' : 'Side-by-side view', () => { split = !split; renderCode(); persist(); }); toolbar.append(toggle);
        const expand = button(`Expand context (${context} lines)`, () => { context = Math.min(100, context + 10); offset = 0; renderCode(); persist(); }); expand.disabled = context >= 100; toolbar.append(expand);
      }
      main.append(toolbar); const holder = node('div'); main.append(holder); layout.append(nav, main); content.append(layout);
      const token = ++fileRequest, requestedFile = file, requestedMode = mode;
      holder.append(paragraph('Loading verified snapshot bytes…'));
      try {
        if (mode === 'edit' && editFiles.has(file)) { renderEditor(editFiles.get(file), holder, file); return; }
        const q = new URLSearchParams({ digest: loadedDigest, path: requestedFile, mode: requestedMode, context: String(context), offset: String(offset) });
        const response = await fetch(apiPath(`pv-revision/file?${q}`)); const data = await response.json();
        if (token !== fileRequest || tab !== 'code') return;
        if (!response.ok || data.digest !== loadedDigest || data.path !== requestedFile || data.mode !== requestedMode) throw new Error(data.error || 'Review package or file changed');
        if (requestedMode === 'edit') {
          const entry = sourceFiles().find(f => f.path === requestedFile);
          if (data.after_hash !== (entry?.after?.sha256_of_raw_bytes ?? null)) throw new Error('Proposed file identity changed; reopen the review.');
          renderEditor(data, holder, requestedFile);
        } else renderRows(data, holder);
      } catch (error) { if (token === fileRequest) holder.replaceChildren(paragraph(error.message)); }
    }
    function showTab() { fileRequest++; goalsTab.setAttribute('aria-pressed', String(tab === 'goals')); codeTab.setAttribute('aria-pressed', String(tab === 'code')); content.replaceChildren(); if (tab === 'goals') renderGoals(); else renderCode(); }
    function updateDecision() {
      if (!pkg) return;
      const available = latest?.digest === loadedDigest && latest?.state === 'awaiting_human';
      stale.textContent = latest?.digest !== loadedDigest ? 'The current package changed. This page still shows the previous digest; reopen the review before deciding.' : latest?.state !== 'awaiting_human' ? `Current revision status: ${latest?.state || 'unavailable'}.` : '';
      const hasDrafts = Object.keys(codeDrafts).length > 0;
      for (const b of decisionForm?.querySelectorAll('button') || []) {
        const action = b.dataset.decision;
        b.disabled = busy || submitted || (action ? !available || (hasDrafts && action !== 'amend') || (action === 'amend' && !!editError()) || (action === 'accept' && pkg.scope_passed === false) : !hasDrafts);
      }
      updateDraftSummary(); updateEditorControls?.();
    }
    function build() {
      const saved = restore(); submitted = false; editFiles.clear();
      codeDrafts = saved.codeDrafts && typeof saved.codeDrafts === 'object' && !Array.isArray(saved.codeDrafts) ? Object.assign(Object.create(null), saved.codeDrafts) : Object.create(null);
      showAllFiles = Boolean(saved.showAllFiles);
      tab = saved.tab === 'code' ? 'code' : 'goals'; file = sourceFiles().some(f => f.path === saved.file) ? saved.file : ''; mode = ['diff', 'before', 'after', 'edit'].includes(saved.mode) ? saved.mode : 'diff'; split = Boolean(saved.split); context = Number.isInteger(saved.context) ? Math.max(0, Math.min(100, saved.context)) : 3; offset = Number.isInteger(saved.offset) ? Math.max(0, Math.min(100000, saved.offset)) : 0;
      page.replaceChildren(); const top = node('div', '', 'pvr-toolbar'); top.append(button('← Back to run', close), node('h1', 'Review goals and code'),
        button('Live agents', () => { close(); window.TrellisSessions?.openLive(); }),
        button('Revision transcripts', () => { close(); window.TrellisSessions?.openHistory('revision'); })); page.append(top);
      const separated = pkg.schema === 'trellis-pv-revision-review/v2';
      page.append(node('p', `Review package: ${loadedDigest}`, 'pvr-digest'), node('p', `${separated ? 'Production and requirements ratification' : 'Source and requirements candidate'}: ${pkg.candidate_digest}`, 'pvr-digest'));
      if (separated) page.append(node('p', `Attached evidence (not ratified): ${pkg.evidence_digest}`, 'pvr-digest'));
      if (pkg.review_repackaging) page.append(paragraph('Repackaged without changing source bytes, requirements, checks or scope verdicts. Recorded checks used the combined snapshot; repackaging did not rerun them.'), paragraph(`Separation review: ${pkg.review_repackaging.separation_reason}`));
      if (pkg.comment_amendment) page.append(paragraph('Comment-only amendment. Earlier checks and scope verdicts remain bound to the prior reviewed source and were not rerun against this edited source.'), paragraph(`Focused review: ${pkg.comment_amendment.review.decision} — ${pkg.comment_amendment.review.reason}`));
      for (const note of pkg.comment_amendment?.review.editorial_notes || []) page.append(paragraph(`Editorial note: ${note}`));
      stale = node('p', '', 'pvr-status'); draftSummary = node('p', '', 'pvr-draft-summary'); draftSummary.setAttribute('role', 'status'); page.append(stale, draftSummary);
      const downloads = node('div', '', 'pvr-toolbar'); downloads.append(document.createTextNode('Download: ')); for (const [format, label] of [['zip', 'complete archive'], ['readme', 'review guide'], ['json', 'package JSON'], ['patch', separated ? 'production patch' : 'source patch'], ...(separated ? [['evidence-patch', 'evidence patch']] : [])]) { const a = node('a', label); a.href = apiPath(`pv-revision/package?format=${format}&digest=${loadedDigest}`); downloads.append(a); } page.append(downloads);
      const tabs = node('div', '', 'pvr-toolbar pvr-tabs'); goalsTab = button('Goals', () => { tab = 'goals'; showTab(); persist(); }); codeTab = button('Code', () => { tab = 'code'; showTab(); persist(); }); tabs.append(goalsTab, codeTab); page.append(tabs); content = node('div'); page.append(content); showTab();
      decisionForm = node('form', '', 'pvr-ratify'); decisionForm.addEventListener('submit', event => event.preventDefault()); decisionForm.append(node('h2', 'Combined goals and code decision'), paragraph(separated ? 'Acceptance ratifies the production changes and proposed requirements for fresh Modeling and verification. Harness files and audit evidence remain attached, not approved. Tests and retirement do not prove a goal.' : 'Acceptance ratifies this exact displayed package for fresh Modeling and verification. Tests and retirement do not prove a goal.'));
      if (!controlAvailable()) decisionForm.append(paragraph('Open the control URL to record a human decision.'));
      else {
        reason = node('textarea'); reason.name = 'reason'; reason.placeholder = 'Reason (optional for ratification)'; reason.maxLength = 20000; reason.value = saved.reason || ''; reason.setAttribute('aria-label', 'Reason (optional for ratification)'); reason.addEventListener('input', persist);
        amendment = node('textarea'); amendment.name = 'amendment'; amendment.placeholder = 'Requested changes (optional when code edits are included)'; amendment.maxLength = 20000; amendment.value = saved.amendment || ''; amendment.setAttribute('aria-label', 'Requested amendment'); amendment.addEventListener('input', persist);
        commentOnly = node('input'); commentOnly.type = 'checkbox'; commentOnly.name = 'comment_only'; commentOnly.checked = !!saved.commentOnly; commentOnly.addEventListener('change', persist);
        const commentLabel = node('label'); commentLabel.append(commentOnly, document.createTextNode(' Comment-only change'));
        decisionForm.append(commentLabel, paragraph('For exact edits to existing Rust line comments: focused review, with earlier checks retained rather than rerun. Other edits need a normal amendment.'));
        decisionForm.append(reason, amendment, button('Reset all code drafts', () => { codeDrafts = Object.create(null); persist(); updateDecision(); if (tab === 'code') renderCode(); })); status = node('p'); status.setAttribute('role', 'status');
        for (const [action, label] of [['accept', 'Ratify goals and code'], ['deny', 'Deny'], ['amend', 'Request amendment']]) { const submit = button(label, async () => {
          if (busy || submitted || latest?.digest !== loadedDigest || latest?.state !== 'awaiting_human') return;
          const codeEdits = edits();
          if (codeEdits.length && action !== 'amend') { status.textContent = 'Send or reset your code edits before making another decision.'; return; }
          if (action === 'accept' && pkg.scope_passed === false) { status.textContent = 'Scope review has not passed. Request an amendment.'; return; }
          if (action === 'amend' && commentOnly.checked && !codeEdits.length) { status.textContent = 'Edit the exact comment text before requesting comment-only review.'; return; }
          if (action === 'amend' && editError()) { status.textContent = editError(); return; }
          if (action !== 'accept' && !reason.value.trim()) { status.textContent = 'Enter a reason.'; reason.focus(); return; }
          if (action === 'amend' && !amendment.value.trim() && !codeEdits.length) { status.textContent = 'Describe the requested amendment.'; amendment.focus(); return; }
          busy = true; updateDecision(); status.textContent = 'Saving decision…';
          try {
            const response = await fetch(controlPath('pv-revision/decide'), { method: 'POST', headers: controlHeaders({ 'Content-Type': 'application/json' }), body: JSON.stringify({ action, digest: loadedDigest, reason: reason.value, amendment: amendment.value, ...(codeEdits.length ? { code_edits: codeEdits } : {}), ...(action === 'amend' && commentOnly.checked ? { comment_only: true } : {}) }) });
            const result = await response.json(); if (!response.ok) throw new Error(result.error || `Request failed (${response.status})`);
            submitted = true;
            status.textContent = result.scheduled ? 'Decision saved. Continuation is being scheduled.' : 'Decision saved.'; await refresh();
          } catch (error) { status.textContent = error.message; } finally { busy = false; updateDecision(); }
        }); submit.dataset.decision = action; decisionForm.append(submit); }
        decisionForm.append(status);
      }
      page.append(decisionForm); updateDecision(); page.scrollTop = saved.scroll || 0;
    }
    async function open(trigger) {
      openTrigger = trigger || openTrigger;
      if (!latest?.digest || loading) return;
      if (location.hash !== '#pv-review') { savedHash = location.hash; history.replaceState(null, '', location.pathname + location.search + '#pv-review'); }
      page.hidden = false;
      if (loadedDigest === latest.digest && pkg) return;
      const expected = latest.digest; persist(); loading = true; page.replaceChildren(button('← Back to run', close), paragraph('Validating the exact review package…'));
      try {
        const response = await fetch(apiPath(`pv-revision/review?digest=${expected}`)); const data = await response.json();
        if (!response.ok || data.digest !== expected || latest?.digest !== expected) throw new Error(data.error || 'Review package changed while loading; reopen it.');
        pkg = data; loadedDigest = expected; build();
      } catch (error) { page.replaceChildren(button('← Back to run', close), paragraph(error.message), button('Retry review', () => open())); }
      finally { loading = false; }
    }
    page.addEventListener('scroll', persist, { passive: true });
    window.addEventListener('hashchange', () => { if (location.hash === '#pv-review') open(); else if (!page.hidden) close(); });
    return { open, update(state) { latest = state; updateDecision(); if (location.hash === '#pv-review' && page.hidden) open(); } };
  };
})();
