'use strict';

const assert = require('assert');
const { spawnSync } = require('child_process');
const fs = require('fs');
const http = require('http');
const path = require('path');
const zlib = require('zlib');

const scratch = process.env.TRELLIS_TEST_SCRATCH
  || path.join(__dirname, '..', 'scratch', 'pv-dialog', 'viewer');
fs.mkdirSync(scratch, { recursive: true });
const root = fs.mkdtempSync(path.join(scratch, 'trellis-pv-viewer-'));
process.env.PROJECTS_ROOT = root;
process.env.STATIC_OUT = path.join(root, 'static');

const server = require('./server');
const pv = require('./pv_create');

function request(listener, method, requestPath, headers, body) {
  return new Promise((resolve, reject) => {
    const address = listener.address();
    const req = http.request({
      host: '127.0.0.1', port: address.port, method, path: requestPath,
      headers: { Host: `127.0.0.1:${address.port}`, ...(headers || {}) },
    }, (res) => {
      const chunks = [];
      res.on('data', chunk => chunks.push(chunk));
      res.on('end', () => resolve({ status: res.statusCode, body: Buffer.concat(chunks) }));
    });
    req.on('error', reject);
    if (body) req.write(body);
    req.end();
  });
}

function tarBytes(entries) {
  const chunks = [];
  for (const entry of entries) {
    const content = Buffer.from(entry.content || '');
    const header = Buffer.alloc(512);
    header.write(entry.name, 0, 100, 'utf-8');
    header.write('0000600\0', 100, 8, 'ascii');
    header.write('0000000\0', 108, 8, 'ascii');
    header.write('0000000\0', 116, 8, 'ascii');
    header.write(`${content.length.toString(8).padStart(11, '0')}\0`, 124, 12, 'ascii');
    header.write('00000000000\0', 136, 12, 'ascii');
    header.fill(0x20, 148, 156);
    header[156] = (entry.type || '0').charCodeAt(0);
    header.write('ustar\0', 257, 6, 'ascii');
    header.write('00', 263, 2, 'ascii');
    const checksum = header.reduce((sum, byte) => sum + byte, 0);
    header.write(`${checksum.toString(8).padStart(6, '0')}\0 `, 148, 8, 'ascii');
    chunks.push(header, content, Buffer.alloc((512 - (content.length % 512)) % 512));
  }
  chunks.push(Buffer.alloc(1024));
  return Buffer.concat(chunks);
}

function tarArchive(entries) {
  return zlib.gzipSync(tarBytes(entries));
}

