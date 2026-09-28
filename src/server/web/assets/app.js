// The app shell: one SSE connection shared by every page, a route table of
// page modules, the theme toggle, and the header chip that follows the
// running remux and opens the Console.

import { $, $$, fill, api, ICON } from './ui.js';
import { openConsole } from './console.js';
import { live, connect, subscribe, publishState } from './bus.js';

const ROUTES = {
  '/library': () => import('./library.js'),
  '/remux': () => import('./remux.js'),
  '/ripper': () => import('./ripper.js'),
  '/settings': () => import('./settings.js'),
  '/system': () => import('./system.js'),
};
const DEFAULT_ROUTE = '/library';

// ── Router ─────────────────────────────────────────────────────────────────

let current = null;
let renderToken = 0;

function routeOf(path) {
  return ROUTES[path] ? path : DEFAULT_ROUTE;
}

async function render(path) {
  const token = ++renderToken;
  const route = routeOf(path);
  if (location.pathname !== route) history.replaceState(null, '', route + location.search + location.hash);
  if (current && current.cleanup) current.cleanup.forEach(f => { try { f(); } catch (e) { /* next */ } });
  $$('.nav-links a').forEach(a => {
    if (a.getAttribute('href') === route) a.setAttribute('aria-current', 'page');
    else a.removeAttribute('aria-current');
  });
  const view = $('#view');
  view.innerHTML = '';
  const mod = (await ROUTES[route]()).default;
  // A newer navigation started while this module loaded: let it win.
  if (token !== renderToken) return;
  const ctx = {
    cleanup: [],
    onState(f) { ctx.cleanup.push(subscribe('state', f)); if (live.state) f(live.state); },
    onLibrary(f) { ctx.cleanup.push(subscribe('library', f)); },
    every(ms, f) { const t = setInterval(f, ms); ctx.cleanup.push(() => clearInterval(t)); },
  };
  current = ctx;
  ctx.stale = () => token !== renderToken;
  document.title = mod.title + ' · freemkv library';
  await mod.mount(view, ctx);
  if (token !== renderToken) ctx.cleanup.forEach(f => { try { f(); } catch (e) { /* next */ } });
}

export function navigate(path) {
  if (location.pathname + location.hash === path) return;
  history.pushState(null, '', path);
  render(location.pathname).then(() => {
    const t = location.hash && document.getElementById(location.hash.slice(1));
    if (!t) window.scrollTo(0, 0);
  });
}

document.addEventListener('click', (e) => {
  const a = e.target.closest('a[data-link]');
  if (!a || e.metaKey || e.ctrlKey || e.shiftKey || e.button !== 0) return;
  e.preventDefault();
  navigate(a.getAttribute('href'));
});
window.addEventListener('popstate', () => render(location.pathname));

// ── Theme: remembered, defaulting to the OS ───────────────────────────────

function paintThemeButton() {
  const dark = document.documentElement.dataset.theme === 'dark';
  const b = $('#theme');
  b.innerHTML = dark ? ICON.sun : ICON.moon;
  b.setAttribute('aria-label', dark ? 'Switch to light' : 'Switch to dark');
}
$('#theme').addEventListener('click', () => {
  const next = document.documentElement.dataset.theme === 'dark' ? 'light' : 'dark';
  document.documentElement.dataset.theme = next;
  try { localStorage.setItem('theme', next); } catch (e) { /* private mode */ }
  paintThemeButton();
});
paintThemeButton();

// ── Header chip: the running remux; click for the Console ─────────────────

function paintChip(running, queued) {
  const chip = $('#jobchip');
  if (!running) {
    chip.hidden = true;
    return;
  }
  chip.hidden = false;
  const pct = running.pct;
  $('.t', chip).textContent = 'Remuxing ' + running.title;
  fill($('.mini i', chip), pct);
  $('.pct', chip).textContent = pct == null ? '' : pct.toFixed(0) + '%';
  chip.title = 'Open the console' + (queued ? ' · ' + queued + ' queued after this' : '');
}
$('#jobchip').addEventListener('click', () => openConsole());
subscribe('library', (f) => paintChip(f.running, f.queued));
api('GET', '/api/library/console').then(c => paintChip(c.running, c.queue && c.queue.queued)).catch(() => {});

// ── Browser notifications for finished and failed rips ─────────────────────

const lastStatus = {};
function notifyRips(state) {
  Object.keys(state).filter(k => !k.startsWith('_')).forEach(dev => {
    const s = state[dev];
    const prev = lastStatus[dev];
    lastStatus[dev] = s.status;
    if (!prev || prev === s.status || typeof Notification === 'undefined' || Notification.permission !== 'granted') return;
    const name = s.tmdb_title || s.disc_name || dev;
    try {
      if (s.status === 'done') new Notification('freemkv', { body: name + ' — rip complete', icon: s.tmdb_poster || '/favicon.svg' });
      if (s.status === 'error') new Notification('freemkv', { body: name + ' — ' + (s.last_error || 'rip failed'), icon: s.tmdb_poster || '/favicon.svg' });
    } catch (e) { /* notifications unavailable */ }
  });
}
subscribe('state', notifyRips);
if (typeof Notification !== 'undefined' && Notification.permission === 'default') {
  // Asked once, on the first click anywhere: browsers ignore an unprompted request.
  document.addEventListener('click', () => Notification.requestPermission().catch(() => {}), { once: true });
}

// ── Start ──────────────────────────────────────────────────────────────────

fetch('/api/state', { cache: 'no-store' })
  .then(r => r.json())
  .then(publishState)
  .catch(() => {})
  .finally(connect);
render(location.pathname);
