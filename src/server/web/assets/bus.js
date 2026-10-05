// One EventSource shared by every page: `/events` carries the rip state each
// second (unnamed frames) and `event: library` frames when the Library moves.

const listeners = { state: new Set(), library: new Set(), reconnect: new Set() };
export const live = { state: null, library: null };
let disconnect = null;

function publish(kind, value) {
  listeners[kind].forEach(f => {
    try { f(value); } catch (e) { console.error('Live update failed:', e); }
  });
}

/** The same transport for local and proxied remote /events endpoints. */
export function openStream(url, handlers) {
  let es, retry, watchdog, closed = false;
  const start = () => {
    if (closed) return;
    clearTimeout(retry); clearTimeout(watchdog);
    if (es) es.close();
    const source = new EventSource(url);
    es = source;
    const heartbeat = () => {
      clearTimeout(watchdog);
      watchdog = setTimeout(() => { handlers.error?.(); start(); }, 15000);
    };
    heartbeat();
    source.onopen = () => { if (es === source && !closed) handlers.reconnect?.(); };
    const receive = (kind, e) => {
      if (es !== source || closed) return;
      let value;
      try { value = JSON.parse(e.data); } catch (_) { return; }
      heartbeat();
      handlers[kind]?.(value);
    };
    source.onmessage = e => receive('state', e);
    source.addEventListener('library', e => receive('library', e));
    source.onerror = () => {
      if (es !== source || closed) return;
      clearTimeout(watchdog); source.close(); es = null;
      handlers.error?.();
      retry = setTimeout(start, 2000);
    };
  };
  start();
  return () => { closed = true; clearTimeout(retry); clearTimeout(watchdog); es?.close(); };
}

export function connect() {
  disconnect?.();
  disconnect = openStream('/events', {
    state(value) { live.state = value; publish('state', value); },
    library(value) { live.library = value; publish('library', value); },
    reconnect() { publish('reconnect'); }
  });
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
