/* Live sessions and task-centered transcript history. Uses the viewer's
 * existing renderer and project/auth URL prefix. */
(() => {
  const el = (tag, text = '', cls = '') => {
    const node = document.createElement(tag); node.textContent = text;
    if (cls) node.className = cls; return node;
  };
  const button = (text, action, cls = '') => {
    const node = el('button', text, cls); node.type = 'button';
    node.addEventListener('click', action); return node;
  };
  const style = el('style');
  style.textContent = `
    body.viewing-sessions .dag-pane,body.viewing-sessions .detail-resizer{display:none}
    body.viewing-sessions .detail-pane{width:100%!important;flex:1;max-width:none}
    body.viewing-sessions #pv-revision-panel{display:none}
    #session-view{max-width:1500px;margin:0 auto}
    #session-live-list{display:grid;grid-template-columns:repeat(auto-fit,minmax(min(100%,440px),1fr));gap:14px;align-items:start}
    .session-toolbar{display:flex;gap:10px;align-items:center;flex-wrap:wrap;margin:8px 0 14px}
    .session-toolbar label{display:flex;gap:5px;align-items:center;font-size:12px;color:#b9c9e8}
    .session-toolbar button,.session-history button,.session-history select,.session-history input,.session-card button{background:#20263c;color:#dce4fa;border:1px solid #414c70;border-radius:4px;padding:5px 8px;font:inherit;font-size:12px}
    .session-toolbar button,.session-history button,.session-card button{cursor:pointer}
    .session-muted{color:#a0acc8;font-size:11px;overflow-wrap:anywhere}
    .session-card{border:1px solid #384463;border-radius:6px;background:#14182d;margin-bottom:14px;overflow:hidden}
    .session-card header{display:block;padding:10px;background:#1a2237;height:auto;min-height:0}
    .session-card h3{color:#d8ecff;margin:0 0 5px;font-size:13px}
    .session-card header button{float:right;margin-left:8px}
    .session-transcript{padding:10px;max-height:42vh;min-height:60px;overflow:auto;overflow-wrap:anywhere}
    .session-transcript pre{white-space:pre-wrap;overflow-wrap:anywhere}
    .session-transcript.thoughts-only .chat-card-tool{display:none}
    .session-transcript .chat-thoughts-empty-state{display:none}
    .session-transcript.thoughts-only .chat-thoughts-empty-state.show{display:block}
    .session-history{display:grid;grid-template-columns:minmax(150px,32%) minmax(0,1fr);gap:12px}
    .session-history nav{max-height:65vh;overflow:auto;border-right:1px solid #384463;padding-right:10px}
    .session-history nav>button,.session-history nav details{display:block;width:100%;text-align:left;margin-bottom:6px}
    .session-history nav button[aria-current=true]{border-color:#8be9fd;background:#253551}
    .session-history summary{cursor:pointer;padding:6px;font-size:12px;color:#d8ecff}
    .session-history details button{display:block;width:calc(100% - 12px);margin:5px 0 5px 12px;text-align:left}
    .session-history nav .session-muted{display:block;margin-top:4px}
    .session-history .session-transcript{max-height:60vh;background:#101529;border:1px solid #384463;border-radius:5px}
    .session-filters{display:flex;gap:6px;flex-wrap:wrap;margin:8px 0}
    .session-filters input,.session-filters select{background:#20263c;color:#dce4fa;border:1px solid #414c70;border-radius:4px;padding:6px 8px;font:inherit;font-size:12px}.session-filters input{flex:1;min-width:100px}.session-filters select{max-width:100%}
    .session-history h3{font-size:13px;margin:6px 0;color:#d8ecff}
    .session-warning{color:#ffb86c;font-size:12px;white-space:pre-wrap}
    @media(max-width:650px){.session-history{grid-template-columns:1fr}.session-history nav{max-height:28vh;border-right:0;border-bottom:1px solid #384463}.session-history .session-transcript{max-height:55vh}}
  `;
  document.head.append(style);
  let generation = 0, timer = null, mode = null, controller = null;
  let shell, status, liveList, filters, navigation, historyMain, groups = [], selectedGroup = null, selectedCall = null;
  let search = '', category = '', model = '', visibleGroups = 50;
  const cards = new Map();
  const date = ms => ms ? new Date(ms).toLocaleString() : '';
  const duration = row => row.started_at_ms && row.ended_at_ms
    ? `${Math.max(0, Math.round((row.ended_at_ms - row.started_at_ms) / 60000))} min` : '';
  const meta = row => [row.role, row.model, row.outcome, date(row.started_at_ms), duration(row)].filter(Boolean).join(' · ');
  function stop() {
    clearTimeout(timer); timer = null; generation++;
    controller?.abort(); controller = null; mode = null;
    document.body.classList.remove('viewing-sessions');
  }
  async function get(query, signal) {
    markInteractiveActivity(); chatRefreshInFlight++;
    try {
      const response = await fetch(apiPath(`sessions.json?${new URLSearchParams(query)}`), { signal });
      const result = await response.json();
      if (!response.ok || result.error) throw new Error(result.error || `HTTP ${response.status}`);
      return result;
    } finally { chatRefreshInFlight = Math.max(0, chatRefreshInFlight - 1); }
  }
  function thoughtsToolbar(parent) {
    const toolbar = el('div', '', 'session-toolbar');
    const label = el('label'), checkbox = el('input');
    checkbox.type = 'checkbox'; checkbox.checked = chatThoughtsOnlyMode;
    checkbox.addEventListener('change', () => {
      chatThoughtsOnlyMode = checkbox.checked;
      for (const transcript of shell.querySelectorAll('.session-transcript')) applyChatThoughtsOnlyClass(transcript);
    });
    label.append(checkbox, document.createTextNode('Thoughts only'));
    status = el('span', '', 'session-muted'); status.setAttribute('role', 'status');
    toolbar.append(label, button('Refresh', () => refresh()), status); parent.append(toolbar);
  }
  function render(view) {
    const content = document.getElementById('detail-content');
    if (mode === view && shell?.parentNode === content) return;
    stop(); mode = view; cards.clear();
    document.body.classList.add('viewing-sessions');
    shell = el('section'); shell.id = 'session-view';
    content.replaceChildren(shell);
    shell.append(el('h2', view === 'live' ? 'Live agents' : 'Transcripts'));
    shell.append(el('p', view === 'live'
      ? 'All currently running agent sessions. Each transcript follows its own session and refreshes automatically.'
      : 'Browse tasks and their attempts, reviewer and verifier sessions, preparation, revisions, or archived cycles.', 'session-muted'));
    thoughtsToolbar(shell);
    if (view === 'live') { liveList = el('div'); liveList.id = 'session-live-list'; shell.append(liveList); }
    else {
      filters = el('div', '', 'session-filters'); shell.append(filters);
      const layout = el('div', '', 'session-history'); navigation = el('nav');
      navigation.setAttribute('aria-label', 'Transcript history');
      historyMain = el('div'); layout.append(navigation, historyMain); shell.append(layout);
      buildFilters();
    }
    refresh();
  }
  function buildFilters() {
    const input = el('input'); input.type = 'search'; input.placeholder = 'Search task, node, role or model';
    input.setAttribute('aria-label', 'Search transcript history'); input.value = search;
    input.addEventListener('input', () => { search = input.value; visibleGroups = 50; renderNavigation(); });
    const categorySelect = el('select'); categorySelect.setAttribute('aria-label', 'Filter session role');
    const options = [['', 'All roles'], ...[...new Set(groups.map(g => g.category))].map(value => [value, value])];
    for (const [value, label] of options) { const option = el('option', label); option.value = value; categorySelect.append(option); }
    categorySelect.value = category;
    categorySelect.addEventListener('change', () => { category = categorySelect.value; visibleGroups = 50; renderNavigation(); });
    const models = el('select'); models.setAttribute('aria-label', 'Filter model');
    for (const value of ['', ...[...new Set(groups.flatMap(g => g.attempts.flatMap(a => a.models || [a.model])).filter(Boolean))].sort()]) {
      const option = el('option', value || 'All models'); option.value = value; models.append(option);
    }
    models.value = model;
    models.addEventListener('change', () => { model = models.value; visibleGroups = 50; renderNavigation(); });
    filters.replaceChildren(input, categorySelect, models);
  }
  function renderNavigation() {
    const terms = search.toLowerCase().split(/\s+/).filter(Boolean);
    const matching = groups.filter(group => (!category || group.category === category)
      && (!model || group.attempts.some(a => (a.models || [a.model]).includes(model)))
      && terms.every(term => JSON.stringify(group).toLowerCase().includes(term)));
    navigation.replaceChildren();
    const current = group => group.attempts.some(attempt => attempt.id === selectedGroup);
    for (const group of matching.slice(0, visibleGroups)) {
      const attemptButton = attempt => {
        const b = button(attempt.label, () => { selectedGroup = attempt.id; selectedCall = null; renderNavigation(); refresh(false); });
        b.setAttribute('aria-current', String(selectedGroup === attempt.id));
        b.append(el('span', [attempt.model, attempt.outcome, duration(attempt)].filter(Boolean).join(' · '), 'session-muted'));
        b.title = meta(attempt);
        if (attempt.nodes?.length) b.append(el('span', attempt.nodes.join(', '), 'session-muted'));
        return b;
      };
      if (group.attempts.length === 1) navigation.append(attemptButton(group.attempts[0]));
      else {
        const details = el('details'); details.open = current(group);
        details.append(el('summary', `${group.label} · ${group.attempts.length} attempts`));
        for (const attempt of group.attempts) details.append(attemptButton(attempt));
        navigation.append(details);
      }
    }
    if (!matching.length) navigation.append(el('p', 'No matching transcripts.', 'session-muted'));
    if (matching.length > visibleGroups) navigation.append(button(`Show more (${matching.length - visibleGroups} remaining)`, () => { visibleGroups += 50; renderNavigation(); }));
  }
  function transcriptData(container, result, follow) {
    const pinned = !container.dataset.rendered || container.scrollTop + container.clientHeight >= container.scrollHeight - 12;
    // Avoid replacing the DOM (and text selections) while nothing changes.
    const signature = JSON.stringify([result.events, result.missing, result.note]);
    if (container._signature !== signature) {
      if (result.missing || !result.events?.length) {
        container.replaceChildren(el('p', result.missing ? 'No transcript has been written for this session yet.' : 'Waiting for transcript events.', 'session-muted'));
        if (result.prompt_excerpt) {
          const prompt = el('details'); prompt.append(el('summary', 'Prompt'), el('pre', result.prompt_excerpt)); container.append(prompt);
        }
      } else renderChatEvents(container, result.events, {});
      if (result.note) container.prepend(el('p', result.note, 'session-warning'));
      container._signature = signature;
    }
    applyChatThoughtsOnlyClass(container); container.dataset.rendered = '1';
    if (follow && pinned) container.scrollTop = container.scrollHeight;
  }
  async function refreshLive(seq, signal) {
    const result = await get({}, signal);
    if (seq !== generation) return;
    const keys = new Set(result.calls.map(call => call.key));
    for (const [key, card] of cards) if (!keys.has(key)) { card.node.remove(); cards.delete(key); }
    liveList.querySelector('[data-empty]')?.remove();
    if (!result.calls.length) {
      const empty = el('p', 'No agent sessions are currently running. Completed sessions are available in Transcripts.', 'session-muted');
      empty.dataset.empty = '1'; liveList.append(empty);
    }
    status.textContent = `${result.calls.length} live session${result.calls.length === 1 ? '' : 's'} · updated ${new Date().toLocaleTimeString()}`;
    shell.querySelector('.session-warning[data-index-warning]')?.remove();
    if (result.warnings?.length) {
      const warning = el('p', result.warnings.join('\n'), 'session-warning'); warning.dataset.indexWarning = '1'; liveList.before(warning);
    }
    await Promise.all(result.calls.map(async call => {
      let card = cards.get(call.key);
      if (!card) {
        const node = el('article', '', 'session-card'), header = el('header');
        const transcript = el('div', '', 'session-transcript');
        transcript.append(el('p', 'Loading transcript…', 'session-muted'));
        header.append(button('History', () => openHistory(call.group)), el('h3', `${call.group_label} · ${call.role || call.kind || 'Agent'}${call.lane ? ` · ${call.lane}` : ''}`),
          el('div', [call.model || call.provider, call.active_node, call.request_id != null ? `Request ${call.request_id}` : '', call.session_id ? `Session ${call.session_id}` : ''].filter(Boolean).join(' · '), 'session-muted'));
        node.append(header, transcript); liveList.append(node); card = { node, transcript }; cards.set(call.key, card);
      }
      try {
        const data = await get({ group: call.group, session: call.key }, signal);
        if (seq === generation) transcriptData(card.transcript, data, true);
      } catch (error) {
        if (seq === generation && error.name !== 'AbortError') card.transcript.replaceChildren(el('p', error.message, 'session-warning'));
      }
    }));
  }
  async function refreshHistory(seq, signal, reloadGroups) {
    if (reloadGroups || !groups.length) {
      const result = await get({ view: 'history' }, signal);
      if (seq !== generation) return;
      groups = result.groups || [];
      if (!selectedGroup) selectedGroup = (groups.find(g => !category || g.category === category) || groups[0])?.attempts[0]?.id || 'cycle:live';
      buildFilters(); renderNavigation();
    }
    const identity = selectedGroup;
    const row = groups.flatMap(g => g.attempts).find(a => a.id === identity);
    const result = await get({ group: identity }, signal);
    if (seq !== generation) return;
    status.textContent = result.provenance || '';
    const calls = result.calls || [];
    if (!calls.some(call => call.key === selectedCall)) selectedCall = calls[calls.length - 1]?.key;
    historyMain.replaceChildren(el('h3', row?.label || identity), el('p', row ? meta(row) : '', 'session-muted'));
    if (row?.retry_of != null) historyMain.append(el('p', `Retry of task #${row.retry_of}`, 'session-muted'));
    if (row?.nodes?.length) historyMain.append(el('p', row.nodes.join(', '), 'session-muted'));
    if (row?.assignment_review != null) historyMain.append(button(`Assignment reviewer #${row.assignment_review}`, () => openHistory(`attempt:review-${row.assignment_review}`)));
    if (row?.report_summary || row?.rejection_reasons?.length) {
      const report = el('details'); report.append(el('summary', 'Task outcome'), el('p', row.report_summary || ''));
      for (const reason of row.rejection_reasons || []) report.append(el('p', typeof reason === 'string' ? reason : JSON.stringify(reason)));
      historyMain.append(report);
    }
    if (Number.isFinite(row?.started_at_ms) && Number.isFinite(row?.ended_at_ms)) {
      const overlapping = groups.flatMap(group => group.attempts).filter(other => other.id !== identity
        && Number.isFinite(other.started_at_ms) && Number.isFinite(other.ended_at_ms)
        && other.started_at_ms < row.ended_at_ms && other.ended_at_ms > row.started_at_ms);
      if (overlapping.length) {
        const parallel = el('details'); parallel.append(el('summary', `Overlapping attempts (${overlapping.length})`),
          el('p', 'Other activity whose recorded start and finish times overlap this attempt.', 'session-muted'));
        for (const other of overlapping.slice(0, 10)) {
          const link = button(other.label, () => openHistory(other.id)); link.title = meta(other); parallel.append(link);
        }
        if (overlapping.length > 10) parallel.append(el('p', `${overlapping.length - 10} more attempts overlap; browse them in the history list.`, 'session-muted'));
        historyMain.append(parallel);
      }
    }
    if (!calls.length) { historyMain.append(el('p', 'No transcript was retained for this selection.', 'session-muted')); return; }
    if (calls.length > 1) {
      const label = el('label', 'Session: '), select = el('select'); select.setAttribute('aria-label', 'Session within selected history');
      for (const call of calls) {
        const option = el('option', [call.role || call.kind, call.request_id != null ? `#${call.request_id}` : '', call.lane, call.model].filter(Boolean).join(' · '));
        option.value = call.key; select.append(option);
      }
      select.value = selectedCall; select.addEventListener('change', () => { selectedCall = select.value; refresh(false); });
      label.append(select); historyMain.append(label);
    }
    const transcript = el('div', '', 'session-transcript'); transcript.dataset.group = identity;
    transcript.append(el('p', 'Loading transcript…', 'session-muted')); historyMain.append(transcript);
    const data = await get({ group: identity, session: selectedCall }, signal);
    if (seq === generation) transcriptData(transcript, data, false);
  }
  async function refresh(reloadGroups = true) {
    clearTimeout(timer); controller?.abort(); controller = new AbortController();
    const signal = controller.signal, seq = ++generation;
    if (!mode) return;
    try {
      if (mode === 'live') await refreshLive(seq, signal);
      else await refreshHistory(seq, signal, reloadGroups);
    } catch (error) {
      if (seq === generation && error.name !== 'AbortError') status.textContent = `Could not refresh: ${error.message}`;
    } finally {
      if (seq === generation && mode === 'live') timer = setTimeout(() => refresh(), 3000);
    }
  }
  function openHistory(group) {
    selectedGroup = group === 'revision' ? null : group; selectedCall = null; category = group === 'revision' ? 'Revision' : ''; model = ''; search = '';
    const alreadyOpen = mode === 'history';
    setDetailMode('chat'); setMobilePane('detail');
    if (alreadyOpen) refresh();
  }
  function openLive() { setDetailMode('live'); setMobilePane('detail'); }
  window.TrellisSessions = { render, stop, openHistory, openLive };
  if (location.hash === '#live-agents') openLive();
})();