(async () => {
  assert.ok(server.CONTROL_ROUTE_TAILS.includes('pv-create-jobs'));
  assert.ok(server.CONTROL_ROUTE_TAILS.includes('pv-create-jobs.json'));
  assert.ok(server.CONTROL_ROUTE_TAILS.includes('pv-create-status/:slug'));
  assert.strictEqual(server.CONTROL_ROUTE_METHODS['pv-create-status/:slug'], 'get');
  const expectedNonBrowserEnv = [
    'TRELLIS_PV_CREATE_ARCHIVE_CMD',
    'TRELLIS_PV_CREATE_ADAPT_CMD',
    'TRELLIS_PV_CREATE_EXTRACT_CMD',
    'TRELLIS_PV_CREATE_PHASE0_CMD',
    'TRELLIS_PV_CREATE_REPLAY_CMD',
    'TRELLIS_PV_CREATE_SETUP_CMD',
    'TRELLIS_PV_CREATE_PHASE0_VERIFY_CMD',
    'TRELLIS_PV_CREATE_PLAN_CMD',
    'TRELLIS_PV_CREATE_BOOTSTRAP_CMD',
    'TRELLIS_PV_CREATE_INIT_CMD',
    'TRELLIS_PV_CREATE_GATE_CMD',
    'TRELLIS_PV_CREATE_RUNNER',
    'TRELLIS_PV_CREATE_NATIVE_CMD',
    'TRELLIS_SYSTEM_FEEDBACK_HALT',
    'RUST_REFERENCE_REV',
    'NOMICON_REV',
    'RUST_STD_REV',
  ];
  assert.deepStrictEqual(pv.NON_BROWSER_ENV, expectedNonBrowserEnv);
  const scriptHooks = [...new Set([
    ...fs.readFileSync(path.join(__dirname, '..', 'scripts', 'pv_create_run.sh'), 'utf-8')
      .matchAll(/\bTRELLIS_PV_CREATE_[A-Z0-9_]+/g),
  ].map(match => match[0]))];
  for (const hook of scriptHooks) {
    assert.ok(pv.NON_BROWSER_ENV.includes(hook), `${hook} must be stripped from browser jobs`);
  }
  const hostileEnv = Object.fromEntries(expectedNonBrowserEnv.map(name => [name, `ambient-${name}`]));
  const childEnv = pv.createChildEnvironment({ PATH: '/bin', ...hostileEnv });
  assert.strictEqual(childEnv.PATH, '/bin');
  for (const name of expectedNonBrowserEnv) {
    assert.strictEqual(childEnv[name], undefined, `${name} survived browser environment stripping`);
  }
  assert.strictEqual(childEnv.TRELLIS_PROJECTS_ROOT, root);

  const pvMeta = pv.pvLauncherMetadata();
  const expectedRoles = [
    'phase0.adapting_worker',
    'phase0.seam_repair_auditor',
    'phase0.correspondence_agents',
    'worker',
    'easy_worker',
    'hard_worker',
    'reviewer',
    'stuck_math_audit',
    'verification.correspondence_agents',
    'verification.soundness_agents',
    'verification.substantiveness_agents',
  ];
  assert.deepStrictEqual(pvMeta.roles.map(role => role.key), expectedRoles,
    'PV settings metadata must expose every actual model-bearing role');
  assert.strictEqual(pvMeta.roles.find(role => role.key === 'phase0.adapting_worker').model,
    'gpt-6.1-sol');
  assert.strictEqual(pvMeta.roles.find(role => role.key === 'phase0.seam_repair_auditor').model,
    'gpt-6.1-sol');
  assert.match(pvMeta.roles.find(role => role.key === 'stuck_math_audit').label,
    /assumptions lane/);
  const defaultSettings = pv.validatedPvSettings({});
  assert.strictEqual(defaultSettings.allow_same_model_lanes, true);
  assert.deepStrictEqual(Object.keys(defaultSettings.role_overrides), expectedRoles);
  for (const role of pvMeta.roles) {
    assert.deepStrictEqual(defaultSettings.role_overrides[role.key], {
      provider: role.provider, model: role.model, effort: role.effort,
    });
  }
  const customRoles = Object.fromEntries(expectedRoles.map((role, index) => [role, {
    provider: 'codex', model: `custom-model-${index}`, effort: index % 2 ? 'high' : 'xhigh',
  }]));
  assert.deepStrictEqual(pv.validatedPvSettings({
    role_overrides: customRoles,
    remote_url: 'git@example.test:team/pv.git',
    allow_same_model_lanes: true,
  }), {
    role_overrides: customRoles,
    remote_url: 'git@example.test:team/pv.git',
    allow_same_model_lanes: true,
  });
  assert.throws(() => pv.validatedPvSettings({
    role_overrides: { 'phase0.not_a_lane': { model: 'gpt-5.6-sol' } },
  }), /unknown launcher role/);

  const landingSource = fs.readFileSync(path.join(__dirname, 'public', 'landing.html'), 'utf-8');
  class FakeElement {
    constructor(tag) {
      this.tagName = tag;
      this.children = [];
      this.dataset = {};
      this.style = {};
      this.value = '';
      this.checked = false;
      this.className = '';
      this.textContent = '';
    }

    appendChild(child) {
      this.children.push(child);
      if (this.tagName === 'select' && child.tagName === 'option'
          && (!this.value || child.selected)) this.value = child.value;
      return child;
    }

    setAttribute(name, value) { this[name] = value; }

    focus() {}
  }
  const componentStart = landingSource.indexOf('function createChoiceRows(');
  const componentEnd = landingSource.indexOf('\nfunction renderRunSettingsSummary(', componentStart);
  // eslint-disable-next-line no-new-func
  const renderSettings = new Function('document', `
    const OTHER_CREATE_OPTION = '__other__';
    function el(tag, cls, text) {
      const node = document.createElement(tag);
      if (cls) node.className = cls;
      if (text !== undefined) node.textContent = text;
      return node;
    }
    ${landingSource.slice(componentStart, componentEnd)}
    return createRunSettingsComponent;
  `)({ createElement: tag => new FakeElement(tag) });
  const renderedPvSettings = renderSettings(pvMeta, { create_options: { models: [], efforts: [] } });
  const descendants = (node) => [node, ...node.children.flatMap(descendants)];
  const renderedNodes = descendants(renderedPvSettings.element);
  const leanInput = renderedNodes.find(node => node.className === 'lean-parallelism');
  assert.strictEqual(Number(leanInput.value), pvMeta.lean_parallelism.recommended);
  leanInput.value = '3';
  assert.strictEqual(renderedPvSettings.collect().lean_parallelism, 3);
  assert.strictEqual(pv.validatedPvSettings(renderedPvSettings.collect()).lean_parallelism, 3);
  leanInput.value = '0';
  assert.throws(() => renderedPvSettings.collect(), /positive integer/);
  leanInput.value = String(pvMeta.lean_parallelism.recommended);
  assert.deepStrictEqual(renderedNodes.filter(node => node.dataset.role
      && node.className === 'role-choice').map(node => node.dataset.role), expectedRoles,
  'rendered PV wizard settings must list every template-derived role');
  const clientSource = fs.readFileSync(path.join(__dirname, 'public', 'pv_create_client.js'), 'utf-8');
  assert.ok(clientSource.includes('createRunSettingsComponent(native ? (settingsMeta.native'),
    'PV wizard must render the same settings component as math');

  // Presets on the PV launcher: the guide lane is the stuck-math audit plus
  // the two Phase-0 audit lanes; the adapting worker, reviewer and verifiers
  // take the base model.
  assert.deepStrictEqual(pvMeta.presets.map(preset => preset.key),
    ['astra-guided-sol', 'all-sol', 'sol-guided-luna', 'all-luna']);
  assert.strictEqual(pvMeta.default_preset, 'all-sol');
  assert.deepStrictEqual(pvMeta.guide_roles,
    ['stuck_math_audit', 'phase0.seam_repair_auditor', 'phase0.correspondence_agents']);
  const resolverStart = landingSource.indexOf('function resolvePresetRoleSettings(');
  const resolverEnd = landingSource.indexOf('\nfunction describePreset(', resolverStart);
  // eslint-disable-next-line no-new-func
  const resolvePresetRoleSettings = new Function(
    `${landingSource.slice(resolverStart, resolverEnd)}\nreturn resolvePresetRoleSettings;`,
  )();
  const astraGuided = resolvePresetRoleSettings(pvMeta.presets[0], pvMeta.roles, pvMeta.guide_roles);
  for (const key of pvMeta.guide_roles) {
    assert.deepStrictEqual(astraGuided[key], { model: 'gpt-6-astra', effort: 'xhigh' }, key);
  }
  assert.deepStrictEqual(astraGuided['phase0.adapting_worker'], { model: 'gpt-6.1-sol', effort: 'xhigh' });
  assert.deepStrictEqual(astraGuided.worker, { model: 'gpt-6.1-sol', effort: 'xhigh' });
  // "all sol" really is all sol, Phase-0 audit lanes included; the component
  // allows same-model lanes for it rather than refusing the launch.
  const allSol = resolvePresetRoleSettings(pvMeta.presets[1], pvMeta.roles, pvMeta.guide_roles);
  assert.ok(Object.values(allSol).every(selected => selected.model === 'gpt-6.1-sol'
    && selected.effort === 'xhigh'));

  // Rendered: the preset select leads, the default preset is applied to the
  // role controls (preset models and efforts are listed dropdown rows), and
  // editing a role flips the select to custom.
  const renderedPreset = renderSettings(pvMeta, { create_options: { models: [], efforts: [] } });
  const presetNodes = descendants(renderedPreset.element);
  const presetSelect = presetNodes.find(node => node.className === 'launch-preset-select');
  assert.ok(presetSelect, 'preset select must render');
  assert.strictEqual(presetSelect.value, 'all-sol');
  const roleTable = presetNodes.find(node => node.className === 'role-table');
  assert.strictEqual(roleTable.style.display, 'none', 'per-role table starts collapsed');
  const auditModel = presetNodes.find(node => node.className === 'role-model-select'
    && node.dataset.role === 'stuck_math_audit');
  assert.strictEqual(auditModel.value, 'gpt-6.1-sol');
  const workerModel = presetNodes.find(node => node.className === 'role-model-select'
    && node.dataset.role === 'worker');
  assert.strictEqual(workerModel.value, 'gpt-6.1-sol');
  const collected = renderedPreset.collect();
  assert.strictEqual(collected.preset, 'all-sol');
  assert.deepStrictEqual(collected.role_overrides.stuck_math_audit, { provider: 'codex', model: 'gpt-6.1-sol', effort: 'xhigh' });
  assert.deepStrictEqual(collected.role_overrides['phase0.adapting_worker'], { provider: 'codex', model: 'gpt-6.1-sol', effort: 'xhigh' });
  presetSelect.value = 'all-luna';
  presetSelect.onchange();
  assert.strictEqual(workerModel.value, 'gpt-6-luna');
  const seamModelCtl = presetNodes.find(node => node.className === 'role-model-select'
    && node.dataset.role === 'phase0.seam_repair_auditor');
  assert.strictEqual(seamModelCtl.value, 'gpt-6-luna', 'all luna puts luna on Phase-0 audit lanes too');
  const workerEffort = presetNodes.find(node => node.className === 'role-effort-select'
    && node.dataset.role === 'worker');
  assert.strictEqual(workerEffort.value, 'high');
  workerModel.value = 'gpt-6.1-sol';
  workerModel.onchange();
  assert.strictEqual(presetSelect.value, '__custom__', 'editing a role switches to custom');
  assert.strictEqual(roleTable.style.display, '', 'custom mode shows the per-role table');
  assert.strictEqual(renderedPreset.collect().preset, undefined);

  const renderedLanding = server.applyLandingHtmlTransforms(
    server.readPublicHtml('landing.html'), { controlMode: true },
  );
  assert.match(renderedLanding, /function createRunSettingsComponent\(/,
    'server-rendered landing must contain the shared settings implementation');
  assert.match(renderedLanding, /settings = createRunSettingsComponent\(meta, \{ create_options: wizard\.options \}\);/,
    'server-rendered landing must contain the math wizard use');
  assert.match(renderedLanding, /createRunSettingsComponent\(native \? \(settingsMeta\.native/,
    'server-rendered landing must contain the PV wizard use');

  const nativeOptions = { query: { format: 'native' } };
  const nativeCollection = Buffer.from(JSON.stringify({ context: 'Shared definitions.', goals: [
    { id: 'accepts', text: 'f accepts every valid input.' },
    { id: 'rounds', text: 'f rounds correctly.' },
  ] }));
  const nativeRecord = pv.goalUploadRecord(nativeCollection, nativeOptions);
  assert.strictEqual(nativeRecord.file_name, 'GOAL.json');
  assert.deepStrictEqual(nativeRecord.content, nativeCollection);
  assert.deepStrictEqual(nativeRecord.response.goal_ids, ['accepts', 'rounds']);
  assert.strictEqual(nativeRecord.response.goal_count, 2);
  for (const invalid of [null, [], {}, { goals: [] }, { context: 1, goals: [{ id: 'g', text: 'x' }] },
    { goals: [{ id: '', text: 'x' }] }, { goals: [{ id: 'g', text: ' ' }] },
    { goals: [{ id: 'g', text: 'x' }, { id: 'g', text: 'y' }] }]) {
    assert.strictEqual(pv.goalUploadRecord(Buffer.from(JSON.stringify(invalid)), nativeOptions).ok, false);
  }
  assert.strictEqual(pv.goalUploadRecord(Buffer.from('## accepts\ntext'), nativeOptions).ok, false);
  assert.strictEqual(pv.goalUploadRecord(nativeCollection, { query: { format: 'unknown' } }).ok, false);
  const nativeMeta = pv.pvLauncherMetadata(undefined, 'native');
  assert.strictEqual(nativeMeta.phase0_independence, null);
  assert.strictEqual(nativeMeta.allow_same_model_lanes, true);
  assert.deepStrictEqual(nativeMeta.guide_roles, ['stuck_math_audit', 'preparation.audit']);
  const allSolRoles = Object.fromEntries(nativeMeta.roles.map(role => [role.key, {
    provider: 'codex', model: 'gpt-6.1-sol', effort: 'xhigh',
  }]));
  for (const role of nativeMeta.roles) {
    assert.strictEqual(role.model, 'gpt-6.1-sol', `${role.key} default model`);
    assert.strictEqual(role.effort, 'xhigh', `${role.key} default effort`);
  }
  for (const selection of [{}, { role_overrides: allSolRoles },
    { role_overrides: allSolRoles, allow_same_model_lanes: false }]) {
    const settings = pv.validatedPvSettings({ goal_format: 'native', ...selection });
    assert.deepStrictEqual(settings.role_overrides, allSolRoles);
    assert.strictEqual(settings.allow_same_model_lanes, true);
  }
  assert.ok(nativeMeta.roles.some(role => role.key === 'preparation.audit'));
  assert.ok(!nativeMeta.roles.some(role => role.key.startsWith('phase0.')));
  const nativeSettings = pv.validatedPvSettings({ goal_format: 'native', role_overrides: {
    'preparation.worker': { model: 'selected-worker', effort: 'high' },
    'preparation.audit': { model: 'selected-audit', effort: 'xhigh' },
  } });
  assert.strictEqual(nativeSettings.role_overrides['preparation.worker'].model, 'selected-worker');
  assert.strictEqual(nativeSettings.role_overrides['preparation.audit'].model, 'selected-audit');

  const utf8 = pv.intakeGoalUpload(Buffer.from('# Goal\n'));
  assert.strictEqual(utf8.ok, true);
  assert.strictEqual(utf8.text, '# Goal\n');
  assert.deepStrictEqual(utf8.bytes, Buffer.from('# Goal\n'));
  const goalRecord = pv.goalUploadRecord(Buffer.from('# Goal\n'));
  assert.ok(Buffer.isBuffer(goalRecord.content), 'GOAL upload record must retain a byte buffer');
  assert.strictEqual(utf8.warnings.length, 0, 'GOAL intake must not emit LaTeX plausibility warnings');
  assert.match(pv.intakeGoalUpload(Buffer.from([65, 0, 66])).error, /NUL/);
  assert.match(pv.intakeGoalUpload(Buffer.from([0x80])).error, /valid UTF-8/);

  const archive = tarArchive([
    { name: 'Cargo.toml', content: '[package]\nname="fixture"\nversion="0.1.0"\n' },
    { name: 'src/lib.rs', content: 'pub fn f() {}\n' },
  ]);
  assert.ok(archive.includes(0), 'fixture gzip should exercise binary/NUL intake');
  const inspected = pv.inspectCrateArchive(archive);
  assert.strictEqual(inspected.ok, true, inspected.error);
  assert.strictEqual(inspected.metadata.source_root, '');
  assert.ok(inspected.metadata.expanded_bytes > 0);
  assert.strictEqual(inspected.metadata.archive_sha256,
    require('crypto').createHash('sha256').update(archive).digest('hex'));
  assert.strictEqual(inspected.metadata.source_tree_sha256,
    inspected.metadata.source_manifest.source_tree_sha256);

  const rawModule = tarArchive([
    { name: 'module/mod.rs', content: 'pub mod inner;\nuse crate::traits::{Float, FloatExt};\n' },
    { name: 'module/inner.rs', content: 'pub fn f() {}\n' },
  ]);
  const rawInspected = pv.inspectSourceArchive(rawModule);
  assert.strictEqual(rawInspected.ok, true, rawInspected.error);
  assert.strictEqual(rawInspected.metadata.source_root, 'module');
  assert.deepStrictEqual(rawInspected.metadata.source_manifest.files.map(row => row.relative_path),
    ['inner.rs', 'mod.rs']);
  const missingImports = pv.scanMissingImports(rawInspected.source_files);
  assert.deepStrictEqual(missingImports, [{
    source_file: 'mod.rs', path: 'crate::traits::{Float, FloatExt}',
  }]);
  const exampleTargetDir = path.join(
    __dirname, '..', 'examples', 'pv_dec2flt', 'source', 'target', 'dec2flt',
  );
  const exampleTargetArchive = tarArchive(fs.readdirSync(exampleTargetDir).sort().map(name => ({
    name: `dec2flt/${name}`,
    content: fs.readFileSync(path.join(exampleTargetDir, name)),
  })));
  const exampleSupport = fs.readFileSync(path.join(
    __dirname, '..', 'examples', 'pv_dec2flt', 'source', 'supporting', 'traits.rs',
  ));
  const supportingFiles = [
    { name: 'traits.rs', content: Buffer.from('pub trait Float {}\npub trait FloatExt {}\n') },
    { name: 'helpers.rs', content: Buffer.from('pub fn helper() {}\n') },
  ];
  assert.deepStrictEqual(
    pv.scanMissingImports(
      rawInspected.source_files,
      [...rawInspected.source_files, ...supportingFiles.map(file => ({
        relative_path: file.name, content: file.content,
      }))],
    ),
    [],
  );
  const packedA = pv.packLooseSupportingFiles(supportingFiles);
  const packedB = pv.packLooseSupportingFiles([...supportingFiles].reverse());
  assert.deepStrictEqual(packedA, packedB, 'loose supporting packing is not deterministic');
  const packedInspection = pv.inspectSourceArchive(packedA);
  assert.strictEqual(packedInspection.ok, true, packedInspection.error);
  assert.deepStrictEqual(packedInspection.metadata.source_manifest.files.map(row => row.relative_path),
    ['helpers.rs', 'traits.rs']);
  const mixedRoot = pv.inspectSourceArchive(tarArchive([
    { name: 'module/mod.rs', content: 'pub fn f() {}\n' },
    { name: 'traits.rs', content: 'pub trait T {}\n' },
  ]));
  assert.strictEqual(mixedRoot.ok, true, mixedRoot.error);
  assert.strictEqual(mixedRoot.metadata.source_root, '');
  assert.deepStrictEqual(mixedRoot.metadata.source_manifest.files.map(row => row.relative_path),
    ['module/mod.rs', 'traits.rs']);
  const rootFile = pv.inspectSourceArchive(tarArchive([
    { name: 'traits.rs', content: 'pub trait T {}\n' },
  ]));
  assert.strictEqual(rootFile.ok, true, rootFile.error);
  assert.strictEqual(rootFile.metadata.source_root, '');

  const tooManyEntries = [{ name: 'Cargo.toml', content: '[package]' }];
  for (let index = 0; index < 10000; index += 1) {
    tooManyEntries.push({ name: `empty/${index}` });
  }
  const decompressionBomb = zlib.gzipSync(Buffer.alloc(
    server.CREATE_UPLOAD_LIMIT_BYTES + 12 * 1024 * 1024,
  ));
  const trailingTarData = zlib.gzipSync(Buffer.concat([
    tarBytes([{ name: 'Cargo.toml', content: '[package]' }]),
    Buffer.from('not tar padding'),
  ]));
  const archiveNegativeControls = [
    ['path traversal', tarArchive([{ name: '../Cargo.toml', content: '[package]' }]), /unsafe archive path/],
    ['ambiguous repeated separator', tarArchive([
      { name: 'source//mod.rs', content: 'pub fn f() {}' },
    ]), /unsafe archive path/],
    ['control character in path', tarArchive([
      { name: 'source/bad\x01.rs', content: 'pub fn f() {}' },
    ]), /unsafe archive path/],
    ['entry-count cap', tarArchive(tooManyEntries), /more than 10000 entries/],
    ['duplicate path', tarArchive([
      { name: 'Cargo.toml', content: '[package]' },
      { name: 'Cargo.toml', content: '[package]' },
    ]), /duplicate archive path/],
    ['hardlink', tarArchive([
      { name: 'Cargo.toml', content: '[package]' },
      { name: 'crate-hardlink', type: '1' },
    ]), /only directories and regular files/],
    ['gzip output cap', decompressionBomb, /larger than|maxOutputLength|output length/i],
    ['trailing data', trailingTarData, /trailing nonzero data/],
  ];
  for (const [label, bytes, errorPattern] of archiveNegativeControls) {
    const rejected = pv.inspectCrateArchive(bytes);
    assert.strictEqual(rejected.ok, false, `${label} negative control was accepted`);
    assert.match(rejected.error, errorPattern, label);
  }
  const traversal = archiveNegativeControls[0][1];
  const absolute = pv.inspectCrateArchive(tarArchive([
    { name: '/Cargo.toml', content: '[package]' },
  ]));
  assert.strictEqual(absolute.ok, false);
  assert.match(absolute.error, /unsafe archive path/);
  assert.match(pv.inspectCrateArchive(tarArchive([
    { name: 'Cargo.toml', content: '[package]' },
    { name: 'crate-link', type: '2' },
  ])).error, /only directories and regular files/);
  assert.match(pv.inspectSourceArchive(tarArchive([
    { name: 'only-dir', type: '5' },
  ])).error, /no regular files/);
  assert.match(pv.inspectCrateArchive(tarArchive([
    { name: 'Cargo.toml', content: Buffer.alloc(server.CREATE_UPLOAD_LIMIT_BYTES + 1) },
  ])).error, /expanded source exceeds/);

  // The optional-module catch in server.js may swallow only absence of the
  // top-level PV module. Syntax errors and missing dependencies inside it are
  // broken private builds and must escape startup.
  for (const failure of [
    {
      label: 'syntax error',
      source: 'const fixtureSyntaxError = ;\n',
      pattern: /Unexpected token/,
    },
    {
      label: 'missing nested dependency',
      source: "require('./pv_nested_fixture');\nmodule.exports = { install() {} };\n",
      pattern: /pv_nested_fixture/,
    },
  ]) {
    const serverPath = path.join(__dirname, 'server.js');
    const brokenDir = path.join(root, `broken-${failure.label.replaceAll(' ', '-')}`);
    const brokenModule = path.join(brokenDir, 'pv_create.js');
    fs.mkdirSync(brokenDir, { recursive: true });
    fs.writeFileSync(brokenModule, failure.source);
    const childScript = `
      const Module = require('module');
      const serverPath = ${JSON.stringify(serverPath)};
      const brokenModule = ${JSON.stringify(brokenModule)};
      const originalLoad = Module._load;
      Module._load = function(request, parent, isMain) {
        if (request === './pv_create' && parent && parent.filename === serverPath) {
          return originalLoad.call(this, brokenModule, parent, isMain);
        }
        return originalLoad.call(this, request, parent, isMain);
      };
      require(serverPath);
    `;
    const result = spawnSync(process.execPath, ['-e', childScript], {
      encoding: 'utf-8',
      env: {
        ...process.env,
        PROJECTS_ROOT: path.join(root, `rethrow-${failure.label.replaceAll(' ', '-')}`),
        STATIC_OUT: path.join(root, `rethrow-static-${failure.label.replaceAll(' ', '-')}`),
      },
    });
    assert.notStrictEqual(result.status, 0, `${failure.label} was swallowed by optional-module guard`);
    assert.match(result.stderr, failure.pattern, failure.label);
  }

  // Development curation checks are unavailable in a public checkout.
  const releaseTool = path.join(__dirname, '..', 'scripts', 'prepare_public_release.sh');
  if (fs.existsSync(releaseTool)) {
    // Exercise the release projection itself: it deletes pv_create.js and must
    // still load server.js while exposing no PV create route.
    const projection = path.join(
      path.dirname(__dirname), `.pv-exclusion-test-${process.pid}-${Date.now()}`,
    );
    try {
      const verified = spawnSync('bash', [
        releaseTool,
        '--verify-pv-exclusion', projection,
      ], { encoding: 'utf-8' });
      assert.strictEqual(
        verified.status, 0,
        `PV exclusion verification failed:\n${verified.stdout}\n${verified.stderr}`,
      );
      assert.strictEqual(fs.existsSync(path.join(projection, 'viewer', 'pv_create.js')), false);
    } finally {
      fs.rmSync(projection, { recursive: true, force: true });
    }

  } else {
    console.log('PV exclusion projection skipped: development curation tool is absent');
  }

  const listener = server.app.listen(0, '127.0.0.1');
  const listening = await new Promise((resolve, reject) => {
    listener.once('listening', () => resolve(true));
    listener.once('error', (error) => {
      if (error && (error.code === 'EPERM' || error.code === 'EACCES')) resolve(false);
      else reject(error);
    });
  });
  if (!listening && process.env.TRELLIS_REQUIRE_VIEWER_HTTP === '1') throw new Error('HTTP integration could not bind its listener');
  if (listening) try {
    const token = server.ensureControlToken(root);
    const landing = await request(listener, 'GET', `/trellis/${token}/`, {}, null);
    assert.strictEqual(landing.status, 200);
    assert.match(landing.body.toString('utf-8'), /\+ new prose verification run/);
    assert.doesNotMatch(landing.body.toString('utf-8'), /<script src="pv_create_client\.js"/);

    const auth = { [server.CONTROL_HEADER]: token, 'Content-Type': 'application/octet-stream' };
    const rawGoal = Buffer.concat([
      Buffer.from([0xef, 0xbb, 0xbf]),
      Buffer.from('# Goal\n\r\nUnicode: π\n', 'utf-8'),
    ]);
    const goalUpload = await request(listener, 'POST', '/trellis/api/control/uploads?kind=goal', auth, rawGoal);
    assert.strictEqual(goalUpload.status, 200, goalUpload.body.toString());
    const goalPayload = JSON.parse(goalUpload.body);
    assert.strictEqual(goalPayload.kind, 'goal');
    const storedGoal = path.join(root, '.trellis-viewer', 'uploads', goalPayload.upload_id, 'GOAL.md');
    assert.ok(fs.existsSync(storedGoal));
    assert.deepStrictEqual(fs.readFileSync(storedGoal), rawGoal, 'GOAL upload bytes were transcoded');

    const nativeUpload = await request(listener, 'POST', '/trellis/api/control/uploads?kind=goal&format=native', auth, nativeCollection);
    assert.strictEqual(nativeUpload.status, 200, nativeUpload.body.toString());
    const nativePayload = JSON.parse(nativeUpload.body);
    assert.deepStrictEqual(nativePayload.goal_ids, ['accepts', 'rounds']);
    assert.deepStrictEqual(fs.readFileSync(path.join(root, '.trellis-viewer', 'uploads', nativePayload.upload_id, 'GOAL.json')), nativeCollection);
    const badNativeUpload = await request(listener, 'POST', '/trellis/api/control/uploads?kind=goal&format=native', auth,
      Buffer.from('{"goals":[{"id":"g","text":"x"},{"id":"g","text":"y"}]}'));
    assert.strictEqual(badNativeUpload.status, 400);
    const targetUpload = await request(listener, 'POST', '/trellis/api/control/uploads?kind=target', auth, archive);
    assert.strictEqual(targetUpload.status, 200, targetUpload.body.toString());
    const targetPayload = JSON.parse(targetUpload.body);
    assert.strictEqual(targetPayload.kind, 'target');
    assert.ok(fs.existsSync(path.join(root, '.trellis-viewer', 'uploads', targetPayload.upload_id, 'target.tar.gz')));

    const rawModuleUpload = await request(
      listener, 'POST', '/trellis/api/control/uploads?kind=target', auth, rawModule,
    );
    assert.strictEqual(rawModuleUpload.status, 200, rawModuleUpload.body.toString());
    const rawModulePayload = JSON.parse(rawModuleUpload.body);
    assert.strictEqual(rawModulePayload.archive.source_root, 'module');
    assert.deepStrictEqual(rawModulePayload.archive.source_manifest.files.map(row => row.relative_path),
      ['inner.rs', 'mod.rs']);
    assert.deepStrictEqual(rawModulePayload.missing_imports, [{
      source_file: 'mod.rs', path: 'crate::traits::{Float, FloatExt}',
    }]);

    const exampleTargetUpload = await request(
      listener, 'POST', '/trellis/api/control/uploads?kind=target', auth, exampleTargetArchive,
    );
    assert.strictEqual(exampleTargetUpload.status, 200, exampleTargetUpload.body.toString());
    const exampleTargetPayload = JSON.parse(exampleTargetUpload.body);
    assert.match(
      exampleTargetPayload.missing_imports.map(row => row.path).join('\n'),
      /Float.*FloatExt|FloatExt.*Float/s,
    );
    const exampleSupportBody = Buffer.concat([
      Buffer.from(`${JSON.stringify({
        schema: pv.LOOSE_SUPPORT_SCHEMA,
        files: [{ name: 'traits.rs', byte_length: exampleSupport.length }],
      })}\n`),
      exampleSupport,
    ]);
    const exampleSupportUpload = await request(
      listener, 'POST',
      `/trellis/api/control/uploads?kind=supporting&format=loose&target_upload_id=${exampleTargetPayload.upload_id}`,
      auth, exampleSupportBody,
    );
    assert.strictEqual(exampleSupportUpload.status, 200, exampleSupportUpload.body.toString());
    const remainingExampleHints = JSON.parse(exampleSupportUpload.body).missing_imports
      .map(row => row.path).join('\n');
    assert.doesNotMatch(remainingExampleHints, /\bFloat(?:Ext)?\b/);

    const looseHeader = Buffer.from(`${JSON.stringify({
      schema: pv.LOOSE_SUPPORT_SCHEMA,
      files: supportingFiles.map(file => ({ name: file.name, byte_length: file.content.length })),
    })}\n`);
    const looseBody = Buffer.concat([looseHeader, ...supportingFiles.map(file => file.content)]);
    const supportUpload = await request(
      listener, 'POST', `/trellis/api/control/uploads?kind=supporting&format=loose&target_upload_id=${rawModulePayload.upload_id}`,
      auth, looseBody,
    );
    assert.strictEqual(supportUpload.status, 200, supportUpload.body.toString());
    const supportPayload = JSON.parse(supportUpload.body);
    assert.strictEqual(supportPayload.kind, 'supporting');
    assert.strictEqual(supportPayload.packed_loose_files, true);
    assert.deepStrictEqual(supportPayload.missing_imports, []);
    const storedSupport = path.join(root, '.trellis-viewer', 'uploads', supportPayload.upload_id, 'support.tar.gz');
    assert.deepStrictEqual(fs.readFileSync(storedSupport), packedA);

    const fakeBin = path.join(root, 'fake-bin');
    fs.mkdirSync(fakeBin);
    fs.writeFileSync(path.join(fakeBin, 'tmux'), '#!/bin/sh\nexit 0\n', { mode: 0o700 });
    process.env.PATH = `${fakeBin}:/usr/bin:/bin`;
    for (const [slug, supportId] of [['viewer_with_support', supportPayload.upload_id], ['viewer_without_support', null]]) {
      const requestedSettings = supportId ? {
        role_overrides: customRoles,
        remote_url: 'git@example.test:team/pv.git',
        allow_same_model_lanes: true,
      } : {};
      const create = await request(
        listener, 'POST', '/trellis/api/control/pv-create-jobs',
        { [server.CONTROL_HEADER]: token, 'Content-Type': 'application/json' },
        Buffer.from(JSON.stringify({
          slug,
          goal_upload_id: goalPayload.upload_id,
          target_upload_id: rawModulePayload.upload_id,
          support_upload_id: supportId,
          goal_name: 'GOAL.md',
          target_name: 'target.tar.gz',
          support_name: supportId ? 'traits.rs' : null,
          ...requestedSettings,
        })),
      );
      assert.strictEqual(create.status, 200, create.body.toString());
      const createPayload = JSON.parse(create.body);
      assert.match(createPayload.output, /launch settings/);
      assert.match(createPayload.output,
        /phase0\.allow_same_model_lanes=true/);
      const job = JSON.parse(fs.readFileSync(path.join(
        root, '.trellis-viewer', 'create-jobs', slug, 'job.json',
      )));
      assert.strictEqual(job.target, 'target.tar.gz');
      assert.strictEqual(job.crate, undefined);
      assert.strictEqual(job.supporting, supportId ? 'support.tar.gz' : undefined);
      const expectedSettings = supportId ? customRoles : defaultSettings.role_overrides;
      assert.deepStrictEqual(job.role_overrides, expectedSettings);
      assert.strictEqual(job.allow_same_model_lanes, true);
      const generatedConfig = JSON.parse(fs.readFileSync(path.join(
        root, '.trellis-viewer', 'create-jobs', slug, 'config-template.json',
      )));
      assert.strictEqual(generatedConfig.phase0.allow_same_model_lanes, true);
      const roleValue = (config, role) => role.split('.').reduce((value, key) => {
        if (Array.isArray(value)) return value.map(entry => entry[key]);
        return value[key];
      }, config);
      for (const role of expectedRoles) {
        const values = roleValue(generatedConfig, role);
        for (const value of Array.isArray(values) ? values : [values]) {
          assert.strictEqual(value.model, expectedSettings[role].model, role);
          assert.strictEqual(value.effort, expectedSettings[role].effort, role);
        }
      }
      if (supportId) {
        assert.strictEqual(generatedConfig.git.remote_url, 'git@example.test:team/pv.git');
      }
      const status = await request(
        listener, 'GET', `/trellis/api/control/pv-create-status/${slug}`,
        { [server.CONTROL_HEADER]: token }, null,
      );
      assert.strictEqual(status.status, 200, status.body.toString());
      assert.deepStrictEqual(JSON.parse(status.body).settings, {
        role_overrides: expectedSettings,
        remote_url: supportId ? 'git@example.test:team/pv.git' : null,
        allow_same_model_lanes: true,
      });
    }

    // Exercise the real browser-to-shell native selection and durable job.
    // Only the installed-tool preflight and tmux background work are substituted.
    fs.writeFileSync(path.join(fakeBin, 'python3'), '#!/bin/sh\ncase "$1:$2" in */pv_native_create.py:preflight) exit 0;; esac\nexec /usr/bin/python3 "$@"\n', { mode: 0o700 });
    const nativeTools = path.join(root, 'native-tools.json');
    fs.writeFileSync(nativeTools, '{}\n');
    process.env.TRELLIS_NATIVE_TOOLS = nativeTools;
    const nativeCreate = await request(listener, 'POST', '/trellis/api/control/pv-create-jobs',
      { [server.CONTROL_HEADER]: token, 'Content-Type': 'application/json' },
      Buffer.from(JSON.stringify({ slug: 'viewer_native', goal_format: 'native',
        goal_upload_id: nativePayload.upload_id, target_upload_id: rawModulePayload.upload_id,
        goal_name: 'GOAL.json', role_overrides: allSolRoles,
      })));
    assert.strictEqual(nativeCreate.status, 200, nativeCreate.body.toString());
    const nativeJobDir = path.join(root, '.trellis-viewer', 'create-jobs', 'viewer_native');
    const nativeJob = JSON.parse(fs.readFileSync(path.join(nativeJobDir, 'job.json')));
    assert.strictEqual(nativeJob.goal_format, 'native');
    assert.strictEqual(nativeJob.goal, 'GOAL.json');
    assert.strictEqual(nativeJob.authority_root, path.join(nativeJobDir, 'native'));
    assert.deepStrictEqual(fs.readFileSync(path.join(nativeJobDir, 'GOAL.json')), nativeCollection);
    const nativeConfig = JSON.parse(fs.readFileSync(path.join(nativeJobDir, 'config-template.json')));
    assert.strictEqual(nativeJob.allow_same_model_lanes, true);
    assert.deepStrictEqual(nativeJob.role_overrides, allSolRoles);
    assert.strictEqual(nativeConfig.phase0.allow_same_model_lanes, true);
    for (const role of nativeMeta.roles) {
      const selected = role.key.split('.').reduce((value, key) => value[key], nativeConfig);
      for (const binding of Array.isArray(selected) ? selected : [selected]) {
        assert.strictEqual(binding.model, 'gpt-6.1-sol', role.key);
        assert.strictEqual(binding.effort, 'xhigh', role.key);
      }
    }
    assert.strictEqual(nativeConfig.system_feedback_halt, true);
    // The HTTP retry action uses the shared resume preflight and writes a
    // resume launcher without replacing the preparation or saved settings.
    const prep = path.join(nativeJobDir, 'native', 'viewer_native-preparation');
    fs.mkdirSync(path.join(prep, '.trellis/modeling'), { recursive: true });
    fs.writeFileSync(path.join(prep, '.trellis/modeling/state.json'), JSON.stringify({
      verification_protocol: 'native-v1', status: 'active', sequence: 16,
    }));
    fs.writeFileSync(path.join(prep, 'config.json'), JSON.stringify({ launch_config: nativeConfig }));
    fs.copyFileSync(path.join(nativeJobDir, 'GOAL.json'), path.join(prep, 'goals.json'));
    fs.symlinkSync(prep, nativeJob.repo);
    const failedStatus = { state: 'resolve_failed', stage: 'preparation', updated_ts: 1 };
    fs.writeFileSync(path.join(nativeJobDir, 'status.json'), JSON.stringify(failedStatus));
    const savedConfig = fs.readFileSync(path.join(nativeJobDir, 'config-template.json'));
    const halt = path.join(`${prep}-runtime`, 'system_feedback_halt.json');
    fs.mkdirSync(path.dirname(halt), { recursive: true });
    fs.writeFileSync(halt, '{}');
    const retryUrl = '/trellis/api/control/pv-create-jobs/viewer_native/retry';
    const haltedRetry = await request(listener, 'POST', retryUrl,
      { ...auth, 'Content-Type': 'application/json' }, Buffer.from('{}'));
    assert.notStrictEqual(haltedRetry.status, 200, haltedRetry.body.toString());
    assert.ok(fs.existsSync(halt), 'retry cannot clear a halt');
    fs.unlinkSync(halt);
    const resumed = await request(listener, 'POST', retryUrl,
      { ...auth, 'Content-Type': 'application/json' }, Buffer.from('{}'));
    assert.strictEqual(resumed.status, 200, resumed.body.toString());
    assert.match(resumed.body.toString(), /resuming preserved preparation/);
    assert.match(fs.readFileSync(path.join(nativeJobDir, 'run_phase.sh'), 'utf8'), /resume-run viewer_native/);
    assert.deepStrictEqual(fs.readFileSync(path.join(nativeJobDir, 'config-template.json')), savedConfig);
    assert.strictEqual(JSON.parse(fs.readFileSync(path.join(prep, '.trellis/modeling/state.json'))).sequence, 16);
    const wrongFormat = await request(listener, 'POST', '/trellis/api/control/pv-create-jobs',
      { [server.CONTROL_HEADER]: token, 'Content-Type': 'application/json' },
      Buffer.from(JSON.stringify({ slug: 'wrong_format', goal_format: 'native',
        goal_upload_id: goalPayload.upload_id, target_upload_id: rawModulePayload.upload_id })));
    assert.strictEqual(wrongFormat.status, 400);
    assert.ok(!fs.existsSync(path.join(root, '.trellis-viewer', 'create-jobs', 'wrong_format')));
    const badUpload = await request(listener, 'POST', '/trellis/api/control/uploads?kind=target', auth, traversal);
    assert.strictEqual(badUpload.status, 400);
    assert.match(JSON.parse(badUpload.body).detail, /unsafe archive path/);
  } finally {
    listener.close();
  }

  assert.ok(clientSource.includes('Upload the upstream source, unmodified: the module or crate you want verified, plus any sibling files in the same crate that its `use` lines import. Never edit, never add a manifest.'));
  assert.strictEqual((clientSource.match(/\.type = 'file'/g) || []).length, 3,
    'launcher must expose only GOAL, target, and supporting file inputs');
  assert.doesNotMatch(clientSource, /unambiguous Cargo\.toml root|source crate/);

  fs.rmSync(root, { recursive: true, force: true });
  console.log(`pv create viewer tests: ok (HTTP ${listening ? 'exercised' : 'skipped'})`);
})().catch((error) => {
  console.error(error);
  process.exitCode = 1;
});
