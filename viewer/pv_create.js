'use strict';
const fs = require('fs');
const path = require('path');
const { recommendedLeanParallelism } = require('./lean_parallelism');

const zlib = require('zlib');
const crypto = require('crypto');

const LOOSE_SUPPORT_SCHEMA = 'trellis-pv-supporting-loose-upload/v1';
const PV_CONFIG_TEMPLATE = path.join(__dirname, '..', 'examples', 'trellis.pv.config.json');
const NATIVE_CONFIG_TEMPLATE = path.join(__dirname, '..', 'examples', 'trellis.native.pv.config.json');

function goalTemplate(format = 'callable') {
  if (format === 'native') return NATIVE_CONFIG_TEMPLATE;
  if (format === 'callable') return PV_CONFIG_TEMPLATE;
  throw new Error('goal_format must be native or callable');
}

// Optional server half of prose program-verification run creation. Public
// releases remove this file, its client chunk, and its job/archive scripts.

let installed = false;
let deps = null;

function canonicalJson(value) {
  if (Array.isArray(value)) return `[${value.map(canonicalJson).join(',')}]`;
  if (value && typeof value === 'object') {
    return `{${Object.keys(value).sort().map(key => `${JSON.stringify(key)}:${canonicalJson(value[key])}`).join(',')}}`;
  }
  return JSON.stringify(value);
}

function taggedHash(tag, payload) {
  const tagBytes = Buffer.from(tag, 'utf-8');
  const tagLength = Buffer.alloc(4);
  tagLength.writeUInt32BE(tagBytes.length);
  const payloadLength = Buffer.alloc(8);
  payloadLength.writeBigUInt64BE(BigInt(payload.length));
  return crypto.createHash('sha256').update(Buffer.concat([
    Buffer.from('trellis-trust-v1\0', 'utf-8'), tagLength, tagBytes, payloadLength, payload,
  ])).digest('hex');
}

