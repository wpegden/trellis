// The coordinator owns this admission signal. Read it on every poll, even
// when protocol_state.json has not changed because preparation is blocked.
const fs = require('fs');
const path = require('path');

function storageWait(runtime) {
  if (!runtime) return null;
  let value;
  try {
    const file = path.join(runtime, 'dispatch/preparation-capacity.json');
    if (fs.statSync(file).size > 65536) return null;
    value = JSON.parse(fs.readFileSync(file, 'utf8'));
  } catch { return null; }
  if (value?.waiting !== true || !Array.isArray(value.filesystems)) return null;
  const count = n => Number.isSafeInteger(n) && n >= 0;
  const filesystems = value.filesystems.slice(0, 8).flatMap(row => {
    if (!row || typeof row.path !== 'string' || !count(row.free_bytes) || !count(row.required_bytes)) return [];
    const bytes = Math.max(0, row.required_bytes - row.free_bytes);
    const inodes = count(row.free_inodes) && count(row.required_inodes)
      ? Math.max(0, row.required_inodes - row.free_inodes) : 0;
    if (!bytes && !inodes) return [];
    return [{ path: row.path.slice(0, 1024), free_bytes: row.free_bytes,
      required_bytes: row.required_bytes, shortfall_bytes: bytes,
      ...(inodes ? { free_inodes: row.free_inodes, required_inodes: row.required_inodes,
        shortfall_inodes: inodes } : {}) }];
  });
  if (!filesystems.length) return null;
  const gib = n => (n / (1024 ** 3)).toFixed(1) + ' GiB';
  const reason = filesystems.map(row => `${row.path}: ` + [
    ...(row.shortfall_bytes ? [`${gib(row.free_bytes)} free; ${gib(row.required_bytes)} required (${gib(row.shortfall_bytes)} short)`] : []),
    ...(row.shortfall_inodes ? [`${row.free_inodes} free inodes; ${row.required_inodes} required (${row.shortfall_inodes} short)`] : []),
  ].join('; ')).join('. ');
  const retry_seconds = count(value.retry_seconds) && value.retry_seconds > 0 ? value.retry_seconds : null;
  return { waiting: true, filesystems, retry_seconds,
    label: filesystems.some(row => row.shortfall_bytes) ? 'WAITING FOR DISK SPACE' : 'WAITING FOR FREE INODES',
    summary: 'New workspace preparation is blocked by storage capacity. Existing work can continue. '
      + (retry_seconds ? `The coordinator retries automatically every ${retry_seconds} seconds; no resume is needed.`
        : 'The coordinator retries automatically; no resume is needed.'), reason };
}

module.exports = { storageWait };
