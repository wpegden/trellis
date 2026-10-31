'use strict';
// Optional private PV surface. Removing this module removes its routes and UI.
const fs = require('fs');
const path = require('path');

function decision(body) {
  if (!body || !['accept', 'deny', 'amend'].includes(body.action)) throw new Error('Choose accept, deny, or amend');
  if (typeof body.digest !== 'string' || !/^[a-f0-9]{64}$/.test(body.digest)) throw new Error('An exact review package digest is required');
  const reason = body.reason === undefined ? '' : body.reason;
  if (typeof reason !== 'string' || reason.length > 20000) throw new Error('Reason must be text of at most 20000 characters');
  if (body.action !== 'accept' && !reason.trim()) throw new Error('A reason of 1–20000 characters is required');
  const edits = body.code_edits ?? [];
  if (body.comment_only !== undefined && typeof body.comment_only !== 'boolean') throw new Error('Comment-only selection must be boolean');
  if (body.comment_only && (body.action !== 'amend' || !Array.isArray(edits) || !edits.length)) throw new Error('Comment-only review requires exact code edits in an amendment');
  if (!Array.isArray(edits) || edits.length > 64) throw new Error('Code amendments accept up to 64 edited files');
  if (edits.length && body.action !== 'amend') throw new Error('Code edits require an amendment request');
  let bytes = 0;
  const seen = new Set();
  for (const edit of edits) {
    if (!edit || Object.keys(edit).sort().join(',') !== 'base_sha256,path,text' ||
        typeof edit.path !== 'string' || !edit.path || edit.path.length > 1024 || /[\x00-\x1f\\]/.test(edit.path) ||
        edit.path.split('/').some(part => !part || part === '.' || part === '..') || seen.has(edit.path) ||
        (edit.base_sha256 !== null && !/^[a-f0-9]{64}$/.test(edit.base_sha256)) ||
        (edit.text !== null && (typeof edit.text !== 'string' || edit.text.includes('\0')))) throw new Error('Invalid code edit: each file requires a unique path, base hash and text (or null for deletion)');
    seen.add(edit.path);
    const size = Buffer.byteLength(edit.text || '', 'utf8'); bytes += size;
    if (size > 1024 * 1024 || bytes > 4 * 1024 * 1024) throw new Error('Code edits exceed 1 MiB per file or 4 MiB total');
  }
  if (body.action === 'amend' && ((body.amendment != null && (typeof body.amendment !== 'string' || body.amendment.length > 20000)) ||
      (!edits.length && !body.amendment?.trim()))) throw new Error('Describe the requested amendment or edit the code');
  return { action: body.action, digest: body.digest, reason,
    ...(body.action === 'amend' ? { amendment: body.amendment || '', ...(body.comment_only ? { comment_only: true } : {}), ...(edits.length ? { code_edits: edits } : {}) } : {}) };
}

function attention(status) {
  if (status?.controller !== 'pv_revision') return null;
  const state = status.state;
  const labels = { awaiting_human: 'PV REVISION — HUMAN REVIEW', ready: 'PV REVISION READY',
    active: 'PV REMEDIATION', accepted_for_modeling: 'PV REVISION RATIFIED', denied: 'PV REVISION DENIED',
    requires_modeling_review: 'PV MODELING REVIEW REQUIRED', halted: 'PV REVISION HALTED', failed: 'PV REVISION FAILED' };
  return { id: 'pv_revision', tier: ['failed', 'halted'].includes(state) ? 'failure' : state === 'awaiting_human' ? 'gate' : 'pause',
    rank: 41, icon: '↻', label: labels[state] || 'PV REVISION',
    summary: state === 'awaiting_human' ? 'Open the review page for the complete goals and code package. Human ratification is required before fresh Modeling and verification.'
      : `The predecessor is retained. Revision status: ${String(state || 'unknown').replace(/_/g, ' ')}.`,
    reason: status.feedback || '', resumable: false };
}

