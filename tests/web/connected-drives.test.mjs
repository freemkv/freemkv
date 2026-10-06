// Run with: node --experimental-vm-modules --test tests/web/*.test.mjs
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import vm from 'node:vm';

async function harness(id = 'p123') {
  let now = 100000, handlers, stopped = false;
  const timers = new Map(), deadlines = new Map(), requests = [];
  const context = vm.createContext({
    AbortController, queueMicrotask, Date: { now: () => now },
    setInterval(fn, ms) { timers.set(fn, ms); return fn; },
    clearInterval(fn) { timers.delete(fn); },
    setTimeout(fn, ms) { deadlines.set(fn, now + ms); return fn; },
    clearTimeout(fn) { deadlines.delete(fn); },
  });
  const module = new vm.SourceTextModule(await readFile(new URL('../../src/server/web/assets/connection.js', import.meta.url), 'utf8'), { context });
  await module.link(name => name === './ui.js'
    ? new vm.SyntheticModule(['api'], function () {
      this.setExport('api', (method, url, body, signal) => new Promise((resolve, reject) => {
        requests.push({ url, resolve, reject });
        signal?.addEventListener('abort', () => { stopped = true; reject(Error('aborted')); });
      }));
    }, { context })
    : new vm.SyntheticModule(['openStream', 'subscribe', 'live'], function () {
      this.setExport('openStream', () => { throw Error('Remote streams exhaust the HTTP/1 connection pool'); });
      this.setExport('subscribe', (kind, fn) => { handlers = { state: fn }; return () => { stopped = true; }; });
      this.setExport('live', { state: null });
    }, { context }));
  await module.evaluate();
  const c = module.namespace.libraryConnection({ id, name: 'Library' }, () => {});
  const flush = async () => { for (let i = 0; i < 10; i++) await Promise.resolve(); };
  await flush();
  return { module: module.namespace, c, requests, handlers, flush, stopped: () => stopped,
    expire(ms) { now += ms; for (const [fn, at] of deadlines) if (at <= now) { deadlines.delete(fn); fn(); } },
    detailTick() { for (const [fn, ms] of timers) if (ms === 5000) fn(); },
    tick(ms) { now += ms; for (const [fn, interval] of timers) if (interval === 3000) fn(); },
    take(path) { const i = requests.findIndex(r => r.url.endsWith(path)); assert(i >= 0, path); return requests.splice(i, 1)[0]; }
  };
}
test('local stream wins over a slow poll and details', async () => {
  const h = await harness('');
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
  h.tick(3000);
  h.take('/api/state').reject(Error('offline'));
  await h.flush();
  assert.equal(h.c.status().offline, false);
  assert.equal(h.c.status().snapshot.sg0.progress_pct, 20);
  h.tick(15000);
  h.take('/api/state').reject(Error('offline')); await h.flush();
  assert.equal(h.c.status().offline, true);
  h.tick(3000);
  h.take('/api/state').resolve({ sg0: { progress_pct: 90 } }); await h.flush();
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
  h.take('/api/state').resolve({ sg0: { status: 'ripping' } }); await h.flush();
  assert.equal(Object.keys(driveState()).length, 2);
  assert.equal(busyDriveCount(driveState()), 2);
  h.tick(3000);
  h.take('/api/state').reject(Error('offline')); await h.flush();
  assert.equal(busyDriveCount(driveState()), 2);
  h.tick(15000);
  assert.equal(busyDriveCount(driveState()), 1);
  h.take('/api/state').resolve({ sg0: { status: 'idle' } }); await h.flush();
  assert.equal(busyDriveCount(driveState()), 1);
  connections.delete('p123');
  assert.equal(Object.keys(driveState()).length, 1);
  h.c.close();
});

