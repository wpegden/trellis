const express = require('express');
const compression = require('compression');
const fs = require('fs');
const path = require('path');
const os = require('os');
const { execSync, execFileSync, spawn } = require('child_process');
const { StringDecoder } = require('string_decoder');

const app = express();
// gzip all responses. Cuts the ~12 MB viewer-state payload to ~1.5 MB on the
// wire (~8x) and helps every other JSON endpoint similarly.
app.use(compression());
const PORT = process.env.PORT || 3300;
const BASE = process.env.BASE_PATH || '/trellis';
const PROMPTS_BASE = process.env.PROMPTS_BASE || '/prompts';
const STATIC_OUT = process.env.STATIC_OUT || path.join(os.homedir(), 'trellis-web');
const PROJECTS_ROOT = process.env.PROJECTS_ROOT || path.join(os.homedir(), 'math');
const DEFAULT_PROJECT_SLUG = process.env.DEFAULT_PROJECT_SLUG || '';
const LEGACY_REPO_PATH = process.env.REPO_PATH || '';
const TRELLIS_ROOT = path.resolve(__dirname, '..');
const TMUX_SOCKET = process.env.TRELLIS_TMUX_SOCKET || 'trellis';
function tmuxArgs(...args) { return ['-L', TMUX_SOCKET, ...args]; }

function isValidProjectSlug(slug) {
  return typeof slug === 'string' && /^[A-Za-z0-9._-]+$/.test(slug);
}

function configPathForRepo(repoPath) {
  if (fs.existsSync(path.join(repoPath, 'trellis.config.json'))) {
    return path.join(repoPath, 'trellis.config.json');
  }
  return path.join(repoPath, 'lagent.config.json');
}

function repoTypeForRepo(repoPath) {
  if (fs.existsSync(path.join(repoPath, 'trellis.config.json'))) return 'trellis';
  if (fs.existsSync(path.join(repoPath, 'lagent.config.json'))) return 'legacy';
  return '';
}

function viewerApiDir(projectInfo) {
  if (projectInfo.repoType === 'trellis') {
    return path.join(projectInfo.stateDir, 'viewer');
  }
  return path.join(projectInfo.repoPath, '.agent-supervisor', 'viewer');
}

function viewerTempDir(stateDir) {
  return path.join(stateDir, 'tmp', 'viewer');
}

function chatsRepoDir(projectInfo) {
  if (projectInfo.repoType === 'trellis') {
    return path.join(projectInfo.stateDir, 'chats');
  }
  return path.join(projectInfo.repoPath, '.agent-supervisor', 'chats');
}

function chatRepoCandidates(projectInfo) {
  const repos = [];
  const addRepo = (repoPath) => {
    if (!repoPath || !fs.existsSync(path.join(repoPath, '.git'))) return;
    if (!repos.includes(repoPath)) repos.push(repoPath);
  };
  addRepo(chatsRepoDir(projectInfo));
  if (projectInfo.repoType !== 'trellis') return repos;

  const projectNames = new Set([projectInfo.slug, path.basename(projectInfo.repoPath)].filter(Boolean));
  try {
    projectNames.add(path.basename(fs.realpathSync(projectInfo.repoPath)));
  } catch {}
  let entries = [];
  try {
    entries = fs.readdirSync(PROJECTS_ROOT, { withFileTypes: true })
      .filter(entry => entry.isDirectory())
      .map(entry => entry.name)
      .sort()
      .reverse();
  } catch {
    return repos;
  }
  for (const name of entries) {
    if (![...projectNames].some(projectName => name.startsWith(`${projectName}-rewind-quarantine-`))) {
      continue;
    }
    addRepo(path.join(PROJECTS_ROOT, name, 'repo', '.trellis', 'chats'));
  }
  return repos;
}

function chatRepoForCycle(projectInfo, cycle) {
  for (const repoPath of chatRepoCandidates(projectInfo)) {
    if (hasChatCycleTag(repoPath, cycle)) return repoPath;
  }
  return null;
}

function discoverProjects() {
  if (LEGACY_REPO_PATH) {
    const repoType = repoTypeForRepo(LEGACY_REPO_PATH);
    return [{
      slug: path.basename(LEGACY_REPO_PATH),
      repoPath: LEGACY_REPO_PATH,
      repoType,
      stateDir: repoType === 'trellis'
        ? path.join(LEGACY_REPO_PATH, '.trellis')
        : path.join(LEGACY_REPO_PATH, '.agent-supervisor'),
    }];
  }
  if (!fs.existsSync(PROJECTS_ROOT)) return [];
  const entryIsProjectDir = (entry) => {
    if (entry.isDirectory()) return true;
    if (!entry.isSymbolicLink()) return false;
    try {
      return fs.statSync(path.join(PROJECTS_ROOT, entry.name)).isDirectory();
    } catch {
      return false;
    }
  };
  return fs.readdirSync(PROJECTS_ROOT, { withFileTypes: true })
    .filter(entry => entryIsProjectDir(entry) && isValidProjectSlug(entry.name))
    .map(entry => {
      const repoPath = path.join(PROJECTS_ROOT, entry.name);
      const repoType = repoTypeForRepo(repoPath);
      return {
        slug: entry.name,
        repoPath,
        repoType,
        stateDir: repoType === 'trellis'
          ? path.join(repoPath, '.trellis')
          : path.join(repoPath, '.agent-supervisor'),
      };
    })
    .filter(entry => !!entry.repoType)
    .sort((a, b) => a.slug.localeCompare(b.slug));
}

function defaultProjectSlug() {
  if (DEFAULT_PROJECT_SLUG) return DEFAULT_PROJECT_SLUG;
  const projects = discoverProjects();
  return projects.length ? projects[0].slug : '';
}

function defaultPromptsProjectSlug() {
  const projects = discoverProjects();
  if (projects.some(project => project.slug === 'current')) return 'current';
  return defaultProjectSlug();
}

function resolveRepoPath(project) {
  const slug = project || defaultProjectSlug();
  if (!isValidProjectSlug(slug)) throw new Error(`Invalid project: ${project}`);
  const repoPath = LEGACY_REPO_PATH && slug === path.basename(LEGACY_REPO_PATH)
    ? LEGACY_REPO_PATH
    : path.join(PROJECTS_ROOT, slug);
  const repoType = repoTypeForRepo(repoPath);
  if (!(LEGACY_REPO_PATH && repoPath === LEGACY_REPO_PATH) && !repoType) {
    throw new Error(`Unknown project: ${slug}`);
  }
  const stateDir = repoType === 'trellis'
    ? path.join(repoPath, '.trellis')
    : path.join(repoPath, '.agent-supervisor');
  return { slug, repoPath, repoType, stateDir };
}

function readJsonFile(filePath) {
  return JSON.parse(fs.readFileSync(filePath, 'utf-8'));
}

function forEachFileLine(filePath, onLine) {
  const fd = fs.openSync(filePath, 'r');
  const decoder = new StringDecoder('utf8');
  const buf = Buffer.allocUnsafe(1024 * 1024);
  let pending = '';
  try {
    while (true) {
      const bytes = fs.readSync(fd, buf, 0, buf.length, null);
      if (!bytes) break;
      const text = pending + decoder.write(buf.subarray(0, bytes));
      let start = 0;
      while (true) {
        const idx = text.indexOf('\n', start);
        if (idx < 0) break;
        onLine(text.slice(start, idx));
        start = idx + 1;
      }
      pending = text.slice(start);
    }
    pending += decoder.end();
    if (pending) onLine(pending);
  } finally {
    fs.closeSync(fd);
  }
}

// Stream `filePath` starting at byte `startOffset`. Yields each newline-
// terminated line via `onLine`; the trailing partial line (no `\n`) is left
// unconsumed. Returns the byte offset right after the last `\n` consumed —
// always a safe resume point on the next call. Used for incremental
// tail-reads of append-only logs.
//
// Works in raw bytes (mirroring the Python helper in chat_history.py): the
// previous StringDecoder-based implementation lost the exact byte boundary
// when a multi-byte UTF-8 codepoint straddled the 1 MiB read boundary
// (decoder-buffered continuation bytes weren't reflected in `pending`'s
// byte length, so the returned resume offset was too high and the next
// call skipped 1–3 bytes — corrupting the following JSON record).
function forEachFileLineFromOffset(filePath, startOffset, onLine) {
  const fd = fs.openSync(filePath, 'r');
  const buf = Buffer.allocUnsafe(1024 * 1024);
  let pending = Buffer.alloc(0);
  let position = startOffset;        // next file byte to read
  let consumedOffset = startOffset;  // file offset just past last consumed '\n'
  try {
    while (true) {
      const bytes = fs.readSync(fd, buf, 0, buf.length, position);
      if (!bytes) break;
      position += bytes;
      // `chunk` is either the freshly-read slice (a view into `buf`, only
      // safe within this iteration) or a fresh buffer concatenating leftover
      // bytes from the previous iteration with the new read.
      const chunk = pending.length
        ? Buffer.concat([pending, buf.subarray(0, bytes)])
        : buf.subarray(0, bytes);
      let start = 0;
      while (true) {
        const idx = chunk.indexOf(0x0A, start);  // '\n'
        if (idx < 0) break;
        onLine(chunk.subarray(start, idx).toString('utf8'));
        consumedOffset += (idx - start) + 1;
        start = idx + 1;
      }
      // The trailing partial-line bytes become next iteration's `pending`.
      // Must be its own buffer because `buf` will be overwritten on the next
      // read; Buffer.from copies.
      const trailing = chunk.subarray(start);
      pending = trailing.length ? Buffer.from(trailing) : Buffer.alloc(0);
    }
    return consumedOffset;
  } finally {
    fs.closeSync(fd);
  }
}

function git(repoPath, args) {
  return execSync(`git ${args}`, {
    cwd: repoPath,
    encoding: 'utf-8',
    timeout: 10000,
    stdio: ['ignore', 'pipe', 'ignore'],
  }).trim();
}

const gitRefExistsCache = new Set();

function gitRefExists(repoPath, ref) {
  const key = `${repoPath}\0${ref}`;
  if (gitRefExistsCache.has(key)) return true;
  try {
    execFileSync('git', ['-C', repoPath, 'rev-parse', '--verify', '--quiet', `${ref}^{commit}`], {
      encoding: 'utf-8',
      timeout: 10000,
      stdio: ['ignore', 'ignore', 'ignore'],
    });
    gitRefExistsCache.add(key);
    return true;
  } catch {
    return false;
  }
}

function chatCycleTag(cycle) {
  return `cycle-${Number(cycle)}`;
}

function hasChatCycleTag(chatsRepo, cycle) {
  const n = Number(cycle);
  return Number.isFinite(n) && gitRefExists(chatsRepo, chatCycleTag(n));
}

function trellisAdapter(projectInfo, command, extraArgs = [], stdinObject = null) {
  const env = {
    ...process.env,
    PYTHONPATH: process.env.PYTHONPATH
      ? `${TRELLIS_ROOT}:${process.env.PYTHONPATH}`
      : TRELLIS_ROOT,
  };
  const args = ['-m', 'trellis.viewer_adapter', command, projectInfo.repoPath, ...extraArgs];
  const options = {
    cwd: TRELLIS_ROOT,
    env,
    encoding: 'utf-8',
    timeout: 30000,
    maxBuffer: 32 * 1024 * 1024,
  };
  if (stdinObject !== null) {
    options.input = JSON.stringify(stdinObject);
  }
  const raw = execFileSync('python3', args, options);
  return JSON.parse(raw);
}

// Async variant of `trellisAdapter` — uses `spawn` so the Node.js event
// loop stays unblocked while python is computing. Use this from any
// endpoint that is HOT (called on every page load / auto-refresh / tab
// open) so concurrent browsers don't serialize behind each other's
// `execFileSync` calls. Returns a Promise of the parsed JSON payload.
//
// Pairs with `_adapterInFlightOnce` for request coalescing: callers
// that want concurrent fetches to share one python spawn should wrap
// the call with `_adapterInFlightOnce(key, () => trellisAdapterAsync(…))`.
function trellisAdapterAsync(projectInfo, command, extraArgs = [], stdinObject = null) {
  return new Promise((resolve, reject) => {
    const env = {
      ...process.env,
      PYTHONPATH: process.env.PYTHONPATH
        ? `${TRELLIS_ROOT}:${process.env.PYTHONPATH}`
        : TRELLIS_ROOT,
    };
    const args = ['-m', 'trellis.viewer_adapter', command, projectInfo.repoPath, ...extraArgs];
    const child = spawn('python3', args, {
      cwd: TRELLIS_ROOT,
      env,
      stdio: ['pipe', 'pipe', 'pipe'],
    });
    let stdout = '';
    let stderr = '';
    let stdoutBytes = 0;
    const MAX = 32 * 1024 * 1024;
    const timer = setTimeout(() => {
      try { child.kill('SIGTERM'); } catch {}
      reject(new Error(`trellisAdapterAsync(${command}) timed out after 30s`));
    }, 30000);
    child.stdout.on('data', (chunk) => {
      stdoutBytes += chunk.length;
      if (stdoutBytes > MAX) {
        try { child.kill('SIGTERM'); } catch {}
        reject(new Error(`trellisAdapterAsync(${command}) stdout exceeded ${MAX} bytes`));
        return;
      }
      stdout += chunk.toString('utf-8');
    });
    child.stderr.on('data', (chunk) => { stderr += chunk.toString('utf-8'); });
    child.on('error', (err) => { clearTimeout(timer); reject(err); });
    child.on('close', (code) => {
      clearTimeout(timer);
      if (code !== 0) {
        reject(new Error(`trellisAdapterAsync(${command}) exited ${code}: ${stderr.slice(0, 500)}`));
        return;
      }
      try { resolve(JSON.parse(stdout)); }
      catch (e) { reject(new Error(`trellisAdapterAsync(${command}) bad JSON: ${e.message}`)); }
    });
    if (stdinObject !== null) {
      try { child.stdin.end(JSON.stringify(stdinObject)); }
      catch (e) { /* child may have already errored */ }
    } else {
      try { child.stdin.end(); } catch {}
    }
  });
}

// ---------------------------------------------------------------------------
// Historical-cycle cache
//
// Past cycles are immutable: once `cycle-N` is tagged, its viewer_state, chats,
// and diff never change. The slow path is `python3 -m trellis.viewer_adapter
// state-at N` (cold-start ~150-300ms each) and chained `git show` calls. Those
// dominate latency when the user drags the cycle slider.
//
// Strategy: in-memory cache, keyed by (projectKey, kind, cycle), populated on
// first request. Historical prewarm exists as an opt-in mode, but it is
// disabled by default because the underlying adapter/git calls are synchronous
// and can block current DAG loads on large live runs.
// ---------------------------------------------------------------------------

function projectCacheKey(projectInfo) {
  return `${projectInfo.repoPath}::${projectInfo.repoType || ''}`;
}

const cycleStateCache = new Map();   // key: `${projectKey}|${cycle}` -> data
const cycleChatsCache = new Map();   // key: `${projectKey}|${cycle}` -> data
const cycleDiffCache = new Map();    // key: `${projectKey}|${cycle}` -> string
const cyclesListCache = new Map();   // key: projectKey -> { data, ts }
const CYCLES_LIST_TTL_MS = 30 * 1000;
const ENABLE_HISTORICAL_PREWARM = process.env.TRELLIS_VIEWER_PREWARM === '1';

function cycleEntryKey(projectInfo, cycle) {
  return `${projectCacheKey(projectInfo)}|${cycle}`;
}

function getCachedHistoricalViewerState(projectInfo, cycle) {
  const key = cycleEntryKey(projectInfo, cycle);
  if (cycleStateCache.has(key)) return cycleStateCache.get(key);
  const data = readHistoricalViewerState(projectInfo, cycle);
  // augmentViewerStateClosure and thinViewerStatePayload are function
  // declarations defined later in the file, so they're hoisted.
  if (typeof augmentViewerStateClosure === 'function') {
    augmentViewerStateClosure(data);
  }
  if (typeof thinViewerStatePayload === 'function') {
    thinViewerStatePayload(data, projectCacheKey(projectInfo));
  }
  cycleStateCache.set(key, data);
  return data;
}

function getCachedHistoricalChats(projectInfo, cycle) {
  const key = cycleEntryKey(projectInfo, cycle);
  if (cycleChatsCache.has(key)) return cycleChatsCache.get(key);
  const data = readHistoricalChats(projectInfo, cycle);
  cycleChatsCache.set(key, data);
  return data;
}

function getCachedCycleDiff(projectInfo, cycle) {
  const key = cycleEntryKey(projectInfo, cycle);
  if (cycleDiffCache.has(key)) return cycleDiffCache.get(key);
  const data = getCycleDiff(projectInfo.repoPath, cycle);
  cycleDiffCache.set(key, data);
  return data;
}

function getCachedCyclesList(projectInfo) {
  const key = projectCacheKey(projectInfo);
  const entry = cyclesListCache.get(key);
  const now = Date.now();
  if (entry && (now - entry.ts) < CYCLES_LIST_TTL_MS) return entry.data;
  const data = getCyclesFromGit(projectInfo);
  cyclesListCache.set(key, { data, ts: now });
  if (ENABLE_HISTORICAL_PREWARM) schedulePrewarm(projectInfo, data);
  return data;
}

// Optional historical prewarm: walk all known cycles and prefetch state+chats.
// Each item still performs synchronous work, so this is off unless explicitly
// enabled with TRELLIS_VIEWER_PREWARM=1.

const prewarmQueued = new Set();   // projectKey|cycle keys already enqueued
const prewarmQueue = [];           // entries: { projectInfo, cycle }
let prewarmWorkerRunning = false;

function schedulePrewarm(projectInfo, cyclesList) {
  if (!Array.isArray(cyclesList)) return;
  for (const c of cyclesList) {
    const cycle = (c && typeof c === 'object') ? c.cycle : c;
    if (!Number.isInteger(cycle)) continue;
    const key = cycleEntryKey(projectInfo, cycle);
    if (prewarmQueued.has(key)) continue;
    if (cycleStateCache.has(key) && cycleChatsCache.has(key)) continue;
    prewarmQueued.add(key);
    prewarmQueue.push({ projectInfo, cycle });
  }
  if (!prewarmWorkerRunning) startPrewarmWorker();
}

function startPrewarmWorker() {
  prewarmWorkerRunning = true;
  const tick = () => {
    const item = prewarmQueue.shift();
    if (!item) {
      prewarmWorkerRunning = false;
      return;
    }
    const { projectInfo, cycle } = item;
    const stateKey = cycleEntryKey(projectInfo, cycle);
    try {
      if (!cycleStateCache.has(stateKey)) getCachedHistoricalViewerState(projectInfo, cycle);
    } catch {}
    try {
      if (!cycleChatsCache.has(stateKey)) getCachedHistoricalChats(projectInfo, cycle);
    } catch {}
    setImmediate(tick);
  };
  setImmediate(tick);
}

function readLiveViewerState(projectInfo) {
  if (projectInfo.repoType === 'trellis') {
    return trellisAdapter(projectInfo, 'live-state');
  }
  return readJsonFile(path.join(projectInfo.repoPath, '.agent-supervisor', 'viewer_state.json'));
}

function readHistoricalViewerState(projectInfo, cycle) {
  if (projectInfo.repoType === 'trellis') {
    return trellisAdapter(projectInfo, 'state-at', [String(cycle)]);
  }
  const tag = `cycle-${cycle}`;
  const raw = git(projectInfo.repoPath, `show ${tag}:.agent-supervisor/viewer_state.json`);
  return JSON.parse(raw);
}

function chatCycleDir(cycle) {
  return `cycle-${String(cycle).padStart(4, '0')}`;
}

function readTextFileSafe(filePath) {
  try {
    return fs.readFileSync(filePath, 'utf-8');
  } catch {
    return '';
  }
}

function listWorkingTreeChatArtifacts(repoPath, cycle) {
  const root = path.join(repoPath, chatCycleDir(cycle));
  if (!fs.existsSync(root)) return [];
  return sortArtifactNames(fs.readdirSync(root, { withFileTypes: true })
    .filter(entry => entry.isDirectory())
    .map(entry => entry.name)
  );
}

function listGitChatArtifacts(repoPath, cycle) {
  const chatsRepo = repoPath;
  if (!fs.existsSync(path.join(chatsRepo, '.git'))) return [];
  if (!hasChatCycleTag(chatsRepo, cycle)) return [];
  const tag = chatCycleTag(cycle);
  const prefix = chatCycleDir(cycle) + '/';
  try {
    const files = git(chatsRepo, `ls-tree -r --name-only ${tag} -- ${prefix}`)
      .split('\n')
      .filter(Boolean);
    return sortArtifactNames(Array.from(new Set(
      files
        .filter(name => name.startsWith(prefix))
        .map(name => name.slice(prefix.length).split('/')[0])
        .filter(Boolean)
    )));
  } catch {
    return [];
  }
}

function readWorkingTreeChatFiles(repoPath, cycle, artifact) {
  const dir = path.join(repoPath, chatCycleDir(cycle), artifact);
  return {
    prompt: readTextFileSafe(path.join(dir, 'prompt.txt')),
    output: readTextFileSafe(path.join(dir, 'output.log')),
    transcriptJsonl: readTextFileSafe(path.join(dir, 'transcript.jsonl')),
    transcriptJson: readTextFileSafe(path.join(dir, 'transcript.json')),
  };
}

function readGitChatFiles(repoPath, cycle, artifact, gitPrefix = null) {
  const chatsRepo = repoPath;
  if (!hasChatCycleTag(chatsRepo, cycle)) {
    return { prompt: '', output: '', transcriptJsonl: '', transcriptJson: '' };
  }
  const tag = chatCycleTag(cycle);
  const base = `${gitPrefix || chatCycleDir(cycle)}/${artifact}`;
  const read = (name) => {
    try {
      return git(chatsRepo, `show ${tag}:${base}/${name}`);
    } catch {
      return '';
    }
  };
  return {
    prompt: read('prompt.txt'),
    output: read('output.log'),
    transcriptJsonl: read('transcript.jsonl'),
    transcriptJson: read('transcript.json'),
  };
}

function artifactTitle(name) {
  let attempt = null;
  let base = name;
  let m = name.match(/^(.*)_attempt_(\d+)$/);
  if (m) {
    base = m[1];
    attempt = Number(m[2]);
  }
  if (base === 'worker_handoff') return attempt ? `Worker attempt ${attempt}` : 'Worker';
  if (base === 'reviewer_decision') return attempt ? `Reviewer attempt ${attempt}` : 'Reviewer';
  m = base.match(/^correspondence_result_(\d+)$/);
  if (m) return attempt ? `Correspondence ${Number(m[1]) + 1} attempt ${attempt}` : `Correspondence ${Number(m[1]) + 1}`;
  m = base.match(/^nl_proof_(.+)_(\d+)$/);
  if (m) {
    const title = `Soundness ${m[1]} (${Number(m[2]) + 1})`;
    return attempt ? `${title} attempt ${attempt}` : title;
  }
  const fallback = base.replace(/_/g, ' ');
  return attempt ? `${fallback} attempt ${attempt}` : fallback;
}

function artifactSortKey(name) {
  let attempt = 0;
  let base = name;
  let m = name.match(/^(.*)_attempt_(\d+)$/);
  if (m) {
    base = m[1];
    attempt = Number(m[2]);
  }
  if (base === 'worker_handoff') return [0, 0, '', attempt, name];
  if (base === 'reviewer_decision') return [3, 0, '', attempt, name];
  m = base.match(/^correspondence_result_(\d+)$/);
  if (m) return [1, Number(m[1]), '', attempt, name];
  m = base.match(/^nl_proof_(.+)_(\d+)$/);
  if (m) return [2, Number(m[2]), String(m[1]), attempt, name];
  return [4, 0, base, attempt, name];
}

function sortArtifactNames(names) {
  return [...names].sort((a, b) => {
    const ka = artifactSortKey(a);
    const kb = artifactSortKey(b);
    for (let i = 0; i < ka.length; i++) {
      if (ka[i] < kb[i]) return -1;
      if (ka[i] > kb[i]) return 1;
    }
    return 0;
  });
}

function collectTextParts(value, parts) {
  if (typeof value === 'string') {
    const trimmed = value.trim();
    if (trimmed) parts.push(trimmed);
    return;
  }
  if (Array.isArray(value)) {
    for (const item of value) collectTextParts(item, parts);
    return;
  }
  if (!value || typeof value !== 'object') return;
  if (typeof value.text === 'string') {
    const trimmed = value.text.trim();
    if (trimmed) parts.push(trimmed);
  }
  for (const key of ['content', 'parts', 'chunks', 'value']) {
    if (key in value) collectTextParts(value[key], parts);
  }
}

function normalizeTranscriptEntry(role, text, kind = 'message', title = '') {
  const trimmed = (text || '').trim();
  if (!trimmed) return null;
  return { role: role || 'entry', kind, title: title || '', text: trimmed };
}

function parseCodexOutputEntries(text) {
  const entries = [];
  for (const rawLine of (text || '').split(/\r?\n/)) {
    const line = rawLine.trim();
    if (!line) continue;
    let rec;
    try {
      rec = JSON.parse(line);
    } catch {
      continue;
    }
    if (rec.type === 'item.completed' && rec.item && rec.item.type === 'agent_message') {
      const entry = normalizeTranscriptEntry('assistant', rec.item.text || '', 'message', 'Assistant');
      if (entry) entries.push(entry);
      continue;
    }
    if (rec.item && rec.item.type === 'command_execution' && (rec.type === 'item.completed' || rec.type === 'item.started')) {
      const command = String(rec.item.command || '').trim();
      const output = String(rec.item.aggregated_output || '').trim();
      const label = rec.type === 'item.started' ? 'Command (running)' : 'Command';
      const combined = [command, output].filter(Boolean).join('\n\n');
      const entry = normalizeTranscriptEntry('tool', combined, 'command', label);
      if (entry) entries.push(entry);
    }
  }
  return entries;
}

function parseJsonlTranscriptEntries(text) {
  const entries = [];
  for (const rawLine of (text || '').split(/\r?\n/)) {
    const line = rawLine.trim();
    if (!line) continue;
    let rec;
    try {
      rec = JSON.parse(line);
    } catch {
      continue;
    }
    const msg = rec.message && typeof rec.message === 'object' ? rec.message : rec;
    const role = msg.role || rec.role || rec.type || '';
    const parts = [];
    collectTextParts(msg.content ?? rec.content ?? msg, parts);
    const entry = normalizeTranscriptEntry(role, parts.join('\n\n'), 'message', role || 'Entry');
    if (entry) entries.push(entry);
  }
  return entries;
}

function parseJsonTranscriptEntries(text) {
  let data;
  try {
    data = JSON.parse(text);
  } catch {
    return [];
  }
  const entries = [];
  const messages = Array.isArray(data?.messages) ? data.messages : [];
  for (const msg of messages) {
    const role = msg.role || msg.author || msg.speaker || '';
    const parts = [];
    collectTextParts(msg.content ?? msg.parts ?? msg, parts);
    const entry = normalizeTranscriptEntry(role, parts.join('\n\n'), 'message', role || 'Entry');
    if (entry) entries.push(entry);
  }
  if (entries.length) return entries;
  const parts = [];
  collectTextParts(data, parts);
  const fallback = normalizeTranscriptEntry('entry', parts.join('\n\n'), 'message', 'Transcript');
  return fallback ? [fallback] : [];
}

function buildArtifactChatData(name, files) {
  const entries = [];
  if (files.prompt) {
    entries.push({
      role: 'prompt',
      kind: 'prompt',
      title: 'Prompt',
      text: files.prompt.trim(),
    });
  }
  if (files.output) entries.push(...parseCodexOutputEntries(files.output));
  else if (files.transcriptJsonl) entries.push(...parseJsonlTranscriptEntries(files.transcriptJsonl));
  else if (files.transcriptJson) entries.push(...parseJsonTranscriptEntries(files.transcriptJson));
  return {
    id: name,
    title: artifactTitle(name),
    entries,
    hasTranscript: Boolean(files.output || files.transcriptJsonl || files.transcriptJson),
  };
}

function currentInFlightCycle(projectInfo) {
  const viewer = readLiveViewerState(projectInfo);
  return Number(viewer?.meta?.in_flight_cycle || viewer?.state?.cycle || 0);
}

