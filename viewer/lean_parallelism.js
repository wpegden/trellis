'use strict';

const fs = require('fs');
const os = require('os');
const path = require('path');

function validateLeanParallelism(value) {
  if (!['number', 'string'].includes(typeof value)
      || !/^[1-9][0-9]*$/.test(String(value))
      || !Number.isSafeInteger(Number(value))) {
    throw new Error('lean_parallelism must be a positive integer');
  }
  return Number(value);
}

// Total memory, capped by every applicable cgroup ancestor. Free memory is
// deliberately not an input: starting another run must not change the host's
// recommendation from one page refresh to the next.
function recommendedLeanParallelism(options = {}) {
  const read = options.readFile || (file => fs.readFileSync(file, 'utf8'));
  const readText = file => { try { return read(file).trim(); } catch { return ''; } };
  let memory = options.memoryBytes === undefined ? os.totalmem() : options.memoryBytes;
  let cpus = options.cpus === undefined
    ? (os.availableParallelism ? os.availableParallelism() : os.cpus().length) : options.cpus;
  memory = Number.isFinite(memory) && memory > 0 ? memory : null;
  cpus = Number.isFinite(cpus) && cpus >= 1 ? Math.floor(cpus) : null;
  const memberships = readText('/proc/self/cgroup').split('\n').map(line => {
    const match = /^\d+:([^:]*):(.*)$/.exec(line);
    return match && { controllers: match[1].split(','), directory: match[2] };
  }).filter(Boolean);
  const unescape = value => value.replace(/\\([0-7]{3})/g,
    (_, octal) => String.fromCharCode(parseInt(octal, 8)));
  for (const line of readText('/proc/self/mountinfo').split('\n')) {
    const [before, after] = line.split(' - ');
    if (!after) continue;
    const fields = before.split(' ');
    const [type, , controllers = ''] = after.split(' ');
    if (!['cgroup', 'cgroup2'].includes(type) || fields.length < 5) continue;
    const root = unescape(fields[3]);
    const mount = unescape(fields[4]);
    for (const member of memberships) {
      const unified = type === 'cgroup2' && member.controllers[0] === '';
      const memoryController = unified || (type === 'cgroup'
        && controllers.split(',').includes('memory') && member.controllers.includes('memory'));
      const cpuController = unified || (type === 'cgroup'
        && controllers.split(',').includes('cpu') && member.controllers.includes('cpu'));
      if (!memoryController && !cpuController) continue;
      const relative = path.posix.relative(root, member.directory);
      if (relative.startsWith('..') || path.posix.isAbsolute(relative)) continue;
      let directory = path.join(mount, relative);
      while (true) {
        if (memoryController) {
          const value = Number(readText(path.join(directory,
            unified ? 'memory.max' : 'memory.limit_in_bytes')));
          if (Number.isFinite(value) && value > 0) memory = memory === null ? value : Math.min(memory, value);
        }
        if (cpuController) {
          const values = unified ? readText(path.join(directory, 'cpu.max')).split(/\s+/)
            : [readText(path.join(directory, 'cpu.cfs_quota_us')),
              readText(path.join(directory, 'cpu.cfs_period_us'))];
          const [quota, period] = values.map(Number);
          if (quota > 0 && period > 0) {
            const limit = Math.max(1, Math.floor(quota / period));
            cpus = cpus === null ? limit : Math.min(cpus, limit);
          }
        }
        if (directory === mount) break;
        directory = path.dirname(directory);
      }
    }
  }
  return {
    recommended: Math.max(1, Math.min(cpus || Infinity,
      memory === null ? 6 : Math.floor(memory / (10 * 1024 ** 3)))),
    memory_bytes: memory,
    cpus,
  };
}

module.exports = { recommendedLeanParallelism, validateLeanParallelism };