// Exercise the supported maximum without reserving any remote SSE sockets.
test('eight remote Libraries poll and release requests on removal', async () => {
  const h = await harness();
  const peers = [h.c, ...Array.from({ length: 7 }, (_, i) =>
    h.module.libraryConnection({ id: 'p' + (i + 2), name: 'Remote' }, () => {}))];
  await h.flush();
  assert.equal(h.requests.filter(r => r.url.endsWith('/api/state')).length, 8);
  for (const r of h.requests.splice(0)) r.resolve(r.url.endsWith('/api/review') ? [] : {});
  await h.flush();
  h.tick(3000);
  assert.equal(h.requests.length, 8);
  for (const c of peers) c.close();
  await h.flush();
  h.requests.length = 0;
  h.tick(3000);
  assert.equal(h.requests.length, 0);
});


test('detail failures are deduplicated and connection outages use only the offline warning', async () => {
  const h = await harness();
  h.take('/api/state').resolve({ sg0: { status: 'idle' } });
  h.take('/api/system').reject(Error('Remote Library is offline or timed out'));
  h.take('/api/review').reject(Error('Remote Library is offline or timed out'));
  await h.flush();
  assert.equal(h.c.status().detailError, '', 'brief detail failures stay quiet');
  h.tick(15000);
  assert.equal(h.c.status().detailError, 'Remote Library is offline or timed out');
  h.tick(3000);
  h.take('/api/state').reject(Error('offline')); await h.flush();
  assert.equal(h.c.status().detailError, '');
  assert.equal(h.c.status().error, null, 'connection failures respect the grace period');
  h.tick(15000);
  assert.equal(h.c.status().offline, true);
  assert.equal(h.c.status().detailError, '');
  h.take('/api/state').resolve({ sg0: { status: 'idle' } });
  await h.flush();
  h.c.refresh();
  h.take('/api/system').resolve({});
  h.take('/api/review').resolve([]); await h.flush();
  assert.equal(h.c.status().offline, false);
  assert.equal(h.c.status().detailError, '');
  h.c.close();
});

test('hung polls expire and retry instead of permanently holding the connection', async () => {
  const h = await harness();
  h.expire(10000); await h.flush();
  assert.equal(h.c.status().reconnecting, true);
  assert.equal(h.c.status().offline, false);
  h.requests.length = 0;
  h.tick(3000);
  h.take('/api/state').resolve({ sg0: { status: 'ripping', progress_pct: 45 } });
  await h.flush();
  assert.equal(h.c.status().reconnecting, false);
  assert.equal(h.c.status().snapshot.sg0.progress_pct, 45);
  assert.equal(h.requests.filter(r => r.url.endsWith('/api/system')).length, 1);
  assert.equal(h.requests.filter(r => r.url.endsWith('/api/review')).length, 1);
  h.c.close();
});

test('outages poll state only, then automatically reload details on recovery', async () => {
  const h = await harness();
  for (const r of h.requests.splice(0)) r.reject(Error('offline'));
  await h.flush();
  h.detailTick(); await h.flush();
  assert.equal(h.requests.length, 0);
  h.tick(3000);
  h.take('/api/state').resolve({ sg0: { status: 'idle' } });
  await h.flush();
  h.take('/api/system').resolve({ debug_enabled: true });
  h.take('/api/review').resolve([]); await h.flush();
  assert.equal(h.c.status().sys.debug_enabled, true);
  assert.equal(h.c.status().detailError, '');
  h.c.close();
});

test('a brief details failure keeps cached details and never flashes a warning', async () => {
  const h = await harness();
  h.take('/api/state').resolve({});
  h.take('/api/system').resolve({ debug_enabled: true });
  h.take('/api/review').resolve([]); await h.flush();
  h.detailTick();
  h.take('/api/system').reject(Error('temporary failure'));
  h.take('/api/review').resolve([]); await h.flush();
  assert.equal(h.c.status().sys.debug_enabled, true);
  assert.equal(h.c.status().detailError, '');
  h.tick(3000); h.take('/api/state').resolve({});
  h.detailTick(); h.take('/api/system').resolve({ debug_enabled: false });
  h.take('/api/review').resolve([]); await h.flush();
  h.tick(15000);
  assert.equal(h.c.status().detailError, '');
  assert.equal(h.c.status().sys.debug_enabled, false);
  h.c.close();
});
