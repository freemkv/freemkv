// Shared pieces every page uses: escaping, the API wrapper (every action gets
// visible feedback and surfaces its error), toasts, two-step confirm buttons,
// modals, the terminal, and number formatting.

export function esc(s){if(s==null)return'';return String(s).replace(/&/g,'&amp;').replace(/</g,'&lt;').replace(/>/g,'&gt;').replace(/"/g,'&quot;').replace(/'/g,'&#39;')}
/* Bare https:// URLs become links. Matched on the RAW text and each piece
   escaped separately, so a URL next to a quote cannot swallow its entity. */
export function escLinks(s){if(s==null)return'';s=String(s);let out='',last=0,m;const re=/https:\/\/[^\s<>"']+/g;
  while((m=re.exec(s))!==null){let u=m[0],tail='';const t=u.match(/[.,;:)\]]+$/);if(t){tail=t[0];u=u.slice(0,-tail.length)}
    out+=esc(s.slice(last,m.index))+'<a href="'+esc(u)+'" target="_blank" rel="noopener noreferrer" style="color:inherit">'+esc(u)+'</a>'+esc(tail);last=m.index+m[0].length}
  return out+esc(s.slice(last))}

export const $ = (sel, root = document) => root.querySelector(sel);
export const $$ = (sel, root = document) => [...root.querySelectorAll(sel)];

/** Set innerHTML only when it changed, so nothing flickers or loses focus. */
export function put(el, html) {
  if (el && el._html !== html) { el.innerHTML = html; el._html = html; }
}

/** Set a bar's fill (0-100) in place. */
export function fill(el, pct) {
  if (!el) return;
  const w = (pct == null || !isFinite(pct) ? 0 : Math.max(0, Math.min(100, pct))).toFixed(2) + '%';
  if (el.style.width !== w) el.style.width = w;
}

// ── API ────────────────────────────────────────────────────────────────────

/** Call the API. Resolves with the JSON (or text) body; rejects with the
    server's error message on any non-2xx, so no failure is ever silent. */
export async function api(method, url, body, signal) {
  const opt = { method, headers: {}, cache: 'no-store', signal };
  if (body !== undefined) { opt.body = JSON.stringify(body); opt.headers['Content-Type'] = 'application/json'; }
  let r;
  try { r = await fetch(url, opt); } catch (e) { throw new Error('the server did not answer'); }
  const text = await r.text();
  let data = text;
  try { data = text ? JSON.parse(text) : null; } catch (e) { /* plain text */ }
  if (!r.ok || (data && data.ok === false)) {
    throw new Error((data && (data.error || data.result)) || ('HTTP ' + r.status));
  }
  return data;
}

/** Run `fn` with `btn` showing a spinner; toast the error if it fails.
    Returns the result, or undefined on failure. */
export async function act(btn, fn, failLabel) {
  if (btn) { btn.disabled = true; btn.classList.add('busy'); }
  try {
    return await fn();
  } catch (e) {
    toast((failLabel ? failLabel + ': ' : '') + e.message, 'bad');
    return undefined;
  } finally {
    if (btn) { btn.disabled = false; btn.classList.remove('busy'); }
  }
}

// ── Toasts ─────────────────────────────────────────────────────────────────

export function toast(msg, kind = 'ok', ms) {
  const box = document.getElementById('toasts');
  if (!box) return;
  const t = document.createElement('div');
  t.className = 'toast ' + kind;
  t.setAttribute('role', kind === 'bad' ? 'alert' : 'status');
  t.innerHTML = '<span class="tx">' + esc(msg) + '</span><button class="x" aria-label="Dismiss">×</button>';
  t.querySelector('.x').onclick = () => t.remove();
  box.appendChild(t);
  setTimeout(() => t.remove(), ms || (kind === 'bad' ? 9000 : 4500));
}

// ── Two-step confirm: the first click arms, a second within 5 s acts ────────

/** One click on a two-step button: arm it (red "Confirm" for 5 s), or, if
    armed, disarm and run `fn`. For delegated handlers on live tables. */
export function twoStep(btn, fn, label = 'Confirm') {
  if (btn.classList.contains('confirm')) {
    clearTimeout(btn._revert);
    btn.classList.remove('confirm');
    btn.innerHTML = btn._label;
    fn(btn);
    return;
  }
  btn._label = btn.innerHTML;
  btn.classList.add('confirm');
  btn.textContent = label;
  btn._revert = setTimeout(() => { btn.classList.remove('confirm'); btn.innerHTML = btn._label; }, 5000);
}

/** Wire `btn` so the first click arms it and only a second click within
    5 s runs `onConfirm`. */
export function confirmButton(btn, onConfirm, label = 'Confirm') {
  btn.addEventListener('click', (e) => {
    e.stopPropagation();
    twoStep(btn, onConfirm, label);
  });
}

/** True while any confirm button on the page is armed (don't re-render it away). */
export function anyArmed(root = document) { return !!root.querySelector('.btn.confirm'); }

// ── Menus ──────────────────────────────────────────────────────────────────

// Open menus, closed by one document-wide listener (added once, not per page).
const openMenus = new Set();
document.addEventListener('click', (e) => {
  for (const m of openMenus) if (!m.list.contains(e.target) || !m.list.isConnected) m.close();
});
document.addEventListener('keydown', (e) => { if (e.key === 'Escape') openMenus.forEach(m => m.close()); });

/** A ⋯ button that toggles `list`; closes on outside click and Esc. */
export function menu(btn, list) {
  const m = {
    list,
    close() { list.hidden = true; btn.setAttribute('aria-expanded', 'false'); openMenus.delete(m); },
  };
  btn.setAttribute('aria-haspopup', 'true');
  btn.addEventListener('click', (e) => {
    e.stopPropagation();
    if (list.hidden) {
      list.hidden = false;
      btn.setAttribute('aria-expanded', 'true');
      openMenus.add(m);
    } else m.close();
  });
  list.addEventListener('click', (e) => { if (e.target.closest('button')) m.close(); });
}

// ── Modals ─────────────────────────────────────────────────────────────────

let openModals = [];
document.addEventListener('keydown', (e) => {
  if (e.key === 'Escape' && openModals.length) { e.preventDefault(); openModals[openModals.length - 1].close(); }
});

/** A centred dialog card. Closes on Esc, the × and a backdrop click.
    Returns { el, body, foot, close, onClose }. */
export function modal({ title = '', body = '', foot = '', wide = false, cls = '' } = {}) {
  const back = document.createElement('div');
  back.className = 'backdrop';
  back.innerHTML = '<div class="modal ' + (wide ? 'wide ' : '') + cls + '" role="dialog" aria-modal="true" aria-labelledby="mt' + openModals.length + '">'
    + '<div class="modal-head"><h2 id="mt' + openModals.length + '">' + title + '</h2><button class="x" aria-label="Close">×</button></div>'
    + '<div class="modal-body">' + body + '</div>'
    + (foot ? '<div class="modal-foot">' + foot + '</div>' : '')
    + '</div>';
  return mountModal(back);
}

function mountModal(back) {
  const prevFocus = document.activeElement;
  const box = back.firstElementChild;
  const handlers = [];
  const m = {
    el: box,
    body: box.querySelector('.modal-body, .term-body'),
    foot: box.querySelector('.modal-foot'),
    onClose(f) { handlers.push(f); },
    close() {
      if (!back.isConnected) return;
      back.remove();
      openModals = openModals.filter(x => x !== m);
      handlers.forEach(f => { try { f(); } catch (e) { /* keep closing */ } });
      if (!openModals.length) document.body.style.overflow = '';
      if (prevFocus && prevFocus.focus) prevFocus.focus();
    },
  };
  back.addEventListener('mousedown', (e) => { if (e.target === back) m.close(); });
  box.querySelectorAll('.x, .close').forEach(b => b.addEventListener('click', () => m.close()));
  document.body.appendChild(back);
  document.body.style.overflow = 'hidden';
  openModals.push(m);
  const focusable = box.querySelector('input, select, textarea, .close, .x');
  if (focusable) focusable.focus();
  return m;
}

/** A confirm dialog in the app's own modal. Resolves true on the action,
    false on Cancel, Esc or a backdrop click. Cancel has the focus. */
export function confirmDialog({ title, body = '', action = 'Confirm', danger = true } = {}) {
  return new Promise((resolve) => {
    let answered = false;
    const m = modal({
      title: esc(title),
      body: '<p style="margin:0;color:var(--slate)">' + esc(body) + '</p>',
      foot: '<button class="btn btn-ghost btn-sm" data-c="no">Cancel</button>'
        + '<button class="btn btn-sm ' + (danger ? 'btn-danger' : 'btn-primary') + '" data-c="yes">' + esc(action) + '</button>',
    });
    const done = (v) => { if (!answered) { answered = true; resolve(v); } m.close(); };
    m.onClose(() => { if (!answered) { answered = true; resolve(false); } });
    m.el.querySelector('[data-c=no]').onclick = () => done(false);
    m.el.querySelector('[data-c=yes]').onclick = () => done(true);
    m.el.querySelector('[data-c=no]').focus();
  });
}

// ── The terminal ───────────────────────────────────────────────────────────

/** A terminal window in a modal: window chrome, a live status bar, a mono
    body of `{ts, kind, text}` lines with an in-place progress line (like a
    `\r` in a real terminal), and a footer. */
export function terminal({ title = 'freemkv', tools = '' } = {}) {
  const back = document.createElement('div');
  back.className = 'backdrop';
  back.innerHTML = '<div class="modal term" role="dialog" aria-modal="true" aria-label="' + esc(title) + '">'
    + '<div class="term-bar"><span class="d d-r"></span><span class="d d-y"></span><span class="d d-g"></span>'
    + '<span class="title"></span>' + tools + '<button class="close" aria-label="Close">×</button></div>'
    + '<div class="term-status" hidden><span class="st"></span><div class="bar"><i></i></div></div>'
    + '<div class="term-body" tabindex="0" role="log" aria-live="off"></div>'
    + '<div class="term-foot"><span class="info"></span><span><button class="copy" type="button">Copy</button> <button class="bottom" type="button">Jump to end</button></span></div>'
    + '</div>';
  const m = mountModal(back);
  const box = m.el;
  const body = box.querySelector('.term-body');
  const statusRow = box.querySelector('.term-status');
  const titleEl = box.querySelector('.term-bar .title');
  let prog = null;
  let lines = [];
  // The terminal keeps the last MAX_LINES lines, like a scrollback buffer.
  const MAX_LINES = 5000;
  const atEnd = () => body.scrollTop + body.clientHeight >= body.scrollHeight - 30;
  const stick = (f) => { const end = atEnd(); f(); if (end) body.scrollTop = body.scrollHeight; };
  const row = (l) => {
    const d = document.createElement('div');
    d.className = 'ln k-' + (l.kind || 'out');
    d.innerHTML = '<span class="ts">' + esc(l.ts ? clock(l.ts) : '') + '</span><span class="tx">' + esc(l.text) + '</span>';
    return d;
  };
  titleEl.textContent = title;
  box.querySelector('.copy').onclick = () => {
    const text = lines.map(l => l.text).join('\n') + (prog ? '\n' + prog.querySelector('.tx').textContent : '');
    navigator.clipboard ? navigator.clipboard.writeText(text).then(() => toast('Copied to the clipboard', 'info')) : null;
  };
  box.querySelector('.bottom').onclick = () => { body.scrollTop = body.scrollHeight; };
  body.focus();
  return {
    ...m,
    box,
    setTitle(t) { titleEl.textContent = t; },
    setInfo(html) { put(box.querySelector('.term-foot .info'), html); },
    /** Status bar: text (HTML) and a 0-100 fill, or null to hide. */
    status(html, pct) {
      if (html == null) { statusRow.hidden = true; return; }
      statusRow.hidden = false;
      put(statusRow.querySelector('.st'), html);
      const bar = statusRow.querySelector('.bar');
      bar.classList.toggle('indet', pct == null);
      fill(bar.firstElementChild, pct);
    },
    clear() { lines = []; body.innerHTML = ''; prog = null; },
    append(ls) {
      if (!ls || !ls.length) return;
      stick(() => {
        const frag = document.createDocumentFragment();
        ls.forEach(l => { lines.push(l); frag.appendChild(row(l)); });
        if (prog) body.insertBefore(frag, prog); else body.appendChild(frag);
        const over = lines.length - MAX_LINES;
        if (over > 0) {
          lines.splice(0, over);
          for (let i = 0; i < over; i++) { const first = body.querySelector('.ln'); if (first) first.remove(); }
        }
      });
    },
    /** The line a terminal rewrites in place; null removes it. */
    progress(text, pct) {
      stick(() => {
        if (!text) { if (prog) { prog.remove(); prog = null; } return; }
        if (!prog) {
          prog = row({ kind: 'prog', text: '', ts: Date.now() / 1000 });
          prog.classList.add('k-prog');
          body.appendChild(prog);
        }
        const tx = prog.querySelector('.tx');
        tx.innerHTML = esc(text) + (pct == null ? '' : '<span class="pbar"><i style="width:' + Math.max(0, Math.min(100, pct)).toFixed(2) + '%"></i></span>');
      });
    },
    /** A dim line under the output (idle / waiting), or null. */
    idle(text) {
      let el = body.querySelector('.idle');
      if (!text) { if (el) el.remove(); return; }
      if (!el) { el = document.createElement('div'); el.className = 'idle cursor'; body.appendChild(el); }
      el.textContent = text;
    },
    scrollEnd() { body.scrollTop = body.scrollHeight; },
    lines: () => lines,
  };
}

// ── Formatting ─────────────────────────────────────────────────────────────

export function clock(ts) {
  const d = new Date(ts * 1000);
  return [d.getHours(), d.getMinutes(), d.getSeconds()].map(n => String(n).padStart(2, '0')).join(':');
}
export function when(ts) {
  if (!ts) return '';
  return new Date(ts * 1000).toLocaleString([], { month: 'short', day: 'numeric', hour: '2-digit', minute: '2-digit' });
}
/** "Updated 08:37" today, "Updated Sep 27, 08:37" on another day. */
export function updated(ts) {
  if (!ts) return '';
  const d = new Date(ts * 1000);
  const time = d.toLocaleTimeString([], { hour: '2-digit', minute: '2-digit' });
  const today = d.toDateString() === new Date().toDateString();
  return 'Updated ' + (today ? time : d.toLocaleDateString([], { month: 'short', day: 'numeric' }) + ', ' + time);
}
export function date(ts) {
  if (!ts) return '';
  return new Date(ts * 1000).toLocaleDateString([], { year: 'numeric', month: 'short', day: 'numeric' });
}
export function ago(ts) {
  if (!ts) return 'never';
  const s = Math.max(0, Date.now() / 1000 - ts);
  if (s < 60) return 'just now';
  if (s < 3600) return Math.floor(s / 60) + ' min ago';
  if (s < 86400) return Math.floor(s / 3600) + ' h ago';
  return Math.floor(s / 86400) + ' d ago';
}
export function hms(s) {
  if (s == null || !isFinite(s)) return '–';
  s = Math.round(s);
  const h = Math.floor(s / 3600), m = Math.floor(s / 60) % 60, x = s % 60;
  return h ? h + ':' + String(m).padStart(2, '0') + ':' + String(x).padStart(2, '0') : m + ':' + String(x).padStart(2, '0');
}
export function runtime(s) {
  if (!s) return '';
  s = Math.round(s);
  const h = Math.floor(s / 3600), m = Math.floor(s / 60) % 60;
  return h ? h + 'h ' + String(m).padStart(2, '0') + 'm' : m + 'm';
}
export function bytes(b) {
  if (b == null) return '';
  if (b >= 1e12) return (b / 1e12).toFixed(2) + ' TB';
  if (b >= 1e9) return (b / 1e9).toFixed(1) + ' GB';
  if (b >= 1e6) return (b / 1e6).toFixed(0) + ' MB';
  if (b >= 1e3) return (b / 1e3).toFixed(0) + ' KB';
  return b + ' B';
}
export function speed(bps) {
  if (!bps) return '';
  return (bps / 1048576).toFixed(1) + ' MB/s';
}
export function plural(n, one, many) { return n + ' ' + (n === 1 ? one : (many || one + 's')); }

export const ICON = {
  disc: '<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.5"><circle cx="12" cy="12" r="10"/><circle cx="12" cy="12" r="3"/></svg>',
  search: '<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round"><circle cx="11" cy="11" r="7"/><path d="m20 20-3.5-3.5"/></svg>',
  sun: '<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><circle cx="12" cy="12" r="4"/><path d="M12 2v2M12 20v2M4.93 4.93l1.41 1.41M17.66 17.66l1.41 1.41M2 12h2M20 12h2M6.34 17.66l-1.41 1.41M19.07 4.93l-1.41 1.41"/></svg>',
  moon: '<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M21 12.79A9 9 0 1 1 11.21 3 7 7 0 0 0 21 12.79z"/></svg>',
  more: '<svg viewBox="0 0 24 24" fill="currentColor"><circle cx="5" cy="12" r="2"/><circle cx="12" cy="12" r="2"/><circle cx="19" cy="12" r="2"/></svg>',
  term: '<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" width="15" height="15"><path d="m4 17 6-5-6-5M12 19h8"/></svg>',
};
