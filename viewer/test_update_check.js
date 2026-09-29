const assert = require('assert');

const {
  parseChangelogRelease,
  versionIsNewer,
  computeUpdateAvailable,
  localTrellisInstall,
} = require('./server');

// --- changelog heading parse ------------------------------------------------
assert.deepStrictEqual(
  parseChangelogRelease('# Changelog\n\n## v0.2.5 — 2026-08-10\n'),
  { version: 'v0.2.5', date: '2026-08-10' });
// First heading wins; later (older) entries are ignored.
assert.deepStrictEqual(
  parseChangelogRelease('# Changelog\n\n## v0.3.0 — 2026-09-01\n\n## v0.2.5 — 2026-08-10\n'),
  { version: 'v0.3.0', date: '2026-09-01' });
// v0.1.0's heading carries no date.
assert.deepStrictEqual(
  parseChangelogRelease('## v0.1.0\n\nInitial public release.\n'),
  { version: 'v0.1.0', date: null });
// The heading must start the line — an inline mention is not a version.
assert.strictEqual(parseChangelogRelease('see ## v9.9.9 for details'), null);
assert.strictEqual(parseChangelogRelease('## Unreleased\n'), null);
assert.strictEqual(parseChangelogRelease('## 0.2.5 — no v prefix\n'), null);
assert.strictEqual(parseChangelogRelease(''), null);
assert.strictEqual(parseChangelogRelease(null), null);

// --- version comparison -----------------------------------------------------
assert.strictEqual(versionIsNewer('v0.2.6', 'v0.2.5'), true);
assert.strictEqual(versionIsNewer('v0.3.0', 'v0.2.9'), true);
assert.strictEqual(versionIsNewer('v1.0.0', 'v0.9.9'), true);
// Numeric, not lexicographic.
assert.strictEqual(versionIsNewer('v0.2.10', 'v0.2.9'), true);
assert.strictEqual(versionIsNewer('v0.2.5', 'v0.2.5'), false);
assert.strictEqual(versionIsNewer('v0.2.4', 'v0.2.5'), false);
// Unparseable input compares as not-newer: a malformed remote heading must
// never raise a banner.
assert.strictEqual(versionIsNewer('v0.2.6', null), false);
assert.strictEqual(versionIsNewer(null, 'v0.2.5'), false);
assert.strictEqual(versionIsNewer('junk', 'v0.2.5'), false);
assert.strictEqual(versionIsNewer('v0.2.6-rc1', 'v0.2.5'), false);

// --- the decision, per install shape ---------------------------------------
const remote = { version: 'v0.2.6', date: '2026-08-20' };

// Public install: exact version comparison; the release date is irrelevant.
assert.strictEqual(computeUpdateAvailable(
  { kind: 'public', version: 'v0.2.5', code_date: '2026-08-10' }, remote), true);
assert.strictEqual(computeUpdateAvailable(
  { kind: 'public', version: 'v0.2.6', code_date: null }, remote), false);
assert.strictEqual(computeUpdateAvailable(
  { kind: 'public', version: 'v0.2.7', code_date: null }, remote), false);

// Dev checkout: by date — the banner means "a public release postdates the
// code you are running". The changelog heading version on dev names the NEXT
// release and must not defeat the date comparison.
assert.strictEqual(computeUpdateAvailable(
  { kind: 'dev', version: 'v0.2.6', code_date: '2026-08-10' }, remote), true);
// Strictly after: the machine that cut today's release stays quiet...
assert.strictEqual(computeUpdateAvailable(
  { kind: 'dev', version: 'v0.2.6', code_date: '2026-08-20' }, remote), false);
// ...and a dev checkout ahead of the release stays quiet.
assert.strictEqual(computeUpdateAvailable(
  { kind: 'dev', version: 'v0.2.6', code_date: '2026-08-21' }, remote), false);
// Any missing side of the date comparison stays quiet — never banner on a
// guess (tarball without git; dateless remote heading).
assert.strictEqual(computeUpdateAvailable(
  { kind: 'dev', version: 'v0.2.6', code_date: null }, remote), false);
assert.strictEqual(computeUpdateAvailable(
  { kind: 'dev', version: null, code_date: '2026-08-10' },
  { version: 'v0.1.0', date: null }), false);
assert.strictEqual(computeUpdateAvailable(null, remote), false);
assert.strictEqual(computeUpdateAvailable(
  { kind: 'dev', version: 'v0.2.6', code_date: '2026-08-10' }, null), false);

// --- local install detection ------------------------------------------------
// This repo is one of the two install shapes (public clone: CHANGELOG.md;
// dev checkout: CHANGELOG.public.md + git), so detection must resolve here.
const local = localTrellisInstall();
assert.ok(local && (local.kind === 'public' || local.kind === 'dev'),
  `localTrellisInstall() => ${JSON.stringify(local)}`);
if (local.kind === 'dev') {
  assert.ok(/^\d{4}-\d{2}-\d{2}$/.test(local.code_date),
    `dev checkout should carry a git HEAD date, got ${JSON.stringify(local.code_date)}`);
} else {
  assert.ok(/^v\d+\.\d+\.\d+$/.test(local.version),
    `public install should carry a version, got ${JSON.stringify(local.version)}`);
}

console.log('test_update_check: OK');