function readLiveChats(projectInfo) {
  if (projectInfo.repoType === 'trellis') {
    return trellisAdapter(projectInfo, 'chats');
  }
  const cycle = currentInFlightCycle(projectInfo);
  if (!cycle) return { cycle: 0, source: 'live', artifacts: [] };
  const chatsRoot = chatsRepoDir(projectInfo);
  const artifacts = listWorkingTreeChatArtifacts(chatsRoot, cycle)
    .map(name => buildArtifactChatData(name, readWorkingTreeChatFiles(chatsRoot, cycle, name)));
  return { cycle, source: 'live', artifacts };
}

function readHistoricalChats(projectInfo, cycle) {
  if (projectInfo.repoType === 'trellis') {
    const { names, prefixes, repos } = listCandidateChatDirs(projectInfo, cycle);
    if (names.length) {
      const artifacts = sortArtifactNames(names)
        .map(name => buildArtifactChatData(
          name,
          readGitChatFiles(repos[name], cycle, name, prefixes[name])
        ));
      return { cycle, source: 'git', artifacts };
    }
    return trellisAdapter(projectInfo, 'chats-at', [String(cycle)]);
  }
  const chatsRoot = chatsRepoDir(projectInfo);
  const artifacts = listGitChatArtifacts(chatsRoot, cycle)
    .map(name => buildArtifactChatData(name, readGitChatFiles(chatsRoot, cycle, name)));
  return { cycle, source: 'git', artifacts };
}

function getCyclesFromGit(projectInfo) {
  if (projectInfo.repoType === 'trellis') {
    return trellisAdapter(projectInfo, 'cycles');
  }
  const repoPath = projectInfo.repoPath;
  let tags;
  try {
    tags = git(repoPath, 'tag -l "cycle-*" --sort=version:refname').split('\n').filter(t => /^cycle-\d+$/.test(t));
  } catch { return []; }

  return tags.map(tag => {
    const cycle = parseInt(tag.replace('cycle-', ''), 10);
    let hash = '', timestamp = '', subject = '';
    try {
      const log = git(repoPath, `log -1 --format=%H%n%aI%n%s ${tag}`);
      const parts = log.split('\n');
      hash = parts[0] || '';
      timestamp = parts[1] || '';
      subject = parts[2] || '';
    } catch {}

    // Read cycle_meta.json from that commit
    let meta = {};
    try {
      const raw = git(repoPath, `show ${tag}:.agent-supervisor/cycle_meta.json`);
      meta = JSON.parse(raw);
    } catch {}

    return { cycle, hash, timestamp, message: subject, ...meta };
  });
}

function getCycleDiff(repoPath, cycle) {
  const tag = `cycle-${cycle}`;
  const prevTag = `cycle-${cycle - 1}`;
  try {
    // Check if previous tag exists
    git(repoPath, `rev-parse ${prevTag}`);
    return git(repoPath, `diff ${prevTag} ${tag} -- Tablet/`);
  } catch {
    try {
      // First cycle — diff against empty tree
      return git(repoPath, `diff 4b825dc642cb6eb9a060e54bf899d15f3bc9 ${tag} -- Tablet/`);
    } catch { return ''; }
  }
}

function ensureSymlink(linkPath, targetPath) {
  fs.mkdirSync(path.dirname(linkPath), { recursive: true });
  try {
    const existing = fs.lstatSync(linkPath);
    if (existing.isSymbolicLink() && fs.readlinkSync(linkPath) === targetPath) return;
    fs.rmSync(linkPath, { recursive: true, force: true });
  } catch {}
  fs.symlinkSync(targetPath, linkPath);
}

function writeProjectStatic(projectInfo, { writeRoot = false } = {}) {
  const { slug } = projectInfo;
  const roots = [path.join(STATIC_OUT, slug)];
  if (writeRoot) roots.unshift(STATIC_OUT);
  const apiTarget = viewerApiDir(projectInfo);
  const htmlSrc = path.join(__dirname, 'public', 'index.html');

  for (const root of roots) {
    fs.mkdirSync(root, { recursive: true });
    ensureSymlink(path.join(root, 'api'), apiTarget);
    if (fs.existsSync(htmlSrc)) {
      ensureSymlink(path.join(root, 'index.html'), htmlSrc);
    }
  }
}

function writeStatic() {
  try {
    const projects = discoverProjects();
    const defaultSlug = defaultProjectSlug();
    for (const projectInfo of projects) {
      writeProjectStatic(projectInfo, { writeRoot: projectInfo.slug === defaultSlug });
    }
  } catch (e) {
    console.error('Static write error:', e.message);
  }
}

function projectFromRequest(req) {
  return req.params.project || defaultProjectSlug();
}

function sendIndex(_req, res) {
  res.sendFile(path.join(__dirname, 'public', 'index.html'));
}

function sendPromptsIndex(_req, res) {
  res.sendFile(path.join(__dirname, 'public', 'prompts.html'));
}

function handleDownloadTablet(res, project) {
  const projectInfo = resolveRepoPath(project);
  const { repoPath, stateDir, repoType } = projectInfo;
  let state = {};
  let nodeEntries = [];
  const configPath = configPathForRepo(repoPath);
  if (repoType === 'trellis') {
    const viewerState = readLiveViewerState(projectInfo);
    state = viewerState.state || {};
    const nodes = viewerState.nodes || {};
    nodeEntries = Object.entries(nodes).filter(([name]) => name !== 'Preamble');
  } else {
    const tablet = JSON.parse(fs.readFileSync(path.join(stateDir, 'tablet.json'), 'utf-8'));
    state = JSON.parse(fs.readFileSync(path.join(stateDir, 'state.json'), 'utf-8'));
    nodeEntries = Object.entries(tablet.nodes || {}).filter(([name]) => name !== 'Preamble');
  }
  const tabletDir = path.join(repoPath, 'Tablet');
  const paperDir = path.join(repoPath, 'paper');
  const paperFiles = fs.existsSync(paperDir)
    ? fs.readdirSync(paperDir).filter((f) => fs.statSync(path.join(paperDir, f)).isFile()).sort()
    : [];

  const nodeList = nodeEntries.map(([n, nd]) => {
      return `  - ${n}: ${nd.status || 'open'} (${nd.kind || '?'})${nd.difficulty ? ', ' + nd.difficulty : ''}${nd.title ? ' -- ' + nd.title : ''}`;
  }).join('\n');

  let mainResultTargets = [];
  if (fs.existsSync(configPath)) {
    try {
      const config = JSON.parse(fs.readFileSync(configPath, 'utf-8'));
      const rawTargets = (((config || {}).workflow || {}).main_result_targets);
      if (Array.isArray(rawTargets)) {
        mainResultTargets = rawTargets;
      }
    } catch (_err) {
      mainResultTargets = [];
    }
  }
  const targetList = (mainResultTargets.length ? mainResultTargets : []).map((target) => {
    const label = String((target || {}).tex_label || '').trim();
    const hasStart = Number.isInteger(target?.start_line);
    const hasEnd = Number.isInteger(target?.end_line);
    let lineText = '';
    if (hasStart && hasEnd) {
      lineText = target.start_line === target.end_line
        ? `line ${target.start_line}`
        : `lines ${target.start_line}-${target.end_line}`;
    }
    if (label && lineText) return `- \`${label}\` (${lineText})`;
    if (label) return `- \`${label}\``;
    if (lineText) return `- ${lineText}`;
    return '- (invalid target entry)';
  }).join('\n');
  const targetSection = targetList || '- No configured main-result targets found in `lagent.config.json`.';
  const trustedEntries = Object.values((state || {}).trusted_main_result_target_state || {}).filter((entry) => entry && typeof entry === 'object');
  const trustedProtectedNodes = [...new Set(trustedEntries.flatMap((entry) => [
    ...(((entry || {}).protected_nodes) || []),
    ...(((entry || {}).nodes) || []),
  ].map((name) => String(name || '').trim()).filter(Boolean)))].sort();
  const pendingProtectedNodes = [...new Set((((state || {}).last_review || {}).protected_nodes || []).map((name) => String(name || '').trim()).filter(Boolean))].sort();
  const pendingTargetNodes = [...new Set((((state || {}).last_review || {}).protected_target_nodes || []).map((name) => String(name || '').trim()).filter(Boolean))].sort();
  // Live coverage = current target → covering-node map. Mirrors what the
  // kernel's `approved_target_nodes()` snapshots at AdvancePhase
  // (`model.rs:2012`). Used as a fallback when no AdvancePhase decision
  // is in flight yet (`state.last_review.protected_*` empty) and no prior
  // advance has been approved (`trusted_main_result_target_state` empty).
  const liveCoverage = (state || {}).coverage || {};
  const liveCoverageRoots = [...new Set(Object.values(liveCoverage).flatMap((nodes) => (nodes || []).map((name) => String(name || '').trim()).filter(Boolean)))].sort();
  const protectedRootsRaw = pendingTargetNodes.length
    ? pendingTargetNodes
    : [...new Set(trustedEntries.flatMap((entry) => (((entry || {}).nodes) || []).map((name) => String(name || '').trim()).filter(Boolean)))].sort();
  const protectedRoots = protectedRootsRaw.length ? protectedRootsRaw : liveCoverageRoots;
  // Narrow Lean semantic closure of the protected roots (per
  // `scripts/lean_semantic_fingerprint.lean`'s closure policy: theorem
  // → walk type only; def → walk type and value; stop at the
  // `Tablet.*` boundary). The `extras` set is the project-defined
  // descendants the seed roots transitively reference in their *type*
  // signatures — i.e., the additional nodes whose meaning-bearing
  // changes would actually shift the targets' meaning, as opposed to
  // proof-only support which the closure policy excludes. Returned as
  // `{ <root>: [<descendant>, ...], ... }`. On any error (no runtime,
  // missing cache, etc.) we fall back to an empty closure so the
  // README still renders the target-root section.
  let semanticClosureByRoot = {};
  let semanticClosureExtras = [];
  let semanticClosureFootnote = '';
  if (repoType === 'trellis' && protectedRoots.length) {
    const closureArgs = protectedRoots.flatMap((root) => ['--node', root]);
    try {
      const closurePayload = trellisAdapter(projectInfo, 'semantic-closure', closureArgs);
      if (closurePayload && closurePayload.ok && closurePayload.closures) {
        semanticClosureByRoot = closurePayload.closures || {};
        const extras = new Set();
        const protectedRootSet = new Set(protectedRoots);
        for (const [, descendants] of Object.entries(semanticClosureByRoot)) {
          for (const name of (descendants || [])) {
            const trimmed = String(name || '').trim();
            if (trimmed && !protectedRootSet.has(trimmed)) {
              extras.add(trimmed);
            }
          }
        }
        semanticClosureExtras = [...extras].sort();
        const missing = protectedRoots.filter((root) => semanticClosureByRoot[root] === null);
        if (missing.length) {
          semanticClosureFootnote = `\n\n_Note: no cached Lean semantic-closure payload was found for ${missing.map((m) => `\`${m}\``).join(', ')}. The reviewer should treat the listed extras as a lower bound and check the type signatures by hand for any project-defined symbol they reference._`;
        }
      } else if (closurePayload && closurePayload.error) {
        semanticClosureFootnote = `\n\n_Note: could not compute the Lean semantic closure (${closurePayload.error}). The "Type-Surface Definitions" list below may be incomplete; the reviewer should also vet any project-defined symbol named in the target-root \`.lean\` files' type signatures._`;
      }
    } catch (err) {
      semanticClosureFootnote = `\n\n_Note: semantic-closure lookup failed (${err && err.message ? err.message : err}); the "Type-Surface Definitions" list below may be incomplete._`;
    }
  }
  const protectedNodesUnion = [...new Set([...(pendingProtectedNodes.length ? pendingProtectedNodes : trustedProtectedNodes), ...protectedRoots, ...semanticClosureExtras])].sort();
  const protectedIntro = (() => {
    if (!protectedNodesUnion.length) {
      return 'No protected-node snapshot is currently available.';
    }
    const baseStanza = pendingProtectedNodes.length
      ? 'If you approve this package, the following nodes will become protected from later meaning-bearing changes without renewed expert review.'
      : (pendingTargetNodes.length || trustedProtectedNodes.length
        ? 'These nodes are currently protected from later meaning-bearing changes without renewed expert review.'
        : 'These are the nodes whose meaning will become protected from later changes without renewed expert review if you approve this package. The list is derived from the live target-coverage map: the target roots below, plus the project-defined definitions the kernel would reach by walking each root\'s Lean type signature (per `scripts/lean_semantic_fingerprint.lean`\'s closure policy — proof bodies are excluded, so lemmas used only in proofs do not appear here).');
    return baseStanza;
  })();
  const protectedSection = (() => {
    if (!protectedNodesUnion.length) {
      return '- (no protected nodes available)';
    }
    const labelOf = (name) => {
      const isRoot = protectedRoots.includes(name);
      const isExtra = semanticClosureExtras.includes(name);
      if (isRoot) return ' (target root)';
      if (isExtra) {
        const sources = Object.entries(semanticClosureByRoot)
          .filter(([, descendants]) => Array.isArray(descendants) && descendants.includes(name))
          .map(([root]) => `\`${root}\``);
        if (sources.length) {
          return ` (in semantic closure of ${sources.join(', ')})`;
        }
        return ' (in semantic closure of a target root)';
      }
      return '';
    };
    return protectedNodesUnion.map((name) => `- \`${name}\`${labelOf(name)}`).join('\n') + semanticClosureFootnote;
  })();
  const protectedClosureExplanation = protectedNodesUnion.length
    ? `

The list above is exactly what the kernel will treat as protected. It is the union of:

- the **target roots** — the per-target covering nodes ${protectedRoots.length ? `(\`${protectedRoots.join('`, `')}\`)` : ''} that the kernel snapshots into \`approved_target_nodes\` at AdvancePhase, AND
- the **type-surface definitions** they reach — the project-defined definitions transitively named in those roots' Lean *type* signatures (or in the values of definitions reached the same way). Proof-body content is intentionally excluded: per the closure policy in \`scripts/lean_semantic_fingerprint.lean\`, a theorem's proof can change without changing what the theorem *means*, so lemmas used only inside a proof do not enter this set.

If any node on the list above changes its Lean or \`.tex\` meaning later, the per-target paper-faithfulness fingerprint diverges from the snapshot the kernel took at this AdvancePhase, and the system reopens this gate for renewed expert review. Other supporting nodes — proof-only lemmas, helper definitions reached only through proof bodies, etc. — are *not* on the protected list and may evolve freely without rebooting expert review, so long as the protected nodes' meanings stay fixed.`
    : '';
  const paperReferenceText = paperFiles.length === 0
    ? 'the `paper/` directory in this zip'
    : paperFiles.length === 1
      ? `\`paper/${paperFiles[0]}\``
      : paperFiles.map((f) => `\`paper/${f}\``).join(', ');

  const readme = `# Proof Tablet Snapshot

This package is for external expert review of statement correspondence.

## Expert Task

The files in this zip are part of a project to formalize results from the paper provided here as ${paperReferenceText}. The particular formalization targets are listed below.

This project has just completed its initial phase: constructing a coarse skeleton of the paper, organized as a DAG of nodes, where each node contains a Lean statement and a corresponding \`.tex\` statement. At this stage, most proof-bearing Lean files still contain \`sorry\` in place of completed proofs.

The next phase of the project is to replace those \`sorry\`s with valid Lean proofs, possibly while adding further supporting nodes, until every Lean statement in this package has a sorry-free proof.

Your task is to decide whether, given the current coarse DAG of Lean/\`.tex\` node pairs, successful completion of that later proof-writing phase would genuinely amount to a formalization of the target paper results, provided the statements and definitions currently present here are not changed.

The natural way to make that judgment is to check that:
- the \`.tex\` statements genuinely cover the formalization targets listed below;
- the Lean statements genuinely correspond to those \`.tex\` statements;
- the supporting nodes form a paper-faithful support package for those targets, rather than introducing unnecessary or paper-distorting claims.

Do not assume that the name of a project-defined symbol, node title, or surrounding prose tells you its real meaning. For any non-Mathlib definition or statement introduced in this package, you should verify the meaning directly from the Lean code and the matching \`.tex\` file, and if necessary from the surrounding tablet files that use it.

A confident judgment should not depend on how the remaining proofs are later filled in, or on what additional support nodes might later be added, so long as the current statement/definition package is left unchanged.

This is not a request to review whether the Lean proofs are finished. In this package, \`sorry\` is expected in proof-bearing Lean declarations. The question is whether the statements are the right ones, and whether they are organized in a way that is sufficient for a faithful formalization of the paper's main results.

## Configured Main-Result Targets

- These are the paper items that matter for this review:
${targetSection}

## Protected Nodes

${protectedIntro}

${protectedSection}${protectedClosureExplanation}

## What To Check

- Each configured target should be covered by one or more non-\`helper\` nodes whose Lean and \`.tex\` statements genuinely match.
- If multiple non-\`helper\` nodes share a target, they should together completely cover that target.
- For every node on the **Protected Nodes** list above (target roots **and** type-surface definitions), confirm the Lean declaration's *type* faithfully matches the paired \`.tex\` statement. The list is exactly what's being frozen by your approval; you do not need to vouch for the meaning of any other supporting node.
- Project-defined definitions in that list are part of the correspondence task: do not trust their intended meaning from naming alone; verify that each such Lean definition really captures the concept used in the matching \`.tex\` statement and in the target roots that reference it.
- Other supporting nodes may exist in the package and may even be referenced by the protected nodes' *proofs*. They are not on the protected list and you are not being asked to vouch for them; they may freely evolve later as long as the protected nodes' meanings stay fixed.
- The relevant question is whether the protected node set above is good enough to treat as the trusted coarse formalization of the paper's main results.

## What May Change Without Further Expert Review

After this package is approved, later work may continue without renewed expert review as long as none of the **Protected Nodes** listed above change Lean/\`.tex\` meaning.

- Lean proof-body work is allowed on every node, including replacing \`sorry\` in protected proof-bearing nodes.
- Proof-only edits to \`.tex\` are allowed on every node.
- New supporting nodes can be added, and existing non-protected supporting nodes can be edited, refactored, or removed — provided the **Protected Nodes** themselves keep the same Lean type signatures and the same \`.tex\` statement meaning.
- Definitions outside the protected list may be edited or removed even if a protected node's *proof* used to reference them, because proof-only support is intentionally outside the protected meaning surface.

## What Would Require Renewed Expert Review

- Changing the meaning of a Lean or \`.tex\` statement for any **Protected Node** listed above (target root or type-surface definition).
- Adding, removing, or swapping the nodes that cover a configured target.
- Changing the configured target list itself.
- Adding a new project-defined definition into a protected node's *type* signature (this would extend the type-surface and is not pre-approved).

## Nodes

${nodeList}

## Structure

- \`Tablet/Preamble.lean\` — shared imports (no definitions here)
- \`Tablet/<name>.lean\` — Lean 4 declaration (theorem/lemma/def)
- \`Tablet/<name>.tex\` — natural-language statement and, for proof-bearing nodes, a rigorous NL proof
- \`paper/\` — source paper files, including ${paperReferenceText}
`;

  const tempDir = viewerTempDir(stateDir);
  fs.mkdirSync(tempDir, { recursive: true });
  const tmpDir = fs.mkdtempSync(path.join(tempDir, 'tablet-snapshot-'));
  const snapDir = path.join(tmpDir, 'tablet-snapshot');
  fs.mkdirSync(path.join(snapDir, 'Tablet'), { recursive: true });
  fs.mkdirSync(path.join(snapDir, 'paper'), { recursive: true });

  if (fs.existsSync(tabletDir)) {
    for (const f of fs.readdirSync(tabletDir)) {
      if (f.endsWith('.lean') || f.endsWith('.tex')) {
        fs.copyFileSync(path.join(tabletDir, f), path.join(snapDir, 'Tablet', f));
      }
    }
  }
  if (fs.existsSync(paperDir)) {
    for (const f of fs.readdirSync(paperDir)) {
      fs.copyFileSync(path.join(paperDir, f), path.join(snapDir, 'paper', f));
    }
  }

  fs.writeFileSync(path.join(snapDir, 'README.md'), readme);

  const zipPath = path.join(tmpDir, 'tablet-snapshot.zip');
  execSync(`cd "${tmpDir}" && zip -r "${zipPath}" tablet-snapshot/`, { timeout: 10000 });

  res.setHeader('Content-Type', 'application/zip');
  res.setHeader('Content-Disposition', `attachment; filename="tablet-snapshot-cycle${state.cycle || 0}.zip"`);
  const zipStream = fs.createReadStream(zipPath);
  let cleaned = false;
  const cleanup = () => {
    if (cleaned) return;
    cleaned = true;
    try {
      fs.rmSync(tmpDir, { recursive: true, force: true });
    } catch {}
  };
  zipStream.pipe(res);
  zipStream.on('close', cleanup);
  zipStream.on('error', cleanup);
  res.on('close', cleanup);
}

// Parse a tmux burst session name into (runtime_namespace, role,
// request_id, lane, retry).
// Examples:
//   "trellis-trellis-smoke-worker-128-worker"      -> { role: "worker",   request_id: 128, lane: "worker",   retry: 0 }
//   "trellis-trellis-smoke-review-129-reviewer"    -> { role: "review",   request_id: 129, lane: "reviewer", retry: 0 }
//   "trellis-trellis-smoke-corr-136-v2"            -> { role: "corr",     request_id: 136, lane: "v2",       retry: 0 }
//   "trellis-trellis-smoke-worker-158-worker-r2"   -> { role: "worker",   request_id: 158, lane: "worker",   retry: 2 }
// Multi-lane verifier requests (paper / corr / sound) use a `v<N>` lane
// suffix; tmux_backend.py:3263 appends `-r{attempt}` to the base session
// name on each restart so a worker that bounced once becomes
// `...-worker-r1`, twice → `-r2`, etc. Returns null if the name doesn't
// match any burst pattern we care about.
function parseBurstSessionName(name) {
  if (typeof name !== 'string') return null;
  if (!name.startsWith('trellis-')) return null;
  if (name === 'trellis_viewer') return null;
  if (name.startsWith('trellis-run-')) return null;
  const body = name.slice('trellis-'.length);
  const m = body.match(/^(.+)-(worker|review|reviewer|paper|corr|sound|stuck_math_audit)-(\d+)-(worker|reviewer|v\d+|audit)(?:-r(\d+))?$/);
  if (!m) return null;
  const [, runtime, role, reqId, suffix, retryStr] = m;
  return {
    session: name,
    runtime,
    role,
    suffix,
    lane: suffix,
    request_id: Number(reqId),
    retry: retryStr ? Number(retryStr) : 0,
  };
}

// Parse a single JSON line from a codex output.log into a compact,
// render-friendly shape. Unknown shapes pass through as kind="other" with
// the raw line preserved so the UI can still show something.
function normalizeBurstLogEvent(line) {
  const trimmed = line.trim();
  if (!trimmed) return null;
  let rec;
  try {
    rec = JSON.parse(trimmed);
  } catch {
    return { kind: 'other', raw: trimmed };
  }
  const type = String(rec.type || '');
  const item = rec.item && typeof rec.item === 'object' ? rec.item : null;
  if (type === 'thread.started') {
    return { kind: 'thread_started', thread_id: rec.thread_id || '' };
  }
  if (type === 'turn.started') return { kind: 'turn_started' };
  if (type === 'turn.completed') {
    return { kind: 'turn_completed', usage: rec.usage || null };
  }
  if ((type === 'item.completed' || type === 'item.started') && item) {
    const itemType = String(item.type || '');
    const status = type === 'item.completed' ? 'completed' : 'in_progress';
    if (itemType === 'agent_message') {
      return { kind: 'agent_message', status, id: item.id || '', text: item.text || '' };
    }
    if (itemType === 'command_execution') {
      return {
        kind: 'command_execution', status,
        id: item.id || '',
        command: item.command || '',
        aggregated_output: item.aggregated_output || '',
        exit_code: item.exit_code == null ? null : Number(item.exit_code),
      };
    }
    if (itemType === 'file_change') {
      return {
        kind: 'file_change', status,
        id: item.id || '',
        changes: Array.isArray(item.changes) ? item.changes.map((c) => ({
          path: String(c.path || ''),
          kind: String(c.kind || ''),
        })) : [],
      };
    }
    // Unknown item type — pass-through.
    return { kind: 'item_other', status, id: item.id || '', item_type: itemType, raw: rec };
  }
  return { kind: 'other', raw: trimmed };
}

// ============================================================================
// Unified "chats" endpoints — one tab in the viewer for both live + historical
// burst transcripts, with structured per-provider event rendering.
// ============================================================================

// Best-effort kind inference from artifact_id / scope.
function inferCallKind(artifactId) {
  const s = String(artifactId || '');
  if (s.includes('worker')) return 'worker';
  if (s.includes('review')) return 'reviewer';
  if (s.includes('paper')) return 'paper';
  if (s.includes('correspondence') || s.includes('corr')) return 'corr';
  if (s.includes('nl_proof') || s.includes('sound')) return 'sound';
  return 'other';
}

// Parse request_id out of an artifact_id if present (e.g. "worker_handoff_128_attempt_0").
function inferRequestId(artifactId) {
  const s = String(artifactId || '');
  // Look for _<digits>_ or trailing _<digits>
  let m = s.match(/_(\d+)(?:_|$)/);
  if (m) return Number(m[1]);
  return null;
}

// Post-bwrap-only, bursts run as the operator, so live chat artifacts are
// operator-owned and a direct read succeeds. Read directly and skip (return
// empty) on any failure — there is no separate sandbox user to sudo into.
function readTextMaybeSudo(filePath) {
  try {
    return fs.readFileSync(filePath, 'utf-8');
  } catch {
    return '';
  }
}

function tmuxSessionAlive(name) {
  try {
    execFileSync('tmux', tmuxArgs('has-session', '-t', String(name)), {
      stdio: ['ignore', 'ignore', 'ignore'],
    });
    return true;
  } catch {
    return false;
  }
}

// Read call.json from the live working tree.
function readLiveCallJson(stateDir, artifactDirName) {
  const p = path.join(stateDir, 'chats', 'live', artifactDirName, 'call.json');
  try {
    const raw = fs.readFileSync(p, 'utf-8');
    return JSON.parse(raw);
  } catch {
    return null;
  }
}

// --- Stale-session filtering ---------------------------------------------
//
// `chats/live/` is append-only on disk: when the active config swaps a
// worker/reviewer model (e.g. gemini-auto → gemini-3.1-pro-preview) the
// previous run's scope dir (`worker_proof_formalization:worker:gemini:
// gemini-auto:default` and the underscore mirror) stays put. Without
// filtering those orphans show up in the chat-calls sidebar and confuse
// the operator. We mark a dir "stale" if BOTH:
//   1. its latest file mtime is older than TRELLIS_VIEWER_STALE_HOURS
//      (default 6h), AND
//   2. the model token encoded in its name is not in the project's
//      currently-active config model set.
// Both conditions must hold. A scope dir for a still-active model that
// merely sat idle between bursts is NOT filtered.
//
// Toggle: query param `?show_stale=1` or env `TRELLIS_VIEWER_SHOW_STALE=1`
// disables filtering globally.
const STALE_CHAT_DIR_HOURS = (() => {
  const raw = Number(process.env.TRELLIS_VIEWER_STALE_HOURS);
  return Number.isFinite(raw) && raw > 0 ? raw : 6;
})();
const STALE_CHAT_DIR_MS = STALE_CHAT_DIR_HOURS * 60 * 60 * 1000;
const SHOW_STALE_ENV = (() => {
  const v = String(process.env.TRELLIS_VIEWER_SHOW_STALE || '').toLowerCase();
  return v === '1' || v === 'true' || v === 'yes';
})();

