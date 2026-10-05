// A Library connection has the same state, details and actions at any API base.
import { api } from './ui.js';
import { openStream, subscribe, live } from './bus.js';

export function libraryConnection(peer, changed) {
  const base = peer.id ? '/api/peers/' + peer.id : '';
  const value = { peer, snapshot: {}, sys: {}, reviews: [], error: null };
  let closed = false, lastState = 0, streamVersion = 0, failedSince = null;
  const pending = new Set(), errors = new Map(), abort = new AbortController();
  const notify = () => { if (!closed) changed(); };
  const failed = () => { failedSince ??= Date.now(); notify(); };
  const state = snapshot => {
    if (!snapshot || typeof snapshot !== 'object' || Array.isArray(snapshot)) return false;
    value.snapshot = snapshot; lastState = Date.now(); failedSince = null; notify(); return true;
  };
  const handlers = { state(snapshot) { if (state(snapshot)) streamVersion++; }, error: failed };
  // Reuse the application's existing local stream; all connection behavior below is shared.
  const stop = peer.id ? openStream(base + '/events', handlers) : (() => {
    const off = subscribe('state', handlers.state);
    if (live.state) handlers.state(live.state);
    return off;
  })();
  async function load(path, field) {
    if (closed || pending.has(path)) return;
    pending.add(path);
    const version = streamVersion;
    try {
      const result = await api('GET', base + path, undefined, abort.signal);
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
      else errors.set(path, e.message);
    } finally { pending.delete(path); notify(); }
  }
  const details = () => Promise.all([load('/api/system', 'sys'), load('/api/review', 'reviews')]);
  const tick = () => {
    if (Date.now() - lastState > 3000) load('/api/state', 'snapshot');
    notify();
  };
  const timer = setInterval(tick, 3000), detailTimer = setInterval(details, 5000);
  // Defer initial notification until the caller has registered this connection.
  queueMicrotask(() => { if (!closed) { tick(); details(); } });
  return {
    value,
    status() {
      return { ...value, offline: failedSince != null && Date.now() - failedSince >= 15000,
        error: failedSince != null && Date.now() - failedSince >= 15000 ? 'Connection unavailable' : null,
        reconnecting: failedSince != null, detailError: [...errors.values()].join('; ') };
    },
    refresh: details,
    close() { closed = true; stop(); abort.abort(); clearInterval(timer); clearInterval(detailTimer); }
  };
}
