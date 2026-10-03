// The Library listing, shared by the Library and Remux pages: fetched once,
// then refetched (at most every 1.5 s) when an SSE frame says the index or
// the queue moved. The running job's progress comes straight off the frame,
// so progress never waits on a refetch.

import { api } from './ui.js';
import { subscribe } from './bus.js';

let data = null;
let gens = null;
let inflight = null;
let lastFetch = 0;
let timer = null;
let dirty = false;
let latestFrame = null;
const subs = new Set();

function applyLive(d) {
  const f = latestFrame;
  if (!d || !f) return;
  d.live = f.running;
  d.indexing = f.indexing;
  if (d.queue) { d.queue.paused = f.paused; d.queue.queued = f.queued; }
  if (f.audits) d.audits = f.audits;
  if ('hold' in f) d.hold = f.hold;
  if (f.folders) d.folders = f.folders;
}

async function fetchNow() {
  if (inflight) return inflight;
  dirty = false;
  lastFetch = Date.now();
  const controller = new AbortController();
  const timeout = setTimeout(() => controller.abort(), 15000);
  const frameAtStart = latestFrame;
  inflight = api('GET', '/api/library', undefined, controller.signal)
    .then(d => {
      data = d;
      // A slow response must not rewind progress received while it loaded.
      if (latestFrame !== frameAtStart) applyLive(data);
      subs.forEach(f => f(d, null));
      return d;
    })
    .catch(e => { dirty = true; subs.forEach(f => f(data, e)); })
    .finally(() => {
      clearTimeout(timeout);
      inflight = null;
      if (dirty && subs.size) refresh();
    });
  return inflight;
}

/** Refetch soon, never more than once per 1.5 s. */
export function refresh() {
  dirty = true;
  if (timer || inflight) return;
  const wait = Math.max(0, 1500 - (Date.now() - lastFetch));
  timer = setTimeout(() => { timer = null; fetchNow(); }, wait);
}

/** Refetch now (after the user did something). */
export function refreshNow() { dirty = true; clearTimeout(timer); timer = null; return fetchNow(); }

subscribe('reconnect', () => { if (subs.size) refresh(); });

subscribe('library', (f) => {
  latestFrame = f;
  const g = f.queue_generation + ':' + f.index_generation + ':' + f.audit_generation;
  if (g !== gens) { gens = g; if (subs.size) refresh(); }
  if (data) {
    applyLive(data);
    subs.forEach(fn => fn(data, null, true));
  }
});

/** Watch the listing: `f(data, error, liveOnly)`. Returns the unsubscribe. */
export function watch(f) {
  subs.add(f);
  if (data) f(data, null);
  fetchNow();
  return () => subs.delete(f);
}

/** While the first scan runs, poll until it lands. */
export function scanning() { return !data || data.scanning; }