// Cache active-models per project for 30 seconds. Config changes are rare
// and the resolver walks every model-bearing role.
const _activeModelsCache = new Map(); // key: repoPath → {ts, set}
const ACTIVE_MODELS_TTL_MS = 30 * 1000;
function getActiveConfigModels(projectInfo) {
  if (!projectInfo || !projectInfo.repoPath) return new Set();
  const key = projectInfo.repoPath;
  const entry = _activeModelsCache.get(key);
  const now = Date.now();
  if (entry && (now - entry.ts) < ACTIVE_MODELS_TTL_MS) return entry.set;
  const configPath = configPathForRepo(projectInfo.repoPath);
  let config = null;
  try { config = JSON.parse(fs.readFileSync(configPath, 'utf-8')); } catch { config = null; }
  const models = new Set();
  const collectModelFields = (obj) => {
    if (!obj || typeof obj !== 'object') return;
    if (typeof obj.model === 'string' && obj.model) models.add(obj.model);
    if (Array.isArray(obj.fallback_models)) {
      for (const m of obj.fallback_models) if (typeof m === 'string' && m) models.add(m);
    }
  };
  if (config && typeof config === 'object') {
    for (const role of ['worker', 'easy_worker', 'hard_worker', 'blockered_worker', 'reviewer', 'verification']) {
      collectModelFields(config[role]);
    }
    const ver = config.verification || {};
    for (const arrKey of ['correspondence_agents', 'soundness_agents', 'paper_faithfulness_agents', 'substantiveness_agents']) {
      const arr = ver[arrKey];
      if (Array.isArray(arr)) for (const a of arr) collectModelFields(a);
    }
  }
  _activeModelsCache.set(key, { ts: now, set: models });
  return models;
}

// Extract the model token from a scope-form chat dir name. Returns null
// if the dir has no embedded model (e.g. `trellis_worker_56_result`).
// Supported shapes:
//   `<role>_<phase>:<role>:<provider>:<model>:<scope>`
//   `<role>_<phase>:<role>:<kind>:<provider>:<model>:...`
//   `<phase>_<role>_<provider>_<model>_<scope>` (underscore mirror)
function parseModelFromChatDirName(name) {
  if (!name || typeof name !== 'string') return null;
  // Skip artifact-id dirs that have no encoded model.
  if (/^trellis_(worker|review|paper|corr|sound)_\d+(?:_|$)/.test(name)) return null;
  const PROVIDERS = new Set(['codex', 'gemini', 'claude']);
  if (name.includes(':')) {
    const parts = name.split(':');
    for (let i = 0; i < parts.length - 1; i++) {
      if (PROVIDERS.has(parts[i])) {
        const model = parts[i + 1];
        if (model && !PROVIDERS.has(model)) return model;
      }
    }
    return null;
  }
  // Underscore mirror: split, find a provider token, take the next.
  const parts = name.split('_');
  for (let i = 0; i < parts.length - 1; i++) {
    if (PROVIDERS.has(parts[i])) {
      const model = parts[i + 1];
      if (model && !PROVIDERS.has(model)) return model;
    }
  }
  return null;
}

// Return the most-recent mtime among the dir's immediate children (and
// the dir itself). Cheap; no recursion. Returns 0 if dir is missing.
function latestMtimeForLiveChatDir(stateDir, name) {
  const dir = path.join(stateDir, 'chats', 'live', name);
  let best = 0;
  try {
    const st = fs.statSync(dir);
    best = st.mtimeMs;
    for (const entry of fs.readdirSync(dir, { withFileTypes: true })) {
      try {
        const cst = fs.statSync(path.join(dir, entry.name));
        if (cst.mtimeMs > best) best = cst.mtimeMs;
      } catch { /* ignore */ }
    }
  } catch { return 0; }
  return best;
}

// True iff `name` is a stale live chat dir for `projectInfo`. Stale means
// latest mtime older than STALE_CHAT_DIR_MS AND its encoded model is not
// in the active-model set. Dirs without an encoded model (artifact-id
// dirs) are never stale.
function isStaleLiveChatDir(projectInfo, name, activeModels, nowMs) {
  const model = parseModelFromChatDirName(name);
  if (!model) return false;
  if (activeModels && activeModels.has(model)) return false;
  const mt = latestMtimeForLiveChatDir(projectInfo.stateDir, name);
  if (!mt) return false;
  return (nowMs - mt) > STALE_CHAT_DIR_MS;
}

// Read call.json from a git-committed cycle tag.
function readGitCallJson(chatsRepo, cycle, artifactDirName) {
  if (!fs.existsSync(path.join(chatsRepo, '.git'))) return null;
  if (!hasChatCycleTag(chatsRepo, cycle)) return null;
  const tag = chatCycleTag(cycle);
  try {
    const raw = git(chatsRepo, `show ${tag}:${chatCycleDir(cycle)}/${artifactDirName}/call.json`);
    return JSON.parse(raw);
  } catch {
    return null;
  }
}

// List artifact dir names for a given cycle ('live' or number).
function listCallArtifacts(stateDir, cycle) {
  if (cycle === 'live') {
    const liveRoot = path.join(stateDir, 'chats', 'live');
    if (!fs.existsSync(liveRoot)) return [];
    return fs.readdirSync(liveRoot, { withFileTypes: true })
      .filter(e => e.isDirectory())
      .map(e => e.name);
  }
  const chatsRepo = path.join(stateDir, 'chats');
  return listGitChatArtifacts(chatsRepo, cycle);
}

// Resolve the runtime_root for this project. The supervisor stores its
// state (event_log.jsonl, protocol_state.json) OUTSIDE the repo, typically
// at a sibling path `<repo>-runtime/`. That's the convention used by
// `scripts/restart_configured_run.sh`. Fall back to scanning the parent
// of the repo for any `*-runtime` dir that contains an event_log.jsonl.
function runtimeRootForProject(projectInfo) {
  // `repoPath` may be a symlink (e.g. ${TRELLIS_ROOT:-/path/to/trellis}/math/current → real
  // path). The sibling `<repo>-runtime` convention is relative to the
  // resolved path, not the symlink itself.
  let realRepoPath = projectInfo.repoPath;
  try { realRepoPath = fs.realpathSync(projectInfo.repoPath); } catch {}
  const direct = `${realRepoPath}-runtime`;
  if (fs.existsSync(path.join(direct, 'event_log.jsonl'))) return direct;
  const parent = path.dirname(realRepoPath);
  if (!fs.existsSync(parent)) return null;
  let best = null;
  for (const entry of fs.readdirSync(parent, { withFileTypes: true })) {
    if (!entry.isDirectory()) continue;
    if (!entry.name.endsWith('-runtime')) continue;
    const candidate = path.join(parent, entry.name);
    const logPath = path.join(candidate, 'event_log.jsonl');
    if (fs.existsSync(logPath)) {
      try {
        const mt = fs.statSync(logPath).mtimeMs;
        if (!best || mt > best.mtime) best = { path: candidate, mtime: mt };
      } catch {}
    }
  }
  return best ? best.path : null;
}

function liveInFlightRequestId(runtimeRoot) {
  if (!runtimeRoot) return null;
  const p = path.join(runtimeRoot, 'protocol_state.json');
  if (!fs.existsSync(p)) return null;
  try {
    const state = JSON.parse(fs.readFileSync(p, 'utf8'));
    const req = state && state.in_flight_request;
    const id = req && req.id;
    return Number.isFinite(id) ? id : null;
  } catch {
    return null;
  }
}

// Scan event_log.jsonl to extract the authoritative list of bursts ever
// issued: every issue_request command inside a RuntimeStepRecord defines
// a burst whose cycle is the RuntimeStep's `cycle` field (or the most
// recent start_cycle event). Returns both a request_id → cycle map and
// the full ordered list of burst records {request_id, kind, cycle,
// active_node, mode, event_index} — the source of truth for what bursts
// the supervisor issued in each cycle. Also returns the live cycle
// number (most recent step's cycle field). This replaces filesystem
// enumeration as the authority: chats/live/ is never pruned, so we can't
// trust its dir listing.
// Incremental cache: event_log.jsonl is strictly append-only, so we only
// re-parse bytes added since the last call. The cache records the byte
// offset right after the last newline we consumed; on the next call we seek
// there and process only the new tail. File shrinkage (truncate/rotate)
// triggers a full re-parse.
let REQUEST_CYCLE_CACHE = {
  path: null, size: 0, offset: 0,
  map: null, bursts: null, eventIndex: 0, currentCycle: null,
};
function requestCyclesFromEventLog(runtimeRoot) {
  if (!runtimeRoot) return { map: new Map(), liveCycle: null, bursts: [] };
  const logPath = path.join(runtimeRoot, 'event_log.jsonl');
  if (!fs.existsSync(logPath)) return { map: new Map(), liveCycle: null, bursts: [] };
  let size = 0;
  try { size = fs.statSync(logPath).size; } catch {}

  let map, bursts, currentCycle, eventIndex, startOffset;
  const reuseCache =
    REQUEST_CYCLE_CACHE.path === logPath
    && REQUEST_CYCLE_CACHE.map
    && REQUEST_CYCLE_CACHE.offset <= size;
  if (reuseCache && size === REQUEST_CYCLE_CACHE.size) {
    return {
      map: REQUEST_CYCLE_CACHE.map,
      liveCycle: REQUEST_CYCLE_CACHE.currentCycle,
      bursts: REQUEST_CYCLE_CACHE.bursts,
    };
  }
  if (reuseCache) {
    map = REQUEST_CYCLE_CACHE.map;
    bursts = REQUEST_CYCLE_CACHE.bursts;
    currentCycle = REQUEST_CYCLE_CACHE.currentCycle;
    eventIndex = REQUEST_CYCLE_CACHE.eventIndex;
    startOffset = REQUEST_CYCLE_CACHE.offset;
  } else {
    map = new Map();
    bursts = [];
    currentCycle = null;
    eventIndex = 0;
    startOffset = 0;
  }

  const lastOffset = forEachFileLineFromOffset(logPath, startOffset, (line) => {
    if (!line.trim()) { eventIndex++; return; }
    let rec;
    try { rec = JSON.parse(line); } catch { eventIndex++; return; }
    const evCycle = rec.cycle;
    if (typeof evCycle === 'number') currentCycle = evCycle;
    const idx = (typeof rec.index === 'number') ? rec.index : eventIndex;
    const cmds = Array.isArray(rec.commands) ? rec.commands : [];
    for (const cmd of cmds) {
      if (cmd && cmd.command === 'issue_request' && cmd.request && typeof cmd.request.id === 'number') {
        const useCycle = (typeof evCycle === 'number') ? evCycle : currentCycle;
        if (useCycle != null) map.set(cmd.request.id, useCycle);
        bursts.push({
          request_id: cmd.request.id,
          kind: String(cmd.request.kind || ''),
          cycle: useCycle,
          active_node: cmd.request.active_node || null,
          mode: cmd.request.mode || null,
          event_index: idx,
        });
      }
    }
    eventIndex++;
  });

  REQUEST_CYCLE_CACHE = {
    path: logPath, size, offset: lastOffset,
    map, bursts, eventIndex, currentCycle,
  };
  return { map, liveCycle: currentCycle, bursts };
}

// Map from Rust `kind` enum name to the lowercase tag used in scope dirs
// and `trellis_<kind>_<id>_*` artifact dirs.
function kindTag(kind) {
  const k = String(kind || '').toLowerCase();
  if (k === 'worker') return 'worker';
  if (k === 'review') return 'review';
  if (k === 'paper') return 'paper';
  if (k === 'corr') return 'corr';
  if (k === 'sound') return 'sound';
  if (k === 'stuckmathaudit') return 'stuck_math_audit';
  // AdvanceGate / HumanGate have no chat artifact.
  return k;
}

// Return the list of candidate chat dir names for a given cycle —
// for `live` this is `chats/live/` on disk; for historical cycles it
// prefers the cycle-bound `cycle-<NNNN>/` snapshot. `live/` is only a
// fallback because old tags can contain a large cumulative live snapshot.
function listCandidateChatDirs(projectInfo, cycleParam) {
  const { stateDir } = projectInfo;
  let chatsRepo = path.join(stateDir, 'chats');
  if (cycleParam === 'live') {
    const liveRoot = path.join(stateDir, 'chats', 'live');
    if (!fs.existsSync(liveRoot)) return { names: [], prefixes: {}, repos: {}, chatsRepo };
    const names = [];
    const prefixes = {};
    const repos = {};
    for (const e of fs.readdirSync(liveRoot, { withFileTypes: true })) {
      if (!e.isDirectory()) continue;
      names.push(e.name);
      prefixes[e.name] = 'live';
      repos[e.name] = chatsRepo;
    }
    return { names, prefixes, repos, chatsRepo };
  }
  const cycle = cycleParam;
  const tag = chatCycleTag(cycle);
  const cyclePrefix = chatCycleDir(cycle) + '/';
  const livePrefix = 'live/';
  // `cycle-NNNN/` has priority over `live/` when both carry the same
  // artifact name; we prefer the cycle-bound snapshot because it isn't
  // rewritten on subsequent bursts.
  const names = [];
  const prefixes = {};
  const repos = {};
  const note = (name, prefix, repoPath) => {
    if (!name || prefixes[name]) return;
    names.push(name);
    prefixes[name] = prefix;
    repos[name] = repoPath;
  };
  const candidateRepos = chatRepoCandidates(projectInfo);
  const liveFallbacks = [];
  for (let i = 0; i < candidateRepos.length; i++) {
    const repoPath = candidateRepos[i];
    if (!hasChatCycleTag(repoPath, cycle)) continue;
    const cycleSet = new Set();
    const liveSet = new Set();
    const collect = (files, prefix, set) => {
      for (const name of files.split('\n')) {
        if (!name || !name.startsWith(prefix)) continue;
        const rest = name.slice(prefix.length);
        const first = rest.split('/')[0];
        if (first) set.add(first);
      }
    };
    try {
      collect(git(repoPath, `ls-tree -r --name-only ${tag} -- ${cyclePrefix}`), cyclePrefix, cycleSet);
    } catch {}
    if (!cycleSet.size) {
      try {
        collect(git(repoPath, `ls-tree -r --name-only ${tag} -- ${livePrefix}`), livePrefix, liveSet);
      } catch {}
    }
    for (const n of cycleSet) note(n, `cycle-${String(cycle).padStart(4, '0')}`, repoPath);
    if (cycleSet.size && i > 0) {
      return { names: sortArtifactNames(names), prefixes, repos, chatsRepo: null };
    }
    if (liveSet.size) liveFallbacks.push({ repoPath, liveSet });
  }
  if (!names.length && liveFallbacks.length) {
    const { repoPath, liveSet } = liveFallbacks[0];
    for (const n of liveSet) note(n, 'live', repoPath);
  }
  return { names: sortArtifactNames(names), prefixes, repos, chatsRepo: null };
}

// Resolve a burst (event-log record) to zero or more chat dirs using a
// fixed preference order:
//   A. Preferred `trellis_<kind>_<id>_*` artifact dirs (matches
//      worker_N_result, review_N_decision, paper/corr/sound_N_vK, etc).
//      Lanes (paper/corr/sound have v1/v2) return multiple entries.
//   B. Scope-based dirs that embed the request_id (reviewer sessions for
//      paper/corr/sound lanes): `*:{kind}:{id}:v1:*` or matching
//      non-colon equivalents `*_{kind}_{id}_v1_*`. Lane v1 and v2 are
//      separate entries.
//   C. Scope-based dirs keyed only by role/kind, with no id in the name
//      (worker session + reviewer-review session; rewritten each burst).
//      Emitted with transcript_is_stale_scope_dir: true.
//   D. If nothing matches, emit a placeholder with no dir — the frontend
//      already handles `missing: true` gracefully.
function resolveBurstArtifacts(burst, dirNames) {
  const tag = kindTag(burst.kind);
  const rid = burst.request_id;
  const results = [];
  if (!tag || rid == null) {
    return results;
  }

  // Preferred A: trellis_<tag>_<rid>(_<suffix>)?
  const preferredRe = new RegExp(`^trellis_${tag}_${rid}(?:_|$)`);
  const preferred = dirNames.filter((n) => preferredRe.test(n));
  if (preferred.length) {
    for (const name of preferred) {
      results.push({ artifact_id: name, transcript_is_stale_scope_dir: false, fallback: 'preferred' });
    }
    // Verifier requests (paper/corr/sound) can have multiple lanes (v1, v2, …).
    // The bridge currently writes the canonical short-name dir
    // `trellis_<tag>_<rid>_vN` only for the v1 (codex) lane; the v2 (gemini)
    // lane lands in a colon-form scope dir like
    // `reviewer_theorem_stating:reviewer:sound:60:v2:gemini:...:default`.
    // Without this supplement scan we'd surface only v1 in the chat dropdown.
    // TODO(bridge): normalize all verifier lanes to `trellis_<tag>_<rid>_vN`
    // and remove this supplement.
    if (tag === 'paper' || tag === 'corr' || tag === 'sound') {
      const preferredLanes = new Set();
      for (const name of preferred) {
        const lm = name.match(/_v(\d+)(?:_|$)/);
        if (lm) preferredLanes.add(`v${lm[1]}`);
      }
      const scopeLaneRe = new RegExp(`(^|:)${tag}:${rid}:(v\\d+):`);
      for (const name of dirNames) {
        const m = name.match(scopeLaneRe);
        if (!m) continue;
        const lane = m[2];
        if (preferredLanes.has(lane)) continue;
        results.push({ artifact_id: name, transcript_is_stale_scope_dir: false, fallback: 'scope_lane_supplement' });
        preferredLanes.add(lane);
      }
    }
    return results;
  }

  // Fallback B: scope dir with id embedded. The canonical colon form is
  // `reviewer_proof_formalization:reviewer:<tag>:<rid>:v1:...`; an older
  // underscore-separated mirror also exists
  // (`proof_formalization_reviewer_<tag>_<rid>_v1_...`). Prefer the colon
  // form; fall back to the underscore mirror only when no colon variant
  // exists (avoids emitting redundant duplicate entries per lane).
  const colonRe = new RegExp(`(^|:)${tag}:${rid}(?::|$)`);
  const underRe = new RegExp(`(^|_)${tag}_${rid}(?:_|$)`);
  const colonMatches = dirNames.filter((n) => colonRe.test(n));
  const underMatches = dirNames.filter((n) => underRe.test(n));
  const byId = colonMatches.length ? colonMatches : underMatches;
  if (byId.length) {
    for (const name of byId) {
      results.push({ artifact_id: name, transcript_is_stale_scope_dir: false, fallback: 'scope_with_id' });
    }
    return results;
  }

  // Fallback C: role-only scope dir (worker / reviewer-review sessions
  // that have no id and are rewritten each burst). Prefer the colon-form
  // naming (current convention) over the legacy underscore mirror; don't
  // emit redundant per-model duplicates — pick just the colon variants,
  // falling back to underscore variants if no colon form exists.
  const pickStale = (colonRe2, underRe2) => {
    const colon = dirNames.filter((n) => colonRe2.test(n));
    if (colon.length) return colon;
    return dirNames.filter((n) => underRe2.test(n));
  };
  if (tag === 'worker') {
    const stale = pickStale(
      /^worker_proof_formalization:worker:/,
      /^proof_formalization_worker_/,
    );
    for (const name of stale) {
      results.push({ artifact_id: name, transcript_is_stale_scope_dir: true, fallback: 'scope_stale' });
    }
  } else if (tag === 'review') {
    const stale = pickStale(
      /^reviewer_proof_formalization:reviewer:review(:|$)/,
      /^proof_formalization_reviewer_review[_:]/,
    );
    for (const name of stale) {
      results.push({ artifact_id: name, transcript_is_stale_scope_dir: true, fallback: 'scope_stale' });
    }
  }
  return results;
}

// Probe whether a chat dir contains a recognizable transcript artifact
// (structured JSONL/JSON/output.log OR a raw claude-style `<uuid>.jsonl`
// session file dropped inside the scope dir).
function chatDirHasTranscript(projectInfo, cycleParam, artifactDirName, gitPrefix, chatsRepoOverride = null) {
  const { stateDir } = projectInfo;
  const CANON = ['transcript.jsonl', 'transcript.json', 'output.log'];
  if (cycleParam === 'live') {
    const dir = path.join(stateDir, 'chats', 'live', artifactDirName);
    if (!fs.existsSync(dir)) return false;
    for (const f of CANON) {
      if (fs.existsSync(path.join(dir, f))) return true;
    }
    try {
      const entries = fs.readdirSync(dir);
      if (entries.some((e) => /\.jsonl$/.test(e) && e !== 'transcript.jsonl')) return true;
    } catch {}
    return false;
  }
  const chatsRepo = chatsRepoOverride || chatRepoForCycle(projectInfo, cycleParam) || path.join(stateDir, 'chats');
  if (!hasChatCycleTag(chatsRepo, cycleParam)) return false;
  const tag = chatCycleTag(cycleParam);
  try {
    const base = `${gitPrefix || chatCycleDir(cycleParam)}/${artifactDirName}`;
    const files = git(chatsRepo, `ls-tree --name-only ${tag} -- ${base}/`)
      .split('\n').filter(Boolean);
    if (files.some(f => /\/(transcript\.jsonl|transcript\.json|output\.log)$/.test(f))) return true;
    if (files.some(f => /\/[0-9a-f-]{20,}\.jsonl$/.test(f))) return true;
    return false;
  } catch {
    return false;
  }
}

function buildHistoricalArtifactCalls(projectInfo, cycleValue, source) {
  const { names, prefixes, repos } = listCandidateChatDirs(projectInfo, cycleValue);
  const calls = [];
  for (const artifactDirName of sortArtifactNames(names)) {
    const gitPrefix = prefixes[artifactDirName] || chatCycleDir(cycleValue);
    const artifactChatsRepo = repos[artifactDirName];
    let meta = null;
    if (artifactChatsRepo) {
      try {
        const raw = git(
          artifactChatsRepo,
          `show ${chatCycleTag(cycleValue)}:${gitPrefix}/${artifactDirName}/call.json`
        );
        meta = JSON.parse(raw);
      } catch {}
    }
    const isStaleScope =
      /^worker_proof_formalization:worker:/.test(artifactDirName)
      || /^proof_formalization_worker_/.test(artifactDirName)
      || /^reviewer_proof_formalization:reviewer:review(:|$)/.test(artifactDirName)
      || /^proof_formalization_reviewer_review[_:]/.test(artifactDirName);
    calls.push({
      artifact_id: artifactDirName,
      kind: inferCallKind(artifactDirName),
      request_id: meta?.request_id ?? inferRequestId(artifactDirName),
      cycle: cycleValue,
      active_node: null,
      mode: null,
      event_index: null,
      provider: meta?.provider || null,
      model: meta?.model || null,
      role: meta?.role || null,
      session_id: meta?.session_id || null,
      scope: meta?.scope || null,
      started_at_ms: meta?.started_at_ms ?? null,
      ended_at_ms: meta?.ended_at_ms ?? null,
      has_transcript: chatDirHasTranscript(projectInfo, cycleValue, artifactDirName, gitPrefix, artifactChatsRepo),
      has_tui_pane: false,
      has_call_json: !!meta,
      transcript_is_stale_scope_dir: isStaleScope,
      tmux_session: null,
      lane: null,
    });
  }
  return { cycle: cycleValue, source, calls };
}