function inspectSourceArchive(buffer, options) {
  const d = (options && options.deps) || deps;
  if (!d) throw new Error('PV create module is not installed');
  if (!Buffer.isBuffer(buffer) || buffer.length === 0) {
    return { ok: false, error: 'empty upload — send the .tar.gz bytes as the raw request body' };
  }
  const maxMembers = 10000;
  let tar;
  try {
    tar = zlib.gunzipSync(buffer, {
      // Payload plus tar headers/padding for the bounded member count.
      maxOutputLength: d.CREATE_UPLOAD_LIMIT_BYTES + (maxMembers + 2) * 1024,
    });
  } catch (error) {
    return { ok: false, error: `not an extractable .tar.gz archive: ${error.message}` };
  }

  const cString = (block, start, length) => {
    const field = block.subarray(start, start + length);
    const nul = field.indexOf(0);
    return new TextDecoder('utf-8', { fatal: true })
      .decode(field.subarray(0, nul < 0 ? field.length : nul));
  };
  const octal = (block, start, length, label) => {
    const raw = cString(block, start, length).trim();
    if (!/^[0-7]+$/.test(raw)) throw new Error(`${label} is not a portable octal field`);
    return Number.parseInt(raw, 8);
  };
  const seen = new Set();
  const members = [];
  const files = [];
  let total = 0;
  let entries = 0;
  let offset = 0;
  let ended = false;
  try {
    while (offset + 512 <= tar.length) {
      const header = tar.subarray(offset, offset + 512);
      if (header.every(byte => byte === 0)) {
        ended = true;
        break;
      }
      entries += 1;
      if (entries > maxMembers) throw new Error(`source archive has more than ${maxMembers} entries`);
      const expectedChecksum = octal(header, 148, 8, 'tar checksum');
      let actualChecksum = 0;
      for (let index = 0; index < 512; index += 1) {
        actualChecksum += index >= 148 && index < 156 ? 0x20 : header[index];
      }
      if (actualChecksum !== expectedChecksum) throw new Error('tar header checksum mismatch');
      const name = cString(header, 0, 100);
      const prefix = cString(header, 345, 155);
      const original = prefix ? `${prefix}/${name}` : name;
      let normalized = original;
      while (normalized.startsWith('./')) normalized = normalized.slice(2);
      const type = String.fromCharCode(header[156] || 0x30);
      const directory = type === '5';
      const regular = type === '0';
      if (directory) normalized = normalized.replace(/\/+$/, '');
      if (!directory && !regular) {
        throw new Error(`unsupported archive entry ${original}: only directories and regular files are allowed`);
      }
      if (!(directory && (normalized === '' || normalized === '.'))) {
        const parts = normalized.split('/');
        if (!normalized || normalized.startsWith('/') || normalized.includes('\\')
            || parts.some(part => !part || part === '.' || part === '..'
              || /[\u0000-\u001f\u007f-\u009f]/u.test(part))
            || (parts[0] && parts[0].includes(':'))) {
          throw new Error(`unsafe archive path: ${JSON.stringify(original)}`);
        }
        if (seen.has(normalized)) throw new Error(`duplicate archive path: ${normalized}`);
        seen.add(normalized);
        members.push({ parts, directory });
      }
      const size = octal(header, 124, 12, 'tar member size');
      if (!Number.isSafeInteger(size) || size < 0) throw new Error('tar member size is invalid');
      if (regular) {
        total += size;
        if (total > d.CREATE_UPLOAD_LIMIT_BYTES) {
          throw new Error(`expanded source exceeds the ${d.CREATE_UPLOAD_LIMIT_BYTES}-byte upload limit`);
        }
        const contentStart = offset + 512;
        const contentEnd = contentStart + size;
        if (contentEnd > tar.length) throw new Error(`tar member ${original} is truncated`);
        const content = tar.subarray(contentStart, contentEnd);
        files.push({
          parts: normalized.split('/'),
          byte_length: size,
          sha256_of_raw_bytes: crypto.createHash('sha256').update(content).digest('hex'),
          content: Buffer.from(content),
        });
      } else if (size !== 0) {
        throw new Error(`directory entry ${original} has nonzero content`);
      }
      offset += 512 + Math.ceil(size / 512) * 512;
      if (offset > tar.length) throw new Error(`tar member ${original} is truncated`);
    }
    if (!ended) throw new Error('tar archive has no complete end marker');
    if (tar.subarray(offset).some(byte => byte !== 0)) throw new Error('tar archive has trailing nonzero data');
    if (!files.length) throw new Error('source archive contains no regular files');
    const topLevels = [...new Set(members.map(member => member.parts[0]))];
    const sourceRoot = topLevels.length === 1
      && !members.some(member => member.parts.length === 1 && !member.directory)
      ? topLevels[0] : '';
    const manifestFiles = files.map(file => ({
      relative_path: (sourceRoot ? file.parts.slice(1) : file.parts).join('/'),
      byte_length: file.byte_length,
      sha256_of_raw_bytes: file.sha256_of_raw_bytes,
    })).sort((left, right) => Buffer.from(left.relative_path).compare(Buffer.from(right.relative_path)));
    const sourceTreeSha256 = taggedHash('manifest-node', Buffer.from(canonicalJson(manifestFiles), 'utf-8'));
    return {
      ok: true,
      source_files: files.map(file => ({
        relative_path: (sourceRoot ? file.parts.slice(1) : file.parts).join('/'),
        content: file.content,
      })),
      metadata: {
        ok: true,
        entries,
        expanded_bytes: total,
        archive_sha256: crypto.createHash('sha256').update(buffer).digest('hex'),
        source_root: sourceRoot,
        source_manifest: {
          schema: 'trellis-source-tree-manifest/v1',
          files: manifestFiles,
          source_tree_sha256: sourceTreeSha256,
        },
        source_tree_sha256: sourceTreeSha256,
      },
    };
  } catch (error) {
    return { ok: false, error: `not an extractable .tar.gz archive: ${error.message}` };
  }
}

function normalizedLooseName(value) {
  const name = String(value || '');
  const parts = name.split('/');
  if (!name || Buffer.byteLength(name, 'utf-8') > 100 || name.startsWith('/')
      || name.includes('\\') || parts.some(part => !part || part === '.' || part === '..'
        || /[\u0000-\u001f\u007f-\u009f]/u.test(part))
      || (parts[0] && parts[0].includes(':'))) {
    throw new Error(`unsafe loose supporting-file name: ${JSON.stringify(name)}`);
  }
  return name;
}

