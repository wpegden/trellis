const assert = require('assert');
const { recommendedLeanParallelism, validateLeanParallelism } = require('./lean_parallelism');

const GiB = 1024 ** 3;
function recommend(memoryGiB, cpus = 16, files = {}) {
  return recommendedLeanParallelism({ memoryBytes: memoryGiB * GiB, cpus,
    readFile: file => files[file] || '' });
}

assert.strictEqual(recommend(62.7).recommended, 6);
assert.strictEqual(recommend(8).recommended, 1);
assert.strictEqual(recommend(160, 4).recommended, 4);
assert.strictEqual(recommend(NaN).recommended, 6);
assert.strictEqual(recommend(NaN, 2).recommended, 2);

const unified = {
  '/proc/self/cgroup': '0::/parent/run',
  '/proc/self/mountinfo': '37 27 0:31 / /sys/fs/cgroup rw - cgroup2 cgroup2 rw',
  '/sys/fs/cgroup/memory.max': 'max',
  '/sys/fs/cgroup/parent/memory.max': String(25 * GiB),
  '/sys/fs/cgroup/parent/run/memory.max': 'max',
};
assert.strictEqual(recommend(64, 16, unified).recommended, 2,
  'an unlimited leaf must still honor its parent memory limit');
assert.strictEqual(recommend(64, 16, unified).memory_bytes, 25 * GiB);
assert.strictEqual(recommend(64, 16, { ...unified,
  '/sys/fs/cgroup/parent/run/memory.max': String(8 * GiB),
}).recommended, 1);
assert.strictEqual(recommend(256, 16, { ...unified,
  '/sys/fs/cgroup/parent/memory.max': 'max',
  '/sys/fs/cgroup/parent/cpu.max': '250000 100000',
}).recommended, 2, 'CPU quotas also cap the recommendation');
assert.strictEqual(recommend(64, 16, {
  '/proc/self/cgroup': '5:memory:/tenant/run',
  '/proc/self/mountinfo': '37 27 0:31 /tenant /sys/fs/cgroup/memory rw - cgroup cgroup rw,memory',
  '/sys/fs/cgroup/memory/run/memory.limit_in_bytes': String(40 * GiB),
  '/sys/fs/cgroup/memory/memory.limit_in_bytes': String(30 * GiB),
}).recommended, 3, 'cgroup v1 mount roots are resolved relative to membership');

for (const n of [1, 6, 12, '3']) assert.strictEqual(validateLeanParallelism(n), Number(n));
for (const invalid of [0, -1, 1.5, NaN, Infinity, true, false, [], {}, '', ' 6', '1e2', '06', '1;exit', Number.MAX_SAFE_INTEGER + 1]) {
  assert.throws(() => validateLeanParallelism(invalid), /positive integer/);
}
console.log('Lean parallelism recommendation tests passed');