// Build chat-calls response for a cycle. Historical cycles are artifact-driven
// so archived chat snapshots remain cheap to browse; live cycles use the
// event_log so in-flight bursts can appear before their chat_dir is written.
function buildChatCalls(projectInfo, cycleParam, options = {}) {
  const { stateDir } = projectInfo;
  const isLive = cycleParam === 'live' || cycleParam === '' || cycleParam == null;
  const chatsRepo = path.join(stateDir, 'chats');
  const showStale = !!options.showStale || SHOW_STALE_ENV;

  let cycleValue;
  let source;
  if (isLive) {
    source = 'live';
  } else {
    const n = parseInt(String(cycleParam), 10);
    if (!Number.isFinite(n)) {
      return { cycle: cycleParam, source: 'git', calls: [], error: 'invalid cycle' };
    }
    cycleValue = n;
    source = `cycle-${n}`;
    return buildHistoricalArtifactCalls(projectInfo, cycleValue, source);
  }

  const runtimeRoot = runtimeRootForProject(projectInfo);
  const ec = requestCyclesFromEventLog(runtimeRoot);
  const inFlightRequestId = isLive ? liveInFlightRequestId(runtimeRoot) : null;
  if (isLive) {
    cycleValue = (ec.liveCycle != null) ? ec.liveCycle : 'live';
  }

  const burstCycle = typeof cycleValue === 'number' ? cycleValue : ec.liveCycle;
  const cycleBursts = (ec.bursts || []).filter((b) => b.cycle === burstCycle);

  const dirsKey = isLive ? 'live' : cycleValue;
  const {
    names: dirNamesRaw,
    prefixes,
    repos: artifactRepos = {},
    chatsRepo: dirsChatsRepo,
  } = listCandidateChatDirs(projectInfo, dirsKey);
  // Hide orphaned scope dirs (config swapped models — old dir lingers on
  // disk but no live burst writes to it). Only filters the LIVE listing;
  // historical cycles are immutable git snapshots.
  let dirNames = dirNamesRaw;
  if (isLive && !showStale) {
    const activeModels = getActiveConfigModels(projectInfo);
    const nowMs = Date.now();
    dirNames = dirNamesRaw.filter((n) => !isStaleLiveChatDir(projectInfo, n, activeModels, nowMs));
  }
  const readChatsRepo = isLive ? chatsRepo : (dirsChatsRepo || chatsRepo);

  // For the live cycle, enumerate tmux burst sessions once and index by
  // (request_id, role, lane). Lets us flag has_tui_pane as soon as the burst's
  // tmux session is up, even before the agent has written call.json — and
  // ALSO supports multi-lane verifier requests (paper/corr/sound spawn one
  // tmux session per lane v1/v2/...). Indexing by lane prevents v2 from
  // clobbering v1 in the map; without that, only one lane per request would
  // show up and entries would get cross-wired to whichever lane was last
  // inserted.
  const liveTmuxByRequestId = new Map();
  if (isLive) {
    try {
      const lsOut = execFileSync('tmux', tmuxArgs('ls', '-F', '#{session_name}'), {
        encoding: 'utf8', stdio: ['ignore', 'pipe', 'ignore'],
      });
      for (const name of lsOut.split('\n').map((s) => s.trim()).filter(Boolean)) {
        const parsed = parseBurstSessionName(name);
        if (parsed && parsed.request_id != null) {
          let sessions = liveTmuxByRequestId.get(parsed.request_id);
          if (!sessions) {
            sessions = [];
            liveTmuxByRequestId.set(parsed.request_id, sessions);
          }
          sessions.push(parsed);
        }
      }
    } catch {}
  }

  // Pull a `vN` lane out of an artifact_id so we can pair it with the
  // matching tmux session. Two artifact-id formats exist on disk:
  //
  //  (1) `trellis_<kind>_<id>_<suffix>` — preferred, written by the
  //      bridge to live/ (e.g. `trellis_worker_56_result`,
  //      `trellis_review_55_decision`, `trellis_corr_38_v1`).
  //  (2) Colon-form scope dirs — legacy fallback used when no preferred
  //      dir exists (e.g.
  //      `reviewer_theorem_stating:reviewer:corr:10:v2:...`).
  //
  // The lane returned here MUST match what `parseBurstSessionName`
  // returns for the corresponding tmux session ('worker', 'reviewer',
  // 'v1', 'v2', …). Without that, the dedup in the leftover-lane
  // backfill loop below misses the chat_dir-based emission and emits a
  // phantom placeholder for the same lane, causing the frontend to
  // surface "no chat directory was located" for an in-flight burst
  // even though `trellis_worker_<id>_result` is on disk.
  function laneFromArtifactId(artifactId) {
    const s = String(artifactId || '');
    if (/^trellis_worker_\d+(?:_|$)/.test(s)) return 'worker';
    if (/^trellis_review_\d+(?:_|$)/.test(s)) return 'reviewer';
    const mu = s.match(/^trellis_(?:paper|corr|sound)_\d+_(v\d+)(?:_|$)/);
    if (mu) return mu[1];
    const m = s.match(/:(v\d+):/);
    if (m) return m[1];
    if (s.startsWith('worker')) return 'worker';
    if (s.startsWith('reviewer')) return 'reviewer';
    return '';
  }

  function normalizedTmuxRole(role) {
    const r = String(role || '');
    return r === 'reviewer' ? 'review' : r;
  }

  function tmuxLanesForBurst(burst) {
    const expectedRole = normalizedTmuxRole(kindTag(burst.kind));
    const sessions = liveTmuxByRequestId.get(burst.request_id) || [];
    const byLane = new Map();
    for (const parsed of sessions) {
      if (expectedRole && normalizedTmuxRole(parsed.role) !== expectedRole) {
        continue;
      }
      const lane = parsed.lane || parsed.suffix || '';
      const existing = byLane.get(lane);
      if (!existing || (parsed.retry || 0) >= (existing.retry || 0)) {
        byLane.set(lane, parsed);
      }
    }
    return byLane;
  }

  // Pick a tmux session for a (request_id, role, lane). Returns null if no live
  // session matches. For workers/reviewers (single lane), accept any
  // session under that request_id when the lane match is empty.
  function pickTmuxSessionForLane(burst, lane) {
    if (!isLive) return null;
    const byLane = tmuxLanesForBurst(burst);
    if (!byLane.size) return null;
    if (lane && byLane.has(lane)) return byLane.get(lane).session;
    // Fall back to "the single available session" only when we don't know
    // the lane. For named lanes (v1, v2, worker, reviewer) a miss means
    // the lane's session has already exited — don't cross-wire to a
    // surviving sibling.
    if (!lane && byLane.size === 1) return byLane.values().next().value.session;
    return null;
  }

  const calls = [];
  const seenKeys = new Set();
  // Track which (request_id, lane) pairs we've emitted from chat_dir
  // matches so we can later emit placeholders for any tmux lanes that
  // don't yet have a chat_dir on disk.
  const emittedLanesByRequest = new Map();
  function noteEmittedLane(requestId, lane) {
    let s = emittedLanesByRequest.get(requestId);
    if (!s) { s = new Set(); emittedLanesByRequest.set(requestId, s); }
    s.add(lane || '');
  }

  for (const burst of cycleBursts) {
    const matched = resolveBurstArtifacts(burst, dirNames);
    if (matched.length === 0) {
      // A live-cycle rewind can leave completed same-cycle requests in
      // event_log.jsonl while their chat dirs have intentionally been removed.
      // Only synthesize a no-dir placeholder for the request the supervisor is
      // currently waiting on; otherwise old completed bursts look live.
      if (isLive && burst.request_id !== inFlightRequestId) {
        continue;
      }
      // No chat_dir on disk yet. Emit one placeholder per tmux lane so
      // multi-lane verifier requests don't collapse to a single entry.
      const byLane = tmuxLanesForBurst(burst);
      const lanes = byLane.size ? Array.from(byLane.entries()) : [['', null]];
      for (const [lane, parsed] of lanes) {
        const sess = parsed ? parsed.session : null;
        calls.push({
          artifact_id: null,
          kind: kindTag(burst.kind) || inferCallKind(''),
          request_id: burst.request_id,
          cycle: burst.cycle,
          active_node: burst.active_node,
          mode: burst.mode,
          event_index: burst.event_index,
          provider: null,
          model: null,
          role: null,
          session_id: null,
          scope: null,
          started_at_ms: null,
          ended_at_ms: null,
          has_transcript: false,
          has_tui_pane: !!sess,
          has_call_json: false,
          transcript_is_stale_scope_dir: false,
          tmux_session: sess,
          lane: lane || null,
        });
        noteEmittedLane(burst.request_id, lane);
      }
      continue;
    }
    for (const match of matched) {
      const artifactDirName = match.artifact_id;
      const key = `${burst.request_id}::${artifactDirName}`;
      if (seenKeys.has(key)) continue;
      seenKeys.add(key);
      const gitPrefix = isLive ? null : (prefixes[artifactDirName] || null);
      const artifactChatsRepo = isLive ? chatsRepo : (artifactRepos[artifactDirName] || readChatsRepo);
      const meta = isLive
        ? readLiveCallJson(stateDir, artifactDirName)
        : (gitPrefix
          ? (() => {
            try {
              const raw = git(artifactChatsRepo, `show ${chatCycleTag(cycleValue)}:${gitPrefix}/${artifactDirName}/call.json`);
              return JSON.parse(raw);
            } catch { return null; }
          })()
          : null);
      const hasTranscript = chatDirHasTranscript(projectInfo, dirsKey, artifactDirName, gitPrefix, artifactChatsRepo);
      // Provider detection: prefer call.json's `provider`. Fall back to
      // inferring codex from `output.log` presence because the supervisor
      // only writes call.json at burst end — during an in-flight codex
      // burst the field is otherwise null and the TUI gate below admits
      // the burst incorrectly. Codex emits its --json event stream to
      // output.log; claude/gemini drive a tmux pane and don't.
      let provider = meta?.provider || null;
      if (!provider && isLive) {
        try {
          if (fs.existsSync(path.join(stateDir, 'chats', 'live', artifactDirName, 'output.log'))) {
            provider = 'codex';
          }
        } catch { /* ignore */ }
      }
      const lane = laneFromArtifactId(artifactDirName);
      const burstTmuxSession = pickTmuxSessionForLane(burst, lane);
      // Enable the TUI toggle when there's *something* we could show:
      // a live tmux session OR the supervisor's on-disk pane.txt
      // snapshot. Without either, the pane view is empty so don't tease
      // the user with a toggle that does nothing. Codex is headless.
      const paneSnapshotExists = isLive && (() => {
        try {
          return fs.existsSync(path.join(stateDir, 'chats', 'live', artifactDirName, 'pane.txt'));
        } catch { return false; }
      })();
      const hasTuiPane = !!(
        isLive
        && provider !== 'codex'
        && (burstTmuxSession || paneSnapshotExists)
      );
      calls.push({
        artifact_id: artifactDirName,
        kind: kindTag(burst.kind) || inferCallKind(artifactDirName),
        request_id: burst.request_id,
        cycle: burst.cycle,
        active_node: burst.active_node,
        mode: burst.mode,
        event_index: burst.event_index,
        provider,
        model: meta?.model || null,
        role: meta?.role || null,
        session_id: meta?.session_id || null,
        scope: meta?.scope || null,
        started_at_ms: meta?.started_at_ms ?? null,
        ended_at_ms: meta?.ended_at_ms ?? null,
        has_transcript: hasTranscript,
        has_tui_pane: hasTuiPane,
        has_call_json: !!meta,
        transcript_is_stale_scope_dir: !!match.transcript_is_stale_scope_dir,
        git_prefix: gitPrefix,
        tmux_session: burstTmuxSession,
        lane: lane || null,
      });
      noteEmittedLane(burst.request_id, lane);
    }

    // After emitting chat_dir-based entries, fill in any tmux lanes
    // whose chat_dir hasn't appeared on disk yet (lane still in flight).
    if (isLive) {
      const byLane = tmuxLanesForBurst(burst);
      const emitted = emittedLanesByRequest.get(burst.request_id) || new Set();
      if (byLane.size) {
        for (const [lane, parsed] of byLane) {
          if (emitted.has(lane)) continue;
          const sess = parsed ? parsed.session : null;
          calls.push({
            artifact_id: null,
            kind: kindTag(burst.kind) || inferCallKind(''),
            request_id: burst.request_id,
            cycle: burst.cycle,
            active_node: burst.active_node,
            mode: burst.mode,
            event_index: burst.event_index,
            provider: null,
            model: null,
            role: null,
            session_id: null,
            scope: null,
            started_at_ms: null,
            ended_at_ms: null,
            has_transcript: false,
            has_tui_pane: !!sess,
            has_call_json: false,
            transcript_is_stale_scope_dir: false,
            tmux_session: sess,
            lane: lane || null,
          });
          noteEmittedLane(burst.request_id, lane);
        }
      }
    }
  }

  // Order: request_id asc, then event_index asc, then artifact_id.
  calls.sort((a, b) => {
    const ar = a.request_id == null ? Infinity : a.request_id;
    const br = b.request_id == null ? Infinity : b.request_id;
    if (ar !== br) return ar - br;
    const ai = a.event_index == null ? Infinity : a.event_index;
    const bi = b.event_index == null ? Infinity : b.event_index;
    if (ai !== bi) return ai - bi;
    return String(a.artifact_id || '').localeCompare(String(b.artifact_id || ''));
  });
  return { cycle: cycleValue, source, calls };
}

// ---- per-provider event parsers -------------------------------------------

function parseClaudeTranscriptEvents(text, startMs, endMs) {
  const events = [];
  for (const rawLine of (text || '').split(/\r?\n/)) {
    const line = rawLine.trim();
    if (!line) continue;
    let rec;
    try {
      rec = JSON.parse(line);
    } catch {
      continue;
    }
    const tsStr = rec.timestamp || rec.ts || '';
    let tsMs = null;
    if (tsStr) {
      const parsed = Date.parse(tsStr);
      if (!Number.isNaN(parsed)) tsMs = parsed;
    }
    if (startMs != null && tsMs != null && tsMs < startMs) continue;
    if (endMs != null && tsMs != null && tsMs > endMs) continue;
    const type = String(rec.type || '');
    const msg = rec.message && typeof rec.message === 'object' ? rec.message : null;
    if (type === 'assistant' && msg) {
      const content = Array.isArray(msg.content) ? msg.content : [];
      for (const block of content) {
        if (!block || typeof block !== 'object') continue;
        const bt = String(block.type || '');
        if (bt === 'text') {
          events.push({ kind: 'agent_message', provider: 'claude', ts_ms: tsMs, text: String(block.text || '') });
        } else if (bt === 'thinking') {
          events.push({ kind: 'thinking', provider: 'claude', ts_ms: tsMs, thinking: String(block.thinking || block.text || '') });
        } else if (bt === 'tool_use') {
          events.push({
            kind: 'tool_call', provider: 'claude', ts_ms: tsMs,
            tool_name: String(block.name || ''),
            tool_input: block.input ?? null,
            id: String(block.id || ''),
          });
        }
      }
      if (msg.usage) {
        events.push({ kind: 'turn_completed', provider: 'claude', ts_ms: tsMs, usage: msg.usage });
      }
    } else if (type === 'user' && msg) {
      // tool_result blocks arrive via user records.
      const content = Array.isArray(msg.content) ? msg.content : null;
      if (Array.isArray(content)) {
        let anyToolResult = false;
        for (const block of content) {
          if (block && typeof block === 'object' && block.type === 'tool_result') {
            anyToolResult = true;
            let txt = '';
            if (typeof block.content === 'string') txt = block.content;
            else if (Array.isArray(block.content)) {
              txt = block.content.map((b) => (b && typeof b === 'object' && typeof b.text === 'string') ? b.text : '').join('\n');
            }
            events.push({
              kind: 'tool_result', provider: 'claude', ts_ms: tsMs,
              id: String(block.tool_use_id || ''),
              tool_output: txt,
              is_error: !!block.is_error,
            });
          }
        }
        if (!anyToolResult) {
          const parts = [];
          for (const block of content) {
            if (block && typeof block === 'object' && typeof block.text === 'string') parts.push(block.text);
            else if (typeof block === 'string') parts.push(block);
          }
          if (parts.length) {
            events.push({ kind: 'user_message', provider: 'claude', ts_ms: tsMs, text: parts.join('\n') });
          }
        }
      } else if (typeof msg.content === 'string') {
        events.push({ kind: 'user_message', provider: 'claude', ts_ms: tsMs, text: msg.content });
      }
    } else if (type === 'thinking') {
      events.push({ kind: 'thinking', provider: 'claude', ts_ms: tsMs, thinking: String(rec.thinking || rec.text || '') });
    }
  }
  return events;
}

function parseGeminiTranscriptEvents(text, startMs, endMs) {
  let data;
  try {
    data = JSON.parse(text);
  } catch {
    return [];
  }
  const events = [];
  const messages = Array.isArray(data?.messages) ? data.messages : [];
  for (const m of messages) {
    const tsMs = Number(m?.timestamp) || null;
    if (startMs != null && tsMs != null && tsMs < startMs) continue;
    if (endMs != null && tsMs != null && tsMs > endMs) continue;
    const mtype = String(m?.type || '');
    let txt = '';
    if (typeof m.content === 'string') txt = m.content;
    else if (Array.isArray(m.content)) {
      const parts = [];
      for (const p of m.content) {
        if (typeof p === 'string') parts.push(p);
        else if (p && typeof p === 'object' && typeof p.text === 'string') parts.push(p.text);
      }
      txt = parts.join('\n');
    }
    if (mtype === 'user') {
      events.push({ kind: 'user_message', provider: 'gemini', ts_ms: tsMs, text: txt });
    } else if (mtype === 'gemini') {
      // Thoughts come as `{subject, description, timestamp}` objects in
      // the current gemini transcript schema (not the earlier `{text}`
      // shape). Format each as "[subject] description" so the card view
      // surfaces the reasoning headings.
      const thoughts = Array.isArray(m.thoughts) ? m.thoughts : [];
      for (const t of thoughts) {
        let tt = '';
        if (typeof t === 'string') tt = t;
        else if (t && typeof t === 'object') {
          if (typeof t.text === 'string' && t.text) tt = t.text;
          else {
            const subj = typeof t.subject === 'string' ? t.subject.trim() : '';
            const desc = typeof t.description === 'string' ? t.description.trim() : '';
            tt = subj && desc ? `[${subj}] ${desc}` : (desc || subj);
          }
        }
        if (tt) events.push({ kind: 'thinking', provider: 'gemini', ts_ms: tsMs, thinking: tt });
      }
      if (txt) events.push({ kind: 'agent_message', provider: 'gemini', ts_ms: tsMs, text: txt });
      // Each gemini turn's actual work is in `toolCalls`. Extract each as a
      // tool_call + tool_result pair so the view shows what the agent did,
      // not just the usage stub.
      const toolCalls = Array.isArray(m.toolCalls) ? m.toolCalls : [];
      for (const tc of toolCalls) {
        if (!tc || typeof tc !== 'object') continue;
        const id = String(tc.id || '');
        const name = String(tc.name || '');
        const args = tc.args ?? tc.input ?? null;
        events.push({
          kind: 'tool_call', provider: 'gemini', ts_ms: tsMs,
          tool_name: name, tool_input: args, id,
        });
        // Result may be a list of functionResponse objects, a string
        // (resultDisplay), or absent while the call is still in flight.
        let output = null;
        if (typeof tc.resultDisplay === 'string' && tc.resultDisplay) {
          output = tc.resultDisplay;
        } else if (Array.isArray(tc.result)) {
          const parts = [];
          for (const r of tc.result) {
            const fr = r && r.functionResponse;
            if (!fr) continue;
            const resp = fr.response || {};
            if (typeof resp.output === 'string') parts.push(resp.output);
            else if (typeof resp.error === 'string') parts.push(resp.error);
            else parts.push(JSON.stringify(resp));
          }
          output = parts.join('\n');
        } else if (tc.result !== undefined) {
          output = typeof tc.result === 'string' ? tc.result : JSON.stringify(tc.result);
        }
        const isError = tc.status && String(tc.status).toLowerCase() !== 'success';
        if (output !== null || tc.status) {
          events.push({
            kind: 'tool_result', provider: 'gemini', ts_ms: tsMs,
            tool_name: name, tool_output: output, id, is_error: !!isError,
          });
        }
      }
      if (m.tokens) {
        events.push({ kind: 'turn_completed', provider: 'gemini', ts_ms: tsMs, usage: m.tokens });
      }
    } else if (mtype === 'tool') {
      // Standalone tool messages — rare; kept for older schemas.
      const name = String(m.toolName || m.name || '');
      if (m.result !== undefined || m.output !== undefined) {
        events.push({ kind: 'tool_result', provider: 'gemini', ts_ms: tsMs, tool_name: name, tool_output: m.result ?? m.output ?? txt });
      } else {
        events.push({ kind: 'tool_call', provider: 'gemini', ts_ms: tsMs, tool_name: name, tool_input: m.input ?? m.args ?? null });
      }
    }
  }
  return events;
}

function parseCodexOutputLogEvents(text) {
  const events = [];
  for (const rawLine of (text || '').split(/\r?\n/)) {
    const ev = normalizeBurstLogEvent(rawLine);
    if (!ev) continue;
    // Augment with provider tag.
    ev.provider = 'codex';
    events.push(ev);
  }
  return events;
}

function readArtifactFile(stateDir, cycle, artifactDirName, filename, gitPrefix, chatsRepoOverride = null) {
  if (cycle === 'live') {
    const p = path.join(stateDir, 'chats', 'live', artifactDirName, filename);
    return readTextMaybeSudo(p);
  }
  const chatsRepo = chatsRepoOverride || path.join(stateDir, 'chats');
  if (!hasChatCycleTag(chatsRepo, cycle)) return '';
  const tag = chatCycleTag(cycle);
  const prefixes = gitPrefix
    ? [gitPrefix]
    : [chatCycleDir(cycle), 'live'];
  for (const p of prefixes) {
    try {
      const txt = git(chatsRepo, `show ${tag}:${p}/${artifactDirName}/${filename}`);
      if (txt) return txt;
    } catch {}
  }
  return '';
}

// For scope dirs, Claude headless sessions drop their transcript as
// `<uuid>.jsonl` directly into the dir. List the dir to find them.
function listArtifactFiles(stateDir, cycle, artifactDirName, gitPrefix, chatsRepoOverride = null) {
  if (cycle === 'live') {
    const dir = path.join(stateDir, 'chats', 'live', artifactDirName);
    try {
      return fs.readdirSync(dir);
    } catch {
      return [];
    }
  }
  const chatsRepo = chatsRepoOverride || path.join(stateDir, 'chats');
  if (!hasChatCycleTag(chatsRepo, cycle)) return [];
  const tag = chatCycleTag(cycle);
  const prefixes = gitPrefix
    ? [gitPrefix]
    : [chatCycleDir(cycle), 'live'];
  for (const p of prefixes) {
    try {
      const out = git(chatsRepo, `ls-tree --name-only ${tag} -- ${p}/${artifactDirName}/`)
        .split('\n').filter(Boolean);
      const basenames = out.map((n) => n.split('/').pop()).filter(Boolean);
      if (basenames.length) return basenames;
    } catch {}
  }
  return [];
}

function buildChatEvents(projectInfo, cycleParam, callId, source, hints = {}) {
  const { stateDir } = projectInfo;
  const isLive = cycleParam === 'live' || cycleParam === '' || cycleParam == null;
  const cycleValue = isLive ? 'live' : parseInt(String(cycleParam), 10);
  if (!isLive && !Number.isFinite(cycleValue)) {
    return { error: 'invalid cycle' };
  }
  const hintedRequestId = Number.isFinite(Number(hints.request_id)) ? Number(hints.request_id) : null;
  const hintedLane = hints.lane ? String(hints.lane) : null;
  const hintedKind = hints.kind ? String(hints.kind) : null;
  const hintedProvider = hints.provider ? String(hints.provider) : null;
  const hintedRole = hints.role ? String(hints.role) : null;
  // Determine git prefix for historical cycles — scope dirs live under
  // `live/`, the few cycles with `cycle-NNNN/` artifacts live there.
  let gitPrefix = null;
  let historicalChatsRepo = null;
  if (!isLive) {
    const { prefixes, repos } = listCandidateChatDirs(projectInfo, cycleValue);
    gitPrefix = prefixes[callId] || null;
    historicalChatsRepo = repos[callId] || null;
  }
  // Resolve call meta.
  const chatsRepo = isLive
    ? path.join(stateDir, 'chats')
    : (historicalChatsRepo || path.join(stateDir, 'chats'));
  const meta = isLive
    ? readLiveCallJson(stateDir, callId)
    : (gitPrefix
      ? (() => {
        try {
          const raw = git(chatsRepo, `show ${chatCycleTag(cycleValue)}:${gitPrefix}/${callId}/call.json`);
          return JSON.parse(raw);
        } catch { return null; }
      })()
      : null);
  const isStaleScope =
    /^worker_proof_formalization:worker:/.test(callId)
    || /^proof_formalization_worker_/.test(callId)
    || /^reviewer_proof_formalization:reviewer:review(:|$)/.test(callId)
    || /^proof_formalization_reviewer_review[_:]/.test(callId);
  let provider = meta?.provider || hintedProvider || null;
  if (!provider && isLive) {
    const liveDir = path.join(stateDir, 'chats', 'live', callId);
    if (fs.existsSync(path.join(liveDir, 'output.log'))) provider = 'codex';
    else if (fs.existsSync(path.join(liveDir, 'transcript.jsonl'))) provider = 'claude';
    else if (fs.existsSync(path.join(liveDir, 'transcript.json'))) provider = 'gemini';
    else {
      // claude headless drops <uuid>.jsonl
      try {
        if (fs.readdirSync(liveDir).some((f) => /^[0-9a-f-]{20,}\.jsonl$/.test(f))) {
          provider = 'claude';
        }
      } catch {}
    }
  }

  // When call.json is missing or scope-based call.json lacks request_id, fall
  // back to the event log / UI hints so live TUI matching stays request-bound.
  let fallbackRequestId = hintedRequestId;
  let fallbackRole = hintedRole;
  if (isLive && (meta?.request_id == null || !fallbackRole)) {
    try {
      const runtimeRoot = runtimeRootForProject(projectInfo);
      const ec = requestCyclesFromEventLog(runtimeRoot);
      const liveBursts = (ec.bursts || []).filter((b) => b.cycle === ec.liveCycle);
      const { names: dirNamesFb } = listCandidateChatDirs(projectInfo, 'live');
      for (const burst of liveBursts) {
        if (hintedRequestId != null && burst.request_id !== hintedRequestId) continue;
        const matches = resolveBurstArtifacts(burst, dirNamesFb);
        const isMissingHint = callId.startsWith('__missing__:') && burst.request_id === hintedRequestId;
        if (isMissingHint || matches.some((m) => m.artifact_id === callId)) {
          fallbackRequestId = burst.request_id;
          const k = String(burst.kind || '').toLowerCase();
          if (k.includes('review')) fallbackRole = 'reviewer';
          else if (k.includes('worker')) fallbackRole = 'worker';
          break;
        }
      }
    } catch {}
  }
  // Last-resort role inference from artifact_id prefix, if still unknown.
  let inferredRole = null;
  if (callId.startsWith('worker_') || callId.includes(':worker:')) inferredRole = 'worker';
  else if (callId.startsWith('reviewer_') || callId.includes(':reviewer:')) inferredRole = 'reviewer';
  const call = {
    artifact_id: callId,
    kind: hintedKind || inferCallKind(callId),
    request_id: meta?.request_id ?? hintedRequestId ?? inferRequestId(callId) ?? fallbackRequestId,
    provider,
    model: meta?.model || null,
    role: meta?.role || fallbackRole || inferredRole,
    session_id: meta?.session_id || null,
    scope: meta?.scope || null,
    started_at_ms: meta?.started_at_ms ?? null,
    ended_at_ms: meta?.ended_at_ms ?? null,
    has_transcript: false,
    // Enabled whenever there's something we could show in the pane view —
    // either a live tmux session to tail, or a pane.txt snapshot the
    // supervisor wrote when the burst ended. Codex is headless (no TUI).
    has_tui_pane: !!(
      isLive
      && provider !== 'codex'
      && (
        (provider === 'claude' || provider === 'gemini' || !provider)
        && (() => {
          try {
            return fs.existsSync(path.join(stateDir, 'chats', 'live', callId, 'pane.txt'));
          } catch { return false; }
        })()
      )
    ),
    transcript_is_stale_scope_dir: isStaleScope,
  };

  // TUI passthrough — capture-pane from the matching live tmux burst
  // session; fall back to the on-disk pane.txt snapshot the supervisor
  // writes when the burst ends. That way recently-finished verifier lanes
  // (which live 1–2 min and have their tmux session torn down on exit)
  // still render a useful pane view instead of "(pane empty)".
  if (source === 'tui') {
    const pickPaneSnapshot = () => {
      if (!isLive) return null;
      try {
        const p = path.join(stateDir, 'chats', 'live', callId, 'pane.txt');
        if (!fs.existsSync(p)) return null;
        const st = fs.statSync(p);
        const text = fs.readFileSync(p, 'utf8');
        return { text, mtimeMs: st.mtimeMs };
      } catch { return null; }
    };

    if (!isLive) {
      return { call, source: 'tui', events: [], missing: true };
    }
    let lsOut = '';
    try {
      lsOut = execFileSync('tmux', tmuxArgs('ls', '-F', '#{session_name}'), {
        encoding: 'utf8', stdio: ['ignore', 'pipe', 'ignore'],
      });
    } catch {}
    const candidates = lsOut.split('\n').map((s) => s.trim()).filter(Boolean);
    // Match heuristic: burst session names embed role + request_id and a
    // lane suffix (v1/v2 for verifiers, worker/reviewer otherwise). Prefer
    // an exact (request_id, lane) match so the v2 lane doesn't cross-wire
    // onto v1's surviving session or vice versa.
    const callLane = (() => {
      const m = String(callId || '').match(/:(v\d+):/);
      if (m) return m[1];
      if (hintedLane) return hintedLane;
      if (String(callId || '').startsWith('worker_')) return 'worker';
      if (String(callId || '').startsWith('reviewer_')) return 'reviewer';
      return null;
    })();
    let matched = null;
    let matchedLane = null;
    for (const name of candidates) {
      const parsed = parseBurstSessionName(name);
      if (!parsed) continue;
      if (call.request_id != null && parsed.request_id === call.request_id) {
        if (callLane && parsed.lane === callLane) { matched = name; matchedLane = parsed.lane; break; }
        // Tentative — keep scanning for an exact lane hit.
        if (matched == null) { matched = name; matchedLane = parsed.lane; }
      }
      if (call.request_id == null && call.role && parsed.role === call.role && matched == null) {
        matched = name; matchedLane = parsed.lane;
      }
    }
    // If the lane we want differs from what we picked tentatively, don't
    // cross-wire — drop the tentative match and fall back to pane.txt.
    if (matched && callLane && matchedLane && matchedLane !== callLane) {
      matched = null;
    }
    if (matched) {
      let paneText = '';
      try {
        paneText = execFileSync('tmux', tmuxArgs('capture-pane', '-t', matched, '-p', '-S', '-600'), {
          encoding: 'utf8', stdio: ['ignore', 'pipe', 'ignore'], maxBuffer: 4 * 1024 * 1024,
        });
      } catch (e) {
        // Capture failed (session died between ls and capture) — fall
        // through to the snapshot.
        paneText = '';
        matched = null;
      }
      if (matched) {
        return {
          call,
          source: 'tui',
          tmux_session: matched,
          events: [{ kind: 'other', provider, text: paneText }],
          missing: false,
        };
      }
    }
    const snapshot = pickPaneSnapshot();
    if (snapshot) {
      return {
        call,
        source: 'tui',
        tmux_session: null,
        snapshot_mtime_ms: snapshot.mtimeMs,
        events: [{ kind: 'other', provider, text: snapshot.text }],
        missing: false,
        note: 'Snapshot from pane.txt — burst has no live tmux session anymore.',
      };
    }
    return { call, source: 'tui', events: [], missing: true, error: 'no matching tmux session' };
  }

  // Determine filename based on provider.
  let events = [];
  let raw = '';
  let transcriptFile = null;
  let promptExcerpt = '';
  const tryClaudeUuid = () => {
    const files = listArtifactFiles(stateDir, cycleValue, callId, gitPrefix, chatsRepo);
    const uuid = files.find((f) => /^[0-9a-f-]{20,}\.jsonl$/.test(f));
    if (!uuid) return '';
    const text = readArtifactFile(stateDir, cycleValue, callId, uuid, gitPrefix, chatsRepo);
    if (text) transcriptFile = uuid;
    return text;
  };
  if (provider === 'codex') {
    transcriptFile = 'output.log';
    raw = readArtifactFile(stateDir, cycleValue, callId, 'output.log', gitPrefix, chatsRepo);
    if (raw) events = parseCodexOutputLogEvents(raw);
  } else if (provider === 'claude') {
    transcriptFile = 'transcript.jsonl';
    raw = readArtifactFile(stateDir, cycleValue, callId, 'transcript.jsonl', gitPrefix, chatsRepo);
    if (!raw) raw = tryClaudeUuid();
    if (raw) events = parseClaudeTranscriptEvents(raw, call.started_at_ms, call.ended_at_ms);
  } else if (provider === 'gemini') {
    transcriptFile = 'transcript.json';
    raw = readArtifactFile(stateDir, cycleValue, callId, 'transcript.json', gitPrefix, chatsRepo);
    if (raw) events = parseGeminiTranscriptEvents(raw, call.started_at_ms, call.ended_at_ms);
  } else {
    // Unknown provider — try each transcript form.
    raw = readArtifactFile(stateDir, cycleValue, callId, 'output.log', gitPrefix, chatsRepo);
    if (raw) {
      events = parseCodexOutputLogEvents(raw);
      call.provider = call.provider || 'codex';
      transcriptFile = 'output.log';
    } else {
      raw = readArtifactFile(stateDir, cycleValue, callId, 'transcript.jsonl', gitPrefix, chatsRepo);
      if (raw) {
        events = parseClaudeTranscriptEvents(raw, call.started_at_ms, call.ended_at_ms);
        call.provider = call.provider || 'claude';
        transcriptFile = 'transcript.jsonl';
      } else {
        raw = readArtifactFile(stateDir, cycleValue, callId, 'transcript.json', gitPrefix, chatsRepo);
        if (raw) {
          events = parseGeminiTranscriptEvents(raw, call.started_at_ms, call.ended_at_ms);
          call.provider = call.provider || 'gemini';
          transcriptFile = 'transcript.json';
        } else {
          raw = tryClaudeUuid();
          if (raw) {
            events = parseClaudeTranscriptEvents(raw, call.started_at_ms, call.ended_at_ms);
            call.provider = call.provider || 'claude';
          }
        }
      }
    }
  }

  const missing = !raw;
  call.has_transcript = !missing;

  // Always attach a prompt excerpt for stale-scope dirs so the frontend
  // can surface the caveat alongside transcript content.
  if (missing || isStaleScope) {
    promptExcerpt = readArtifactFile(stateDir, cycleValue, callId, 'prompt.txt', gitPrefix, chatsRepo).slice(0, 4000);
  }
  let note = null;
  if (isStaleScope) {
    note = 'This scope dir is rewritten every burst in this role/scope. Its content may belong to a later burst; attribution to this specific request is best-effort.';
  }
  const byteSize = Buffer.byteLength(raw || '', 'utf8');
  return {
    call,
    source: 'transcript',
    transcript_file: transcriptFile,
    events,
    size: byteSize,
    next_offset: byteSize,
    missing,
    prompt_excerpt: promptExcerpt,
    note,
  };
}

