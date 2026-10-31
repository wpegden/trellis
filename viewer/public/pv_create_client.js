'use strict';

(() => {
  const flowName = 'prose-program-verification';

  const uploadRule = 'Upload the upstream source, unmodified: the module or crate you want verified, plus any sibling files in the same crate that its `use` lines import. Never edit, never add a manifest.';
  const looseSupportSchema = 'trellis-pv-supporting-loose-upload/v1';

  async function upload(file, kind, query) {
    const suffix = Object.entries(query || {})
      .map(([key, value]) => `&${encodeURIComponent(key)}=${encodeURIComponent(value)}`).join('');
    const response = await fetch(apiPath(`control/uploads?kind=${encodeURIComponent(kind)}${suffix}`), {
      method: 'POST',
      headers: { ...controlHeaders(false), 'Content-Type': 'application/octet-stream' },
      body: await file.arrayBuffer(),
    });
    let data = {};
    try { data = await response.json(); } catch {}
    return { ok: response.ok, status: response.status, data };
  }

  async function looseSupportingBundle(files) {
    const rows = await Promise.all([...files].map(async (file) => ({
      name: file.webkitRelativePath || file.name,
      bytes: new Uint8Array(await file.arrayBuffer()),
    })));
    rows.sort((left, right) => new TextEncoder().encode(left.name).toString()
      .localeCompare(new TextEncoder().encode(right.name).toString()));
    const header = new TextEncoder().encode(`${JSON.stringify({
      schema: looseSupportSchema,
      files: rows.map(row => ({ name: row.name, byte_length: row.bytes.length })),
    })}\n`);
    return new Blob([header, ...rows.map(row => row.bytes)], { type: 'application/octet-stream' });
  }

  async function action(slug, name, body) {
    const result = await controlPost(`pv-create-jobs/${slug}/${name}`, body || {});
    if (!result.ok) wizardError(result.data.detail || result.data.error || `HTTP ${result.status}`);
    return result;
  }

  async function remove(job) {
    const slug = job.slug;
    const first = await action(slug, 'delete', {});
    if (first.status !== 409 || !Array.isArray(first.data.paths)) return;
    const message = `Delete failed creation "${slug}"?\n\nThis permanently removes:\n\n`
      + first.data.paths.join('\n');
    if (!window.confirm(message)) return;
    const second = await action(slug, 'delete', { paths: first.data.paths });
    if (!second.ok) return;
    if (wizard.slug === slug) wizardClose();
    refresh();
  }

  function renderStatus(payload) {
    const panel = wizardHeader(`Creating prose verification run: ${payload.slug}`);
    const state = payload.effective_state;
    panel.appendChild(el('div', 'wstage',
      `${state}${payload.stage && payload.stage !== state ? ` — stage: ${payload.stage}` : ''}`));
    const settingsSummary = renderRunSettingsSummary(payload.settings);
    if (settingsSummary) panel.appendChild(settingsSummary);
    if (payload.error) panel.appendChild(el('div', 'err', payload.error));
    if (payload.log && payload.log.notice) panel.appendChild(el('div', 'hint', payload.log.notice));
    const log = el('pre', 'wlog', (payload.log && payload.log.lines || []).join('\n'));
    panel.appendChild(log);

    if (state === 'done') {
      panel.appendChild(el('div', 'verdict ok',
        payload.goal_format === 'native' ? 'The native modeling campaign is initialized and running.'
          : 'The campaign passed bootstrap, initialized from the trust-bearing config, and is running.'));
      const link = document.createElement('a');
      link.className = 'run-link';
      link.href = projectHref(payload.slug);
      link.textContent = `open the ${payload.slug} run viewer →`;
      panel.appendChild(link);
      if (wizard.timer) { clearInterval(wizard.timer); wizard.timer = null; }
      refresh();
      return;
    }

    if (payload.goal_format === 'native' && payload.viewer_available && state !== 'done') {
      if (payload.preparation) panel.appendChild(el('div', 'hint',
        `Preparation: ${payload.preparation.stage}; ${payload.preparation.status}; request ${payload.preparation.sequence} (${payload.preparation.role || 'handoff'})`));
      const link = el('a', null, 'open preparation and agent activity →');
      link.href = projectHref(payload.slug);
      panel.appendChild(link);
    }
    if (payload.bucket === 'creating') {
      const controls = el('div', 'row');
      const stop = el('button', 'danger', 'stop this creation');
      stop.onclick = async () => {
        if (!window.confirm(`Stop creating "${payload.slug}"?\n\n`
          + 'This creation job and its active preparation agent will stop. Its files remain available for inspection.')) return;
        stop.disabled = true;
        await action(payload.slug, 'stop', {});
        if (wizard.poll) wizard.poll();
      };
      controls.appendChild(stop);
      panel.appendChild(controls);
    }

    if (payload.bucket === 'create_failed') {
      const controls = el('div', 'row');
      if (payload.retry_eligible) {
        const retry = el('button', 'primary', payload.retry_mode === 'resume-preparation'
          ? 'resume preparation' : 'retry from uploads');
        retry.onclick = async () => {
          retry.disabled = true;
          const result = await action(payload.slug, 'retry', {});
          if (result.ok) open({ slug: payload.slug });
        };
        controls.appendChild(retry);
      } else {
        controls.appendChild(el('span', 'warn',
          payload.retry_blocked_reason || (payload.retry_mode === 'resume-preparation'
            ? 'Preparation is preserved. Resume is unavailable while the campaign is running.'
            : 'Fresh-only boundary: once campaign construction creates an output, retry is disabled. Delete and use a new slug.')));
      }
      const del = el('button', 'danger', 'delete failed creation');
      del.onclick = () => remove(payload);
      controls.appendChild(del);
      panel.appendChild(controls);
    }
  }

  function open(job) {
    const slug = job.slug;
    wizard.slug = slug;
    wizard.lastState = null;
    wizardHeader(`Creating prose verification run: ${slug}`);
    if (wizard.timer) clearInterval(wizard.timer);
    const poll = async () => {
      if (wizard.slug !== slug) return;
      const result = await controlGet(`pv-create-status/${slug}`);
      if (wizard.slug !== slug) return;
      if (!result.ok) {
        wizardError(result.data.detail || result.data.error || `HTTP ${result.status}`);
        return;
      }
      renderStatus(result.data);
    };
    wizard.poll = poll;
    poll();
    wizard.timer = setInterval(poll, 2000);
  }

  async function retry(job) {
    const result = await action(job.slug, 'retry', {});
    if (result.ok) open(job);
  }

  async function openNew() {
    if (wizard.timer) { clearInterval(wizard.timer); wizard.timer = null; }
    wizard.slug = null;
    const panel = wizardHeader('New prose verification run');
    panel.appendChild(el('div', 'hint', uploadRule));
    panel.appendChild(el('div', 'hint',
      'The GOAL document and source uploads are frozen as run inputs. The run cannot fetch missing files or references.'));

    let settingsMeta = { roles: [] };
    try {
      const response = await controlGet('pv-create-jobs.json');
      if (response.ok) settingsMeta = response.data;
    } catch {}
    if (!wizard.options) {
      try {
        const response = await controlGet('create-options.json');
        wizard.options = response.ok ? response.data : { models: [], efforts: [] };
      } catch {
        wizard.options = { models: [], efforts: [] };
      }
    }

    const slugRow = el('div', 'row');
    slugRow.appendChild(el('label', null, 'name'));
    const slugInput = el('input');
    slugInput.type = 'text';
    slugInput.placeholder = 'e.g. parser_proof';
    slugRow.appendChild(slugInput);
    slugRow.appendChild(el('span', 'hint', 'must be fresh; campaign output is never reused'));
    panel.appendChild(slugRow);

    const formatRow = el('div', 'row');
    formatRow.appendChild(el('label', null, 'goal format'));
    const formatInput = el('select');
    for (const [value, label] of [['native', 'Independent goals (GOAL.json)'], ['callable', 'Function contracts (GOAL.md)']]) {
      const option = el('option', null, label); option.value = value; formatInput.appendChild(option);
    }
    formatRow.appendChild(formatInput);
    panel.appendChild(formatRow);
    const goalRow = el('div', 'row');
    const goalLabel = el('label', null, 'GOAL.json');
    goalRow.appendChild(goalLabel);
    const goalInput = el('input');
    goalInput.type = 'file';
    goalInput.accept = '.json,application/json';
    goalRow.appendChild(goalInput);
    const goalHint = el('span', 'hint', 'JSON with shared context and a goals list; one result per goal ID');
    goalRow.appendChild(goalHint);
    panel.appendChild(goalRow);

    const targetRow = el('div', 'row');
    targetRow.appendChild(el('label', null, 'target source'));
    const targetInput = el('input');
    targetInput.type = 'file';
    targetInput.accept = '.tar.gz,application/gzip';
    targetRow.appendChild(targetInput);
    targetRow.appendChild(el('span', 'hint', 'required .tar.gz of the upstream module directory or crate'));
    panel.appendChild(targetRow);

    const supportRow = el('div', 'row');
    supportRow.appendChild(el('label', null, 'supporting upstream files'));
    const supportInput = el('input');
    supportInput.type = 'file';
    supportInput.multiple = true;
    supportInput.accept = '.tar.gz,.rs,application/gzip,text/plain';
    supportRow.appendChild(supportInput);
    supportRow.appendChild(el('span', 'hint', 'optional: one .tar.gz, or one or more loose sibling files; uploads must be unmodified'));
    panel.appendChild(supportRow);

    const importHint = el('div', 'hint', 'Select the target upload to scan its imports.');
    supportRow.appendChild(importHint);
    let targetUpload = null;
    let supportUpload = null;

    const showImportHint = (missing) => {
      importHint.replaceChildren();
      if (!missing || !missing.length) {
        importHint.textContent = 'No unresolved imported paths were found by the upload helper.';
        return;
      }
      importHint.appendChild(document.createTextNode(
        'The upload helper could not resolve these imports. You can add supporting upstream files, or let the adapting agent write shims for them:',
      ));
      const list = document.createElement('ul');
      for (const item of missing) list.appendChild(el('li', null, `${item.path} — imported in ${item.source_file}`));
      importHint.appendChild(list);
    };

    const uploadTarget = async () => {
      const file = targetInput.files && targetInput.files[0];
      if (!file) throw new Error('pick a .tar.gz target source upload');
      if (targetUpload && targetUpload.file === file) return targetUpload.result;
      supportUpload = null;
      const result = await upload(file, 'target');
      if (!result.ok) throw new Error(result.data.detail || result.data.error || `target upload failed (${result.status})`);
      targetUpload = { file, result };
      showImportHint(result.data.missing_imports);
      return result;
    };

    const uploadSupport = async () => {
      const files = [...(supportInput.files || [])];
      if (!files.length) { supportUpload = null; return null; }
      const target = await uploadTarget();
      if (supportUpload && supportUpload.files.length === files.length
          && supportUpload.files.every((file, index) => file === files[index])) return supportUpload.result;
      const archives = files.filter(file => file.name.toLowerCase().endsWith('.tar.gz'));
      if (archives.length && (files.length !== 1 || archives.length !== 1)) {
        throw new Error('supporting uploads must be one .tar.gz or loose files, not a mixture');
      }
      const body = archives.length ? archives[0] : await looseSupportingBundle(files);
      const result = await upload(body, 'supporting', {
        target_upload_id: target.data.upload_id,
        format: archives.length ? 'archive' : 'loose',
      });
      if (!result.ok) throw new Error(result.data.detail || result.data.error || `supporting upload failed (${result.status})`);
      supportUpload = { files, result };
      showImportHint(result.data.missing_imports);
      return result;
    };

    targetInput.onchange = async () => {
      targetUpload = null;
      supportUpload = null;
      try { await uploadTarget(); } catch (error) { wizardError(error.message); }
    };
    supportInput.onchange = async () => {
      supportUpload = null;
      try { await uploadSupport(); } catch (error) { wizardError(error.message); }
    };

    const settingsHost = el('div');
    panel.appendChild(settingsHost);
    let settings;
    const selectFormat = () => {
      const native = formatInput.value === 'native';
      goalLabel.textContent = native ? 'GOAL.json' : 'GOAL.md';
      goalInput.accept = native ? '.json,application/json' : '.md,text/markdown,text/plain';
      goalHint.textContent = native ? 'JSON with shared context and a goals list; one result per goal ID'
        : 'UTF-8 Markdown; one contract per exact level-three function heading';
      settings = createRunSettingsComponent(native ? (settingsMeta.native || { roles: [] }) : settingsMeta, {
        create_options: wizard.options,
      });
      settingsHost.replaceChildren(settings.element);
    };
    formatInput.onchange = selectFormat;
    selectFormat();

    const goRow = el('div', 'row');
    const go = el('button', 'primary', 'upload & launch');
    go.onclick = async () => {
      const slug = slugInput.value.trim();
      const goalFile = goalInput.files && goalInput.files[0];
      if (!slug) { wizardError('pick a name'); return; }
      if (!goalFile) { wizardError(`pick ${goalLabel.textContent}`); return; }
      if (!(targetInput.files && targetInput.files[0])) { wizardError('pick a .tar.gz target source upload'); return; }
      let selectedSettings;
      try {
        selectedSettings = settings.collect();
      } catch (error) {
        wizardError(error.message);
        return;
      }
      go.disabled = true;
      go.textContent = 'validating uploads…';
      try {
        const [goalUpload, resolvedTarget] = await Promise.all([upload(goalFile, 'goal', { format: formatInput.value }), uploadTarget()]);
        if (!goalUpload.ok) {
          wizardError(goalUpload.data.detail || goalUpload.data.error || `GOAL upload failed (${goalUpload.status})`);
          return;
        }
        const resolvedSupport = await uploadSupport();
        const start = await controlPost('pv-create-jobs', {
          slug,
          goal_format: formatInput.value,
          goal_upload_id: goalUpload.data.upload_id,
          target_upload_id: resolvedTarget.data.upload_id,
          support_upload_id: resolvedSupport ? resolvedSupport.data.upload_id : null,
          goal_name: goalFile.name,
          target_name: targetInput.files[0].name,
          support_name: resolvedSupport
            ? [...supportInput.files].map(file => file.name).join(', ') : null,
          ...selectedSettings,
        });
        if (!start.ok) {
          wizardError(start.data.detail || start.data.error || `create failed (${start.status})`);
          return;
        }
        open({ slug });
      } finally {
        go.disabled = false;
        go.textContent = 'upload & launch';
      }
    };
    goRow.appendChild(go);
    panel.appendChild(goRow);
  }

  registerCreateFlow(flowName, { open, retry, delete: remove });
  if (window.__TRELLIS_CONTROL__) {
    const mathButton = document.getElementById('new-run-btn');
    const button = el('button', null, '+ new prose verification run');
    button.onclick = openNew;
    mathButton.parentNode.insertBefore(button, mathButton.nextSibling);
  }
})();
