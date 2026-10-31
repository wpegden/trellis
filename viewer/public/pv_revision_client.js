/* Optional project-level PV remediation controls; uses viewer auth helpers. */
(() => {
  const panel = document.createElement('section');
  panel.id = 'pv-revision-panel';
  panel.hidden = true;
  panel.style.cssText = 'margin:12px;padding:16px;border:1px solid #8572bc;border-radius:6px;background:#20202d;color:#eee;max-height:55vh;overflow:auto;flex-shrink:0';
  const header = document.querySelector('header');
  header?.insertAdjacentElement('afterend', panel);
  const history = document.createElement('section');
  history.id = 'pv-history-panel';
  history.hidden = true;
  history.style.cssText = panel.style.cssText;
  header?.insertAdjacentElement('afterend', history);
  const historyToggle = document.createElement('button');
  historyToggle.id = 'pv-history-toggle';
  historyToggle.type = 'button';
  historyToggle.textContent = 'Verification history';
  historyToggle.hidden = true;
  historyToggle.style.cssText = 'background:none;border:0;padding:0;color:#8be9fd;font:inherit;font-size:11px;text-decoration:underline;cursor:pointer;white-space:nowrap';
  historyToggle.setAttribute('aria-controls', history.id);
  historyToggle.setAttribute('aria-expanded', 'false');
  header?.querySelector('h1')?.insertAdjacentElement('afterend', historyToggle);
  let historyAvailable = false;
  let boundary = '';
  let historyChoice = null;
  function updateHistoryVisibility() {
    history.hidden = !historyAvailable || !(historyChoice ?? Boolean(boundary));
    historyToggle.hidden = !historyAvailable;
    historyToggle.setAttribute('aria-expanded', String(!history.hidden));
  }
  historyToggle.addEventListener('click', () => {
    historyChoice = history.hidden;
    updateHistoryVisibility();
  });
  const review = window.PVRevisionReview({ apiPath, controlPath, controlHeaders, controlAvailable, refresh });
  let latest = null;
  let shownKey = '';
  const node = (tag, text) => { const element = document.createElement(tag); element.textContent = text; return element; };
  const pretty = value => String(value || '').replace(/_/g, ' ');
  function render(state, campaign) {
    // Use the live gate, even while the timeline displays an older cycle.
    const live = typeof liveState === 'undefined' ? null : liveState;
    const nextBoundary = live?.attention_gate?.id === 'advance'
      ? `advance:${live.state?.phase}:${live.state?.cycle}`
      : ['ready', 'awaiting_human'].includes(state?.state)
        ? `revision:${state.state}:${state.digest || ''}`
        : ['complete', 'all_requirements_retired'].includes(campaign?.terminal_outcome)
          ? `final:${campaign.terminal_outcome}` : '';
    review.update(state);
    const key = JSON.stringify({ state, campaign, boundary: nextBoundary });
    if (key === shownKey) return;
    shownKey = key;
    latest = state;
    bannerAction();
    if (boundary !== nextBoundary) historyChoice = null;
    boundary = nextBoundary;
    historyAvailable = Boolean(campaign);
    updateHistoryVisibility();
    panel.replaceChildren();
    panel.hidden = !state;
    history.replaceChildren();
    if (campaign) {
      const close = node('button', 'Close');
      close.type = 'button';
      close.style.cssText = historyToggle.style.cssText + ';float:right';
      close.setAttribute('aria-label', 'Close verification history');
      close.addEventListener('click', () => {
        historyChoice = false;
        updateHistoryVisibility();
        historyToggle.focus();
      });
      history.append(close, node('h2', 'Verification history'));
      if (campaign.error) history.append(node('p', campaign.error));
      if (campaign.gaps?.length) history.append(node('p', 'The archive documents gaps in retained historical evidence.'));
      const final = campaign.can_export_final === true;
      const terminal = ['complete', 'all_requirements_retired'].includes(campaign.terminal_outcome);
      history.append(node('p', final
        ? 'The final archive retains each code version, requirements, proofs and disproofs, extraction history, model changes, and recorded decisions.'
        : terminal ? 'The run has ended, but its final archive needs attention. A history snapshot labels any missing or unverified evidence.'
          : 'This run is in progress. A history snapshot retains the available evidence and labels unfinished work.'));
      if (campaign.terminal_outcome === 'all_requirements_retired') history.append(node('p', 'All active requirements were retired by an approved revision. This outcome does not establish the original requirements.'));
      const download = node('a', final ? 'Download final verification archive' : 'Download history snapshot (incomplete)');
      download.href = apiPath(`pv-revision/campaign.zip${final ? '' : '?snapshot=1'}`);
      download.style.cssText = 'color:#a6d8ff';
      history.append(download);
      if (terminal && !final) {
        const retry = node('a', 'Retry final archive export');
        retry.href = apiPath('pv-revision/campaign.zip');
        retry.style.cssText = 'color:#a6d8ff;margin-left:16px';
        history.append(retry);
      }
    }
    if (!state) return;
    panel.append(node('h2', 'Program verification revision'));
    const sessions = node('p', 'Agent sessions: ');
    for (const [label, action] of [
      ['Live agents', () => window.TrellisSessions?.openLive()],
      ['Revision transcripts', () => window.TrellisSessions?.openHistory('revision')],
    ]) {
      const link = node('button', label); link.type = 'button';
      link.style.cssText = historyToggle.style.cssText + ';margin-right:16px';
      link.addEventListener('click', action); sessions.append(link);
    }
    panel.append(sessions);
    panel.append(node('p', `${pretty(state.state)} · ${pretty(state.phase)} · round ${state.round || 0} · ${state.role || 'human'} · ${state.attempt_count || 0} implementation attempts`));
    if (state.digest) {
      const open = node('button', 'Open goals and code review'); open.type = 'button';
      open.addEventListener('click', () => review.open(open)); panel.append(open);
    } else if (state.audit_summary) panel.append(node('p', state.audit_summary));
    if (state.feedback) panel.append(node('p', `Controller feedback: ${state.feedback}`));
    if (state.launch?.error) panel.append(node('p', `Continuation could not start: ${state.launch.error}. The human decision is saved; use the run resume control after resolving the launch issue.`));
    if (state.digest) {
      panel.append(node('p', `Review package: ${state.digest}`));
      const links = node('p', 'Download: ');
      for (const [format, label] of [['zip', 'complete review archive'], ['readme', 'review guide'], ['json', 'package JSON'], ['patch', 'source patch']]) {
        const link = node('a', label);
        link.href = apiPath(`pv-revision/package?format=${format}&digest=${encodeURIComponent(state.digest)}`);
        link.style.cssText = 'color:#a6d8ff;margin-right:16px';
        links.append(link);
      }
      panel.append(links);
    }
  }

  async function refresh() {
    try {
      const [response, campaignResponse] = await Promise.all([
        fetch(apiPath('pv-revision.json')), fetch(apiPath('pv-revision/campaign.json'))]);
      if (!response.ok) return;
      const data = await response.json();
      const campaign = campaignResponse.ok ? (await campaignResponse.json()).campaign : null;
      render(data.revision, campaign);
    } catch { /* Existing page and unsent decision remain available. */ }
  }
  function bannerAction() {
    for (const row of document.querySelectorAll('.attention-pv_revision')) {
      if (!latest?.digest || row.querySelector('[data-pv-review]')) continue;
      const open = node('button', 'Open goals and code review'); open.type = 'button'; open.dataset.pvReview = '1';
      open.addEventListener('click', () => review.open(open)); row.append(open);
    }
  }
  new MutationObserver(bannerAction).observe(document.querySelector('#attention-strip') || document.body, { childList: true, subtree: true });
  refresh();
  setInterval(() => { if (!document.hidden) refresh(); }, 15000);
})();
