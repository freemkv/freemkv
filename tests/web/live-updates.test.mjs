// Run with: node --experimental-vm-modules --test tests/web/*.test.mjs
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import vm from 'node:vm';

async function harness() {
  let now = 10000, nextId = 0;
  const timers = new Map(), requests = [], sources = [], errors = [];
  class EventSource {
    constructor() { this.handlers = {}; sources.push(this); }
    addEventListener(kind, fn) { this.handlers[kind] = fn; }
    close() { this.closed = true; }
    send(kind, value) {
      (kind === 'state' ? this.onmessage : this.handlers[kind])({ data: JSON.stringify(value) });
    }
  }
  const context = vm.createContext({
    AbortController, EventSource,
    console: { error: (...args) => errors.push(args) },
    Date: { now: () => now },
    setTimeout: (fn, ms) => { const id = ++nextId; timers.set(id, { fn, at: now + ms }); return id; },
    clearTimeout: id => timers.delete(id),
  });
  const modules = new Map();
  const ui = new vm.SyntheticModule(['api'], function () {
    this.setExport('api', (_method, _url, _body, signal) => new Promise((resolve, reject) => {
      requests.push({ resolve, reject });
      signal?.addEventListener('abort', () => reject(new Error('timeout')));
    }));
  }, { context });
  async function load(name) {
    if (name === './ui.js') return ui;
    if (!modules.has(name)) {
      const source = await readFile(new URL('../../src/server/web/assets/' + name, import.meta.url), 'utf8');
      const mod = new vm.SourceTextModule(source, { context });
      modules.set(name, mod);
      await mod.link(load);
    }
    return modules.get(name);
  }
  const lib = await load('./libdata.js');
  await lib.evaluate();
  const bus = modules.get('./bus.js').namespace;
  const flush = async () => { for (let i = 0; i < 20; i++) await Promise.resolve(); };
  async function advance(ms) {
    const end = now + ms;
    while (true) {
      const entry = [...timers].filter(([, t]) => t.at <= end).sort((a, b) => a[1].at - b[1].at)[0];
      if (!entry) break;
      now = entry[1].at;
      timers.delete(entry[0]);
      entry[1].fn();
      await flush();
    }
    now = end;
    await flush();
  }
  bus.connect();
  return { lib: lib.namespace, bus, sources, requests, errors, advance, flush };
}
const frame = (generation, job = 2, pct = 0) => ({
  queue_generation: generation, index_generation: 1, audit_generation: 1,
  running: job == null ? null : { job_id: job, pct }, queued: 1, paused: false,
});
const listing = job => ({ rows: [{ job: { id: job } }], live: { job_id: job, pct: 100 }, queue: {} });

test('completion during a slow listing fetch still loads the next job', async () => {
  const h = await harness();
  let shown;
  h.lib.watch((d, err) => { if (!err) shown = d; });
  h.sources[0].send('library', frame(1, 1, 100));
  await h.advance(2000);
  h.sources[0].send('library', frame(2, 2, 5));
  h.requests[0].resolve(listing(1));
  await h.flush();
  assert.equal(shown.live.job_id, 2, 'old response cannot rewind the live job');
  await h.advance(0);
  assert.equal(h.requests.length, 2, 'change during the request triggers another fetch');
  h.requests[1].resolve(listing(2));
  await h.flush();
  assert.equal(shown.rows[0].job.id, 2);
});

test('failed listing request retries without another generation change', async () => {
  const h = await harness();
  h.lib.watch(() => {});
  h.requests[0].reject(new Error('network'));
  await h.flush();
  await h.advance(1500);
  assert.equal(h.requests.length, 2);
});

test('a hung listing request times out and retries', async () => {
  const h = await harness();
  h.lib.watch(() => {});
  await h.advance(15000);
  assert.equal(h.requests.length, 2);
});

test('a silent stream reconnects and resynchronizes the listing', async () => {
  const h = await harness();
  h.lib.watch(() => {});
  h.requests[0].resolve(listing(1));
  await h.flush();
  await h.advance(15000);
  assert.equal(h.sources[0].closed, true);
  assert.equal(h.sources.length, 2);
  h.sources[1].onopen();
  await h.advance(0);
  assert.equal(h.requests.length, 2);
});

test('heartbeats keep a healthy stream open; errors reconnect once', async () => {
  const h = await harness();
  for (let i = 0; i < 20; i++) {
    await h.advance(1000);
    h.sources[0].send('state', {});
  }
  assert.equal(h.sources.length, 1);
  h.sources[0].onerror();
  await h.advance(2000);
  assert.equal(h.sources.length, 2);
  h.sources[0].onerror(); // Late callbacks from a replaced stream are ignored.
  await h.advance(2000);
  assert.equal(h.sources.length, 2);
});

test('one broken subscriber cannot freeze other live views', async () => {
  const h = await harness();
  let received = 0;
  h.bus.subscribe('library', () => { throw new Error('render failed'); });
  h.bus.subscribe('library', () => received++);
  h.sources[0].send('library', frame(1));
  h.sources[0].send('library', frame(2));
  assert.equal(received, 2);
  assert.equal(h.errors.length, 2);
});
