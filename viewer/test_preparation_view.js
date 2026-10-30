// Preparation links and pause controls; live/history selection is covered by test_session_views.
const assert = require('assert');
const fs = require('fs');
const path = require('path');
const vm = require('vm');

function functionsFrom(file, names) {
  const html = fs.readFileSync(path.join(__dirname, 'public', file), 'utf8');
  return names.map(name => {
    const match = html.match(new RegExp(`\\n(?:async )?function ${name}\\([^)]*\\) \\{[\\s\\S]*?\\n\\}`));
    assert.ok(match, `missing ${file}:${name}`);
    return match[0];
  }).join('\n');
}

function element(tag, cls, text) {
  return { tag, className: cls, textContent: text || '', childNodes: [], dataset: {}, style: {},
    classList: { remove() {}, toggle() {} },
    appendChild(child) { this.childNodes.push(child); },
  };
}

async function main() {
  const context = vm.createContext({ document: {}, });
  context.document.createElement = element;
  context.PAUSE_THRESHOLD_LABELS = [];
  vm.runInContext(functionsFrom('index.html', ['buildBudgetPauseControl']), context);
  for (const [state, text] of [['preparing', 'preparation is active'],
    ['preparation_stopped', 'preparation is stopped'], ['down', 'supervisor is down']]) {
    context.pauseState = { state };
    const control = context.buildBudgetPauseControl();
    assert.ok(control.childNodes.some(e => e.textContent === text));
  }

  const landing = vm.createContext({ window: { __TRELLIS_CONTROL__: false }, el: element,
    projectHref: slug => `/trellis/${slug}/`, fmtAge: () => '', fmtDur: () => '' });
  vm.runInContext(functionsFrom('landing.html', ['renderCreateCard']), landing);
  const card = landing.renderCreateCard({ slug: 'parser', effective_state: 'resolving', bucket: 'creating',
    viewer_available: true, preparation: { stage: 'setup', sequence: 1, role: 'worker' } });
  const descendants = node => [node, ...node.childNodes.flatMap(descendants)];
  assert.ok(descendants(card).some(e => e.tag === 'a' && e.href === '/trellis/parser/'),
    'even a read-only creation card opens preparation');
  assert.ok(descendants(card).some(e => e.textContent.includes('setup · request 1 (worker)')));
  const unready = landing.renderCreateCard({ slug: 'pending', effective_state: 'claimed', bucket: 'creating' });
  assert.ok(!descendants(unready).some(e => e.tag === 'a'), 'no link before a viewable repository exists');
  console.log('preparation view tests passed');
}
main().catch(error => { console.error(error); process.exitCode = 1; });
