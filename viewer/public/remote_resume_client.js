'use strict';
(() => {
  async function openNew() {
    wizardClose();
    const result = await controlGet('create-jobs.json');
    if (!result.ok) { wizardError(result.data.detail || result.data.error); return; }
    const panel = wizardHeader('Resume run');
    panel.appendChild(el('div', 'hint', 'Continue from a published checkpoint in a tablet Git remote. The first resume rebuilds Lean outputs and may take hours. Recorded review decisions are retained.'));
    const inputs = {};
    for (const [key, label, placeholder] of [
      ['slug', 'New run name', 'my-resumed-run'], ['remote_url', 'Tablet Git remote', 'git@host:owner/tablet.git'],
      ['branch', 'Branch (optional)', 'remote default'], ['commit', 'Checkpoint (optional)', 'latest supported idle checkpoint'],
    ]) {
      const row = el('div', 'row');
      const fieldLabel = el('label', null, label); fieldLabel.htmlFor = `resume-${key}`;
      row.appendChild(fieldLabel);
      const input = el('input', 'wide-input'); input.type = 'text'; input.placeholder = placeholder;
      input.id = fieldLabel.htmlFor;
      inputs[key] = input; row.appendChild(input); panel.appendChild(row);
    }
    const recommended = result.data.lean_parallelism;
    const row = el('div', 'row');
    const leanLabel = el('label', null, 'Lean parallelism'); leanLabel.htmlFor = 'resume-lean-parallelism';
    row.appendChild(leanLabel);
    const parallelism = el('input', 'lean-parallelism'); parallelism.type = 'number'; parallelism.min = '1'; parallelism.step = '1';
    parallelism.id = leanLabel.htmlFor;
    parallelism.value = String(recommended.recommended);
    row.appendChild(parallelism);
    row.appendChild(el('span', 'hint', `Recommended for this host: ${recommended.recommended}. Lower this when sharing the machine.`));
    panel.appendChild(row);
    const handoff = el('input'); handoff.type = 'checkbox';
    const handoffRow = el('label', 'row checkbox-row'); handoffRow.appendChild(handoff);
    handoffRow.appendChild(el('span', null, 'I have stopped the previous writer. This run will continue mirroring to the same branch.'));
    panel.appendChild(handoffRow);
    const start = el('button', 'primary', 'Resume run');
    start.onclick = async () => {
      start.disabled = true;
      try {
        const body = Object.fromEntries(Object.entries(inputs).map(([key, input]) => [key, input.value.trim()]));
        Object.assign(body, { kind: 'resume', create_flow: 'resume', handoff_confirmed: handoff.checked,
          lean_parallelism: Number(parallelism.value) });
        const response = await controlPost('create-jobs', body);
        if (!response.ok) { wizardError(response.data.detail || response.data.error); return; }
        open({ slug: body.slug });
      } finally { start.disabled = false; }
    };
    const actions = el('div', 'row'); actions.appendChild(start); panel.appendChild(actions);
  }

  function render(payload) {
    const panel = wizardHeader(`Resuming: ${payload.slug}`);
    panel.appendChild(el('div', 'wstage', payload.stage || payload.effective_state));
    const selection = payload.resume_selection;
    if (selection) {
      panel.appendChild(el('div', 'hint', `Fetched tip ${selection.fetched_tip.slice(0, 12)} · selected checkpoint ${selection.selected_commit.slice(0, 12)} · cycle ${selection.cycle} · ${selection.stage}`));
      if (selection.skipped_commits) panel.appendChild(el('div', 'warn', `${selection.skipped_commits} later commits were skipped. In-flight and unpublished work beyond this checkpoint will not be resumed.`));
    }
    if (payload.error) panel.appendChild(el('div', 'err', payload.error));
    panel.appendChild(logBlock(payload));
    const actions = el('div', 'row');
    if (payload.effective_state === 'resume_awaiting_rewind' && selection) {
      const button = el('button', 'danger', 'Approve this earlier checkpoint');
      button.onclick = async () => {
        if (!window.confirm(`Continue from ${selection.selected_commit}?\n\nMirroring will replace the later remote branch history at ${selection.fetched_tip}. Confirm the previous writer is stopped.`)) return;
        await wizardAction(payload.slug, 'confirm-resume-rewind', {
          expected_tip: selection.fetched_tip, selected_commit: selection.selected_commit,
        });
      };
      actions.appendChild(button);
    }
    if (payload.retry_eligible) {
      const button = el('button', 'primary', 'Retry');
      button.onclick = () => wizardAction(payload.slug, 'retry', {});
      actions.appendChild(button);
    }
    if (actions.childElementCount) panel.appendChild(actions);
    if (payload.effective_state === 'done') {
      panel.appendChild(el('div', 'hint', payload.stage === 'held' ? 'Restored at its human hold.' : payload.stage === 'complete' ? 'Restored as completed.' : 'Resumed and verified.'));
      const link = el('a', 'run-link', 'Open run'); link.href = projectHref(payload.slug); panel.appendChild(link);
      if (wizard.timer) clearInterval(wizard.timer);
      wizard.timer = null;
      refresh();
    }
  }

  function open(job) {
    wizardClose();
    wizard.slug = job.slug;
    const poll = async () => {
      if (wizard.slug !== job.slug) return;
      const result = await controlGet(`create-status/${job.slug}`);
      if (wizard.slug !== job.slug) return;
      if (!result.ok || !result.data.exists) { wizardError(result.data.detail || 'Resume job unavailable.'); return; }
      render(result.data);
    };
    poll(); wizard.timer = setInterval(poll, 2000);
  }
  registerCreateFlow('resume', { open, retry: async job => { await wizardAction(job.slug, 'retry', {}); open(job); }, delete: job => wizardDelete(job.slug) });
  if (window.__TRELLIS_CONTROL__) {
    const button = el('button', null, 'Resume run'); button.onclick = openNew;
    const next = document.getElementById('new-run-btn'); next.parentNode.insertBefore(button, next.nextSibling);
  }
})();