function parseLooseSupportingBundle(buffer) {
  const newline = buffer.indexOf(0x0a);
  if (newline <= 0 || newline > 256 * 1024) {
    throw new Error('loose supporting upload lacks its bounded canonical header');
  }
  let header;
  try { header = JSON.parse(buffer.subarray(0, newline).toString('utf-8')); }
  catch (error) { throw new Error(`loose supporting upload header is invalid: ${error.message}`); }
  if (!header || header.schema !== LOOSE_SUPPORT_SCHEMA || !Array.isArray(header.files)
      || Object.keys(header).sort().join(',') !== 'files,schema') {
    throw new Error('loose supporting upload header has the wrong shape');
  }
  let offset = newline + 1;
  const seen = new Set();
  const files = [];
  for (const row of header.files) {
    if (!row || Object.keys(row).sort().join(',') !== 'byte_length,name'
        || !Number.isSafeInteger(row.byte_length) || row.byte_length < 0) {
      throw new Error('loose supporting upload contains a malformed file row');
    }
    const name = normalizedLooseName(row.name);
    if (seen.has(name)) throw new Error(`duplicate loose supporting-file name: ${name}`);
    seen.add(name);
    const end = offset + row.byte_length;
    if (end > buffer.length) throw new Error(`loose supporting file is truncated: ${name}`);
    files.push({ name, content: Buffer.from(buffer.subarray(offset, end)) });
    offset = end;
  }
  if (!files.length || offset !== buffer.length) {
    throw new Error(files.length ? 'loose supporting upload has trailing bytes' : 'no loose supporting files were uploaded');
  }
  return files;
}

function writeTarText(header, start, length, value, label) {
  const bytes = Buffer.from(value, 'utf-8');
  if (bytes.length > length) throw new Error(`${label} is too long for the canonical tar format`);
  bytes.copy(header, start);
}

function writeTarOctal(header, start, length, value, label) {
  const rendered = value.toString(8).padStart(length - 1, '0');
  if (rendered.length !== length - 1) throw new Error(`${label} is too large for the canonical tar format`);
  header.write(`${rendered}\0`, start, length, 'ascii');
}

function packLooseSupportingFiles(inputFiles) {
  const files = inputFiles.map(file => ({
    name: normalizedLooseName(file.name),
    content: Buffer.from(file.content),
  })).sort((left, right) => Buffer.from(left.name).compare(Buffer.from(right.name)));
  if (!files.length) throw new Error('no loose supporting files were uploaded');
  if (new Set(files.map(file => file.name)).size !== files.length) {
    throw new Error('loose supporting upload repeats a file name');
  }
  const chunks = [];
  for (const file of files) {
    const header = Buffer.alloc(512);
    writeTarText(header, 0, 100, file.name, 'supporting-file name');
    writeTarOctal(header, 100, 8, 0o644, 'mode');
    writeTarOctal(header, 108, 8, 0, 'owner');
    writeTarOctal(header, 116, 8, 0, 'group');
    writeTarOctal(header, 124, 12, file.content.length, 'file size');
    writeTarOctal(header, 136, 12, 0, 'mtime');
    header.fill(0x20, 148, 156);
    header[156] = 0x30;
    header.write('ustar\0', 257, 6, 'ascii');
    header.write('00', 263, 2, 'ascii');
    const checksum = header.reduce((sum, byte) => sum + byte, 0);
    header.write(`${checksum.toString(8).padStart(6, '0')}\0 `, 148, 8, 'ascii');
    chunks.push(header, file.content, Buffer.alloc((512 - (file.content.length % 512)) % 512));
  }
  chunks.push(Buffer.alloc(1024));
  return zlib.gzipSync(Buffer.concat(chunks), { level: 9, mtime: 0 });
}

