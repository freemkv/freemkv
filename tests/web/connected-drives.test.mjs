// Run with: node --experimental-vm-modules --test tests/web/*.test.mjs
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import vm from 'node:vm';

async function harness(id = 'p123') {
  let now = 100000, handlers, stopped = false;
  const timers = new Map(), requests = [];
  const context = vm.createContext({
    AbortController, queueMicrotask, Date: { now: () => now },
    setInterval(fn, ms) { timers.set(fn, ms); return fn; },
    clearInterval(fn) { timers.delete(fn); },
  });
  const module = new vm.SourceTextModule(await readFile(new URL('../../src/server/web/assets/connection.js', import.meta.url), 'utf8'), { context });
  await module.link(name => name === './ui.js'
    ? new vm.SyntheticModule(['api'], function () {
      this.setExport('api', (method, url) => new Promise((resolve, reject) => requests.push({ url, resolve, reject })));
    }, { context })
    : new vm.SyntheticModule(['openStream', 'subscribe', 'live'], function () {
      this.setExport('openStream', (url, h) => { assert.equal(url, '/api/peers/p123/events'); handlers = h; return () => { stopped = true; }; });
      this.setExport('subscribe', (kind, fn) => { handlers = { state: fn }; return () => { stopped = true; }; });
      this.setExport('live', { state: null });
    }, { context }));
  await module.evaluate();
  const c = module.namespace.libraryConnection({ id, name: 'Library' }, () => {});
  const flush = async () => { for (let i = 0; i < 10; i++) await Promise.resolve(); };
  await flush();
  return { module: module.namespace, c, requests, handlers, flush, stopped: () => stopped,
    tick(ms) { now += ms; for (const [fn, interval] of timers) if (interval === 3000) fn(); },
    take(path) { const i = requests.findIndex(r => r.url.endsWith(path)); assert(i >= 0, path); return requests.splice(i, 1)[0]; }
  };
}
for (const id of ['', 'p123']) test('same stream wins over a slow poll and details: ' + (id || 'local'), async () => {
  const h = await harness(id);
  h.handlers.state({ sg0: { progress_pct: 80 } });
  assert.equal(h.c.status().snapshot.sg0.progress_pct, 80);
  h.take('/api/state').resolve({ sg0: { progress_pct: 10 } });
  await h.flush();
  assert.equal(h.c.status().snapshot.sg0.progress_pct, 80);
  h.take('/api/system').resolve({ debug_enabled: true });
  h.take('/api/review').resolve([]);
  await h.flush();
  assert.equal(h.c.status().sys.debug_enabled, true);
  h.c.close(); assert(h.stopped());
});
test('poll fallback retains last state through grace, recovers, and stops on removal', async () => {
  const h = await harness();
  h.take('/api/state').resolve({ sg0: { progress_pct: 20 } });
  await h.flush();
  h.handlers.error();
  h.tick(6000);
  h.take('/api/state').reject(Error('offline'));
  await h.flush();
  assert.equal(h.c.status().offline, false);
  assert.equal(h.c.status().snapshot.sg0.progress_pct, 20);
  h.tick(12000);
  h.take('/api/state').reject(Error('offline')); await h.flush();
  assert.equal(h.c.status().offline, true);
  h.handlers.state({ sg0: { progress_pct: 90 } });
  assert.equal(h.c.status().offline, false);
  h.c.close();
  h.take('/api/system').resolve({ debug_enabled: true }); await h.flush();
  assert.equal(h.c.status().sys.debug_enabled, undefined);
  assert(h.stopped());
});


test('badge and page share namespaced drives and exclude sustained offline activity', async () => {
  const h = await harness();
  const { connections, driveState, busyDriveCount } = h.module;
  connections.set('', { status: () => ({ peer: { id: '' }, snapshot: { sg0: { status: 'ripping' }, _mux: { status: 'ripping' } } }) });
  connections.set('p123', h.c);
  h.handlers.state({ sg0: { status: 'ripping' } });
  assert.equal(Object.keys(driveState()).length, 2);
  assert.equal(busyDriveCount(driveState()), 2);
  h.handlers.error(); h.tick(6000);
  assert.equal(busyDriveCount(driveState()), 2);
  h.tick(12000);
  assert.equal(busyDriveCount(driveState()), 1);
  h.handlers.state({ sg0: { status: 'idle' } });
  assert.equal(busyDriveCount(driveState()), 1);
  connections.delete('p123');
  assert.equal(Object.keys(driveState()).length, 1);
  h.c.close();
});
