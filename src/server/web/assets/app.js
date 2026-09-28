// The app shell: one SSE connection shared by every page, a route table of
// page modules, the theme toggle, and the header chip that follows the
// running remux and opens the Console.

import { $, $$, api, ICON } from './ui.js';
import { live, connect, subscribe, publishState } from './bus.js';

const ROUTES = {
  '/library': () => import('./library.js'),
  '/remux': () => import('./remux.js'),
  '/drives': () => import('./ripper.js'),
  '/settings': () => import('./settings.js'),
  '/system': () => import('./system.js'),
};
const DEFAULT_ROUTE = '/library';

// ── Router ─────────────────────────────────────────────────────────────────

let current = null;
let renderToken = 0;

// Old bookmarks: the Drives page was called Ripper.
const RENAMED = { '/ripper': '/drives' };

function routeOf(path) {
  path = RENAMED[path] || path;
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

// ── Activity: a small badge on the tab that owns the work ──────────────────
// Drives shows how many drives are busy; Remux a dot while a remux runs. Each
// page shows its own jobs' progress; nothing picks one job for the header.

function badge(href, text, label) {
  const a = $('.nav-links a[href="' + href + '"]');
  if (!a) return;
  let b = a.querySelector('.nav-badge');
  if (!text) { if (b) b.remove(); a.removeAttribute('title'); return; }
  if (!b) { b = document.createElement('span'); b.className = 'nav-badge'; a.appendChild(b); }
  // "●" is drawn as a plain dot; a number as a count.
  b.classList.toggle('dot', text === '●');
  const shown = text === '●' ? '' : text;
  if (b.textContent !== shown) b.textContent = shown;
  b.setAttribute('aria-label', label);
  a.title = label;
}
const RIPPING = ['ripping', 'scanning', 'detecting'];
subscribe('state', (s) => {
  const busy = Object.keys(s).filter(k => !k.startsWith('_') && RIPPING.includes(s[k].status)).length;
  badge('/drives', busy ? String(busy) : '', busy + (busy === 1 ? ' drive' : ' drives') + ' busy');
});
const paintRemux = (running) => badge('/remux', running ? '●' : '', running ? 'Remuxing ' + running.title : '');
subscribe('library', (f) => paintRemux(f.running));
api('GET', '/api/library/console').then(c => paintRemux(c.running)).catch(() => {});

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
