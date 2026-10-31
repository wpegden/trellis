'use strict';

const { Worker, isMainThread, parentPort } = require('worker_threads');

// One worker serializes expensive rollups across projects while keeping the
// HTTP event loop responsive. It stays alive to retain per-file history folds.
let worker = null;
let nextId = 0;
const pending = new Map();

function runUsageRollup(projectInfo) {
  if (!worker) {
    const current = new Worker(__filename);
    worker = current;
    const fail = error => {
      if (worker !== current) return;
      worker = null;
      for (const task of pending.values()) task.reject(error);
      pending.clear();
    };
    current.on('message', message => {
      if (worker !== current) return;
      const task = pending.get(message.id);
      if (!task) return;
      pending.delete(message.id);
      if (!pending.size) current.unref();
      if (message.error) task.reject(new Error(message.error));
      else task.resolve(message.value);
    });
    current.on('error', fail);
    current.on('exit', code => fail(new Error(`Usage worker exited (${code})`)));
  }
  return new Promise((resolve, reject) => {
    const id = ++nextId;
    pending.set(id, { resolve, reject });
    worker.ref();
    try { worker.postMessage({ id, projectInfo }); }
    catch (error) {
      pending.delete(id);
      if (!pending.size) worker.unref();
      reject(error);
    }
  });
}

module.exports = { runUsageRollup };

if (!isMainThread) {
  parentPort.on('message', ({ id, projectInfo }) => {
    try {
      const { buildUsageRollup } = require('./server');
      parentPort.postMessage({ id, value: buildUsageRollup(projectInfo) });
    } catch (error) {
      parentPort.postMessage({ id, error: error.message || String(error) });
    }
  });
}
