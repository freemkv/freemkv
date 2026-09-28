// One EventSource shared by every page: `/events` carries the rip state each
// second (unnamed frames) and `event: library` frames when the Library moves.

const listeners = { state: new Set(), library: new Set() };
export const live = { state: null, library: null };
let es = null;

export function connect() {
  if (es) es.close();
  es = new EventSource('/events');
  es.onmessage = (e) => {
    try { live.state = JSON.parse(e.data); } catch (x) { return; }
    listeners.state.forEach(f => f(live.state));
  };
  es.addEventListener('library', (e) => {
    try { live.library = JSON.parse(e.data); } catch (x) { return; }
    listeners.library.forEach(f => f(live.library));
  });
  es.onerror = () => { es.close(); es = null; setTimeout(connect, 2000); };
}

/** Listen for `state` or `library` frames; returns the unsubscribe. */
export function subscribe(kind, f) {
  listeners[kind].add(f);
  return () => listeners[kind].delete(f);
}

/** Hand a state snapshot fetched outside the stream to every listener. */
export function publishState(s) {
  live.state = s;
  listeners.state.forEach(f => f(s));
}
