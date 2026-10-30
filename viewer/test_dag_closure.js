'use strict';

const assert = require('assert');
const fs = require('fs');
const path = require('path');
const vm = require('vm');

const html = fs.readFileSync(path.join(__dirname, 'public', 'index.html'), 'utf8');
const source = html.slice(html.indexOf('function getRecursivelyClosedNodes('),
  html.indexOf('// Patch C local-closure status'));
assert(source.includes('function isCommittedClosed('));
const context = vm.createContext({ currentState: {} });
vm.runInContext(source, context);
function classify(nodes, edges, state = {}) {
  context.currentState = { state };
  context.nodes = nodes;
  context.edges = edges;
  return [...vm.runInContext('getRecursivelyClosedNodes(nodes, edges)', context, { timeout: 1000 })].sort();
}

// Shared dependencies, open transitive dependencies, and unrelated components.
const nodes = Object.fromEntries(['root', 'left', 'right', 'tip', 'other']
  .map(name => [name, { status: 'closed' }]));
const edges = [['left', 'root'], ['right', 'root'], ['tip', 'left'], ['tip', 'right']];
assert.deepStrictEqual(classify(nodes, edges), Object.keys(nodes).sort());
nodes.root.status = 'open';
assert.deepStrictEqual(classify(nodes, edges), ['other']);
// Committed membership overrides live status, including absent committed nodes.
const committed = { present_nodes: Object.keys(nodes), open_nodes: [] };
assert.deepStrictEqual(classify(nodes, edges, { committed }), Object.keys(nodes).sort());
committed.open_nodes = ['left'];
assert.deepStrictEqual(classify(nodes, edges, { committed }), ['other', 'right', 'root']);
committed.present_nodes = ['left', 'right', 'tip', 'other'];
assert.deepStrictEqual(classify(nodes, edges, { committed }), ['other']);
// Rewired visible edges govern closure; hidden/missing nodes cannot leak in.
assert.deepStrictEqual(classify({ tip: nodes.tip, other: nodes.other },
  [['tip', 'other'], ['tip', 'hidden']]), ['other', 'tip']);
// Cycles do not recurse forever; an open dependency invalidates the whole cycle.
const cycle = { a: { status: 'closed' }, b: { status: 'closed' }, c: { status: 'open' } };
assert.deepStrictEqual(classify(cycle, [['a', 'b'], ['b', 'a']]), ['a', 'b']);
assert.deepStrictEqual(classify(cycle, [['a', 'b'], ['b', 'a'], ['b', 'c']]), []);

// A small shared DAG already has exponentially many paths. A much deeper
// version must also finish without depending on the JavaScript call stack.
const large = {}, sharedEdges = [];
for (let i = 0; i < 10000; i++) {
  large[`n${i}`] = { status: 'closed' };
  if (i) sharedEdges.push([`n${i}`, `n${i - 1}`]);
  if (i > 1) sharedEdges.push([`n${i}`, `n${i - 2}`]);
}
assert.strictEqual(classify(large, sharedEdges).length, 10000);
large.n0.status = 'open';
assert.deepStrictEqual(classify(large, sharedEdges), []);
large.n0.status = 'closed';
assert.strictEqual(classify(large, sharedEdges).length, 10000);

// Warming recent history must not become an exhaustive history walk on the
// next refresh once the nearest checkpoints are already in the cache.
const warmSource = html.slice(html.indexOf('function prewarmHistoricalStates('),
  html.indexOf('async function updateCycleInfo('));
const timers = [], fetched = [];
const warmContext = vm.createContext({
  historicalStateCache: new Map([[20, {}]]), historicalStateInflight: new Map(),
  lastInteractiveActivityAt: 0, chatRefreshInFlight: 0,
  setTimeout: fn => timers.push(fn),
  fetchHistoricalState: cycle => { fetched.push(cycle); return Promise.resolve({}); },
});
vm.runInContext(warmSource + '; prewarmHistoricalStates(Array.from({length:20}, (_, i) => i + 1));', warmContext);
while (timers.length) timers.shift()();
assert.deepStrictEqual(fetched, [19]);
warmContext.historicalStateCache.set(19, {});
fetched.length = 0;
vm.runInContext('prewarmHistoricalStates(Array.from({length:20}, (_, i) => i + 1));', warmContext);
while (timers.length) timers.shift()();
assert.deepStrictEqual(fetched, []);
console.log('DAG closure tests passed');