function scanMissingImports(targetFiles, resolutionFiles) {
  const files = new Map((resolutionFiles || targetFiles)
    .map(file => [file.relative_path, file.content]));
  const declarations = new Set();
  for (const content of files.values()) {
    const text = content.toString('utf-8');
    for (const match of text.matchAll(/\b(?:pub\s+)?(?:mod|fn|struct|enum|trait|type|const|static)\s+([A-Za-z_][A-Za-z0-9_]*)/g)) {
      declarations.add(match[1]);
    }
  }
  const resolves = (kind, tail, sourcePath) => {
    const first = /^\s*([A-Za-z_][A-Za-z0-9_]*)/.exec(tail);
    if (!first) return true;
    const brace = /\{([^{}]*)\}/.exec(tail);
    const importedNames = (brace ? brace[1].split(',') : [tail.split('::').pop()])
      .map(name => /^\s*([A-Za-z_][A-Za-z0-9_]*)/.exec(name))
      .filter(Boolean)
      .map(match => match[1])
      .filter(name => name !== 'self');
    if (importedNames.length && importedNames.every(name => declarations.has(name))) return true;
    if (kind === 'crate' && declarations.has(first[1])) return true;
    let base = '';
    if (kind === 'super') {
      const parts = sourcePath.split('/');
      parts.pop();
      if (parts.length && sourcePath.endsWith('/mod.rs')) parts.pop();
      base = parts.join('/');
    }
    const prefix = base ? `${base}/${first[1]}` : first[1];
    return files.has(`${prefix}.rs`) || files.has(`${prefix}/mod.rs`)
      || files.has(`src/${prefix}.rs`) || files.has(`src/${prefix}/mod.rs`);
  };
  const missing = new Map();
  for (const { relative_path: sourcePath, content: bytes } of targetFiles) {
    let text;
    try { text = new TextDecoder('utf-8', { fatal: true }).decode(bytes); }
    catch { continue; }
    // This is intentionally only upload help: a mechanical text scan. Its
    // output never enters Genesis, adaptation routing, a checker, or a gate.
    const withoutComments = text.replace(/\/\*[\s\S]*?\*\//g, '').replace(/\/\/[^\n]*/g, '');
    const covered = [];
    for (const match of withoutComments.matchAll(/\buse\s+(crate|super)::([\s\S]*?);/g)) {
      const tail = match[2].replace(/\s+/g, ' ').trim();
      covered.push([match.index, match.index + match[0].length]);
      if (!resolves(match[1], tail, sourcePath)) {
        const path = `${match[1]}::${tail}`;
        missing.set(`${sourcePath}\0${path}`, { source_file: sourcePath, path });
      }
    }
    for (const match of withoutComments.matchAll(/\bcrate::([A-Za-z_][A-Za-z0-9_]*(?:::[A-Za-z_][A-Za-z0-9_]*)*)/g)) {
      if (covered.some(([start, end]) => match.index >= start && match.index < end)) continue;
      if (!resolves('crate', match[1], sourcePath)) {
        const path = `crate::${match[1]}`;
        missing.set(`${sourcePath}\0${path}`, { source_file: sourcePath, path });
      }
    }
  }
  return [...missing.values()].sort((left, right) => Buffer.from(`${left.path}\0${left.source_file}`)
    .compare(Buffer.from(`${right.path}\0${right.source_file}`)));
}

// Kept as a compatibility export for callers that still name the upload kind
// ``crate``; root discovery itself is source-generic.
const inspectCrateArchive = inspectSourceArchive;

function intakeGoalUpload(buffer, options) {
  const d = (options && options.deps) || deps;
  if (!d) throw new Error('PV create module is not installed');
  if (!Buffer.isBuffer(buffer) || buffer.length === 0) {
    return { ok: false, error: 'empty upload — send the GOAL.md bytes as the raw request body' };
  }
  if (buffer.includes(0)) {
    return { ok: false, error: 'binary content (NUL bytes) — upload a GOAL.md text file' };
  }
  let text;
  try {
    text = new TextDecoder('utf-8', { fatal: true }).decode(buffer);
  } catch {
    return { ok: false, error: 'GOAL.md must be valid UTF-8; transcoding is not permitted' };
  }
  const format = (options && options.query && options.query.format) || 'callable';
  if (!['native', 'callable'].includes(format)) return { ok: false, error: 'goal format must be native or callable' };
  let goalIds = null;
  if (format === 'native') {
    let collection;
    try { collection = JSON.parse(text); }
    catch { return { ok: false, error: 'Native goals must be a JSON collection with context and goals' }; }
    if (!collection || Array.isArray(collection) || !Array.isArray(collection.goals)
        || !collection.goals.length
        || (collection.context !== undefined && typeof collection.context !== 'string')
        || collection.goals.some(goal => !goal || typeof goal.id !== 'string' || !goal.id.trim()
          || typeof goal.text !== 'string' || !goal.text.trim())) {
      return { ok: false, error: 'Each native goal needs a nonempty string id and text; context must be text' };
    }
    goalIds = collection.goals.map(goal => goal.id);
    if (new Set(goalIds).size !== goalIds.length) return { ok: false, error: 'Native goal IDs must be unique' };
  }
  return {
    ok: true,
    goal_format: format,
    goal_ids: goalIds,
    bytes: Buffer.from(buffer),
    text,
    transcoded_from: null,
    warnings: [],
  };
}

function goalUploadRecord(buffer, options) {
  const intake = intakeGoalUpload(buffer, options);
  if (!intake.ok) return intake;
  return {
    ok: true,
    file_name: intake.goal_format === 'native' ? 'GOAL.json' : 'GOAL.md',
    content: intake.bytes,
    response: {
      goal_format: intake.goal_format,
      goal_count: intake.goal_ids && intake.goal_ids.length,
      goal_ids: intake.goal_ids,
      transcoded_from: intake.transcoded_from,
      warnings: intake.warnings,
    },
  };
}

function safeDisplayName(raw, fallback) {
  const clean = String(raw || '').replace(/[\x00-\x1f\x7f]/g, '').slice(0, 200);
  return clean ? clean.replace(/^.*[\\/]/, '') : fallback;
}

function uploadFile(id, name) {
  if (!deps.CREATE_UPLOAD_ID_RE.test(String(id || ''))) return null;
  return deps.path.join(deps.createUploadsRoot(), id, name);
}

function targetFilesForUpload(id) {
  const target = uploadFile(id, 'target.tar.gz');
  if (!target || !deps.fs.existsSync(target)) {
    throw new Error('target_upload_id does not name a stored target archive');
  }
  const intake = inspectSourceArchive(deps.fs.readFileSync(target));
  if (!intake.ok) throw new Error(intake.error);
  return intake.source_files;
}

const NON_BROWSER_ENV = [
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
  // Config is the recorded authority for this from-genesis invariant. Do not
  // let an ambient process-level override silently negate it.
  'TRELLIS_SYSTEM_FEEDBACK_HALT',
  // The browser path owns these reviewed framework pins; an ambient server
  // setting must not turn them into extra operator-controlled seed inputs.
  'RUST_REFERENCE_REV',
  'NOMICON_REV',
  'RUST_STD_REV',
];

function createChildEnvironment(base) {
  const env = {
    ...(base || {}),
    TRELLIS_PROJECTS_ROOT: deps.PROJECTS_ROOT,
    TRELLIS_CREATE_UPLOAD_LIMIT_BYTES: String(deps.CREATE_UPLOAD_LIMIT_BYTES),
  };
  // These hooks exist solely so the shell state-machine test can substitute
  // expensive external tools. A browser-started job must always execute the
  // real archive, extraction, setup, kernel, initialization, and runner path.
  for (const name of NON_BROWSER_ENV) delete env[name];
  return env;
}

function runCreateScript(args, timeoutMs, callback) {
  deps.execFile('bash', [deps.path.join(deps.TRELLIS_ROOT, 'scripts', 'pv_create_run.sh'), ...args], {
    env: createChildEnvironment(process.env),
    timeout: timeoutMs,
    maxBuffer: 8 * 1024 * 1024,
    encoding: 'utf-8',
  }, (error, stdout, stderr) => callback(error, String(stdout || ''), String(stderr || '')));
}

function scriptResponse(res, body) {
  return (error, stdout, stderr) => {
    if (error) {
      res.status(400).json({
        error: 'create_script_failed',
        detail: (stderr || stdout || error.message || '').trim().slice(-4000),
        exit_code: typeof error.code === 'number' ? error.code : null,
      });
      return;
    }
    res.json({ ok: true, output: stdout.trim(), ...(body || {}) });
  };
}

function requireOwnJob(slug, res) {
  const job = deps.readJsonSafe(deps.path.join(deps.createJobsRoot(), slug, 'job.json'));
  if (!job || job.create_flow !== 'prose-program-verification') {
    res.status(404).json({ error: 'create_job_not_found', detail: 'no prose verification create job has this slug' });
    return null;
  }
  return job;
}

function pvLauncherRoles(serverDeps, format = 'callable') {
  const d = serverDeps || deps;
  if (!d) throw new Error('PV create module is not installed');
  const config = JSON.parse(fs.readFileSync(goalTemplate(format), 'utf-8'));
  const labels = {
    'preparation.worker': 'Preparation worker',
    'preparation.reviewer': 'Preparation reviewer / route planner',
    'preparation.audit': 'Preparation auditor / route critic',
    'phase0.adapting_worker': 'Phase-0 adapting worker',
    'phase0.seam_repair_auditor': 'Phase-0 seam-repair auditor',
    'phase0.correspondence_agents': 'Phase-0 source-correspondence lane',
    stuck_math_audit: 'stuck math audit / assumptions lane',
  };
  return d.launcherRoleSettingsFromConfigTemplate(config).map(role => ({
    ...role,
    label: labels[role.key] || role.label,
  }));
}

function pvLauncherMetadata(serverDeps, format = 'callable') {
  const d = serverDeps || deps;
  const config = JSON.parse(fs.readFileSync(goalTemplate(format), 'utf-8'));
  const roles = pvLauncherRoles(d, format);
  return {
    template: goalTemplate(format),
    roles,
    goal_format: format,
    pinned_models: d.CREATE_PINNED_MODELS,
    default_model: d.CREATE_DEFAULT_MODEL,
    default_effort: d.CREATE_DEFAULT_EFFORT,
    presets: d.CREATE_PRESETS,
    default_preset: d.CREATE_DEFAULT_PRESET,
    lean_parallelism: recommendedLeanParallelism(),
    guide_roles: d.launcherGuideRoles(roles, {
      audit_roles: format === 'native'
        ? ['preparation.audit']
        : ['phase0.seam_repair_auditor', 'phase0.correspondence_agents'],
    }),
    defaults_note: format === 'native'
      ? 'Preparation and main runtime roles default to Sol at xhigh. Roles may share a model.'
      : 'Defaults come from the explicit PV config template. Phase-0 audit lanes start independent of the adapting worker.',
    allow_same_model_lanes: config.phase0.allow_same_model_lanes,
    phase0_independence: format === 'native' ? null : {
      worker_role: 'phase0.adapting_worker',
      audit_roles: [
        'phase0.seam_repair_auditor',
        'phase0.correspondence_agents',
      ],
    },
  };
}

function validatedPvSettings(body, serverDeps) {
  const d = serverDeps || deps;
  const roles = pvLauncherRoles(d, body.goal_format || 'callable');
  const settings = d.validatedOverrides(body, { roles });
  const selected = settings.role_overrides || {};
  settings.role_overrides = Object.fromEntries(roles.map(role => {
    const choice = selected[role.key] || {};
    const provider = choice.provider || role.provider;
    return [role.key, {
      provider,
      ...(choice.base_url ? { base_url: choice.base_url } : {}),
      model: choice.model || role.model,
      effort: choice.effort || role.effort,
    }];
  }));
  // Lanes may share the adapting worker's model; the launcher never asks
  // genesis to refuse a launch over it.
  settings.allow_same_model_lanes = true;
  return settings;
}

function install(serverDeps) {
  if (installed) return;
  installed = true;
  deps = serverDeps;

  deps.registerCreateUploadKind('goal', (buffer, options) => goalUploadRecord(buffer, options));
  deps.registerCreateUploadKind('target', (buffer) => {
    const intake = inspectSourceArchive(buffer);
    if (!intake.ok) return intake;
    return {
      ok: true,
      file_name: 'target.tar.gz',
      content: buffer,
      response: {
        archive: intake.metadata,
        missing_imports: scanMissingImports(intake.source_files),
        warnings: [],
      },
    };
  });
  deps.registerCreateUploadKind('supporting', (buffer, options) => {
    let archive = buffer;
    try {
      if (String(options.query.format || '') === 'loose') {
        archive = packLooseSupportingFiles(parseLooseSupportingBundle(buffer));
      } else if (options.query.format && String(options.query.format) !== 'archive') {
        return { ok: false, error: 'supporting upload format must be archive or loose' };
      }
      const intake = inspectSourceArchive(archive);
      if (!intake.ok) return intake;
      const targetFiles = targetFilesForUpload(options.query.target_upload_id);
      return {
        ok: true,
        file_name: 'support.tar.gz',
        content: archive,
        response: {
          archive: intake.metadata,
          packed_loose_files: String(options.query.format || '') === 'loose',
          missing_imports: scanMissingImports(
            targetFiles, [...targetFiles, ...intake.source_files],
          ),
          warnings: [],
        },
      };
    } catch (error) {
      return { ok: false, error: error.message };
    }
  });

  deps.registerControlReadRoute('pv-create-jobs.json', (_req, res) => {
    try {
      res.json({
        generated_ts: Date.now(),
        projects_root: deps.PROJECTS_ROOT,
        jobs: deps.createJobsIndex().filter(job => (
          job.create_flow === 'prose-program-verification'
        )),
        ...pvLauncherMetadata(),
        native: pvLauncherMetadata(deps, 'native'),
        slug_rule: String(deps.CREATE_SLUG_RE),
      });
    } catch (error) { res.status(500).json({ error: error.message }); }
  });

  deps.registerControlRoute('pv-create-jobs', [deps.express.json()], (req, res) => {
    const body = req.body || {};
    const slug = String(body.slug || '');
    const issue = deps.createSlugIssue(slug);
    if (issue) {
      res.status(issue.code === 'invalid_slug' ? 400 : 409).json({ error: issue.code, ...issue });
      return;
    }
    const format = body.goal_format || 'callable';
    if (!['native', 'callable'].includes(format)) {
      res.status(400).json({ error: 'bad_request', detail: 'goal_format must be native or callable' });
      return;
    }
    const goalName = format === 'native' ? 'GOAL.json' : 'GOAL.md';
    const goal = uploadFile(body.goal_upload_id, goalName);
    const target = uploadFile(body.target_upload_id, 'target.tar.gz');
    const support = body.support_upload_id
      ? uploadFile(body.support_upload_id, 'support.tar.gz') : null;
    if (!goal || !deps.fs.existsSync(goal)) {
      res.status(400).json({ error: 'upload_missing', detail: `goal_upload_id does not name a stored ${goalName} upload` });
      return;
    }
    if (!target || !deps.fs.existsSync(target)) {
      res.status(400).json({ error: 'upload_missing', detail: 'target_upload_id does not name a stored target archive' });
      return;
    }
    if (body.support_upload_id && (!support || !deps.fs.existsSync(support))) {
      res.status(400).json({ error: 'upload_missing', detail: 'support_upload_id does not name a stored supporting archive' });
      return;
    }
    let settings;
    try {
      settings = validatedPvSettings(body);
    } catch (error) {
      res.status(400).json({ error: 'bad_request', detail: error.message });
      return;
    }
    const args = [
      'start', slug,
      '--goal-format', format,
      '--goal', goal,
      '--target', target,
      '--goal-name', safeDisplayName(body.goal_name, goalName),
      '--target-name', safeDisplayName(body.target_name, 'target.tar.gz'),
    ];
    if (support) {
      args.push('--support', support,
        '--support-name', safeDisplayName(body.support_name, 'support.tar.gz'));
    }
    for (const [role, selected] of Object.entries(settings.role_overrides || {})) {
      if (selected.provider) args.push('--role-provider', `${role}=${selected.provider}`);
      if (selected.base_url) args.push('--role-base-url', `${role}=${selected.base_url}`);
      if (selected.model) args.push('--role-model', `${role}=${selected.model}`);
      if (selected.effort) args.push('--role-effort', `${role}=${selected.effort}`);
    }
    if (settings.lean_parallelism !== undefined) args.push('--lean-parallelism', String(settings.lean_parallelism));
    if (settings.remote_url) args.push('--remote-url', settings.remote_url);
    args.push('--allow-same-model-lanes', settings.allow_same_model_lanes ? 'on' : 'off');
    runCreateScript(args, 30000, scriptResponse(res, { slug }));
  });

  deps.registerControlReadRoute('pv-create-status/:slug', (req, res) => {
    const slug = String(req.params.slug || '');
    if (!deps.CREATE_SLUG_RE.test(slug)) {
      res.status(400).json({ error: 'invalid_slug' });
      return;
    }
    if (!requireOwnJob(slug, res)) return;
    try { res.json(deps.createStatusPayload(slug)); }
    catch (error) { res.status(500).json({ error: error.message }); }
  });

  deps.registerControlRoute('pv-create-jobs/:slug/retry', [deps.express.json()], (req, res) => {
    const slug = String(req.params.slug || '');
    if (!deps.CREATE_SLUG_RE.test(slug)) { res.status(400).json({ error: 'invalid_slug' }); return; }
    if (!requireOwnJob(slug, res)) return;
    runCreateScript(['retry', slug], 30000, scriptResponse(res, { slug }));
  });

  deps.registerControlRoute('pv-create-jobs/:slug/stop', [deps.express.json()], (req, res) => {
    const slug = String(req.params.slug || '');
    if (!deps.CREATE_SLUG_RE.test(slug)) { res.status(400).json({ error: 'invalid_slug' }); return; }
    if (!requireOwnJob(slug, res)) return;
    runCreateScript(['stop', slug], 60000, scriptResponse(res, { slug }));
  });

  deps.registerControlRoute('pv-create-jobs/:slug/delete', [deps.express.json()], (req, res) => {
    const slug = String(req.params.slug || '');
    if (!deps.CREATE_SLUG_RE.test(slug)) { res.status(400).json({ error: 'invalid_slug' }); return; }
    if (!requireOwnJob(slug, res)) return;
    const args = ['delete', slug];
    for (const doomed of Array.isArray(req.body && req.body.paths) ? req.body.paths : []) {
      args.push('--confirm', String(doomed));
    }
    runCreateScript(args, 60000, (error, stdout, stderr) => {
      if (error && error.code === 3) {
        let payload = null;
        try { payload = JSON.parse(stdout); } catch {}
        res.status(409).json(payload || { error: 'confirmation_required', detail: stdout.trim() });
        return;
      }
      scriptResponse(res, { slug })(error, stdout, stderr);
    });
  });

  // The client is inlined rather than referenced by path: under the
  // control-token prefix a relative `src` resolves to a project-shaped URL
  // that the project route redirects and answers with the landing HTML, so
  // the script never loaded and the launcher button never appeared.
  deps.registerLandingHtmlTransform((html) => {
    const client = fs.readFileSync(path.join(__dirname, 'public', 'pv_create_client.js'), 'utf-8')
      .replace(/<\/script/gi, '<\\/script');
    return html.replace('</body>', `<script>\n${client}\n</script>\n</body>`);
  });
}

module.exports = {
  install,
  inspectSourceArchive,
  inspectCrateArchive,
  intakeGoalUpload,
  goalUploadRecord,
  parseLooseSupportingBundle,
  packLooseSupportingFiles,
  scanMissingImports,
  PV_CONFIG_TEMPLATE,
  NATIVE_CONFIG_TEMPLATE,
  pvLauncherRoles,
  pvLauncherMetadata,
  validatedPvSettings,
  LOOSE_SUPPORT_SCHEMA,
  safeDisplayName,
  createChildEnvironment,
  NON_BROWSER_ENV,
};