// In-process TTL caches for chat calls/events. The underlying builders
// re-parse JSONL state files on each call. For live data we cache 10s
// (run state evolves on a ~minute scale); for historical cycles, the
// data is immutable so cache lives until process exit.
const _chatCallsCache = new Map();   // key: project|cycle → {ts, value}
const _chatEventsCache = new Map();  // key: project|cycle|call|source → {ts, value}
const CHAT_LIVE_TTL_MS = 10 * 1000;

function _isLiveKey(cycle) {
  return cycle === 'live' || cycle === '';
}

function handleChatCalls(req, res, project) {
  const projectInfo = typeof project === 'string' ? resolveRepoPath(project) : project;
  const cycle = (req.query.cycle || '').toString().trim();
  const cycleKey = cycle || 'live';
  // `?show_stale=1` overrides the default stale-dir filter (also see
  // TRELLIS_VIEWER_SHOW_STALE env var).
  const showStaleRaw = (req.query.show_stale || '').toString().toLowerCase();
  const showStale = showStaleRaw === '1' || showStaleRaw === 'true' || showStaleRaw === 'yes';
  const cacheKey = `${projectInfo.slug || projectInfo.repoPath}|${cycleKey}|stale=${showStale ? 1 : 0}`;
  const entry = _chatCallsCache.get(cacheKey);
  const now = Date.now();
  const ttl = _isLiveKey(cycleKey) ? CHAT_LIVE_TTL_MS : Infinity;
  if (entry && (now - entry.ts) < ttl) {
    res.json(entry.value);
    return;
  }
  try {
    const value = buildChatCalls(projectInfo, cycleKey, { showStale });
    _chatCallsCache.set(cacheKey, { ts: now, value });
    res.json(value);
  } catch (e) {
    res.status(500).json({ error: e.message });
  }
}

function handleChatEvents(req, res, project) {
  const projectInfo = typeof project === 'string' ? resolveRepoPath(project) : project;
  const cycle = (req.query.cycle || 'live').toString().trim();
  const callId = (req.query.call_id || '').toString().trim();
  const source = (req.query.source || 'transcript').toString().trim();
  const hints = {
    request_id: (req.query.request_id || '').toString().trim(),
    event_index: (req.query.event_index || '').toString().trim(),
    lane: (req.query.lane || '').toString().trim(),
    kind: (req.query.kind || '').toString().trim(),
    provider: (req.query.provider || '').toString().trim(),
    role: (req.query.role || '').toString().trim(),
  };
  if (!callId) {
    res.status(400).json({ error: 'missing call_id' });
    return;
  }
  if (!/^[A-Za-z0-9._+\-:]+$/.test(callId)) {
    res.status(400).json({ error: 'invalid call_id' });
    return;
  }
  const hintKey = JSON.stringify(hints);
  const cacheKey = `${projectInfo.slug || projectInfo.repoPath}|${cycle}|${callId}|${source}|${hintKey}`;
  const entry = _chatEventsCache.get(cacheKey);
  const now = Date.now();
  const isLive = _isLiveKey(cycle);
  // Per-entry TTL: cached `{missing:true}` payloads (which can happen when a
  // historical chat-events query races with `rebuild_cycle_chat_dirs`, or hits
  // a burst whose dir is being written) must NOT pin forever — otherwise a
  // transient miss permanently masks valid content for that (cycle, call_id).
  // Live entries keep their existing short TTL; live missing also short.
  const cachedTtl = entry && entry.value && entry.value.missing
    ? CHAT_LIVE_TTL_MS
    : (isLive ? CHAT_LIVE_TTL_MS : Infinity);
  if (entry && (now - entry.ts) < cachedTtl) {
    res.json(entry.value);
    return;
  }
  try {
    const value = buildChatEvents(projectInfo, cycle, callId, source, hints);
    _chatEventsCache.set(cacheKey, { ts: now, value });
    res.json(value);
  } catch (e) {
    res.status(500).json({ error: e.message });
  }
}

function handleFeedbackPost(req, res, project) {
  const { action, feedback } = req.body;
  const projectInfo = resolveRepoPath(project);
  const { repoPath, stateDir, repoType } = projectInfo;

  if (repoType === 'trellis') {
    try {
      res.json(trellisAdapter(projectInfo, 'feedback-post', [String(action || ''), '--feedback', String(feedback || '')]));
    } catch (e) {
      res.status(500).json({ error: e.message });
    }
    return;
  }
  JSON.parse(fs.readFileSync(path.join(stateDir, 'state.json'), 'utf-8'));

  if (action === 'approve') {
    const signalPath = path.join(stateDir, 'human_approve.json');
    fs.writeFileSync(signalPath, JSON.stringify({ action: 'approve', timestamp: new Date().toISOString() }));
    return res.json({ ok: true, message: 'Approval signal written. Supervisor will continue.' });
  }
  if (action === 'feedback') {
    const feedbackPath = path.join(repoPath, 'HUMAN_INPUT.md');
    fs.writeFileSync(feedbackPath, feedback || '');
    const signalPath = path.join(stateDir, 'human_feedback.json');
    fs.writeFileSync(signalPath, JSON.stringify({ action: 'feedback', feedback: feedback || '', timestamp: new Date().toISOString() }));
    const pausePath = path.join(stateDir, 'pause');
    try { fs.unlinkSync(pausePath); } catch {}
    return res.json({ ok: true, message: 'Feedback written. Supervisor will run another cycle.' });
  }
  return res.status(400).json({ error: 'action must be "approve" or "feedback"' });
}

function handleFeedbackGet(res, project) {
  const projectInfo = resolveRepoPath(project);
  const { repoPath, stateDir, repoType } = projectInfo;
  if (repoType === 'trellis') {
    res.json(trellisAdapter(projectInfo, 'feedback-get'));
    return;
  }
  const state = JSON.parse(fs.readFileSync(path.join(stateDir, 'state.json'), 'utf-8'));
  const awaiting = state.awaiting_human_input || false;
  const phase = state.phase || '';
  const lastReview = state.last_review || {};
  let humanInput = '';
  try { humanInput = fs.readFileSync(path.join(repoPath, 'HUMAN_INPUT.md'), 'utf-8'); } catch {}

  res.json({
    awaiting_input: awaiting,
    phase,
    last_review_decision: lastReview.decision || '',
    last_review_reason: lastReview.reason || '',
    human_input: humanInput,
  });
}

// Legacy URL redirect: any /leanbelt[...] path forwards to the corresponding
// trellis path under BASE. Preserves bookmarks/links from before the
// leanbelt → trellis rename (2026-05-20). 301 so caches/agents stop hitting
// the old path. Query strings carry through because we forward originalUrl.
app.get(/^\/leanbelt(\/.*)?$/, (req, res) => {
  const rest = req.originalUrl.substring('/leanbelt'.length);
  res.redirect(301, `${BASE}${rest}`);
});

app.get(BASE, (_req, res) => res.redirect(`${BASE}/${defaultProjectSlug()}/`));
app.get(`${BASE}/`, (_req, res) => res.redirect(`${BASE}/${defaultProjectSlug()}/`));
app.get(`${BASE}/:project`, (req, res, next) => {
  if ((req.originalUrl || '').endsWith('/')) {
    next();
    return;
  }
  res.redirect(`${BASE}/${req.params.project}/`);
});
app.get(`${BASE}/:project/`, sendIndex);
app.use(BASE, express.static(path.join(__dirname, 'public')));

app.get([PROMPTS_BASE, `${PROMPTS_BASE}/`], sendPromptsIndex);

app.get(`${PROMPTS_BASE}/api/projects.json`, (_req, res) => {
  try {
    const projects = discoverProjects().map(project => ({
      slug: project.slug,
      repoType: project.repoType,
    }));
    res.json({
      default_project: defaultPromptsProjectSlug(),
      projects,
    });
  } catch (e) {
    res.status(500).json({ error: e.message });
  }
});

app.get(`${PROMPTS_BASE}/api/catalog.json`, (req, res) => {
  try {
    const project = String(req.query.project || defaultPromptsProjectSlug());
    const projectInfo = resolveRepoPath(project);
    if (projectInfo.repoType !== 'trellis') {
      return res.status(400).json({ error: `prompt browser only supports trellis projects (got ${projectInfo.repoType || 'unknown'})` });
    }
    const payload = trellisAdapter(projectInfo, 'prompts-catalog');
    payload.project = projectInfo.slug;
    res.json(payload);
  } catch (e) {
    res.status(500).json({ error: e.message });
  }
});

app.get(`${PROMPTS_BASE}/api/render/:scenarioId`, (req, res) => {
  try {
    const project = String(req.query.project || defaultPromptsProjectSlug());
    const projectInfo = resolveRepoPath(project);
    if (projectInfo.repoType !== 'trellis') {
      return res.status(400).json({ error: `prompt browser only supports trellis projects (got ${projectInfo.repoType || 'unknown'})` });
    }
    const payload = trellisAdapter(projectInfo, 'prompts-render', [String(req.params.scenarioId)]);
    payload.project = projectInfo.slug;
    res.json(payload);
  } catch (e) {
    res.status(500).json({ error: e.message });
  }
});

// In-process TTL cache for live viewer-state and chats. Both endpoints
// re-read big JSON files (event_log, runtime state) on every request and
// take ~1s under load (mostly python startup + JSON parsing). Run state
// evolves on a ~minute scale, so a 30 s TTL is safe and matches the SPA's
// `AUTO_REFRESH_INTERVAL_MS`. Combined with the in-flight coalescer
// below, concurrent tabs/refreshes share a single python spawn instead
// of each spawning their own and blocking the Node.js event loop in
// sequence.
const LIVE_VIEWER_TTL_MS = 30 * 1000;
const _liveViewerStateCache = new Map();
const _liveChatsCache = new Map();

// In-flight Promise coalescer for `trellisAdapter`-style heavy calls.
// Key: arbitrary string identifying (project, command, args-fingerprint).
// Value: the Promise of the in-flight result.
//
// Concurrent callers for the same key await the SAME promise — they
// observe one python3 spawn cost instead of `n × python3` blocking the
// event loop in sequence. Once the promise settles (resolve or reject),
// the entry is deleted so the next call can re-run.
const _adapterInFlight = new Map();

function _adapterInFlightOnce(key, runner) {
  const existing = _adapterInFlight.get(key);
  if (existing) return existing;
  const p = Promise.resolve()
    .then(() => runner())
    .finally(() => _adapterInFlight.delete(key));
  _adapterInFlight.set(key, p);
  return p;
}

// Augment a viewer-state payload (live or historical) with per-node
// `closure_status` and a top-level `local_closure_summary`, sourced from the
// Patch C-A *committed* closure mirrors (`committed_local_closure_unverified_nodes`,
// `committed_local_closure_failures`, `committed_local_closure_records`) so the
// per-node colors stay tier-consistent with the committed DAG fields below
// (`committed.present_nodes` / `committed.open_nodes`). Read-only: never
// throws, always returns the input shape.
//
// Tier alignment (fix for audit LOW "viewer closure status mixes live closure
// fields with committed DAG fields"): the DAG layout and `open_nodes` are
// committed-tier; reading live-tier closure fields meant during an in-flight
// or just-accepted cycle a node could be colored from a different state-tier
// than the DAG it's drawn on. We now source closure status from committed
// mirrors so the two halves of the display agree.
//
// Defensive fallback chain (per node-shape field, in order):
//   1. committed_local_closure_* (kernel Patch C-A mirror; preferred)
//   2. local_closure_* (live-tier; fallback for pre-Patch-C-A state or migrated
//      checkpoints that don't yet carry the committed mirror)
//   3. empty (pre-Patch-C entirely; augmentation degrades to legacy
//      "verified == not in open_nodes" coloring)
//
// closure_status decision (per LOCAL_CLOSURE_IMPL_PLAN.md §9 viewer touch):
//   - "open"       in committed.open_nodes
//   - "unverified" not in committed.open_nodes AND in committed_local_closure_unverified_nodes
//   - "verified"   not in committed.open_nodes AND in committed_local_closure_records AND not unverified
//   - "absent"     not in committed.present_nodes
//
// Expected wire-shape (from trellis_adapter.py live-state / state-at):
//   payload = {
//     state: {
//       committed: { present_nodes: string[], open_nodes: string[] },
//       // Patch C-A committed mirrors (preferred source):
//       committed_local_closure_unverified_nodes?: string[],
//       committed_local_closure_records?: { [node: string]: object },
//       committed_local_closure_failures?: { [node: string]: ErrorSummary },
//       // Live-tier closure fields (fallback if committed mirrors absent):
//       local_closure_unverified_nodes?: string[],
//       local_closure_records?: { [node: string]: object },
//       local_closure_failures?: { [node: string]: ErrorSummary },
//       ...
//     },
//     nodes: { [name: string]: object },
//     ...
//   }
// Heavy per-node content (leanContent / texContent / declaration) is stripped
// from viewer-state and state-at responses and parked here so the client can
// lazy-fetch it for the *one* node it's about to render. The full live state
// is ~13 MB / ~1.6 MB gzipped, of which ~5.4 MB raw is these three string
// fields summed across ~500 nodes; the user typically views ≤1 detail panel
// at a time.
//
// key: `${projectKey}::${cycle}::${name}` -> { leanContent, texContent, declaration }
const _nodeContentCache = new Map();
const NODE_CONTENT_CACHE_MAX = 50000;

function _trimNodeContentCache() {
  if (_nodeContentCache.size <= NODE_CONTENT_CACHE_MAX) return;
  const overflow = _nodeContentCache.size - NODE_CONTENT_CACHE_MAX;
  let i = 0;
  for (const k of _nodeContentCache.keys()) {
    if (i++ >= overflow) break;
    _nodeContentCache.delete(k);
  }
}

const _DEF_RE = /^(noncomputable\s+)?def\s/m;

// Thin the viewer-state payload before it goes on the wire:
//
//   * For each node: replace `leanContent` / `texContent` / `declaration`
//     with empty strings (parked in `_nodeContentCache` for lazy fetch), and
//     precompute `isDefinition` so the DAG shape selector doesn't need the
//     full Lean text.
//
//   * Drop `state.committed_local_closure_records`. The client only existence-
//     checks records, and the fallback path already prefers
//     `local_closure_records` (same key set in practice — 405/405 in the
//     current run, with only ~10 entries differing in value). `closure_status`
//     (cheap, precomputed by augmentViewerStateClosure) is the real source.
//
// Saves ~7-9 MB raw / ~1 MB gz on each fetch.
function thinViewerStatePayload(payload, projectKey) {
  if (!payload || typeof payload !== 'object') return payload;
  const cycle = (payload.state && typeof payload.state.cycle === 'number')
    ? String(payload.state.cycle) : 'live';
  const nodes = (payload.nodes && typeof payload.nodes === 'object') ? payload.nodes : {};
  for (const [name, node] of Object.entries(nodes)) {
    if (!node || typeof node !== 'object') continue;
    const lean = typeof node.leanContent === 'string' ? node.leanContent : '';
    const tex = typeof node.texContent === 'string' ? node.texContent : '';
    const decl = typeof node.declaration === 'string' ? node.declaration : '';
    if (lean || tex || decl) {
      _nodeContentCache.set(`${projectKey}::${cycle}::${name}`,
        { leanContent: lean, texContent: tex, declaration: decl });
    }
    // DAG shape selector previously did `(node.leanContent || '').match(/^...def\s/m)`.
    // Precompute the bit so we can drop leanContent from the wire payload.
    node.isDefinition = node.texEnv === 'definition' || _DEF_RE.test(lean);
    node.leanContent = '';
    node.texContent = '';
    node.declaration = '';
  }
  _trimNodeContentCache();
  if (payload.state && payload.state.committed_local_closure_records) {
    delete payload.state.committed_local_closure_records;
  }
  payload.node_content_inline = false;
  return payload;
}

function augmentViewerStateClosure(payload) {
  if (!payload || typeof payload !== 'object') return payload;
  const state = payload.state || {};
  const committed = state.committed || {};
  const present = new Set(Array.isArray(committed.present_nodes) ? committed.present_nodes : []);
  const open = new Set(Array.isArray(committed.open_nodes) ? committed.open_nodes : []);
  // Prefer committed mirrors so coloring stays on the same tier as the
  // committed DAG; fall back to live-tier fields when the committed mirror is
  // absent (pre-Patch-C-A state, or migrated checkpoints without mirror-ready).
  const unverifiedList = Array.isArray(state.committed_local_closure_unverified_nodes)
    ? state.committed_local_closure_unverified_nodes
    : (Array.isArray(state.local_closure_unverified_nodes)
        ? state.local_closure_unverified_nodes // fallback
        : []);
  const unverified = new Set(unverifiedList);
  const failures = (state.committed_local_closure_failures && typeof state.committed_local_closure_failures === 'object')
    ? state.committed_local_closure_failures
    : ((state.local_closure_failures && typeof state.local_closure_failures === 'object')
        ? state.local_closure_failures // fallback
        : {});
  const records = (state.committed_local_closure_records && typeof state.committed_local_closure_records === 'object')
    ? state.committed_local_closure_records
    : ((state.local_closure_records && typeof state.local_closure_records === 'object')
        ? state.local_closure_records // fallback
        : {});

  const nodes = (payload.nodes && typeof payload.nodes === 'object') ? payload.nodes : {};
  const closureStatus = {};
  const allNames = new Set([
    ...Object.keys(nodes),
    ...present,
    ...unverifiedList,
  ]);
  for (const name of allNames) {
    if (!present.has(name) && !nodes[name]) {
      closureStatus[name] = 'absent';
    } else if (open.has(name)) {
      closureStatus[name] = 'open';
    } else if (unverified.has(name)) {
      closureStatus[name] = 'unverified';
    } else if (records[name]) {
      closureStatus[name] = 'verified';
    } else {
      // Sorry-free node with no record yet (pre-migration / fresh node) — treat
      // as "verified" if not in open_nodes, matching legacy color semantics so
      // existing runs without the closure layer don't suddenly turn yellow.
      closureStatus[name] = 'verified';
    }
  }

  // Failure summary: counts by status + the latest 3 messages by captured cycle.
  let transportErrors = 0, scriptErrors = 0, otherErrors = 0;
  const failureRows = [];
  for (const [node, summary] of Object.entries(failures)) {
    if (!summary || typeof summary !== 'object') continue;
    const status = String(summary.status || '');
    if (status === 'transport_error') transportErrors++;
    else if (status === 'elaboration_error' || status === 'missing_declaration' || status === 'internal_error') scriptErrors++;
    else otherErrors++;
    failureRows.push({
      node,
      status,
      stderr_excerpt: String(summary.stderr_excerpt || '').slice(0, 400),
      axiom_violations: Array.isArray(summary.axiom_violations) ? summary.axiom_violations.slice(0, 8) : [],
      strict_errors: Array.isArray(summary.strict_errors) ? summary.strict_errors.slice(0, 8) : [],
      retry_count: Number(summary.retry_count || 0),
      retry_exhausted: !!summary.retry_exhausted,
      captured_at_cycle: Number(summary.captured_at_cycle || 0),
      returncode: Number(summary.returncode || 0),
    });
  }
  failureRows.sort((a, b) => (b.captured_at_cycle || 0) - (a.captured_at_cycle || 0));
  const latestFailures = failureRows.slice(0, 3);

  payload.closure_status = closureStatus;
  payload.local_closure_summary = {
    unverified_count: unverifiedList.length,
    transport_error_count: transportErrors,
    script_error_count: scriptErrors,
    other_error_count: otherErrors,
    record_count: Object.keys(records).length,
    latest_failures: latestFailures,
  };
  return payload;
}

function readLiveViewerStateCached(projectInfo) {
  const key = projectInfo.slug || projectInfo.repoPath;
  const entry = _liveViewerStateCache.get(key);
  const now = Date.now();
  if (entry && (now - entry.ts) < LIVE_VIEWER_TTL_MS) return entry.value;
  const value = thinViewerStatePayload(
    augmentViewerStateClosure(readLiveViewerState(projectInfo)),
    projectCacheKey(projectInfo),
  );
  _liveViewerStateCache.set(key, { ts: now, value });
  return value;
}

// Async coalesced version of `readLiveViewerStateCached` for use by HTTP
// endpoints. When N tabs hit `/api/viewer-state.json` simultaneously and
// the cache is stale, only the first one spawns python; the rest await
// the same Promise. Python runs in `spawn` mode so it doesn't block the
// Node.js event loop while it computes — other endpoints remain
// responsive.
async function readLiveViewerStateCachedAsync(projectInfo) {
  const key = projectInfo.slug || projectInfo.repoPath;
  const entry = _liveViewerStateCache.get(key);
  const now = Date.now();
  if (entry && (now - entry.ts) < LIVE_VIEWER_TTL_MS) return entry.value;
  return _adapterInFlightOnce(`live-state::${key}`, async () => {
    // Re-check the cache: while we were queued behind the coalescer's
    // mutex, another caller may have populated it.
    const fresh = _liveViewerStateCache.get(key);
    if (fresh && (Date.now() - fresh.ts) < LIVE_VIEWER_TTL_MS) return fresh.value;
    let raw;
    if (projectInfo.repoType === 'trellis') {
      raw = await trellisAdapterAsync(projectInfo, 'live-state');
    } else {
      // Non-trellis repos use a sync file read; cheap, no event-loop concern.
      raw = readLiveViewerState(projectInfo);
    }
    const value = thinViewerStatePayload(
      augmentViewerStateClosure(raw),
      projectCacheKey(projectInfo),
    );
    _liveViewerStateCache.set(key, { ts: Date.now(), value });
    return value;
  });
}

function readLiveChatsCached(projectInfo) {
  const key = projectInfo.slug || projectInfo.repoPath;
  const entry = _liveChatsCache.get(key);
  const now = Date.now();
  if (entry && (now - entry.ts) < LIVE_VIEWER_TTL_MS) return entry.value;
  const value = readLiveChats(projectInfo);
  _liveChatsCache.set(key, { ts: now, value });
  return value;
}

// API endpoints
app.get(`${BASE}/api/viewer-state.json`, async (req, res) => {
  try {
    const projectInfo = resolveRepoPath(defaultProjectSlug());
    res.json(await readLiveViewerStateCachedAsync(projectInfo));
  } catch (e) { res.status(500).json({ error: e.message }); }
});

app.get(`${BASE}/:project/api/viewer-state.json`, async (req, res) => {
  try {
    const projectInfo = resolveRepoPath(projectFromRequest(req));
    res.json(await readLiveViewerStateCachedAsync(projectInfo));
  } catch (e) { res.status(500).json({ error: e.message }); }
});

// ---------------------------------------------------------------------------
// Per-node Lean semantic closure (lazy, cached per (project, node, cycle)).
//
// The viewer adapter's `semantic-closure` command reads each node's
// most-recent `checker-state/semantic-payloads/*.json` sidecar and returns
// `{ closures: { node: [closure_nodes…] | null } }`. Each adapter call is
// fast (one disk read + one parse per requested node — ~50-100 ms warm).
// We still cache the answer here per cycle so navigating back to a node
// the user already viewed doesn't re-spawn python3.
//
// Cache is invalidated when the live `cycle` advances, so freshly-edited
// statements pick up the new closure on the next cycle commit. A node
// for which there's no cached payload (new helper just introduced)
// returns `null`; the UI shows a "no cached payload" hint and the next
// supervisor cycle will populate it.
// ---------------------------------------------------------------------------
const semanticClosureCache = new Map(); // key: `${projectKey}::${cycle}::${node}` -> closure array | null

function liveCycleFor(projectInfo) {
  try {
    const live = readLiveViewerStateCached(projectInfo);
    const c = live && live.state && live.state.cycle;
    return typeof c === 'number' ? c : 0;
  } catch {
    return 0;
  }
}

function semanticClosureHandler(projectInfo, nodeName, res) {
  if (typeof nodeName !== 'string' || !/^[A-Za-z][A-Za-z0-9_]*$/.test(nodeName)) {
    return res.status(400).json({ error: 'Invalid node name' });
  }
  const cycle = liveCycleFor(projectInfo);
  const key = `${projectInfo.projectKey || projectInfo.repoPath}::${cycle}::${nodeName}`;
  if (semanticClosureCache.has(key)) {
    return res.json({ node: nodeName, cycle, closure: semanticClosureCache.get(key), cached: true });
  }
  try {
    const payload = trellisAdapter(projectInfo, 'semantic-closure', ['--node', nodeName]);
    if (!payload || payload.ok === false) {
      return res.status(503).json({
        node: nodeName,
        cycle,
        closure: null,
        error: (payload && payload.error) || 'semantic-closure unavailable',
      });
    }
    const closures = (payload.closures || {});
    const result = Object.prototype.hasOwnProperty.call(closures, nodeName) ? closures[nodeName] : null;
    semanticClosureCache.set(key, result);
    // Bound cache size — drop oldest entries past 2000.
    if (semanticClosureCache.size > 2000) {
      const firstKey = semanticClosureCache.keys().next().value;
      semanticClosureCache.delete(firstKey);
    }
    return res.json({ node: nodeName, cycle, closure: result, cached: false });
  } catch (e) {
    return res.status(500).json({ node: nodeName, cycle, closure: null, error: e.message });
  }
}

// Per-node heavy content (leanContent / texContent / declaration). Populated
// as a side-effect of thinViewerStatePayload; lazy-fetched by the client when
// it actually opens a node-detail pane. Cache misses (LRU eviction, server
// restart) repopulate by re-reading the underlying state.
function nodeContentHandler(projectInfo, cycleParam, nodeName, res) {
  if (typeof nodeName !== 'string' || !/^[A-Za-z][A-Za-z0-9_]*$/.test(nodeName)) {
    return res.status(400).json({ error: 'Invalid node name' });
  }
  const projectKey = projectCacheKey(projectInfo);
  const wantsLive = cycleParam === 'live' || cycleParam === 'current' || cycleParam == null;
  let cycleNum = null;
  if (!wantsLive) {
    cycleNum = parseInt(cycleParam, 10);
    if (!Number.isInteger(cycleNum)) return res.status(400).json({ error: 'Invalid cycle' });
  }
  const cycleStr = wantsLive ? String(liveCycleFor(projectInfo)) : String(cycleNum);
  const key = `${projectKey}::${cycleStr}::${nodeName}`;
  let hit = _nodeContentCache.get(key);
  if (!hit) {
    try {
      if (wantsLive) {
        _liveViewerStateCache.delete(projectInfo.slug || projectInfo.repoPath);
        readLiveViewerStateCached(projectInfo);
      } else {
        cycleStateCache.delete(cycleEntryKey(projectInfo, cycleNum));
        getCachedHistoricalViewerState(projectInfo, cycleNum);
      }
      hit = _nodeContentCache.get(key);
    } catch (e) {
      return res.status(500).json({ error: e.message });
    }
  }
  if (!hit) return res.status(404).json({ error: `No content for ${nodeName} @ ${cycleStr}` });
  return res.json({ node: nodeName, cycle: cycleStr, ...hit });
}

