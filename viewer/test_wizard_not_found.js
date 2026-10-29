// The create wizard must not freeze when its job disappears.
//
// A job can vanish under an open wizard: deleted from another tab, or its
// directory removed on disk. `create-status` then answers `{exists:false}`
// with no state and no updated_ts, so the redraw guard's stateKey never
// changes. When the not-found branch sat *below* that guard, the panel
// rendered "job not found" once and then short-circuited every later poll —
// the wizard sat dead with no route back to the form, and the operator saw a
// create button that did nothing.
//
// These pages have no build step and no module system, so the function is
// lifted out of the HTML and evaluated against stubs, matching
// test_landing_index.js.

const assert = require('assert');
const fs = require('fs');
const path = require('path');

const html = fs.readFileSync(path.join(__dirname, 'public', 'landing.html'), 'utf-8');

// 1. Source order: the not-found branch must precede the redraw guard.
{
  const body = html.match(/\nfunction wizardRender\([^)]*\) \{[\s\S]*?\n\}/);
  assert.ok(body, 'landing.html no longer defines wizardRender()');
  const notFound = body[0].indexOf('payload.exists');
  const guard = body[0].indexOf('wizard.lastState === stateKey');
  assert.ok(notFound >= 0, 'wizardRender no longer checks payload.exists');
  assert.ok(guard >= 0, 'wizardRender no longer has the redraw guard');
  assert.ok(notFound < guard,
    'the exists:false branch must run BEFORE the redraw guard, or a missing '
    + 'job short-circuits every poll and the wizard freezes');
}

// 2. Behaviour: tolerate a few polls, then fall back to the form.
{
  const src = html.match(/\nfunction wizardRender\([^)]*\) \{[\s\S]*?\n\}/)[0];
  const pollsMatch = html.match(/const WIZARD_NOT_FOUND_POLLS = (\d+);/);
  assert.ok(pollsMatch, 'landing.html no longer defines WIZARD_NOT_FOUND_POLLS');
  const polls = Number(pollsMatch[1]);
  assert.ok(polls >= 2, 'the grace window must tolerate at least one poll');

  const calls = { openNew: 0, errors: [] };
  const wizard = { slug: 'gone', notFound: 0, lastState: null };
  // eslint-disable-next-line no-new-func
  const render = new Function(
    'wizard', 'WIZARD_NOT_FOUND_POLLS', 'wizardOpenNew', 'wizardError', 'wizardEl',
    `${src}\nreturn wizardRender;`,
  )(
    wizard,
    polls,
    async () => { calls.openNew += 1; },
    (msg) => { calls.errors.push(msg); },
    () => { throw new Error('wizardRender must not touch the DOM for a missing job'); },
  );

  const payload = { slug: 'gone', exists: false };
  for (let i = 1; i < polls; i += 1) {
    render(payload);
    assert.strictEqual(calls.openNew, 0,
      `poll ${i} must stay put: create-status briefly reports exists:false `
      + 'between the create POST returning and the job directory appearing');
    assert.strictEqual(wizard.notFound, i);
  }

  render(payload);
  assert.strictEqual(calls.openNew, 1, 'the wizard must return to the form');

  return new Promise((resolve) => setImmediate(() => {
    assert.strictEqual(calls.errors.length, 1, 'the operator must be told why');
    assert.ok(calls.errors[0].includes('gone'), 'the message must name the job');
    assert.strictEqual(wizard.slug, 'gone',
      'wizardOpenNew owns clearing the slug; wizardRender must not');
    console.log('test_wizard_not_found: ok');
    resolve();
  }));
}
