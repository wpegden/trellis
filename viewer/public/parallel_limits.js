/* Live worker admission limits; existing work keeps its slots. */
(() => {
  const header = document.querySelector('body > header');
  if (!header) return;
  const style = document.createElement('style');
  style.textContent = `
    body > header{flex-wrap:wrap}
    #parallel-limits{flex-basis:100%;display:flex;gap:12px 24px;align-items:center;flex-wrap:wrap;font-size:12px;color:#bdc9df}
    #parallel-limits[hidden]{display:none}
    .agent-limit{width:240px;display:grid;grid-template-columns:1fr auto;gap:2px 8px}
    .agent-limit label{color:#e0e0e0}.agent-limit output{font-variant-numeric:tabular-nums}
    .agent-limit-track{grid-column:1 / -1;position:relative;height:22px;margin:0 7px}
    .agent-limit-track input{width:calc(100% + 14px);margin:0 -7px;height:22px;position:relative;z-index:1;accent-color:#8be9fd;background:transparent;cursor:pointer}
    .agent-limit-current{position:absolute;left:var(--active);bottom:-2px;width:0;height:0;border-left:4px solid transparent;border-right:4px solid transparent;border-bottom:5px solid #f1fa8c;transform:translateX(-4px)}
    .agent-limit.over-limit output{color:#ffb86c}
    #agent-limits-note{flex:1;min-width:180px;font-size:11px;color:#9caac4}
    #agent-limits-message{flex-basis:100%;color:#ffb86c;font-size:12px}
    @media(max-width:600px){.agent-limit{width:100%}}
  `;
  document.head.append(style);
  const panel = document.createElement('div'); panel.id = 'parallel-limits'; panel.hidden = true;
  panel.innerHTML = `
    <div class="agent-limit" data-limit="workers"><label for="agent-limit-workers">Parallel agents</label><output></output>
      <div class="agent-limit-track"><input id="agent-limit-workers" type="range" min="1" max="16" step="1" aria-describedby="agent-limits-note"><span class="agent-limit-current" aria-hidden="true"></span></div></div>
    <div class="agent-limit" data-limit="heavy"><label for="agent-limit-heavy">Heavy agents</label><output></output>
      <div class="agent-limit-track"><input id="agent-limit-heavy" type="range" min="1" max="16" step="1" aria-describedby="agent-limits-note"><span class="agent-limit-current" aria-hidden="true"></span></div></div>
    <div id="agent-limits-note">Worker slots include checking and cleanup. △ marks current use. Lowering a limit leaves active work running. The reviewer and main audit are separate; scoped audits share the verifier queue.</div>
    <div id="agent-limits-message" role="status" hidden></div>`;
  header.append(panel);
  const message = panel.querySelector('#agent-limits-message');
  const note = panel.querySelector('#agent-limits-note'), baseNote = note.textContent;
  let saved = null, busy = false, editing = false, timer;
  const rows = [...panel.querySelectorAll('[data-limit]')];
  const notify = text => { message.textContent = text; message.hidden = !text; };
  function draw() {
    for (const row of rows) {
      const name = row.dataset.limit, input = row.querySelector('input');
      const active = name === 'workers' ? saved.active : saved.active_heavy;
      const limit = Number(input.value), maximum = Number(input.max);
      row.querySelector('output').textContent = `${active} active · limit ${limit}`;
      input.setAttribute('aria-valuetext', `Limit ${limit}; ${active} worker slots occupied`);
      row.querySelector('.agent-limit-track').style.setProperty('--active', `${100 * (Math.max(1, active) - 1) / (maximum - 1)}%`);
      row.querySelector('.agent-limit-current').hidden = active === 0;
      row.classList.toggle('over-limit', active > limit);
      input.disabled = busy || !controlAvailable() || !saved.configurable;
      input.title = !controlAvailable() ? 'Open the control-enabled viewer to change limits.'
        : !saved.configurable ? 'Configure lightweight true or false on every worker profile first.'
          : 'Changes apply to future dispatch. Active tasks keep running.';
    }
  }
  function show(limits) {
    saved = limits;
    note.textContent = baseNote + (limits.expected_heavy === null
      ? ' This run uses a legacy Sol cap. Saving enables the new heavy limit using configured model classes.' : '');
    panel.querySelector('[for="agent-limit-heavy"]').textContent = limits.expected_heavy === null ? 'Heavy agents (new limit)' : 'Heavy agents';
    for (const row of rows) {
      const input = row.querySelector('input');
      input.max = Math.max(16, limits.workers, limits.heavy, limits.active, limits.active_heavy);
      input.value = limits[row.dataset.limit];
    }
    draw();
  }
  async function refresh(force = false) {
    clearTimeout(timer);
    try {
      if (busy || editing || document.hidden
          || (!force && typeof autoRefreshEnabled !== 'undefined' && !autoRefreshEnabled)) return;
      const response = await fetch(apiPath('parallel.json'));
      if (!response.ok) throw new Error('Agent counts are temporarily unavailable.');
      const data = await response.json();
      if (busy || editing) return;
      panel.hidden = !data.enabled;
      if (data.enabled) show(data.agent_limits);
    } catch (error) { if (!panel.hidden) notify(error.message); }
    finally { timer = setTimeout(() => refresh(), 5000); }
  }
  async function save() {
    if (busy || !saved) return;
    editing = false;
    const requested = Object.fromEntries(rows.map(row => [row.dataset.limit, Number(row.querySelector('input').value)]));
    if (saved.expected_heavy !== null && requested.workers === saved.workers && requested.heavy === saved.heavy) { draw(); return; }
    busy = true; draw(); notify('Saving limits…');
    try {
      const response = await fetch(apiPath('control/parallel-limits'), {
        method: 'POST', headers: controlHeaders({'Content-Type': 'application/json'}),
        body: JSON.stringify({expected_worker_slots: saved.workers, expected_heavy_worker_slots: saved.expected_heavy,
          worker_slots: requested.workers, heavy_worker_slots: requested.heavy}),
      });
      const result = await response.json();
      if (!response.ok || result.error) throw new Error(result.error || 'Could not save limits.');
      notify('Saved. Active work continues; new dispatch uses these limits.');
    } catch (error) { notify(error.message); }
    finally { busy = false; await refresh(true); }
  }
  for (const row of rows) {
    const input = row.querySelector('input');
    input.addEventListener('input', () => { editing = true; draw(); });
    input.addEventListener('change', save);
  }
  document.getElementById('manual-refresh-btn')?.addEventListener('click', () => refresh(true));
  document.getElementById('auto-refresh-checkbox')?.addEventListener('change', event => {
    if (event.target.checked) refresh(true);
  });
  refresh(true);
})();