app.get(`${BASE}/api/node-content/:cycle/:node`, (req, res) => {
  const projectInfo = resolveRepoPath(defaultProjectSlug());
  const node = String(req.params.node).replace(/\.json$/, '');
  nodeContentHandler(projectInfo, req.params.cycle, node, res);
});
app.get(`${BASE}/:project/api/node-content/:cycle/:node`, (req, res) => {
  const projectInfo = resolveRepoPath(projectFromRequest(req));
  const node = String(req.params.node).replace(/\.json$/, '');
  nodeContentHandler(projectInfo, req.params.cycle, node, res);
});

app.get(`${BASE}/api/semantic-closure/:node`, (req, res) => {
  try {
    const projectInfo = resolveRepoPath(defaultProjectSlug());
    semanticClosureHandler(projectInfo, req.params.node, res);
  } catch (e) { res.status(500).json({ error: e.message }); }
});

app.get(`${BASE}/:project/api/semantic-closure/:node`, (req, res) => {
  try {
    const projectInfo = resolveRepoPath(projectFromRequest(req));
    semanticClosureHandler(projectInfo, req.params.node, res);
  } catch (e) { res.status(500).json({ error: e.message }); }
});

app.get(`${BASE}/api/cycles.json`, (req, res) => {
  try {
    const projectInfo = resolveRepoPath(defaultProjectSlug());
    res.json(getCachedCyclesList(projectInfo));
  } catch { res.json([]); }
});

app.get(`${BASE}/:project/api/cycles.json`, (req, res) => {
  try {
    const projectInfo = resolveRepoPath(projectFromRequest(req));
    res.json(getCachedCyclesList(projectInfo));
  } catch { res.json([]); }
});

app.get(`${BASE}/api/state-at/:cycle`, (req, res) => {
  const cycle = parseInt(String(req.params.cycle).replace(/\.json$/, ''), 10);
  if (isNaN(cycle)) return res.status(400).json({ error: 'Invalid cycle' });
  try {
    const projectInfo = resolveRepoPath(defaultProjectSlug());
    res.json(getCachedHistoricalViewerState(projectInfo, cycle));
  } catch (e) {
    res.status(404).json({ error: `Cycle ${cycle} not found: ${e.message}` });
  }
});

app.get(`${BASE}/:project/api/state-at/:cycle`, (req, res) => {
  const cycle = parseInt(String(req.params.cycle).replace(/\.json$/, ''), 10);
  if (isNaN(cycle)) return res.status(400).json({ error: 'Invalid cycle' });
  try {
    const projectInfo = resolveRepoPath(projectFromRequest(req));
    res.json(getCachedHistoricalViewerState(projectInfo, cycle));
  } catch (e) {
    res.status(404).json({ error: `Cycle ${cycle} not found: ${e.message}` });
  }
});

app.get(`${BASE}/api/chats.json`, (req, res) => {
  try {
    const projectInfo = resolveRepoPath(defaultProjectSlug());
    res.json(readLiveChatsCached(projectInfo));
  } catch (e) {
    res.status(500).json({ error: e.message });
  }
});

app.get(`${BASE}/:project/api/chats.json`, (req, res) => {
  try {
    const projectInfo = resolveRepoPath(projectFromRequest(req));
    res.json(readLiveChatsCached(projectInfo));
  } catch (e) {
    res.status(500).json({ error: e.message });
  }
});

app.get(`${BASE}/api/chats-at/:cycle`, (req, res) => {
  const cycle = parseInt(String(req.params.cycle).replace(/\.json$/, ''), 10);
  if (isNaN(cycle)) return res.status(400).json({ error: 'Invalid cycle' });
  try {
    const projectInfo = resolveRepoPath(defaultProjectSlug());
    res.json(getCachedHistoricalChats(projectInfo, cycle));
  } catch (e) {
    res.status(404).json({ error: `Chat cycle ${cycle} not found: ${e.message}` });
  }
});

app.get(`${BASE}/:project/api/chats-at/:cycle`, (req, res) => {
  const cycle = parseInt(String(req.params.cycle).replace(/\.json$/, ''), 10);
  if (isNaN(cycle)) return res.status(400).json({ error: 'Invalid cycle' });
  try {
    const projectInfo = resolveRepoPath(projectFromRequest(req));
    res.json(getCachedHistoricalChats(projectInfo, cycle));
  } catch (e) {
    res.status(404).json({ error: `Chat cycle ${cycle} not found: ${e.message}` });
  }
});

app.get(`${BASE}/api/diff/:cycle`, (req, res) => {
  const cycle = parseInt(req.params.cycle, 10);
  if (isNaN(cycle)) return res.status(400).send('Invalid cycle');
  const projectInfo = resolveRepoPath(defaultProjectSlug());
  const diff = getCachedCycleDiff(projectInfo, cycle);
  res.type('text/plain').send(diff);
});

app.get(`${BASE}/:project/api/diff/:cycle`, (req, res) => {
  const cycle = parseInt(req.params.cycle, 10);
  if (isNaN(cycle)) return res.status(400).send('Invalid cycle');
  const projectInfo = resolveRepoPath(projectFromRequest(req));
  const diff = getCachedCycleDiff(projectInfo, cycle);
  res.type('text/plain').send(diff);
});

// API: download tablet snapshot as zip
app.get(`${BASE}/api/download-tablet`, (req, res) => {
  try {
    handleDownloadTablet(res, defaultProjectSlug());
  } catch (e) {
    res.status(500).json({ error: e.message });
  }
});

app.get(`${BASE}/:project/api/download-tablet`, (req, res) => {
  try {
    handleDownloadTablet(res, projectFromRequest(req));
  } catch (e) {
    res.status(500).json({ error: e.message });
  }
});

// Paper-defined KaTeX macros. The frontend fetches this once at init
// and merges the \newcommand bodies into renderMathInElement's macros
// option. Returns the project's paper/header.tex if present, else
// empty body. We deliberately don't 404 — a project without a header
// just gets the viewer's hardcoded defaults.
function handlePaperHeader(res, slug) {
  try {
    const projectInfo = resolveRepoPath(slug);
    const headerPath = path.join(projectInfo.repoPath, 'paper', 'header.tex');
    if (fs.existsSync(headerPath)) {
      res.type('text/plain').send(fs.readFileSync(headerPath, 'utf8'));
    } else {
      res.type('text/plain').send('');
    }
  } catch (e) {
    res.status(500).type('text/plain').send(`% error: ${e.message}`);
  }
}
app.get(`${BASE}/api/paper-header.tex`, (_req, res) => handlePaperHeader(res, defaultProjectSlug()));
app.get(`${BASE}/:project/api/paper-header.tex`, (req, res) => handlePaperHeader(res, projectFromRequest(req)));

// External-codex tracker: hidden viewer feature for marking cycles
// during which the user was running codex CLI sessions outside this
// project. Those cycles' burn deltas are tainted and should be
// dropped from β-calibration.
//
// State lives at <stateDir>/external_codex.json with shape:
//   { active: bool,
//     since_cycle: int|null, since_iso: str|null,
//     intervals: [{since_cycle, since_iso, until_cycle, until_iso}, ...] }
//
// The UI toggles via Ctrl+Shift+E. POST /toggle flips `active`:
// turning ON captures since_cycle/iso; turning OFF closes the open
// interval into `intervals`. The GET response also surfaces a
// derived `marked_cycles` (union over closed intervals + open one)
// for easy consumption by scripts/fit_codex_burn_beta.py.
function externalCodexStatePath(projectInfo) {
  return path.join(projectInfo.stateDir, 'external_codex.json');
}
function readExternalCodex(projectInfo) {
  const p = externalCodexStatePath(projectInfo);
  if (!fs.existsSync(p)) {
    return { active: false, since_cycle: null, since_iso: null, intervals: [] };
  }
  try {
    const d = JSON.parse(fs.readFileSync(p, 'utf-8'));
    return {
      active: !!d.active,
      since_cycle: d.since_cycle ?? null,
      since_iso: d.since_iso ?? null,
      intervals: Array.isArray(d.intervals) ? d.intervals : [],
    };
  } catch {
    return { active: false, since_cycle: null, since_iso: null, intervals: [] };
  }
}
function writeExternalCodex(projectInfo, state) {
  const p = externalCodexStatePath(projectInfo);
  fs.mkdirSync(path.dirname(p), { recursive: true });
  const tmp = p + '.tmp';
  fs.writeFileSync(tmp, JSON.stringify(state, null, 2));
  fs.renameSync(tmp, p);
}
function currentCycleForProject(projectInfo) {
  try {
    const v = readLiveViewerStateCached(projectInfo);
    return v?.meta?.in_flight_cycle ?? v?.state?.cycle ?? null;
  } catch {
    return null;
  }
}
function deriveMarkedCycles(state, currentCycle) {
  const set = new Set();
  for (const iv of state.intervals || []) {
    const a = iv.since_cycle;
    const b = iv.until_cycle ?? a;
    if (a == null) continue;
    for (let c = a; c <= b; c++) set.add(c);
  }
  if (state.active && state.since_cycle != null) {
    const cur = currentCycle ?? state.since_cycle;
    for (let c = state.since_cycle; c <= cur; c++) set.add(c);
  }
  return Array.from(set).sort((a, b) => a - b);
}
function externalCodexResponse(projectInfo) {
  const s = readExternalCodex(projectInfo);
  const cur = currentCycleForProject(projectInfo);
  return {
    ...s,
    current_cycle: cur,
    marked_cycles: deriveMarkedCycles(s, cur),
  };
}
function handleToggleExternalCodex(projectInfo) {
  const cur = currentCycleForProject(projectInfo);
  const s = readExternalCodex(projectInfo);
  const nowIso = new Date().toISOString();
  if (s.active) {
    s.intervals.push({
      since_cycle: s.since_cycle,
      since_iso: s.since_iso,
      until_cycle: cur,
      until_iso: nowIso,
    });
    s.active = false;
    s.since_cycle = null;
    s.since_iso = null;
  } else {
    s.active = true;
    s.since_cycle = cur;
    s.since_iso = nowIso;
  }
  writeExternalCodex(projectInfo, s);
  return externalCodexResponse(projectInfo);
}
app.get(`${BASE}/api/external-codex.json`, (_req, res) => {
  try { res.json(externalCodexResponse(resolveRepoPath(defaultProjectSlug()))); }
  catch (e) { res.status(500).json({ error: e.message }); }
});
app.get(`${BASE}/:project/api/external-codex.json`, (req, res) => {
  try { res.json(externalCodexResponse(resolveRepoPath(projectFromRequest(req)))); }
  catch (e) { res.status(500).json({ error: e.message }); }
});
app.post(`${BASE}/api/external-codex/toggle`, (_req, res) => {
  try { res.json(handleToggleExternalCodex(resolveRepoPath(defaultProjectSlug()))); }
  catch (e) { res.status(500).json({ error: e.message }); }
});
app.post(`${BASE}/:project/api/external-codex/toggle`, (req, res) => {
  try { res.json(handleToggleExternalCodex(resolveRepoPath(projectFromRequest(req)))); }
  catch (e) { res.status(500).json({ error: e.message }); }
});

// Halt-state surface for the fail-loudly checker-disagreement halt.
// Returns `{ halted: false }` when the marker is absent, else
// `{ halted: true, marker_path, marker }` with the parsed JSON body so
// the frontend can render a banner WITHOUT a second fetch.
function checkerDisagreementHaltState(projectInfo) {
  const runtimeRoot = runtimeRootForProject(projectInfo);
  if (!runtimeRoot) return { halted: false, reason: 'no_runtime_root' };
  const markerPath = path.join(runtimeRoot, 'checker_disagreement_halt.json');
  if (!fs.existsSync(markerPath)) return { halted: false };
  let marker = null;
  try { marker = JSON.parse(fs.readFileSync(markerPath, 'utf-8')); }
  catch (e) { return { halted: true, marker_path: markerPath, parse_error: e.message }; }
  return { halted: true, marker_path: markerPath, marker };
}
app.get(`${BASE}/api/halt-state.json`, (_req, res) => {
  try { res.json(checkerDisagreementHaltState(resolveRepoPath(defaultProjectSlug()))); }
  catch (e) { res.status(500).json({ error: e.message }); }
});
app.get(`${BASE}/:project/api/halt-state.json`, (req, res) => {
  try { res.json(checkerDisagreementHaltState(resolveRepoPath(projectFromRequest(req)))); }
  catch (e) { res.status(500).json({ error: e.message }); }
});

// Unified Chats tab: list of calls for a cycle + per-call structured events.
// (Replaces the old live-panes.json / burst-log.json endpoints. Those were
// removed after the frontend migrated to chat-calls.json / chat-events.json.)
app.get(`${BASE}/api/chat-calls.json`, (req, res) => {
  handleChatCalls(req, res, defaultProjectSlug());
});

app.get(`${BASE}/:project/api/chat-calls.json`, (req, res) => {
  handleChatCalls(req, res, projectFromRequest(req));
});

app.get(`${BASE}/api/chat-events.json`, (req, res) => {
  handleChatEvents(req, res, defaultProjectSlug());
});

app.get(`${BASE}/:project/api/chat-events.json`, (req, res) => {
  handleChatEvents(req, res, projectFromRequest(req));
});

// API: submit human feedback
app.use(express.json());

app.post(`${BASE}/api/feedback`, (req, res) => {
  try {
    handleFeedbackPost(req, res, defaultProjectSlug());
  } catch (e) {
    res.status(500).json({ error: e.message });
  }
});

app.post(`${BASE}/:project/api/feedback`, (req, res) => {
  try {
    handleFeedbackPost(req, res, projectFromRequest(req));
  } catch (e) {
    return res.status(500).json({ error: e.message });
  }
});

// API: get current human feedback status
app.get(`${BASE}/api/feedback`, (req, res) => {
  try {
    handleFeedbackGet(res, defaultProjectSlug());
  } catch (e) {
    res.status(500).json({ error: e.message });
  }
});

app.get(`${BASE}/:project/api/feedback`, (req, res) => {
  try {
    handleFeedbackGet(res, projectFromRequest(req));
  } catch (e) {
    res.status(500).json({ error: e.message });
  }
});

// =====================================================================
// /api/usage — provider+role rollups + check ledger + per-stage walltime
// + latest quota snapshot per provider. Mirrors trellis/usage_report.py.
// =====================================================================

function readJsonlSync(p) {
  if (!p || !fs.existsSync(p)) return [];
  try {
    return fs.readFileSync(p, 'utf8').split('\n').filter(Boolean).map((l) => {
      try { return JSON.parse(l); } catch { return null; }
    }).filter(Boolean);
  } catch { return []; }
}

// Monthly subscription prices for the Usage tab's "effective USD/mo"
// estimate. These are the user's stated personal-plan figures; if you
// upgrade/downgrade, edit here.
const MONTHLY_SUBSCRIPTION_USD = {
  claude: 200,
  gemini: 250,
  codex: 200,
};

function buildUsageRollup(projectInfo) {
  const repo = projectInfo.repoPath;
  const runtimeRoot = runtimeRootForProject(projectInfo);
  const cost = readJsonlSync(path.join(repo, '.trellis', 'logs', 'cost-ledger.jsonl'));
  const check = readJsonlSync(path.join(repo, '.trellis', 'logs', 'check-ledger.jsonl'));
  const quota = readJsonlSync(path.join(repo, '.trellis', 'logs', 'quota-snapshots.jsonl'));
  const events = runtimeRoot ? readJsonlSync(path.join(runtimeRoot, 'event_log.jsonl')) : [];

  function emptyAgg() {
    return { bursts: 0, ok: 0, duration_s: 0, input: 0, output: 0,
             cache_read: 0, cache_write: 0, messages: 0, bursts_with_msgs: 0 };
  }

  function addCostRow(map, key, r) {
    if (!map[key]) map[key] = emptyAgg();
    const a = map[key];
    a.bursts++;
    if (r.ok) a.ok++;
    a.duration_s += parseFloat(r.duration_seconds || 0) || 0;
    const u = r.usage || {};
    a.input += parseInt(u.input_tokens || u.input || 0) || 0;
    a.output += parseInt(u.output_tokens || u.output || 0) || 0;
    a.cache_read += parseInt(u.cache_read_input_tokens || u.cached_input_tokens || u.cached || 0) || 0;
    a.cache_write += parseInt(u.cache_creation_input_tokens || 0) || 0;
    if (typeof r.message_count === 'number' && r.message_count >= 0) {
      a.messages += r.message_count;
      a.bursts_with_msgs++;
    }
  }

  // role in the cost ledger is just "worker" or "reviewer", but the
  // reviewer slot covers four distinct callers — paper, corr, sound (the
  // three verifier kinds) and the actual reviewer (`review`). The
  // verifier kind is encoded in the scope, e.g.
  //   proof_formalization:reviewer:paper:135:v1:claude:claude-opus-4-6:max
  //   proof_formalization:reviewer:review:claude:claude-opus-4-6:max
  // Split them out for the per-(provider, category) breakdown so cost
  // attribution actually reflects which kind of caller spent the budget.
  function categoryFor(r) {
    const role = r.role || '?';
    if (role === 'worker') return 'worker';
    const scope = String(r.scope || '');
    for (const k of ['paper', 'corr', 'sound', 'review']) {
      if (scope.includes(`:${k}:`)) return k;
    }
    return role;
  }

  const byProvider = {};
  const byProviderCategory = {};
  for (const r of cost) {
    const prov = r.provider || '?';
    const cat = categoryFor(r);
    addCostRow(byProvider, prov, r);
    addCostRow(byProviderCategory, `${prov}::${cat}`, r);
  }

  // Check ledger split by kind
  const byCheckSub = {};
  const byGitSub = {};
  for (const r of check) {
    const kind = r.kind || 'check';
    const sub = r.subcommand || '?';
    const dur = parseFloat(r.duration_seconds || 0) || 0;
    const map = (kind === 'git') ? byGitSub : byCheckSub;
    if (!map[sub]) map[sub] = { count: 0, ok: 0, duration_s: 0 };
    map[sub].count++;
    if (r.ok) map[sub].ok++;
    map[sub].duration_s += dur;
  }

  // Per-stage wall-clock from event_log ts_ms deltas
  const byStage = {};
  for (let i = 0; i < events.length - 1; i++) {
    const a = events[i], b = events[i + 1];
    const ta = parseInt(a.ts_ms || 0), tb = parseInt(b.ts_ms || 0);
    if (ta <= 0 || tb <= ta) continue;
    const stage = String(a.stage || '?');
    const dt = (tb - ta) / 1000;
    if (!byStage[stage]) byStage[stage] = { intervals: 0, duration_s: 0 };
    byStage[stage].intervals++;
    byStage[stage].duration_s += dt;
  }

  // Latest quota snapshot per provider (success or failure)
  const latestQuota = {};
  for (const r of quota) {
    const p = r.provider;
    if (!p) continue;
    if (!latestQuota[p] || (Number(r.ts) > Number(latestQuota[p].ts))) {
      latestQuota[p] = r;
    }
  }

  // 5h burn data is hidden here per the model-based cost-reporting
  // policy. The β model (built below as `codexCostRollup`) is the
  // canonical source for cost USD; the latest snapshot's per-window
  // burn rates and any quota-derived effective_monthly_usd estimate
  // are no longer surfaced in the API response. We continue to write
  // 5h data into quota-snapshots.jsonl going forward — that record
  // stays available on disk for re-calibrating the β model when more
  // data accumulates — but it is intentionally not exposed here.
  //
  // Subscription metadata (account, plan_tier, credits, ts, ok) is
  // preserved on each snapshot as operational state.
  for (const prov of Object.keys(latestQuota)) {
    const snap = latestQuota[prov];
    if (!snap) continue;
    const price = MONTHLY_SUBSCRIPTION_USD[prov];
    if (price) snap.subscription_usd = price;
    delete snap.windows;
    delete snap.models;
    delete snap.effective_monthly_usd;
    delete snap.effective_monthly_usd_basis;
  }

  // Build per-provider and per-(provider, category) arrays first so we
  // can annotate them with attributed monthly_burn USD/% (replacing the
  // old API-equivalent USD column, which was fictitious for subscription
  // accounts). `category` ∈ {worker, paper, corr, sound, review}.
  const byProviderArr = Object.entries(byProvider)
    .map(([k, v]) => ({ provider: k, ...v }))
    .sort((x, y) => x.provider.localeCompare(y.provider));
  const byProviderCategoryArr = Object.entries(byProviderCategory)
    .map(([k, v]) => { const [p, c] = k.split('::'); return { provider: p, category: c, ...v }; })
    .sort((x, y) => `${x.provider}::${x.category}`.localeCompare(`${y.provider}::${y.category}`));

  function totalTokens(r) {
    return (r.input || 0) + (r.output || 0) + (r.cache_read || 0) + (r.cache_write || 0);
  }

  // Per-provider total tokens (kept for token_share reporting only — the
  // USD/burn columns no longer use it). Total tokens were the old
  // attribution-share metric; we now use per-burst quota deltas instead.
  const providerTotalTokens = {};
  for (const r of byProviderArr) providerTotalTokens[r.provider] = totalTokens(r);

  // ===== per-burst quota-delta USD attribution =============================
  //
  // For each cost-ledger row that has bracketing probes (quota_pre +
  // quota_post), compute the "weekly_pct" (what fraction of one week's quota
  // this single burst consumed) by diffing pct_used. Reset-aware:
  //   pre_pct=80, post_pct=5 with post_resets_at > pre_resets_at  →
  //     burst spanned a quota reset; consumption = (100 - 80) + 5 = 25.
  //
  // For codex/claude we PREFER the 5h delta (×1/7 to project to weekly)
  // because the 5h window resets often enough that mid-burst noise from
  // OTHER agents is bounded. Fall back to the weekly delta when no 5h
  // signal is present.
  //
  // For gemini there is no 5h vs weekly distinction — only per-category
  // daily windows. Take the max delta across categories ×1/7 as the
  // weekly_pct contribution.
  //
  // Sum weekly_pct over all bursts in a (provider) or (provider, category)
  // bucket, then convert to monthly_burn_usd via the configured monthly
  // subscription price. Coverage = (rows with both probes) / (rows total),
  // surfaced so a low coverage flags an underestimate.

  const PROVIDER_WEEKLY_TO_MONTHLY = { claude: 1/4, codex: 1/4 };
  // codex: empirical calibration history —
  //   1/7 (initial naïve assumption) underestimated by ~27% over a
  //   24h dedicated-account check (predicted 6.57 weekly-pct, observed 9.0).
  //   1/6 (interim correction) still underestimated; the β-model+1/6
  //   total ($7.79/mo) was ~22% below the direct sum of per-burst
  //   weekly_pct deltas ($10.00/mo) over cycles 1-77 of example-run.
  //   1/5 was overshooting the codex meter by ~8% over a clean 19.4h
  //   verifier-complete window: model 19.5 weekly_pct vs meter 18.0.
  //   1/5.5 (current) trims the over-prediction; lands within ~1% of
  //   the meter's reading on that window.
  // Revisit once a token-based cost proxy lands or per-phase β
  // calibration tightens the lane breakdown.
  const PROVIDER_5H_TO_WEEKLY = { claude: 1/7, codex: 1/5.5 };

  function deltaPctReset(prePct, postPct, preResetsAt, postResetsAt) {
    if (prePct == null || postPct == null) return null;
    const preNum = Number(prePct), postNum = Number(postPct);
    if (!Number.isFinite(preNum) || !Number.isFinite(postNum)) return null;
    if (preResetsAt != null && postResetsAt != null
        && Number(postResetsAt) > Number(preResetsAt) + 60) {
      // Burst spanned a reset boundary. Pre-reset depletion is invisible
      // (the meter zeroed out), so estimate total consumption as 2× the
      // post-reset depletion under the assumption of constant burst rate.
      // Cap at 100 since one window can't deplete more than itself.
      return Math.min(100, 2 * Math.max(0, postNum));
    }
    return Math.max(0, postNum - preNum);
  }

  function weeklyPctForBurstCodexLike(qpre, qpost, prov) {
    if (!qpre || !qpost) return null;
    const fhDelta = deltaPctReset(
      qpre.five_hour_pct, qpost.five_hour_pct,
      qpre.five_hour_resets_at, qpost.five_hour_resets_at,
    );
    if (fhDelta != null) return fhDelta * (PROVIDER_5H_TO_WEEKLY[prov] || 1/7);
    const wDelta = deltaPctReset(
      qpre.weekly_pct, qpost.weekly_pct,
      qpre.weekly_resets_at, qpost.weekly_resets_at,
    );
    return wDelta;
  }

  function weeklyPctForBurstGemini(qpre, qpost) {
    if (!qpre || !qpost) return null;
    const preCats = (qpre.models || []).reduce((m, x) => {
      if (x && x.category) m[x.category] = x;
      return m;
    }, {});
    let maxWeekly = 0;
    let saw = false;
    for (const post of (qpost.models || [])) {
      if (!post || !post.category) continue;
      const pre = preCats[post.category];
      if (!pre) continue;
      const d = deltaPctReset(pre.pct_used, post.pct_used, pre.resets_at, post.resets_at);
      if (d != null) {
        saw = true;
        const weekly = d * (1/7);
        if (weekly > maxWeekly) maxWeekly = weekly;
      }
    }
    return saw ? maxWeekly : null;
  }

  function weeklyPctToUsd(weeklyPct, prov) {
    const sub = MONTHLY_SUBSCRIPTION_USD[prov];
    const w2m = PROVIDER_WEEKLY_TO_MONTHLY[prov] || 1/4;
    if (!sub) return null;
    return Number((weeklyPct * w2m * sub / 100).toFixed(3));
  }

  // Aggregation: per-(provider) and per-(provider, category), sum the
  // weekly_pct contributions. probe_coverage tracks how many rows had usable
  // bracketing probes vs total rows.
  const burnPctByProv = {};
  const burnPctByProvCat = {};
  const probeCoverageByProv = {};
  const probeCoverageByProvCat = {};

  for (const r of cost) {
    const prov = r.provider || '?';
    const cat = categoryFor(r);
    const k = `${prov}::${cat}`;
    if (!probeCoverageByProv[prov]) probeCoverageByProv[prov] = { withProbes: 0, total: 0 };
    probeCoverageByProv[prov].total++;
    if (!probeCoverageByProvCat[k]) probeCoverageByProvCat[k] = { withProbes: 0, total: 0 };
    probeCoverageByProvCat[k].total++;

    let weeklyPct = null;
    let estimated = false;
    if (prov === 'codex' || prov === 'claude') {
      weeklyPct = weeklyPctForBurstCodexLike(r.quota_pre, r.quota_post, prov);
    } else if (prov === 'gemini') {
      weeklyPct = weeklyPctForBurstGemini(r.quota_pre, r.quota_post);
    }
    // Backfill: when a row has no usable bracket (pre-deploy bursts, probe
    // failures), estimate weekly_pct from burst duration alone, treating it
    // as if it ran at the rate that would saturate the weekly quota over a
    // full week. duration_seconds / weekly_window_seconds × 100. Coarse but
    // self-documenting; over a full run the over/under-estimates average out.
    if (weeklyPct == null || !Number.isFinite(weeklyPct)) {
      const dur = parseFloat(r.duration_seconds || 0) || 0;
      if (dur > 0 && (prov === 'codex' || prov === 'claude' || prov === 'gemini')) {
        const WEEKLY_WINDOW_SECONDS = 7 * 24 * 3600;
        weeklyPct = (dur / WEEKLY_WINDOW_SECONDS) * 100;
        estimated = true;
      }
    }
    if (weeklyPct == null || !Number.isFinite(weeklyPct)) continue;
    if (!estimated) {
      probeCoverageByProv[prov].withProbes++;
      probeCoverageByProvCat[k].withProbes++;
    }
    burnPctByProv[prov] = (burnPctByProv[prov] || 0) + weeklyPct;
    burnPctByProvCat[k] = (burnPctByProvCat[k] || 0) + weeklyPct;
  }

  // For codex, replace the quota-delta-derived weekly_pct with a
  // model-derived prediction: model_burn_5h_pct = β(phase, lane) ·
  // llm_seconds, where llm_seconds comes from the rollout files. β is
  // calibrated empirically per (phase, lane) — see
  // trellis.codex_timing.BETA_BY_PHASE_LANE for the constants and the
  // β-stability analysis under cycles 1-72 of the live run for the
  // methodology. We pin a single β model across phases initially; the
  // dict structure leaves room for per-phase calibration as evidence
  // accumulates.
  //
  // The 5h-pct → weekly conversion (× 1/6 for codex) and the
  // weekly → monthly USD conversion are unchanged — those are
  // subscription-policy constants, not part of the β fit.
  //
  // Non-codex providers (claude, gemini) don't have rollouts to feed
  // the β model, so they fall back to the legacy per-burst quota-delta
  // path. When/if rollouts exist for them too, lift them into the
  // model side here.
  const codexCostRollup = (() => {
    try {
      const ledgerPath = path.join(repo, '.trellis', 'logs', 'cost-ledger.jsonl');
      const env = {
        ...process.env,
        PYTHONPATH: process.env.PYTHONPATH
          ? `${TRELLIS_ROOT}:${process.env.PYTHONPATH}`
          : TRELLIS_ROOT,
      };
      const raw = execFileSync('python3', [
        '-m', 'trellis.codex_timing', 'cost-rollup',
        '--ledger', ledgerPath,
      ], {
        cwd: TRELLIS_ROOT, env, encoding: 'utf-8',
        timeout: 30000, maxBuffer: 16 * 1024 * 1024,
      });
      return JSON.parse(raw);
    } catch (e) {
      return null;
    }
  })();
  const modelByProv = {};
  const modelByProvCat = {};
  if (codexCostRollup) {
    for (const r of codexCostRollup.by_provider || []) {
      modelByProv[r.provider] = r;
    }
    for (const r of codexCostRollup.by_provider_category || []) {
      modelByProvCat[`${r.provider}::${r.category}`] = r;
    }
  }

  function applyCostFields(rec, model5h, fallbackWeekly) {
    // All `total_*` fields are RUN-CUMULATIVE: a sum of per-burst
    // contributions since the start of the cost ledger. They are NOT
    // rates, despite the legacy labels (`monthly_burn_pct`,
    // `monthly_burn_usd`) suggesting otherwise. Multiply through by the
    // unit conversions the original `monthly_burn_*` chain used:
    //
    //     total_5h_pct ─×fhToW─→ total_weekly_pct ─×wToM─→ total_monthly_pct
    //                          (1/5 for codex)         (1/4 for codex/claude)
    //     total_monthly_pct ─×subscription_usd/100─→ total_usd
    //
    // The fhToW and wToM factors are calibration constants; they don't
    // turn the cumulative sum into a rate. A run that takes 30 days
    // would have total_monthly_pct ~= 100 if its monthly burn rate
    // matched its monthly subscription. For shorter runs
    // total_monthly_pct < 100 just because the run hasn't been long
    // enough to consume a month's quota; that's not a "low burn rate".
    //
    // To compute an actual rate, divide total_usd by the run duration.
    const prov = rec.provider;
    const sub = MONTHLY_SUBSCRIPTION_USD[prov] || null;
    const wToM = PROVIDER_WEEKLY_TO_MONTHLY[prov] || 1/4;
    const fhToW = PROVIDER_5H_TO_WEEKLY[prov] || 1/7;
    if (model5h != null) {
      const burn5h = Number(model5h.model_burn_5h_pct);
      const wpct = burn5h * fhToW;
      const mpct = wpct * wToM;
      const usd = sub != null ? Number((mpct * sub / 100).toFixed(3)) : null;
      return {
        total_5h_pct: Number(burn5h.toFixed(3)),
        total_weekly_pct: Number(wpct.toFixed(3)),
        total_monthly_pct: Number(mpct.toFixed(3)),
        total_usd: usd,
        // Legacy aliases — same values as the total_* fields above; kept so
        // any existing consumer (e.g. an external dashboard polling
        // /api/usage.json) that hard-coded the old names doesn't show
        // blank cells. New consumers should prefer the total_* names which
        // accurately convey that these are run-cumulative sums, not rates.
        weekly_pct: Number(wpct.toFixed(3)),
        monthly_burn_pct: Number(mpct.toFixed(3)),
        monthly_burn_usd: usd,
        subscription_usd: sub,
        attribution_basis: 'beta_model',
        rollout_coverage: model5h.n != null
          ? `${model5h.with_rollout}/${model5h.n}`
          : null,
      };
    }
    // Fallback (non-codex or codex with no rollouts at all).
    if (fallbackWeekly != null) {
      const wpct = fallbackWeekly;
      const mpct = wpct * wToM;
      const usd = weeklyPctToUsd(wpct, prov);
      return {
        total_5h_pct: null,
        total_weekly_pct: Number(wpct.toFixed(3)),
        total_monthly_pct: Number(mpct.toFixed(3)),
        total_usd: usd,
        // Legacy aliases (same values).
        weekly_pct: Number(wpct.toFixed(3)),
        monthly_burn_pct: Number(mpct.toFixed(3)),
        monthly_burn_usd: usd,
        subscription_usd: sub,
        attribution_basis: 'per_burst_quota_delta',
        rollout_coverage: null,
      };
    }
    return {
      total_5h_pct: null, total_weekly_pct: null,
      total_monthly_pct: null, total_usd: null,
      // Legacy aliases.
      weekly_pct: null, monthly_burn_pct: null, monthly_burn_usd: null,
      subscription_usd: sub, attribution_basis: 'unavailable',
      rollout_coverage: null,
    };
  }

  for (const r of byProviderArr) {
    const fields = applyCostFields(
      r,
      r.provider === 'codex' ? modelByProv[r.provider] : null,
      burnPctByProv[r.provider],
    );
    Object.assign(r, fields);
  }
  for (const r of byProviderCategoryArr) {
    const k = `${r.provider}::${r.category}`;
    const fields = applyCostFields(
      r,
      r.provider === 'codex' ? modelByProvCat[k] : null,
      burnPctByProvCat[k],
    );
    Object.assign(r, fields);
    const provTokens = providerTotalTokens[r.provider] || 0;
    if (provTokens > 0) {
      r.token_share = Number((totalTokens(r) / provTokens).toFixed(4));
    }
  }

  // Merge stage wall-clock + check-subcommand totals into one
  // wall-clock breakdown table. Stages come from event_log ts_ms
  // deltas (Worker/Reviewer/...); check rows come from
  // .trellis/logs/check-ledger.jsonl (lake compile, sync, etc.). Both
  // are real supervisor wall-clock; presenting them in one table makes
  // the picture coherent. Drop the per-call "average" — only totals
  // matter when comparing time spent across categories.
  const wallClockArr = [];
  for (const [name, v] of Object.entries(byStage)) {
    wallClockArr.push({ kind: 'stage', name, duration_s: v.duration_s, intervals: v.intervals });
  }
  for (const [name, v] of Object.entries(byCheckSub)) {
    wallClockArr.push({ kind: 'check', name, duration_s: v.duration_s, count: v.count });
  }
  for (const [name, v] of Object.entries(byGitSub)) {
    wallClockArr.push({ kind: 'git', name, duration_s: v.duration_s, count: v.count });
  }
  wallClockArr.sort((a, b) => b.duration_s - a.duration_s);

  // Codex wall-clock decomposition (tool_exec / file_change / llm) computed
  // ON DEMAND from the codex rollout files under the burst user's
  // ~/.codex/sessions/. Spawn the python module once per
  // page load and pass it the same ledger this rollup is reading.
  // Best-effort: any failure (timeout, missing python module, etc.) just
  // omits the table.
  let codexTimingByProvider = [];
  try {
    const ledgerPath = path.join(repo, '.trellis', 'logs', 'cost-ledger.jsonl');
    const env = {
      ...process.env,
      PYTHONPATH: process.env.PYTHONPATH
        ? `${TRELLIS_ROOT}:${process.env.PYTHONPATH}`
        : TRELLIS_ROOT,
    };
    const raw = execFileSync('python3', [
      '-m', 'trellis.codex_timing', 'aggregate',
      '--ledger', ledgerPath, '--by', 'provider', '--codex-only',
    ], {
      cwd: TRELLIS_ROOT,
      env,
      encoding: 'utf-8',
      timeout: 15000,
      maxBuffer: 16 * 1024 * 1024,
    });
    const parsed = JSON.parse(raw);
    if (Array.isArray(parsed)) codexTimingByProvider = parsed;
  } catch (e) {
    codexTimingByProvider = [];
  }

  return {
    runtime_root: runtimeRoot || null,
    counts: { cost_rows: cost.length, check_rows: check.length, quota_rows: quota.length, event_rows: events.length },
    by_provider: byProviderArr,
    by_provider_category: byProviderCategoryArr,
    wall_clock: wallClockArr,
    codex_timing_by_provider: codexTimingByProvider,
    quota: latestQuota,
  };
}

