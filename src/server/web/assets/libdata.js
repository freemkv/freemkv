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
const subs = new Set();

async function fetchNow() {
  if (inflight) return inflight;
  lastFetch = Date.now();
  inflight = api('GET', '/api/library')
    .then(d => { data = d; subs.forEach(f => f(d, null)); return d; })
    .catch(e => { subs.forEach(f => f(data, e)); })
    .finally(() => { inflight = null; });
  return inflight;
}

/** Refetch soon, never more than once per 1.5 s. */
export function refresh() {
  if (timer) return;
  const wait = Math.max(0, 1500 - (Date.now() - lastFetch));
  timer = setTimeout(() => { timer = null; fetchNow(); }, wait);
}

/** Refetch now (after the user did something). */
export function refreshNow() { clearTimeout(timer); timer = null; return fetchNow(); }

subscribe('library', (f) => {
  const g = f.queue_generation + ':' + f.index_generation + ':' + f.audit_generation;
  if (g !== gens) { gens = g; if (subs.size) refresh(); }
  if (data) {
    data.live = f.running;
    data.indexing = f.indexing;
    if (data.queue) { data.queue.paused = f.paused; data.queue.queued = f.queued; }
    if (f.audits) data.audits = f.audits;
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