function install(d) {
  const launches = new Map();
  const script = path.join(d.TRELLIS_ROOT, 'scripts', 'pv_revision.py');
  function runtime(req) {
    const project = d.resolveProject(req);
    const root = d.runtimeRootForProject(project);
    if (!root) throw new Error('No runtime belongs to this project');
    return { root, project };
  }
  function run(args, timeout = 30000, input = null) {
    return new Promise((resolve, reject) => {
      const child = d.execFile('python3', [script, ...args], {
        cwd: d.TRELLIS_ROOT, encoding: 'utf-8', timeout, maxBuffer: 32 * 1024 * 1024,
      }, (error, stdout, stderr) => {
        if (error) {
          let detail;
          try { detail = JSON.parse(stdout).error; } catch {}
          reject(new Error(String(detail || stderr || error.message).trim())); return;
        }
        try { resolve(JSON.parse(stdout)); } catch { reject(new Error('Revision controller returned invalid JSON')); }
      });
      if (input !== null) {
        child.stdin.on('error', reject);
        child.stdin.end(input);
      }
    });
  }
  d.registerReadRoute('pv-revision/campaign.json', async (req, res) => {
    try {
      const { root } = runtime(req);
      res.set('Cache-Control', 'no-store').json({ campaign: await run(['campaign-status', root]) });
    } catch (error) { res.status(400).json({ error: error.message }); }
  });
  d.registerReadRoute('pv-revision/campaign.zip', async (req, res) => {
    try {
      if (req.query.snapshot !== undefined && req.query.snapshot !== '1') return res.status(400).json({ error: 'Unknown snapshot selection' });
      const { root } = runtime(req);
      const snapshot = req.query.snapshot === '1';
      const exported = await run(['campaign-export', root, ...(snapshot ? ['--allow-incomplete'] : [])], 120000);
      if (!/^[a-f0-9]{64}$/.test(exported?.sha256 || '')) throw new Error('No verified campaign archive was returned');
      const directory = fs.realpathSync(path.join(root, 'campaign-archives'));
      const archive = fs.realpathSync(exported.path);
      const relative = path.relative(directory, archive);
      if (!relative || relative === '..' || relative.startsWith('..' + path.sep) || path.isAbsolute(relative) || path.extname(archive) !== '.zip') throw new Error('Controller export is outside its campaign archives');
      res.set('Cache-Control', 'no-store');
      return res.download(archive, `pv-campaign-${snapshot ? 'snapshot' : 'final'}-${exported.sha256}.zip`);
    } catch (error) { res.status(409).json({ error: error.message }); }
  });
  function schedule(root) {
    launches.set(root, { state: 'scheduling' });
    d.execFile('python3', [script, 'schedule', root], {
      cwd: d.TRELLIS_ROOT, encoding: 'utf-8', timeout: 30000, maxBuffer: 1024 * 1024,
    }, (error, stdout, stderr) => {
      launches.set(root, error ? { state: 'failed', error: String(stderr || error.message).trim() }
        : { state: 'scheduled', output: String(stdout || '').trim() });
    });
  }
  d.registerReadRoute('pv-revision.json', async (req, res) => {
    try {
      const { root } = runtime(req);
      const state = await run(['status', root]);
      let details = {};
      if (state) {
        const file = path.join(root, 'pv-revision', 'state.json');
        if (fs.existsSync(file)) {
          const saved = JSON.parse(fs.readFileSync(file, 'utf8'));
          details = { audit_summary: saved.plan?.summary || '', dispositions: saved.plan?.dispositions || [],
            tasks: Object.entries(saved.tasks || {}).map(([id, task]) => ({ id, title: task.authorization?.title || id,
              authorization: task.authorization?.authorization || '', status: task.status, attempts: task.attempts?.length || 0 })),
            attempt_count: saved.attempts?.length || 0, round: saved.round || 0 };
        }
      }
      res.set('Cache-Control', 'no-store').json({ revision: state ? { ...state, ...details, launch: launches.get(root) || null } : null });
    } catch (error) { res.status(400).json({ error: error.message }); }
  });
  d.registerReadRoute('pv-revision/review', async (req, res) => {
    try {
      const { root } = runtime(req);
      if (!/^[a-f0-9]{64}$/.test(req.query.digest || '')) throw new Error('An exact review package digest is required');
      const review = await run(['review', root]);
      if (review.digest !== req.query.digest) throw new Error('Review package changed; reopen the review');
      res.set('Cache-Control', 'no-store').json(review);
    } catch (error) { res.status(409).json({ error: error.message }); }
  });
  d.registerReadRoute('pv-revision/file', async (req, res) => {
    try {
      const { root } = runtime(req);
      const q = req.query;
      if (typeof q.path !== 'string' || q.path.length > 1024 || !/^[a-f0-9]{64}$/.test(q.digest || '') ||
          (q.mode !== undefined && !['diff', 'before', 'after', 'edit'].includes(q.mode)) ||
          (q.context !== undefined && !/^\d{1,3}$/.test(q.context)) ||
          (q.offset !== undefined && !/^\d{1,6}$/.test(q.offset))) throw new Error('Invalid frozen-file request');
      const result = await run(['review-file', root, '--digest', q.digest, '--path', q.path,
        '--mode', q.mode || 'diff', '--context', q.context || '3', '--offset', q.offset || '0']);
      res.set('Cache-Control', 'no-store').json(result);
    } catch (error) { res.status(409).json({ error: error.message }); }
  });
  d.registerReadRoute('pv-revision/package', async (req, res) => {
    try {
      const { root } = runtime(req);
      const format = req.query.format || 'json';
      if (!['json', 'patch', 'evidence-patch', 'readme', 'zip'].includes(format)) return res.status(400).json({ error: 'Unknown package format' });
      if (format === 'zip') {
        const exported = await run(['export', root]);
        if (!exported || !/^[a-f0-9]{64}$/.test(exported.digest || '')) throw new Error('No immutable review archive is available');
        if (req.query.digest && req.query.digest !== exported.digest) return res.status(409).json({ error: 'Review package changed; refresh before downloading' });
        const revision = fs.realpathSync(path.join(root, 'pv-revision'));
        const archive = fs.realpathSync(exported.path);
        const relative = path.relative(revision, archive);
        if (!relative || relative.startsWith('..' + path.sep) || path.isAbsolute(relative) || path.extname(archive) !== '.zip') throw new Error('Controller export is outside its revision');
        res.set('Cache-Control', 'no-store');
        return res.download(archive, `pv-revision-${exported.digest}.zip`);
      }
      const pkg = await run(['package', root]);
      if (!pkg || !/^[a-f0-9]{64}$/.test(pkg.digest || '')) throw new Error('No immutable review package is available');
      if (req.query.digest && req.query.digest !== pkg.digest) return res.status(409).json({ error: 'Review package changed; refresh before reviewing' });
      res.set('Cache-Control', 'no-store');
      if (format === 'json') return res.set('Content-Disposition', `attachment; filename="pv-revision-${pkg.digest}.json"`).json(pkg);
      if (format === 'patch') return res.type('text/plain').set('Content-Disposition', `attachment; filename="pv-revision-${pkg.digest}.patch"`).send(pkg.aggregate_diff?.patch || '');
      if (format === 'evidence-patch') return res.type('text/plain').set('Content-Disposition', `attachment; filename="pv-revision-evidence-${pkg.digest}.patch"`).send(pkg.evidence_diff?.patch || '');
      // The immutable README is selected by a controller-derived digest only.
      const readme = path.join(root, 'pv-revision', 'reviews', pkg.digest, 'README.md');
      return res.type('text/plain').set('Content-Disposition', `attachment; filename="pv-revision-${pkg.digest}.md"`).send(fs.readFileSync(readme, 'utf8'));
    } catch (error) { res.status(409).json({ error: error.message }); }
  });
  const decisionJson = d.express.json({ limit: '8mb' });
  d.registerControlRoute('pv-revision/decide', [(req, res, next) => decisionJson(req, res, error => {
    if (error) return res.status(400).json({ error: error.type === 'entity.too.large'
      ? 'Encoded code amendment exceeds 8 MiB' : 'Invalid decision JSON' });
    next();
  })], async (req, res) => {
    let input;
    try { input = decision(req.body); } catch (error) { return res.status(400).json({ error: error.message }); }
    try {
      const { root, project } = runtime(req);
      const args = ['decide', root, '--action', input.action, '--digest', input.digest, '--reason', input.reason];
      if (input.amendment) args.push('--amendment', input.amendment);
      const result = input.code_edits
        ? await run(['decide-json', root], 30000, JSON.stringify(input)) : await run(args);
      d.invalidatePauseStatus?.(project);
      // Persisting the human decision finishes before scheduling a controller;
      // the HTTP request never waits for agents or fresh verification.
      if (input.action !== 'deny') schedule(root);
      res.status(input.action === 'deny' ? 200 : 202).json({ ok: true, decision: result, scheduled: input.action !== 'deny' });
    } catch (error) { res.status(409).json({ error: error.message }); }
  });
  d.registerProjectHtmlTransform(html => {
    const client = ['pv_revision_review.js', 'pv_revision_client.js'].map(file => fs.readFileSync(path.join(__dirname, 'public', file), 'utf8')).join('\n').replace(/<\/script/gi, '<\\/script');
    return html.replace('</body>', `<script>${client}</script></body>`);
  });
}

module.exports = { install, decision, attention };