// In-process TTL cache for the usage rollup. buildUsageRollup spawns two
// Python subprocesses (aggregate + cost-rollup) that each walk every
// codex rollout file — single-call work is ~2-4s of subprocess
// time on top of ~10s of synchronous JSON parsing. The cache makes every
// page load after the first cheap; the run state evolves on a ~minute
// scale so a 30s TTL is plenty fresh.
const USAGE_ROLLUP_TTL_MS = 30 * 1000;
const _usageRollupCache = new Map();  // key: project slug → { ts, value }

function buildUsageRollupCached(projectInfo) {
  const key = projectInfo.slug || projectInfo.repoPath;
  const entry = _usageRollupCache.get(key);
  const now = Date.now();
  if (entry && (now - entry.ts) < USAGE_ROLLUP_TTL_MS) {
    return entry.value;
  }
  const value = buildUsageRollup(projectInfo);
  _usageRollupCache.set(key, { ts: now, value });
  return value;
}

app.get(`${BASE}/api/usage.json`, (req, res) => {
  try {
    const projectInfo = resolveRepoPath(defaultProjectSlug());
    res.json(buildUsageRollupCached(projectInfo));
  } catch (e) { res.status(500).json({ error: e.message }); }
});
app.get(`${BASE}/:project/api/usage.json`, (req, res) => {
  try {
    const projectInfo = resolveRepoPath(projectFromRequest(req));
    res.json(buildUsageRollupCached(projectInfo));
  } catch (e) { res.status(500).json({ error: e.message }); }
});

// =====================================================================
// /api/kernel-pane — capture-pane the supervisor's tmux session.
// Falls back to tailing the most recent run-resume-*.log under the
// runtime root if no tmux session exists yet (transitional case).
// =====================================================================

function findSupervisorTmuxSession(projectInfo) {
  // Convention: trellis-run-<project_slug>. The slug is derived from the
  // project entry; fall back to scanning live tmux sessions for any
  // trellis-run-* and picking the one whose name matches the project.
  let lsOut = '';
  try {
    lsOut = execFileSync('tmux', tmuxArgs('ls', '-F', '#{session_name}'), {
      encoding: 'utf8', stdio: ['ignore', 'pipe', 'ignore'],
    });
  } catch { return null; }
  const candidates = lsOut.split('\n').map((s) => s.trim())
    .filter((s) => s.startsWith('trellis-run-'));
  if (candidates.length === 0) return null;
  // If only one, use it. If multiple, prefer one whose suffix matches
  // the project slug or the repoPath basename.
  const slug = projectInfo.slug || path.basename(projectInfo.repoPath);
  const exact = candidates.find((n) => n === `trellis-run-${slug}`);
  if (exact) return exact;
  const partial = candidates.find((n) => n.includes(slug));
  return partial || candidates[0];
}

function findLatestRunResumeLog(runtimeRoot) {
  if (!runtimeRoot) return null;
  const logsDir = path.join(runtimeRoot, 'logs');
  if (!fs.existsSync(logsDir)) return null;
  let best = null;
  for (const name of fs.readdirSync(logsDir)) {
    if (!/^run-resume-\d+\.log$/.test(name)) continue;
    const p = path.join(logsDir, name);
    try {
      const mt = fs.statSync(p).mtimeMs;
      if (!best || mt > best.mtime) best = { path: p, mtime: mt };
    } catch {}
  }
  return best ? best.path : null;
}

function readKernelPane(projectInfo, opts) {
  const tail = (opts && opts.tail) || 200;
  const out = { events: [], pane: { source: 'none', text: '' } };

  // Structured event-log tail. This is the supervisor's actual activity
  // signal: cycle starts, wrapper responses, commit_checkpoints, etc.
  // Lives in event_log.jsonl regardless of how stdout is wired.
  const runtimeRoot = runtimeRootForProject(projectInfo);
  if (runtimeRoot) {
    const evPath = path.join(runtimeRoot, 'event_log.jsonl');
    if (fs.existsSync(evPath)) {
      try {
        // Read only the tail to keep this cheap when the log is huge.
        const stat = fs.statSync(evPath);
        const readBytes = Math.min(stat.size, 256 * 1024);
        const fd = fs.openSync(evPath, 'r');
        const buf = Buffer.alloc(readBytes);
        fs.readSync(fd, buf, 0, readBytes, Math.max(0, stat.size - readBytes));
        fs.closeSync(fd);
        const text = buf.toString('utf8');
        const lines = text.split('\n').filter(Boolean);
        const recent = lines.slice(-tail);
        for (const line of recent) {
          try {
            const r = JSON.parse(line);
            const ev = r.event || {};
            const kind = String(ev.event || '?');
            const summary = summarizeEvent(r, ev);
            const cmds = (r.commands || []).map((c) => (c && c.command) || '?');
            out.events.push({
              index: r.index, kind, summary, commands: cmds,
            });
          } catch {}
        }
      } catch (e) {
        out.event_error = String(e.message || e);
      }
    }
  }

  // Live tmux pane. Blank in normal operation — catches errors / stack
  // traces that the kernel writes to stdout/stderr when something
  // unexpected happens.
  const session = findSupervisorTmuxSession(projectInfo);
  if (session) {
    try {
      const text = execFileSync('tmux',
        tmuxArgs('capture-pane', '-t', session, '-p', '-S', `-${tail * 4}`),
        { encoding: 'utf8', stdio: ['ignore', 'pipe', 'ignore'], maxBuffer: 4 * 1024 * 1024 });
      out.pane = { source: 'tmux', session, text: (text || '').trimEnd() };
    } catch (e) {
      out.pane = { source: 'tmux', session, text: '', error: String(e.message || e) };
    }
  } else {
    const logPath = findLatestRunResumeLog(runtimeRoot);
    if (logPath) {
      try {
        const buf = fs.readFileSync(logPath, 'utf8');
        const lines = buf.split('\n');
        const slice = lines.slice(Math.max(0, lines.length - tail)).join('\n');
        out.pane = { source: 'logfile', logfile: logPath, text: slice };
      } catch (e) {
        out.pane = { source: 'logfile', logfile: logPath, text: '', error: String(e.message || e) };
      }
    } else {
      out.pane = {
        source: 'none', text: '',
        note: 'no trellis-run-* tmux session and no run-resume-*.log',
      };
    }
  }
  return out;
}

// Compact one-line summary of a kernel event for the activity tail.
function summarizeEvent(record, ev) {
  const kind = String(ev.event || '?');
  if (kind === 'start_cycle') {
    const req = (record.commands || [])
      .map((c) => (c && c.request) || null).find(Boolean);
    if (req) {
      return `cycle=${req.cycle} ${req.kind || '?'}#${req.id || '?'} active=${req.active_node || '—'} mode=${req.mode || '—'}`;
    }
    return '';
  }
  if (kind === 'wrapper_response') {
    const resp = ev.response || {};
    if (!resp || typeof resp !== 'object') return '';
    // Response is a flat dict with `kind` discriminating the variant.
    const parts = [];
    if (resp.kind) parts.push(`kind=${resp.kind}`);
    if (resp.request_id != null) parts.push(`req=${resp.request_id}`);
    if (resp.cycle != null) parts.push(`cycle=${resp.cycle}`);
    if (resp.status && resp.status !== 'Ok') parts.push(`status=${resp.status}`);
    if (resp.outcome) parts.push(`outcome=${resp.outcome}`);
    if (resp.decision) {
      const dec = String(resp.decision);
      const next = resp.next_active ? ` next=${resp.next_active}` : '';
      const reset = resp.reset && resp.reset !== 'None' ? ` reset=${resp.reset}` : '';
      parts.push(`decision=${dec}${next}${reset}`);
    }
    return parts.join(' ');
  }
  return '';
}

app.get(`${BASE}/api/kernel-pane.json`, (req, res) => {
  try {
    const projectInfo = resolveRepoPath(defaultProjectSlug());
    res.json(readKernelPane(projectInfo, { tail: parseInt(req.query.tail || 200, 10) || 200 }));
  } catch (e) { res.status(500).json({ error: e.message }); }
});
app.get(`${BASE}/:project/api/kernel-pane.json`, (req, res) => {
  try {
    const projectInfo = resolveRepoPath(projectFromRequest(req));
    res.json(readKernelPane(projectInfo, { tail: parseInt(req.query.tail || 200, 10) || 200 }));
  } catch (e) { res.status(500).json({ error: e.message }); }
});

// =====================================================================
// /api/progress — time series of {total, corr_passing, sound_or_waived,
// sound_unknown, lean_closed, lean_closed_transitive} for all nodes across all
// supervisor2 checkpoint commits in the live repo.
// "lean_closed" is the kernel's shallow committed closed predicate. Cached
// per-sha (the data at a given sha never changes).
// =====================================================================

const PROGRESS_CACHE = new Map();  // sha -> {ts, data}
const PROGRESS_CACHE_MAX = 1024;
const PROGRESS_DISK_CACHE_VERSION = 8;
const PROGRESS_DISK_CACHE_LOADED = new Set();
const PROGRESS_SERIES_CACHE = new Map();  // repoPath -> {headSha, ts, data}
const PROGRESS_WORKERS = new Map();  // repoPath -> {headSha, child}

function progressCacheKey(repoPath, sha) {
  return `${repoPath}\0${sha}`;
}

function trimDisabledProgressBuckets(data) {
  if (!data || typeof data !== 'object') return data;
  delete data.coarse;
  delete data.coarse_proofs_only;
  delete data.coarse_fallback;
  delete data.lean_proof_words;
  for (const bucketName of ['all', 'all_proofs_only']) {
    if (data[bucketName] && typeof data[bucketName] === 'object') {
      delete data[bucketName].lean_proof_words;
    }
  }
  if (Array.isArray(data.checkpoints)) {
    for (const checkpoint of data.checkpoints) trimDisabledProgressBuckets(checkpoint);
  }
  return data;
}

function progressDiskCachePath(projectInfo) {
  return path.join(viewerApiDir(projectInfo), `progress-cache-v${PROGRESS_DISK_CACHE_VERSION}.json`);
}

function progressSeriesDiskCachePath(projectInfo) {
  return path.join(viewerApiDir(projectInfo), `progress-series-cache-v${PROGRESS_DISK_CACHE_VERSION}.json`);
}

function isProofNodeKind(kind) {
  const k = String(kind || '').toLowerCase();
  return k !== 'definition' && k !== 'preamble';
}

// Find the FIRST byte of the `-- BODY` marker line, or -1 if no such
// line exists / there's more than one. The marker line is the FILESPEC
// v2 statement/proof boundary; mirrors the kernel's
// `filespec_split::find_marker_line` in viewer-display semantics.
function findBodyMarkerStart(text) {
  if (!text) return -1;
  let offset = 0;
  let first = -1;
  for (const line of text.split('\n')) {
    if (line.trim() === '-- BODY') {
      if (first !== -1) return -1;  // multiple markers → ambiguous
      first = offset;
    }
    offset += line.length + 1;  // +1 for the consumed '\n'
  }
  return first;
}

function extractLeanProofTextForMetrics(leanText) {
  const text = String(leanText || '');
  const markerStart = findBodyMarkerStart(text);
  if (markerStart >= 0) {
    // Body = everything after the marker line (skip past its newline).
    const eol = text.indexOf('\n', markerStart);
    return text.slice(eol < 0 ? markerStart : eol + 1).trim();
  }
  // Pre-migration fallback: strip imports/comments, then split at the
  // last `:=`. Wrong on let-in-type signatures and (k := k) named args,
  // but no longer load-bearing for current HEAD content — only
  // relevant for historical commits shown in the viewer's git-history
  // pane.
  const stripped = text.replace(/^(?:\s*--[^\n]*\n|\s*import[^\n]*\n)+/g, '');
  const idx = stripped.lastIndexOf(':=');
  if (idx < 0) return '';
  return stripped.slice(idx + 2).trim();
}

function extractTexProofTextForMetrics(texText) {
  const m = String(texText || '').match(/\\begin\{proof\}[\s\S]*?\\end\{proof\}/);
  return m ? m[0] : '';
}

