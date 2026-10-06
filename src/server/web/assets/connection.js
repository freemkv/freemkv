// A Library connection has the same state, details and actions at any API base.
import { api } from './ui.js';
import { subscribe, live } from './bus.js';

export function libraryConnection(peer, changed) {
  const grace = 15000, requestLimit = 10000;
  const base = peer.id ? '/api/peers/' + peer.id : '';
  const value = { peer, snapshot: {}, sys: {}, reviews: [], error: null };
  let closed = false, lastState = 0, streamVersion = 0, failedSince = null;
  const pending = new Set(), errors = new Map(), abort = new AbortController();
  const notify = () => { if (!closed) changed(); };
  const failed = () => { failedSince ??= Date.now(); notify(); };
  const state = snapshot => {
    if (!snapshot || typeof snapshot !== 'object' || Array.isArray(snapshot)) return false;
    const recovered = failedSince != null;
    value.snapshot = snapshot; lastState = Date.now(); failedSince = null;
    if (recovered) { errors.clear(); details(); }
    notify(); return true;
  };
  const handlers = { state(snapshot) { if (state(snapshot)) streamVersion++; }, error: failed };
  // Remote snapshots use short polls: one SSE per peer would exhaust the
  // browser's per-origin HTTP/1 connection pool and block controls/navigation.
  // The local connection reuses the application's existing stream.
  const stop = peer.id ? () => {} : (() => {
    const off = subscribe('state', handlers.state);
    if (live.state) handlers.state(live.state);
    return off;
  })();
  async function load(path, field) {
    if (closed || pending.has(path)) return;
    pending.add(path);
    const version = streamVersion;
    const request = new AbortController();
    const cancel = () => request.abort();
    abort.signal.addEventListener('abort', cancel, { once: true });
    const timeout = setTimeout(cancel, requestLimit);
    try {
      const result = await api('GET', base + path, undefined, request.signal);
      if (closed) return;
      if (field === 'snapshot') {
        if (version === streamVersion && !state(result)) throw new Error('Invalid drive state');
      } else {
        value[field] = result || (field === 'reviews' ? [] : {});
        errors.delete(path);
      }
    } catch (e) {
      if (closed) return;
      if (field === 'snapshot') { if (version === streamVersion) failed(); }
      else errors.set(path, { message: e.message, since: errors.get(path)?.since ?? Date.now() });
    } finally {
      clearTimeout(timeout); abort.signal.removeEventListener('abort', cancel);
      pending.delete(path); notify();
    }
  }
  const details = () => failedSince == null
    ? Promise.all([load('/api/system', 'sys'), load('/api/review', 'reviews')]) : Promise.resolve();
  const tick = () => {
    if (peer.id || Date.now() - lastState >= 3000) load('/api/state', 'snapshot');
    notify();
  };
  const timer = setInterval(tick, 3000), detailTimer = setInterval(details, 5000);
  // Defer initial notification until the caller has registered this connection.
  queueMicrotask(() => { if (!closed) { tick(); details(); } });
  return {
    value,
    status() {
      return { ...value, offline: failedSince != null && Date.now() - failedSince >= grace,
        error: failedSince != null && Date.now() - failedSince >= grace ? 'Connection unavailable' : null,
        reconnecting: failedSince != null,
        // Connection failures have their own grace period and offline banner.
        detailError: failedSince != null ? '' : [...new Set([...errors.values()].filter(e => Date.now() - e.since >= grace).map(e => e.message))].join('; ') };
    },
    refresh: details,
    close() { closed = true; stop(); abort.abort(); clearInterval(timer); clearInterval(detailTimer); }
  };
}

// App-wide connections keep navigation and the Drives page on one snapshot,
// including while another page is open.
export const connections = new Map();
const listeners = new Set();
let discoveryTimer, discovering = false;
function changed() { for (const f of listeners) f(); }
export function subscribeConnections(fn) {
  listeners.add(fn); fn();
  return () => listeners.delete(fn);
}
export function driveState() {
  const state = {};
  for (const c of connections.values()) {
    const r = c.status();
    for (const [dev, snapshot] of Object.entries(r.snapshot)) {
      if (!/^(?:ioreg:)?[a-zA-Z0-9]+$/.test(dev) || !snapshot || typeof snapshot !== 'object') continue;
      const key = r.peer.id ? r.peer.id + ':' + dev : dev;
      state[key] = { ...snapshot, _owner: r.peer.id ? r.peer.name : '', _offline: !!r.offline };
    }
  }
  return state;
}
export function busyDriveCount(state) {
  return Object.values(state).filter(s => !s._offline && ['ripping', 'scanning', 'detecting'].includes(s.status)).length;
}
export function startConnections() {
  if (discoveryTimer) return;
  connections.set('', libraryConnection({ id: '', name: location.hostname }, changed));
  const discover = async () => {
    if (discovering) return;
    discovering = true;
    try {
      const peers = await api('GET', '/api/peers');
      for (const [id, c] of connections) {
        if (id && !peers.some(p => p.id === id && p.url === c.value.peer.url)) {
          c.close(); connections.delete(id);
        }
      }
      for (const peer of peers) {
        if (!connections.has(peer.id)) connections.set(peer.id, libraryConnection(peer, changed));
        else connections.get(peer.id).value.peer = peer;
      }
      changed();
    } catch (_) { /* Keep existing connections through a discovery failure. */ }
    finally { discovering = false; }
  };
  discoveryTimer = setInterval(discover, 3000);
  discover();
}
