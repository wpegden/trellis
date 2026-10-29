'use strict';
const { validateLeanParallelism } = require('./lean_parallelism');

function resumeCreateArgs(body) {
  const remote = String(body.remote_url || '').trim();
  if (!remote || remote.startsWith('-') || /\s|[\x00-\x1f]/.test(remote)) {
    throw new Error('Enter a Git remote URL.');
  }
  if (!remote.startsWith('/') && !/^[\w.-]+@[\w.-]+:[\w./-]+$/.test(remote)) {
    let url;
    try { url = new URL(remote); } catch { throw new Error('Enter a valid HTTPS or SSH Git remote URL.'); }
    if (!['https:', 'ssh:', 'file:'].includes(url.protocol) || url.password
        || (url.protocol === 'https:' && url.username) || url.search || url.hash) {
      throw new Error('Use the host Git credential helper or SSH login; omit credentials from the URL.');
    }
  }
  if (body.handoff_confirmed !== true) {
    throw new Error('Confirm that the previous writer is stopped before resuming this remote.');
  }
  const args = ['start-resume', String(body.slug || ''), '--remote-url', remote, '--handoff-confirmed'];
  for (const key of ['branch', 'commit']) {
    if (body[key]) {
      const value = String(body[key]);
      if (value.startsWith('-') || !/^[A-Za-z0-9_./-]+$/.test(value)) throw new Error(`Invalid ${key}.`);
      args.push(`--${key}`, value);
    }
  }
  if (body.lean_parallelism != null) {
    args.push('--lean-parallelism', String(validateLeanParallelism(body.lean_parallelism)));
  }
  return args;
}
module.exports = { resumeCreateArgs };