function texNaturalLanguageWordCount(texText) {
  const prose = String(texText || '')
    .replace(/\$\$[\s\S]*?\$\$/g, ' ')
    .replace(/\\\[[\s\S]*?\\\]/g, ' ')
    .replace(/\\\([\s\S]*?\\\)/g, ' ')
    .replace(/\$[^$\n]*\$/g, ' ')
    .replace(/%[^\n]*/g, ' ')
    .replace(/\\(?:begin|end)\{[^}]*\}/g, ' ')
    .replace(/\\[A-Za-z]+\*?(?:\[[^\]]*\])?(?:\{[^{}]*\})?/g, ' ')
    .replace(/[{}\\^_&~#]/g, ' ');
  const words = prose.match(/[A-Za-z]+(?:[-'][A-Za-z]+)*/g);
  return words ? words.length : 0;
}

function blockerKindKey(kind) {
  const k = String(kind || '').toLowerCase();
  if (k === 'paperfaithfulness') return 'paper';
  if (k === 'nodecorr') return 'corr';
  if (k === 'substantiveness') return 'subst';
  if (k === 'soundness') return 'sound';
  return null;
}

function taskBlockerMetrics(state) {
  const metrics = { total: 0, paper: 0, corr: 0, subst: 0, sound: 0 };
  const task = state && state.pending_task;
  const blockers = task && Array.isArray(task.task_blockers) ? task.task_blockers : [];
  for (const blocker of blockers) {
    metrics.total++;
    const key = blockerKindKey(blocker && blocker.kind);
    if (key && Object.prototype.hasOwnProperty.call(metrics, key)) metrics[key]++;
  }
  return metrics;
}

function readSupervisorStateForCommit(repoPath, sha) {
  let stateRaw;
  try {
    stateRaw = execSync(`git -C ${JSON.stringify(repoPath)} show ${sha}:.trellis-history/supervisor_state.json`,
      { encoding: 'utf8', stdio: ['ignore', 'pipe', 'ignore'], maxBuffer: 32 * 1024 * 1024 });
  } catch {
    return null;
  }
  try {
    const parsed = JSON.parse(stateRaw);
    return parsed.state || {};
  } catch {
    return null;
  }
}

function ensureTaskBlockerMetrics(repoPath, sha, data) {
  if (!data || data.task_blockers !== undefined) return false;
  const state = readSupervisorStateForCommit(repoPath, sha);
  data.task_blockers = taskBlockerMetrics(state || {});
  return true;
}

function progressSeriesHasTaskBlockers(data) {
  return !!(data && Array.isArray(data.checkpoints)
    && data.checkpoints.every((c) => c && c.task_blockers !== undefined));
}

function repoHeadSha(repoPath) {
  try {
    return execFileSync('git', ['-C', repoPath, 'rev-parse', 'HEAD'], {
      encoding: 'utf8',
      stdio: ['ignore', 'pipe', 'ignore'],
    }).trim();
  } catch {
    return '';
  }
}

function loadProgressDiskCache(projectInfo) {
  const repoPath = projectInfo.repoPath;
  if (PROGRESS_DISK_CACHE_LOADED.has(repoPath)) return;
  PROGRESS_DISK_CACHE_LOADED.add(repoPath);
  const p = progressDiskCachePath(projectInfo);
  if (!fs.existsSync(p)) return;
  let parsed;
  try {
    parsed = JSON.parse(fs.readFileSync(p, 'utf8'));
  } catch {
    return;
  }
  if (!parsed || parsed.version !== PROGRESS_DISK_CACHE_VERSION || !parsed.entries) return;
  for (const [sha, data] of Object.entries(parsed.entries)) {
    if (!sha || !data) continue;
    PROGRESS_CACHE.set(progressCacheKey(repoPath, sha), { ts: Date.now(), data });
  }
}

function saveProgressDiskCache(projectInfo) {
  const repoPath = projectInfo.repoPath;
  const prefix = `${repoPath}\0`;
  const entries = {};
  for (const [key, value] of PROGRESS_CACHE.entries()) {
    if (!key.startsWith(prefix)) continue;
    entries[key.slice(prefix.length)] = value.data;
  }
  const p = progressDiskCachePath(projectInfo);
  fs.mkdirSync(path.dirname(p), { recursive: true });
  const tmp = `${p}.tmp`;
  fs.writeFileSync(tmp, JSON.stringify({
    version: PROGRESS_DISK_CACHE_VERSION,
    generated_at: new Date().toISOString(),
    entries,
  }));
  fs.renameSync(tmp, p);
}

function readProgressSeriesDiskCache(projectInfo, headSha) {
  const p = progressSeriesDiskCachePath(projectInfo);
  if (!headSha || !fs.existsSync(p)) return null;
  let parsed;
  try {
    parsed = JSON.parse(fs.readFileSync(p, 'utf8'));
  } catch {
    return null;
  }
  if (!parsed || parsed.version !== PROGRESS_DISK_CACHE_VERSION || parsed.headSha !== headSha) return null;
  if (!parsed.data || !Array.isArray(parsed.data.checkpoints)) return null;
  if (!progressSeriesHasTaskBlockers(parsed.data)) return null;
  return trimDisabledProgressBuckets(parsed.data);
}

function saveProgressSeriesDiskCache(projectInfo, headSha, data) {
  if (!headSha || !data) return;
  const p = progressSeriesDiskCachePath(projectInfo);
  fs.mkdirSync(path.dirname(p), { recursive: true });
  const tmp = `${p}.tmp`;
  fs.writeFileSync(tmp, JSON.stringify({
    version: PROGRESS_DISK_CACHE_VERSION,
    headSha,
    generated_at: new Date().toISOString(),
    data,
  }));
  fs.renameSync(tmp, p);
}

function progressForCommit(repoPath, sha) {
  const cacheKey = progressCacheKey(repoPath, sha);
  if (PROGRESS_CACHE.has(cacheKey)) {
    const data = trimDisabledProgressBuckets(PROGRESS_CACHE.get(cacheKey).data);
    ensureTaskBlockerMetrics(repoPath, sha, data);
    return data;
  }
  const state = readSupervisorStateForCommit(repoPath, sha);
  if (!state) return null;
  const nodeKinds = state.node_kinds || {};
  const presentNodes = Object.keys(nodeKinds);
  // The historical snapshot writes `deps` (and `committed_deps`); the live
  // protocol_state.json calls the same field `current_deps`. Accept any.
  const deps = state.deps || state.committed_deps || state.current_deps || {};
  const corrStatus = state.corr_status || {};
  const soundStatus = state.sound_status || {};
  // Fingerprint maps so we can mirror the kernel's `current_*_state` logic:
  // a Fail/Structural status only counts as currently failing when the
  // approved fingerprint matches the current one. If they drift the kernel
  // treats it as Unknown (re-needs verification), and so does this chart.
  const corrApprovedFp = state.corr_approved_fingerprints || {};
  const corrCurrentFp = (state.live && state.live.corr_current_fingerprints) || {};
  const soundApprovedFp = state.sound_approved_fingerprints || {};
  const soundCurrentFp = (state.live && state.live.sound_current_fingerprints) || {};
  // Substantiveness lane was added in kernel commit 7198d6c (April 2026).
  // Older checkpoints predate it and omit these fields. Detect presence
  // at the lane level (any of the three field families present) so the
  // metrics emit `null` for pre-lane checkpoints — the chart's path()
  // helper breaks the line at nulls, rendering as a gap rather than a
  // misleading zero.
  const substantivenessStatusRaw = state.substantiveness_status;
  const substantivenessPresent = substantivenessStatusRaw !== undefined
    || state.substantiveness_approved_fingerprints !== undefined
    || (state.live && state.live.substantiveness_current_fingerprints !== undefined);
  const substantivenessStatus = substantivenessStatusRaw || {};
  const substantivenessApprovedFp = state.substantiveness_approved_fingerprints || {};
  const substantivenessCurrentFp =
    (state.live && state.live.substantiveness_current_fingerprints) || {};

  function currentlyPassing(status, approvedFp, currentFp) {
    // Mirror the kernel's `current_*_state` predicate exactly (model.rs
    // ~line 1648). A node passes iff status=Pass AND both fingerprint
    // entries are PRESENT in the map AND equal. Use a strict
    // `=== undefined` check (not `!fp`) so an empty-string fingerprint
    // counts as present: Preamble has no .tex content to fingerprint, so
    // the kernel records both `corr_approved_fingerprints["Preamble"]` and
    // `live.corr_current_fingerprints["Preamble"]` as `Some("")`, which
    // satisfies the kernel's `Some(current) == Some(approved)` check.
    // The previous `!approvedFp` truthiness check incorrectly treated the
    // empty-string fingerprint as missing and excluded Preamble forever,
    // showing a permanent +1 gap between `total` and `corr_passing`.
    if (status !== 'Pass') return false;
    if (approvedFp === undefined || currentFp === undefined) return false;
    return approvedFp === currentFp;
  }

  // Mirror of currentlyPassing for the rejection side: Fail and Structural
  // both mean "kernel currently considers this node not sound" once the
  // fingerprints agree. Used for the soundness-band's upper edge: the band
  // covers every node that's NOT currently rejected on this axis.
  function currentlyFailing(status, approvedFp, currentFp) {
    if (status !== 'Fail' && status !== 'Structural') return false;
    if (approvedFp === undefined || currentFp === undefined) return false;
    return approvedFp === currentFp;
  }

  // Kernel-tracked closed-ness — what the DAG view colors blue/green.
  // A node is "closed" iff it's in committed.present_nodes AND NOT in
  // committed.open_nodes. This is the kernel's authoritative view: it
  // reflects the latest deterministic-checker outcome AND the kernel's
  // structural classification, not just .lean text grep. Use this for
  // chart progress so the chart matches what the DAG view shows the user.
  //
  // The hasSorry map below stays — it's still needed for the
  // sound_or_waived calculation (sound waives when own .lean is sorry-free,
  // independent of the kernel's lifecycle on the node).
  const committed = state.committed || {};
  const committedPresent = new Set(committed.present_nodes || []);
  const committedOpen = new Set(committed.open_nodes || []);
  // Patch C local-closure unverified set (per LOCAL_CLOSURE_IMPL_PLAN.md §9).
  // A sorry-free node not in `open_nodes` may still be `local_closure_unverified`:
  // the chart counts these in a parallel `lean_unverified` series so the operator
  // can see the gap between "kernel-closed but not yet locally verified" and
  // "fully verified." When the new fields are absent (pre-Patch-C / pre-migration
  // checkpoints) the set is empty and the series renders as zero.
  //
  // Tier alignment: this chart is committed-DAG-based (uses `committed.open_nodes`
  // above), so we prefer the committed mirror over the live-tier field. Fall back
  // to the live field for backward compat with pre-Patch-C-A snapshots.
  const localClosureUnverified = new Set(
    Array.isArray(state.committed_local_closure_unverified_nodes)
      ? state.committed_local_closure_unverified_nodes
      : (Array.isArray(state.local_closure_unverified_nodes)
          ? state.local_closure_unverified_nodes // fallback
          : [])
  );
  function isKernelClosed(n) {
    return committedPresent.has(n) && !committedOpen.has(n);
  }

  // Fetch .lean content for each node at this commit. Build hasSorry map.
  // Truthful "compiled-proof uses sorryAx" would require running lean per
  // node; this is the cheap text-only approximation: strip comments, then
  // look for literal `sorry` — but treat the file as sorry-free if it
  // contains a `local macro_rules` rule that rewrites the `sorry` tactic
  // to a real proof (the literal token is in the source but the compiled
  // term contains no sorryAx). Mirrors trellis/viewer_adapter.py.
  const macroRulesSorryRe = /macro_rules\s*\|\s*`\(\s*(?:tactic|term)\|\s*sorry\s*\)\s*=>/;
  // Mirrors `_proof_starts_with_sketch_marker` in
  // trellis/agent_wrapper/executor.py: first non-blank line of the
  // \begin{proof}…\end{proof} block is exactly "SKETCH:". Such nodes would
  // be auto-failed by the supervisor's `_maybe_synthesize_sketch_soundness_artifact`
  // if Sound-dispatched, even without a real verifier round-trip.
  const sketchProofRe = /\\begin\{proof\}([\s\S]*?)\\end\{proof\}/;
  function texProofStartsWithSketch(texContent) {
    const m = sketchProofRe.exec(texContent || '');
    if (!m) return false;
    for (const line of m[1].split('\n')) {
      const s = line.trim();
      if (!s) continue;
      return s === 'SKETCH:';
    }
    return false;
  }
  const hasSorry = {};
  const hasSketch = {};
  const leanProofMetrics = {};
  const nlProofWordCounts = {};
  for (const node of presentNodes) {
    const proofBearing = isProofNodeKind(nodeKinds[node]);
    if (node === 'Preamble') { hasSorry[node] = false; continue; }
    let leanText = '';
    try {
      leanText = execSync(`git -C ${JSON.stringify(repoPath)} show ${sha}:Tablet/${node}.lean`,
        { encoding: 'utf8', stdio: ['ignore', 'pipe', 'ignore'], maxBuffer: 4 * 1024 * 1024 });
    } catch {
      hasSorry[node] = false;
      continue;
    }
    if (proofBearing) {
      const proofText = extractLeanProofTextForMetrics(leanText);
      leanProofMetrics[node] = {
        chars: proofText.length,
      };
      let texText = '';
      try {
        texText = execSync(`git -C ${JSON.stringify(repoPath)} show ${sha}:Tablet/${node}.tex`,
          { encoding: 'utf8', stdio: ['ignore', 'pipe', 'ignore'], maxBuffer: 4 * 1024 * 1024 });
      } catch {
        texText = '';
      }
      nlProofWordCounts[node] = texNaturalLanguageWordCount(extractTexProofTextForMetrics(texText));
      if (texProofStartsWithSketch(texText)) hasSketch[node] = true;
    }
    const cleaned = leanText
      .replace(/\/-[\s\S]*?-\//g, '')
      .replace(/--[^\n]*/g, '');
    if (!/\bsorry\b/.test(cleaned)) { hasSorry[node] = false; continue; }
    if (macroRulesSorryRe.test(cleaned)) { hasSorry[node] = false; continue; }
    hasSorry[node] = true;
  }

  // Recursively-closed memo: a node is "transitively kernel-closed" iff
  // IT is kernel-closed AND every direct dep is recursively kernel-closed.
  // Mirrors viewer/public/index.html `isRecursivelyClosed`. Cycle-safe.
  const transClosedMemo = {};
  function isRecursivelyKernelClosed(n, stack) {
    if (n in transClosedMemo) return transClosedMemo[n];
    if (stack.has(n)) return true;  // cycle guard
    if (!isKernelClosed(n)) { transClosedMemo[n] = false; return false; }
    stack.add(n);
    const childList = Array.isArray(deps[n]) ? deps[n] : [];
    for (const c of childList) {
      if (!isRecursivelyKernelClosed(c, stack)) {
        stack.delete(n);
        transClosedMemo[n] = false;
        return false;
      }
    }
    stack.delete(n);
    transClosedMemo[n] = true;
    return true;
  }

  // Coarse-DAG shallow closure: a node x is "coarse-shallowly closed" iff
  // IT is kernel-closed AND every non-coarse-DAG descendant of x (descending
  // through `deps`) is closed. Recursion stops AT another coarse-DAG node —
  // that node is taken as an opaque leaf, its closure is reported as its own
  // coarse-DAG entry. So a coarse-DAG island can be "shallowly complete" even
  // if its sibling coarse-DAG dependencies are still open, telling us "this
  // cluster of work is structurally finished."
  //
  // Cycle-safe via the `stack` set; memoized at every node (including
  // non-coarse helpers) so a helper subtree that's reused under multiple
  // coarse parents is computed once.
  const coarseSet = new Set(state.coarse_dag_nodes || []);
  const coarseShallowMemo = {};
  function isCoarseShallowlyClosed(n, stack) {
    if (n in coarseShallowMemo) return coarseShallowMemo[n];
    if (stack && stack.has(n)) return true;  // cycle guard
    if (!isKernelClosed(n)) { coarseShallowMemo[n] = false; return false; }
    const childStack = stack || new Set();
    childStack.add(n);
    const childList = Array.isArray(deps[n]) ? deps[n] : [];
    for (const c of childList) {
      if (coarseSet.has(c)) continue;  // STOP — coarse-DAG opaque leaf
      if (!isCoarseShallowlyClosed(c, childStack)) {
        childStack.delete(n);
        coarseShallowMemo[n] = false;
        return false;
      }
    }
    childStack.delete(n);
    coarseShallowMemo[n] = true;
    return true;
  }

  function metricsFor(nodeSet, opts = {}) {
    let total = 0, corrPass = 0, substPass = 0, soundOrWaived = 0, soundUnknown = 0, closedShallow = 0, closedTrans = 0;
    let leanProofChars = 0, nlProofWords = 0, nlProofUnclosedWords = 0;
    let leanUnverified = 0;
    const shallowClosed = opts.shallowClosed || isKernelClosed;
    for (const n of presentNodes) {
      if (!nodeSet.has(n)) continue;
      total++;
      const leanProofMetric = leanProofMetrics[n];
      if (leanProofMetric) {
        leanProofChars += leanProofMetric.chars || 0;
      }
      nlProofWords += nlProofWordCounts[n] || 0;
      if (!shallowClosed(n)) {
        nlProofUnclosedWords += nlProofWordCounts[n] || 0;
      }
      // Use kernel-strict "actually verified" semantics: status=Pass AND
      // current_fp == approved_fp. Missing entries, Unknown status, and
      // fingerprint drift all mean "not yet verified" and don't count.
      if (currentlyPassing(corrStatus[n], corrApprovedFp[n], corrCurrentFp[n])) corrPass++;
      // Substantiveness mirrors the kernel's `current_substantiveness_state`
      // predicate: Preamble is exempt (model.rs:2380 filters PREAMBLE_NAME
      // out of the verifier frontier and the state accessor returns Pass
      // unconditionally for it), so count it as auto-pass here. Other
      // nodes pass iff status=Pass AND fingerprints match.
      if (substantivenessPresent) {
        if (n === 'Preamble') {
          substPass++;
        } else if (currentlyPassing(
          substantivenessStatus[n],
          substantivenessApprovedFp[n],
          substantivenessCurrentFp[n],
        )) {
          substPass++;
        }
      }
      // For soundness we additionally waive when the node's lean is closed
      // (no real `sorry`): if the Lean proof is verified, the kernel's view
      // of the NL proof's soundness doesn't matter for our progress
      // tracking — the node is verified by the Lean side.
      const soundPass = currentlyPassing(soundStatus[n], soundApprovedFp[n], soundCurrentFp[n]);
      const soundFail = currentlyFailing(soundStatus[n], soundApprovedFp[n], soundCurrentFp[n]);
      const myFree = !hasSorry[n];
      if (soundPass || myFree) soundOrWaived++;
      else if (!soundFail) soundUnknown++;
      // Use kernel `committed.open_nodes` as the source of truth for
      // closed-ness — same as the DAG view. Previously this used a .lean
      // text grep for `sorry`, which silently disagreed with the kernel:
      // a node whose .lean had been edited sorry-free but the kernel
      // hadn't yet re-blessed (waiting on a deterministic check, or a
      // structural classification update) would be counted closed by the
      // chart but shown open by the DAG view. The kernel's view is what
      // determines whether downstream proofs can rely on it, so it's the
      // honest progress signal.
      if (shallowClosed(n)) closedShallow++;
      if (isRecursivelyKernelClosed(n, new Set())) closedTrans++;
      // Patch C unverified count: kernel-closed (not in open_nodes) but the
      // local-closure probe has not produced a fresh record. Counted on top of
      // the closed series so the operator sees both "shallowly closed by the
      // kernel" and "the subset of those that are still pending local-closure
      // verification."
      if (localClosureUnverified.has(n)) leanUnverified++;
    }
    return {
      total, corr_passing: corrPass, sound_or_waived: soundOrWaived,
      sound_unknown: soundUnknown,
      // null for pre-substantiveness-lane checkpoints — chart renders as a gap.
      substantiveness_passing: substantivenessPresent ? substPass : null,
      lean_closed: closedShallow,
      lean_closed_transitive: closedTrans,
      lean_unverified: leanUnverified,
      lean_proof_chars: leanProofChars,
      nl_proof_words: nlProofWords,
      nl_proof_unclosed_words: nlProofUnclosedWords,
    };
  }

  const allSet = new Set(presentNodes);
  // Proof-only filter: definition + preamble nodes are excluded so the
  // totals reflect just the proof-bearing work. The viewer renders both
  // views and toggles client-side.
  const proofOnlySet = new Set(presentNodes.filter((n) => {
    return isProofNodeKind(nodeKinds[n]);
  }));
  // List of nodes whose .tex proof block opens with the literal `SKETCH:`
  // marker — used by `attachSoundVerifierFailCounts` to widen the chart's
  // "definitive Fail" set (verifier-Fail ∪ SKETCH-marked) since SKETCH nodes
  // are auto-failed by the supervisor whenever they get Sound-dispatched.
  const sketchNodes = presentNodes.filter((n) => hasSketch[n]);
  const data = {
    sha,
    cycle: parseInt(state.cycle || 0, 10),
    phase: state.phase || '',
    sketch_nodes: sketchNodes,
    all: metricsFor(allSet),
    all_proofs_only: metricsFor(proofOnlySet),
    // Coarse-DAG shallow closure — see `isCoarseShallowlyClosed` above.
    // Empty bucket (`total: 0`) when the state predates the coarse_dag_nodes
    // field (theorem-stating phase, legacy runs); chart renders as a gap.
    //
    // Two parallel buckets so the chart can honor the "include definitions"
    // toggle the same way the Nodes / Proofs charts do:
    //   - `coarse_shallow`              — all coarse-DAG nodes (Definition + Proof + Preamble)
    //   - `coarse_shallow_proofs_only`  — only proof-bearing coarse-DAG nodes
    // Closure semantics are identical (the predicate walks the same deps);
    // only the *counted* set differs. metricsFor's iteration over
    // presentNodes does the filtering naturally.
    coarse_shallow: metricsFor(coarseSet, { shallowClosed: (n) => isCoarseShallowlyClosed(n) }),
    coarse_shallow_proofs_only: metricsFor(
      new Set([...coarseSet].filter((n) => isProofNodeKind(nodeKinds[n]))),
      { shallowClosed: (n) => isCoarseShallowlyClosed(n) },
    ),
    task_blockers: taskBlockerMetrics(state),
  };
  // LRU-ish bound
  if (PROGRESS_CACHE.size >= PROGRESS_CACHE_MAX) {
    const oldest = [...PROGRESS_CACHE.entries()].sort((a, b) => a[1].ts - b[1].ts)[0];
    if (oldest) PROGRESS_CACHE.delete(oldest[0]);
  }
  PROGRESS_CACHE.set(cacheKey, { ts: Date.now(), data });
  return data;
}

// Walk event_log.jsonl once and attach soundness-Fail counts to each
// checkpoint's metrics buckets. Two counts get attached per nodeset:
//   - `sound_verifier_fail`: nodes whose most recent Sound verifier verdict
//     was Fail/Structural AND that verdict's fingerprint matches the node's
//     current sound fingerprint at the checkpoint (no worker drift since).
//   - `sound_definitive_fail`: the union of verifier-Fail with SKETCH-marked
//     nodes (top-level `sketch_nodes` on the data bucket). A Sound dispatch
//     on a SKETCH-marked node is auto-failed by the supervisor's
//     `_maybe_synthesize_sketch_soundness_artifact` in agent_wrapper/executor.py,
//     so SKETCHes are "Fail-by-construction" even without a verifier
//     round-trip.
//
// Both counts are stable under reviewer task-adjudication and reset-blocker
// relabeling, unlike the kernel-derived `current_sound_state == Fail` count
// which oscillates as the reviewer alternates Fail↔Unknown labels each
// cycle without verifier evidence.
//
// Cost: one linear pass over event_log.jsonl per series-rebuild (~30MB at
// 459 events for the current designs run; sub-second). Cached implicitly via
// PROGRESS_SERIES_CACHE — same lifecycle as the rest of the series.
function attachSoundVerifierFailCounts(projectInfo, checkpoints) {
  if (!Array.isArray(checkpoints) || !checkpoints.length) return;
  const runtimeRoot = runtimeRootForProject(projectInfo);
  if (!runtimeRoot) return;
  const evPath = path.join(runtimeRoot, 'event_log.jsonl');
  if (!fs.existsSync(evPath)) return;
  const repo = projectInfo.repoPath;

  // For each checkpoint sha, read its supervisor_state.json once to grab
  // {event_count, sound_current_fingerprints, node_sets}. Index by event_count
  // for O(1) snapshot lookup during the event-log walk.
  const cpInfo = new Map(); // event_count -> { sha, currentFps, nodeSets }
  for (const cp of checkpoints) {
    let raw;
    try {
      raw = execSync(
        `git -C ${JSON.stringify(repo)} show ${cp.sha}:.trellis-history/supervisor_state.json`,
        { encoding: 'utf8', stdio: ['ignore', 'pipe', 'ignore'], maxBuffer: 32 * 1024 * 1024 },
      );
    } catch { continue; }
    let parsed;
    try { parsed = JSON.parse(raw); } catch { continue; }
    const evCount = parsed.event_count;
    if (typeof evCount !== 'number') continue;
    const state = parsed.state || {};
    const nodeKinds = state.node_kinds || {};
    const present = Object.keys(nodeKinds);
    const currentFps = (state.live && state.live.sound_current_fingerprints) || {};
    const coarseList = state.coarse_dag_nodes || [];
    cpInfo.set(evCount, {
      sha: cp.sha,
      currentFps,
      nodeSets: {
        all: new Set(present),
        all_proofs_only: new Set(present.filter((n) => isProofNodeKind(nodeKinds[n]))),
        coarse_shallow: new Set(coarseList),
        coarse_shallow_proofs_only: new Set(coarseList.filter((n) => isProofNodeKind(nodeKinds[n]))),
      },
    });
  }
  if (!cpInfo.size) return;

  // Build a sha → checkpoint object index for fast attach.
  const cpBySha = new Map();
  for (const cp of checkpoints) cpBySha.set(cp.sha, cp);

  // Maps maintained during the event-log walk.
  const pendingReq = new Map(); // request_id -> { node: fingerprint }
  const lastVerdict = new Map(); // node -> { status, fingerprint }

  // Stream-parse the event log line-by-line (file is up to ~tens of MB).
  const content = fs.readFileSync(evPath, 'utf8');
  let cursor = 0;
  while (cursor < content.length) {
    const nl = content.indexOf('\n', cursor);
    const line = nl < 0 ? content.slice(cursor) : content.slice(cursor, nl);
    cursor = nl < 0 ? content.length : nl + 1;
    if (!line) continue;
    let ev;
    try { ev = JSON.parse(line); } catch { continue; }

    // Cache per-node fingerprints from issued Sound requests, keyed by request id.
    const commands = ev.commands || [];
    for (const cmd of commands) {
      if (cmd && cmd.command === 'issue_request') {
        const req = cmd.request;
        if (req && req.kind === 'Sound') {
          const fps = {};
          for (const b of (req.blockers || [])) {
            if (b && b.kind === 'Soundness' && b.object && b.object.node && b.fingerprint) {
              fps[b.object.node] = b.fingerprint;
            }
          }
          pendingReq.set(req.id, fps);
        }
      }
    }

    // Apply Sound responses to per-node verdict map.
    const evt = ev.event || {};
    if (evt.event === 'wrapper_response') {
      const resp = evt.response;
      if (resp && resp.kind === 'sound') {
        const fps = pendingReq.get(resp.request_id) || {};
        const lanes = resp.lane_updates || {};
        for (const laneKey of Object.keys(lanes)) {
          const laneMap = lanes[laneKey] || {};
          for (const node of Object.keys(laneMap)) {
            const update = laneMap[node];
            if (update && typeof update === 'object' && Object.prototype.hasOwnProperty.call(update, 'Set')) {
              const fp = fps[node];
              if (fp !== undefined) {
                lastVerdict.set(node, { status: update.Set, fingerprint: fp });
              }
            }
            // Update::Same (string "Same") means no change to the verdict.
          }
        }
        pendingReq.delete(resp.request_id);
      }
    }

    // At each checkpoint's event_count, snapshot per-nodeset counts onto
    // the corresponding checkpoint object.
    const idx = ev.index;
    if (typeof idx === 'number' && cpInfo.has(idx)) {
      const info = cpInfo.get(idx);
      const cp = cpBySha.get(info.sha);
      if (cp) {
        const sketchSet = new Set(Array.isArray(cp.sketch_nodes) ? cp.sketch_nodes : []);
        for (const setKey of Object.keys(info.nodeSets)) {
          const nodes = info.nodeSets[setKey];
          let verifierFail = 0;
          let definitiveFail = 0;
          for (const node of nodes) {
            const v = lastVerdict.get(node);
            // The kernel treats `Structural` as Fail (model.rs:6163 includes
            // both in `current_sound_state == Fail`), so a Sound verdict of
            // `Structural` also counts here.
            const isVerifierFail = v
              && (v.status === 'Fail' || v.status === 'Structural')
              && v.fingerprint === info.currentFps[node];
            if (isVerifierFail) verifierFail++;
            if (isVerifierFail || sketchSet.has(node)) definitiveFail++;
          }
          if (cp[setKey] && typeof cp[setKey] === 'object') {
            cp[setKey].sound_verifier_fail = verifierFail;
            cp[setKey].sound_definitive_fail = definitiveFail;
          }
        }
      }
    }
  }
}

function computeProgressSeriesSync(projectInfo, headSha = repoHeadSha(projectInfo.repoPath)) {
  const repo = projectInfo.repoPath;
  loadProgressDiskCache(projectInfo);
  // Walk only HEAD-reachable checkpoint commits. This gives a consistent
  // single-timeline progress view: cycles are monotone in time, totals
  // and counts only move forward (never regress across cycles).
  //
  // After a rewind across cycle boundaries, pre-rewind checkpoints become
  // orphaned (still tagged, but unreachable from HEAD). Including them
  // would mix two timelines — e.g. pre-rewind c50 sitting between
  // post-rewind c49 and c51 — and create non-monotonic cycles + apparent
  // regressions in closed-counts. The chart trades historical visibility
  // for honest current-timeline coherence.
  //
  // The DAG view's `_build_historical_viewer_state` (in
  // trellis/viewer_adapter.py) walks git TAGS and picks the highest
  // event-count per cycle — that rule is fine for a single-cycle inspector
  // (it surfaces the latest snapshot of that cycle, even from an orphaned
  // timeline) but it's the wrong rule for a time-series chart.
  let logOut = '';
  try {
    logOut = execSync(`git -C ${JSON.stringify(repo)} log --reverse --format='%H %ct' --grep='supervisor2 checkpoint'`,
      { encoding: 'utf8', stdio: ['ignore', 'pipe', 'ignore'], maxBuffer: 4 * 1024 * 1024 });
  } catch (e) {
    return { error: String(e.message || e), checkpoints: [] };
  }
  const lines = logOut.split('\n').filter(Boolean);
  const checkpoints = [];
  let diskCacheDirty = false;
  for (const line of lines) {
    const sp = line.indexOf(' ');
    if (sp < 0) continue;
    const sha = line.slice(0, sp).replace(/^'/, '').replace(/'$/, '');
    const ts = parseInt(line.slice(sp + 1).replace(/'$/, ''), 10);
    const cachedEntry = PROGRESS_CACHE.get(progressCacheKey(repo, sha));
    const hadCached = !!cachedEntry;
    const hadTaskBlockers = cachedEntry && cachedEntry.data && cachedEntry.data.task_blockers !== undefined;
    const data = progressForCommit(repo, sha);
    if (!data) continue;
    if (!hadCached || !hadTaskBlockers) diskCacheDirty = true;
    checkpoints.push({ ts: ts * 1000, ...data });
  }
  try {
    attachSoundVerifierFailCounts(projectInfo, checkpoints);
  } catch (e) {
    // Non-fatal: chart falls back to upper edge = sound_or_waived + sound_unknown
    // when sound_verifier_fail is missing from the bucket.
    console.error(`[progress] attachSoundVerifierFailCounts failed: ${e && e.message || e}`);
  }
  const out = { checkpoints };
  if (headSha) {
    PROGRESS_SERIES_CACHE.set(repo, { headSha, ts: Date.now(), data: out });
    try { saveProgressSeriesDiskCache(projectInfo, headSha, out); } catch {}
  }
  if (diskCacheDirty) {
    try { saveProgressDiskCache(projectInfo); } catch {}
  }
  return out;
}

function startProgressWorker(projectInfo, headSha) {
  if (!headSha) return;
  const repo = projectInfo.repoPath;
  const running = PROGRESS_WORKERS.get(repo);
  if (running && running.headSha === headSha) return;
  const child = spawn(process.execPath, [__filename, '--progress-worker', projectInfo.slug, headSha], {
    cwd: __dirname,
    env: process.env,
    stdio: 'ignore',
    detached: true,
  });
  child.unref();
  PROGRESS_WORKERS.set(repo, { headSha, child });
  child.on('exit', () => {
    const current = PROGRESS_WORKERS.get(repo);
    if (current && current.child === child) PROGRESS_WORKERS.delete(repo);
  });
}

function buildProgressSeries(projectInfo) {
  const repo = projectInfo.repoPath;
  const headSha = repoHeadSha(repo);
  const seriesCache = PROGRESS_SERIES_CACHE.get(repo);
  if (headSha && seriesCache && seriesCache.headSha === headSha) {
    return seriesCache.data;
  }
  const diskSeries = readProgressSeriesDiskCache(projectInfo, headSha);
  if (diskSeries) {
    PROGRESS_SERIES_CACHE.set(repo, { headSha, ts: Date.now(), data: diskSeries });
    return diskSeries;
  }

  // Do not compute progress synchronously in the request path: it can take
  // tens of seconds and Node would block DAG/state requests. Return stale data
  // if we have it, otherwise an empty building response, and let a detached
  // worker populate the disk cache for the next refresh.
  startProgressWorker(projectInfo, headSha);
  const stale = seriesCache && seriesCache.data
    ? { ...seriesCache.data, stale: true, building: true }
    : { checkpoints: [], building: true };
  return stale;
}

function runProgressWorker() {
  const slug = process.argv[3] || defaultProjectSlug();
  const expectedHead = process.argv[4] || '';
  const projectInfo = resolveRepoPath(slug);
  const headSha = repoHeadSha(projectInfo.repoPath);
  if (expectedHead && headSha && expectedHead !== headSha) {
    // HEAD moved after this worker was spawned. The next request will spawn a
    // fresh worker for the new head; avoid writing immediately-stale series.
    return;
  }
  computeProgressSeriesSync(projectInfo, headSha || expectedHead);
}

app.get(`${BASE}/api/progress.json`, (req, res) => {
  try {
    const projectInfo = resolveRepoPath(defaultProjectSlug());
    res.json(buildProgressSeries(projectInfo));
  } catch (e) { res.status(500).json({ error: e.message }); }
});
app.get(`${BASE}/:project/api/progress.json`, (req, res) => {
  try {
    const projectInfo = resolveRepoPath(projectFromRequest(req));
    res.json(buildProgressSeries(projectInfo));
  } catch (e) { res.status(500).json({ error: e.message }); }
});

function startServer() {
  return app.listen(PORT, () => {
    writeStatic();
    console.log(`Tablet viewer at http://localhost:${PORT}${BASE}/${defaultProjectSlug()}/`);
    console.log(`Projects root: ${PROJECTS_ROOT}`);
  });
}

if (require.main === module) {
  if (process.argv[2] === '--progress-worker') {
    runProgressWorker();
  } else {
    startServer();
  }
}

module.exports = {
  app,
  buildArtifactChatData,
  parseCodexOutputEntries,
  parseJsonlTranscriptEntries,
  parseJsonTranscriptEntries,
  readHistoricalViewerState,
  readHistoricalChats,
  readLiveViewerState,
  readLiveChats,
  startServer,
  writeStatic,
};
