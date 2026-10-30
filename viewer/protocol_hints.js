// Read a few root scalar fields without materializing nested protocol history.
// Missing fields outside the byte budget remain unknown, never inferred from
// similarly named fields in a task, request, or quoted agent text.
const fs = require('fs');

function* scanTopLevelScalars(file, fields, maxBytes, objectFields = {}, windows = false) {
  let wanted = new Set(fields);
  let values = Object.create(null);
  let fd;
  try { fd = fs.openSync(file, 'r'); } catch { return null; }
  const chunk = Buffer.alloc(Math.min(64 * 1024, maxBytes));
  let state = 'start', key = null, token = [], oversized = false;
  let stringKind = null, escaped = false;
  const nested = [];
  // Selected root objects reuse the scalar scanner. Large contracts/history
  // are skipped in place, never accumulated in an object-sized buffer.
  let parent = null;
  function finishObject() {
    if (parent) {
      parent.values[parent.key] = values;
      parent.wanted.delete(parent.key);
      ({ wanted, values } = parent);
      parent = null;
      state = 'separator';
    } else state = 'done';
  }
  const whitespace = c => c === 32 || c === 9 || c === 10 || c === 13;
  function capture(c) {
    // Only short field names/scalars are hints. Skipped strings use no buffer.
    if (token.length < 256) token.push(c);
    else oversized = true;
  }
  function scalar() {
    if (wanted.has(key) && !oversized) {
      const value = JSON.parse(Buffer.from(token).toString('utf8'));
      if (value === null || ['string', 'number', 'boolean'].includes(typeof value)) {
        values[key] = value;
        wanted.delete(key);
      }
    }
    token = [];
    oversized = false;
    state = 'separator';
  }
  try {
    let offset = 0;
    let windowEnd = maxBytes;
    while (state !== 'done' && (wanted.size || parent)) {
      if (offset >= windowEnd) {
        if (!windows) break;
        fs.closeSync(fd); fd = undefined;
        yield null; // Keep parser state, with no file descriptor held between ticks.
        fd = fs.openSync(file, 'r');
        windowEnd += maxBytes;
      }
      const n = fs.readSync(fd, chunk, 0, Math.min(chunk.length, windowEnd - offset), offset);
      if (!n) break;
      offset += n;
      for (let i = 0; i < n && state !== 'done' && (wanted.size || parent); i++) {
        const c = chunk[i];
        if (stringKind) {
          if (stringKind !== 'skip') capture(c);
          if (escaped) { escaped = false; continue; }
          if (c === 92) { escaped = true; continue; }
          if (c !== 34) continue;
          if (stringKind === 'key') {
            key = oversized ? null : JSON.parse(Buffer.from(token).toString('utf8'));
            token = []; oversized = false; state = 'colon';
          } else if (stringKind === 'value') scalar();
          stringKind = null;
          continue;
        }
        if (nested.length) {
          if (c === 34) stringKind = 'skip';
          else if (c === 123 || c === 91) {
            if (nested.length >= 128) return null;
            nested.push(c);
          } else if (c === 125 || c === 93) {
            if (nested.pop() !== (c === 125 ? 123 : 91)) return null;
            if (!nested.length) {
              state = 'separator';
            }
          }
          continue;
        }
        if (state === 'scalar') {
          if (whitespace(c) || c === 44 || c === 125) scalar();
          else { capture(c); continue; }
        }
        if (whitespace(c)) continue;
        if (state === 'start') {
          if (c !== 123) return null;
          state = 'key';
        } else if (state === 'key') {
          if (c === 125) { finishObject(); continue; }
          if (c !== 34) return null;
          stringKind = 'key'; token = [c];
        } else if (state === 'colon') {
          if (c !== 58) return null;
          state = 'value';
        } else if (state === 'value') {
          if (c === 123 || c === 91) {
            if (c === 123 && !parent && wanted.has(key)
                && Object.prototype.hasOwnProperty.call(objectFields, key)) {
              parent = { wanted, values, key };
              wanted = new Set(objectFields[key]); values = Object.create(null);
              state = 'key'; key = null;
            } else nested.push(c);
          }
          else if (c === 34) { stringKind = 'value'; token = [c]; }
          else { state = 'scalar'; token = [c]; }
        } else if (state === 'separator') {
          if (c === 44) state = 'key';
          else if (c === 125) finishObject();
          else return null;
        }
      }
    }
    if (parent) values = parent.values; // An incomplete selected object is unknown.
    return Object.keys(values).length ? values : null;
  } catch {
    return null;
  } finally {
    if (fd !== undefined) fs.closeSync(fd);
  }
}

function readTopLevelScalars(file, fields, maxBytes, objectFields = {}) {
  return scanTopLevelScalars(file, fields, maxBytes, objectFields).next().value;
}

function createWindowedScalarReader(fields, windowBytes, objectFields = {}) {
  // Each state file gets at most one fixed-size scan window per invocation.
  // Stable completed projections are cached; atomic replacement or in-place
  // edits discard all prior parser state. No historical/nested ID is inferred.
  const cache = new Map();
  function stamp(file) {
    const s = fs.statSync(file, {bigint: true});
    return [s.dev, s.ino, s.size, s.mtimeNs, s.ctimeNs].join(':');
  }
  return file => {
    try {
      const before = stamp(file);
      let entry = cache.get(file);
      if (!entry || entry.stamp !== before) {
        entry?.scanner?.return();
        entry = {stamp: before, scanner: scanTopLevelScalars(file, fields, windowBytes, objectFields, true)};
        cache.delete(file); cache.set(file, entry);
        if (cache.size > 32) {
          const oldest = cache.keys().next().value;
          cache.get(oldest).scanner?.return(); cache.delete(oldest);
        }
      }
      if (entry.scanner) {
        const step = entry.scanner.next();
        if (step.done) { entry.value = step.value; entry.scanner = null; }
      }
      if (stamp(file) !== before) { entry.scanner?.return(); cache.delete(file); return null; }
      return entry.value || null;
    } catch {
      cache.get(file)?.scanner?.return(); cache.delete(file); return null;
    }
  };
}

module.exports = { readTopLevelScalars, createWindowedScalarReader };
