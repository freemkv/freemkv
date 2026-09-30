// One EventSource shared by every page: `/events` carries the rip state each
// second (unnamed frames) and `event: library` frames when the Library moves.

const listeners = { state: new Set(), library: new Set(), reconnect: new Set() };
export const live = { state: null, library: null };
let es = null;
let reconnectTimer = null;
let watchdog = null;

function publish(kind, value) {
  listeners[kind].forEach(f => {
    try { f(value); } catch (e) { console.error('Live update failed:', e); }
  });
}

export function connect() {
  clearTimeout(reconnectTimer);
  clearTimeout(watchdog);
  if (es) es.close();
  const source = new EventSource('/events');
  es = source;
  // The server sends state every second, even when no work is running.
  // Half-open connections do not always fire onerror (sleep, proxy, network).
  const heartbeat = () => {
    clearTimeout(watchdog);
    watchdog = setTimeout(connect, 15000);
  };
  heartbeat();
  source.onopen = () => { if (es === source) publish('reconnect'); };
  source.onmessage = (e) => {
    if (es !== source) return;
    heartbeat();
    try { live.state = JSON.parse(e.data); } catch (x) { return; }
    publish('state', live.state);
  };
  source.addEventListener('library', (e) => {
    if (es !== source) return;
    heartbeat();
    try { live.library = JSON.parse(e.data); } catch (x) { return; }
    publish('library', live.library);
  });
  source.onerror = () => {
    if (es !== source) return;
    clearTimeout(watchdog);
    source.close();
    es = null;
    reconnectTimer = setTimeout(connect, 2000);
  };
}

/** Listen for `state`, `library` or `reconnect`; returns the unsubscribe. */
export function subscribe(kind, f) {
  listeners[kind].add(f);
  return () => listeners[kind].delete(f);
}

/** Hand a state snapshot fetched outside the stream to every listener. */
export function publishState(s) {
  live.state = s;
  publish('state', s);
}
