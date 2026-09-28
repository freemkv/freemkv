// Ripper: every drive side by side, each with its disc, live rip progress,
// its own actions and its own terminal; below them the pipeline every rip
// goes through afterwards (mux, move, titles held for review).
//
// Every device action goes through `api()` via `driveAction`, which reports
// a refusal (a 409 while a worker unwinds) instead of reading it as success.

import { esc, escLinks, $, put, fill, api, act, toast, twoStep, terminal, modal, bytes, plural, ICON } from './ui.js';
import { subscribe } from './bus.js';

const ACTIVE = ['ripping', 'scanning', 'detecting'];

// ── Formatting, as autorip showed it ──────────────────────────────────────

function fmtSpeed(mbs) {
  mbs = +mbs || 0;
  if (mbs >= 1) return mbs.toFixed(1) + ' MB/s';
  if (mbs * 1024 >= 1) return (mbs * 1024).toFixed(0) + ' KB/s';
  return Math.round(mbs * 1048576) + ' B/s';
}
function fmtBytes(b) {
  return b >= 1073741824 ? (b / 1073741824).toFixed(2) + ' GB' : b >= 1048576 ? (b / 1048576).toFixed(1) + ' MB' : b >= 1024 ? (b / 1024).toFixed(1) + ' KB' : b + ' B';
}
function fmtMs(ms) {
  if (ms == null || !isFinite(ms)) return '';
  if (ms < 1) return '<1 ms';
  if (ms < 1000) return ms.toFixed(0) + ' ms';
  const t = ms / 1000;
  if (t < 60) return t.toFixed(2) + ' s';
  const h = Math.floor(t / 3600), m = Math.floor(t % 3600 / 60), s = Math.floor(t % 60);
  return h ? h + ':' + String(m).padStart(2, '0') + ':' + String(s).padStart(2, '0') : m + ':' + String(s).padStart(2, '0');
}
function fmtElapsed(s) {
  if (!s || s < 0) return '';
  const h = Math.floor(s / 3600), m = Math.floor(s % 3600 / 60), x = s % 60;
  return h ? h + 'h ' + String(m).padStart(2, '0') + 'm ' + String(x).padStart(2, '0') + 's' : m + 'm ' + String(x).padStart(2, '0') + 's';
}
/* The rip passes, without the trailing mux pass the backend counts in total_passes. */
function passLabel(s) {
  if (s.pass > 0 && s.total_passes > 0) {
    const ripTotal = Math.max(s.total_passes - 1, 1);
    if (s.pass >= s.total_passes) return 'recovery complete';
    return 'pass ' + s.pass + '/' + ripTotal + ' · ' + (s.pass === 1 ? 'copying' : 'retrying');
  }
  if (s.pass === 1 && s.total_passes === 0) return 'pass 1/1 · copying';
  return '';
}
const discIn = s => s.disc_present || !!(s.tmdb_title || s.disc_name) || ACTIVE.includes(s.status);
const lossAborted = s => s.loss_aborted || /lost in main movie|lost at mux/.test(s.last_error || '');

function stateLabel(s) {
  if (s.status === 'ripping') return 'Ripping' + (passLabel(s) ? ' · ' + passLabel(s) : '');
  if (s.status === 'scanning') return 'Scanning';
  if (s.status === 'detecting') return 'Detecting';
  if (s.status === 'error') return 'Error';
  if (s.status === 'done') return 'Done';
  if (s.status === 'moving') return 'Finishing up';
  if (!discIn(s)) return 'No disc';
  if (!(s.tmdb_title || s.disc_name)) return 'Disc inserted';
  return 'Ready';
}
function dotClass(s) {
  if (ACTIVE.includes(s.status)) return 'dot-teal';
  if (s.status === 'error') return 'dot-bad';
  if (s.status === 'done') return 'dot-ok';
  return discIn(s) ? 'dot-warn' : 'dot-idle';
}

// ── Sections of a drive card ───────────────────────────────────────────────

function discHtml(s) {
  const title = s.tmdb_title || s.disc_name;
  if (!discIn(s)) return '<div class="drive-idle">' + ICON.disc + 'No disc</div>';
  if (!title) return '<div class="drive-idle">' + ICON.disc + 'Disc inserted' + (s.status === 'idle' ? ' · press Scan to see what it is' : '') + '</div>';
  const poster = s.tmdb_poster ? '<img class="poster" src="' + esc(s.tmdb_poster) + '" alt="">' : '<div class="poster">' + ICON.disc + '</div>';
  const fmt = s.disc_format && s.disc_format !== 'unknown' ? '<span class="badge fmt-' + esc(s.disc_format) + '">' + esc(s.disc_format.toUpperCase()) + '</span>' : '';
  const ks = s.key_status || '';
  const keys = s.status === 'idle' && ks ? '<div class="keys" style="color:' + (ks.indexOf('Missing') === 0 ? 'var(--warn)' : 'var(--ok)') + '">' + esc(ks) + '</div>' : '';
  return '<div class="drive-body">' + poster + '<div class="drive-info">'
    + '<div class="t">' + esc(title) + '</div>'
    + '<div class="meta">' + (s.tmdb_year > 0 ? '<span>' + s.tmdb_year + '</span>' : '') + (s.duration ? '<span>' + esc(s.duration) + '</span>' : '') + fmt
    + (s.tmdb_media_type === 'tv' ? '<span class="badge badge-muted">TV</span>' : '') + '</div>'
    + (s.tmdb_overview ? '<div class="ov">' + esc(s.tmdb_overview) + '</div>' : '')
    + (s.codecs ? '<div class="codecs">' + esc(s.codecs) + '</div>' : '')
    + keys + '</div></div>';
}

function stepsHtml(s) {
  const st = s.status;
  const steps = st === 'scanning' ? ['now', '', ''] : st === 'ripping' ? ['done', 'now', ''] : (st === 'moving' || st === 'done') ? ['done', 'done', 'done'] : null;
  if (!steps) return '';
  const names = ['Read', 'Rip', 'Finish'];
  return '<div class="steps">' + names.map((n, i) => '<span class="s ' + steps[i] + '">' + (steps[i] === 'done' ? '✓' : steps[i] === 'now' ? '●' : '○') + ' ' + n + '</span>').join('<span class="sep">›</span>') + '</div>';
}

/* The disc map. Pass 1: green grows to the read head; only damage already
   swept shows red (unread is unknown, not bad). Pass 2+: the whole disc is
   green with each still-bad range in red at its real offset. */
function badRangesHtml(s) {
  const total = s.bytes_total_disc || 0;
  const ranges = s.bad_ranges || [];
  if (!total || !ranges.length) return '';
  const positional = s.pass > 1;
  const swept = positional ? 100 : (s.last_sector > 0 ? s.last_sector * 2048 / total * 100 : 0);
  return ranges.map(r => {
    let off = Math.max(0, Math.min(100, r.lba * 2048 / total * 100));
    let w = Math.max(r.count * 2048 / total * 100, 0.5);
    if (off + w > 100) w = 100 - off;
    if (!positional) {
      if (off >= swept) return '';
      if (off + w > swept) w = swept - off;
      if (w <= 0) return '';
    }
    return '<span class="badr" style="left:' + off.toFixed(3) + '%;width:' + w.toFixed(3) + '%" title="' + fmtMs(r.duration_ms) + (r.chapter ? ' · chapter ' + r.chapter : '') + '"></span>';
  }).join('');
}
function headPct(s) {
  const total = s.bytes_total_disc || 0;
  if (!total || s.status !== 'ripping') return null;
  const ranges = s.bad_ranges || [];
  if (s.pass > 1 && ranges.length) {
    let a = ranges[0];
    ranges.forEach(r => { if (r.count > a.count) a = r; });
    const off = Math.max(0, Math.min(100, a.lba * 2048 / total * 100));
    let w = Math.max(a.count * 2048 / total * 100, 0.5);
    if (off + w > 100) w = 100 - off;
    return off + w;
  }
  return s.last_sector > 0 ? s.last_sector * 2048 / total * 100 : null;
}

function ripFigures(s) {
  const pct = typeof s.pass_progress_pct === 'number' ? s.pass_progress_pct : (s.progress_pct || 0);
  const nSec = s.num_bad_ranges > 0 ? s.num_bad_ranges : (s.bad_ranges || []).length;
  const rem = (s.bytes_maybe || 0) + (s.bytes_lost || 0);
  const figs = ['<b>' + pct + '%</b>'];
  if (s.pass_eta) figs.push('ETA ' + esc(s.pass_eta));
  if (s.speed_mbs != null) figs.push(fmtSpeed(s.speed_mbs));
  const left = rem > 0 ? '<div class="small" style="width:100%;color:var(--slate)">' + (nSec ? plural(nSec, 'damaged area') + ' · ' : '') + fmtBytes(rem) + ' still to read</div>' : '';
  const pills = [];
  if (s.bytes_good > 0) pills.push('<span class="badge badge-ok">Read ' + fmtBytes(s.bytes_good) + '</span>');
  if (rem > 0) {
    const risk = s.main_at_risk_ms > 0 ? '~' + fmtMs(s.main_at_risk_ms) : '0:00';
    pills.push('<span class="badge badge-warn" title="Not read cleanly yet; the time is how much of the movie it affects">Not read yet ' + fmtBytes(rem) + ' · ' + risk + ' of the movie</span>');
  }
  return { figs: figs.join('<span class="muted"> · </span>') + left, pills: pills.join(' ') };
}

function bannersHtml(s) {
  let h = '';
  if (s.last_error && (s.errors > 0 || s.status === 'error' || lossAborted(s))) {
    h += '<div class="banner bad">⚠ <span>' + escLinks(s.last_error) + '</span></div>';
  }
  if (s.status === 'ripping' && s.current_batch > 0 && s.preferred_batch > 0 && s.current_batch < s.preferred_batch) {
    h += '<div class="banner info">↺ Reading a damaged area slowly (' + s.current_batch + ' / ' + s.preferred_batch + ' sectors at a time)</div>';
  }
  return h;
}

/* [label, endpoint, kind, confirm?] per state: exactly the choices autorip offered. */
function actionsFor(dev, s) {
  const active = ACTIVE.includes(s.status);
  const scanned = !!(s.tmdb_title || s.disc_name);
  const a = [];
  if (active) {
    a.push(['Stop', '/api/stop/' + dev, 'ghost', true]);
    return a;
  }
  if (lossAborted(s)) {
    a.push(['Run one more pass', '/api/rip/' + dev + '?resume=yes', 'primary']);
    a.push(['Accept & deliver', '/api/accept-loss/' + dev, 'secondary', true]);
  } else if (scanned) {
    if ((s.key_status || '').indexOf('Missing') === 0) a.push(['Scan again', '/api/scan/' + dev, 'secondary']);
    else if (s.resumable) {
      a.push([s.resumable === 'remux' ? 'Resume (re-mux)' : 'Resume', '/api/rip/' + dev + '?resume=yes', 'primary']);
      a.push(['Start over', '/api/rip/' + dev + '?resume=no', 'ghost', true]);
    } else a.push(['Rip', '/api/rip/' + dev + '?resume=no', 'primary']);
  } else if (discIn(s)) {
    a.push(['Scan', '/api/scan/' + dev, 'primary']);
  }
  if (discIn(s)) a.push(['Eject', '/api/eject/' + dev, 'ghost']);
  return a;
}

const DONE_MSG = {
  Stop: 'Stopping', Rip: 'Rip started', Resume: 'Resuming', 'Resume (re-mux)': 'Re-muxing the staged image',
  'Start over': 'Starting over', Scan: 'Scanning', 'Scan again': 'Scanning again', Eject: 'Ejecting',
  'Run one more pass': 'Running another recovery pass', 'Accept & deliver': 'Delivering as-is',
};

/** POST a device action; the answer (including a refusal) is always shown. */
async function driveAction(btn, dev, label, url) {
  const r = await act(btn, () => api('POST', url), label + ' on ' + dev);
  if (r !== undefined) toast((DONE_MSG[label] || label) + ' on ' + dev, 'ok');
}

function actionsHtml(dev, s) {
  const btns = actionsFor(dev, s).map(([label, url, kind, two]) =>
    '<button class="btn btn-' + kind + ' btn-sm" data-url="' + esc(url) + '" data-label="' + esc(label) + '"' + (two ? ' data-two' : '') + '>' + esc(label) + '</button>').join('');
  const edit = s.status === 'idle' && (s.tmdb_title || s.disc_name) ? '<button class="btn btn-ghost btn-sm" data-title>Change title</button>' : '';
  const elapsed = ACTIVE.includes(s.status) ? '<span class="elapsed" data-started="' + (s.started_epoch_secs || 0) + '"></span>' : '';
  return btns + edit + elapsed;
}

// ── A drive card, updated in place ────────────────────────────────────────

function makeCard(dev) {
  const el = document.createElement('article');
  el.className = 'drive';
  el.dataset.dev = dev;
  el.innerHTML = '<div class="drive-head"><span class="dot"></span><span class="dev"></span><span class="state"></span>'
    + '<div class="tools"><button class="btn btn-ghost btn-sm" data-console>' + ICON.term + ' Console</button></div></div>'
    + '<div data-s="disc"></div>'
    + '<div class="rip" data-s="rip" hidden><div data-s="steps"></div><div class="phase"><b data-s="phase"></b><span class="muted small" data-s="pass"></span></div>'
    + '<div class="discmap"><span class="good"></span><span data-s="bad"></span><span class="head" hidden></span></div>'
    + '<div class="figs" data-s="figs"></div><div style="margin-top:.5rem" data-s="pills"></div></div>'
    + '<div data-s="steps2"></div>'
    + '<div data-s="banners"></div>'
    + '<div class="drive-actions" data-s="actions"></div>';
  $('.dev', el).textContent = dev;
  return el;
}

function paintCard(el, dev, s) {
  const q = (k) => el.querySelector('[data-s="' + k + '"]');
  const active = ACTIVE.includes(s.status);
  el.classList.toggle('active', active);
  $('.dot', el).className = 'dot ' + dotClass(s);
  put($('.state', el), esc(stateLabel(s)));
  put(q('disc'), discHtml(s));
  const ripping = s.status === 'ripping';
  q('rip').hidden = !ripping;
  put(q('steps2'), ripping ? '' : (stepsHtml(s) ? '<div style="padding:0 1.1rem">' + stepsHtml(s) + '</div>' : ''));
  if (ripping) {
    put(q('steps'), stepsHtml(s));
    put(q('phase'), 'Rip');
    put(q('pass'), esc(passLabel(s)));
    const total = s.bytes_total_disc || 0;
    const positional = s.pass > 1;
    const goodPct = positional ? 100 : (total > 0 && s.last_sector > 0 ? s.last_sector * 2048 / total * 100 : (s.pass_progress_pct || 0));
    fill($('.discmap .good', el), goodPct);
    put(q('bad'), badRangesHtml(s));
    const hp = headPct(s);
    const head = $('.discmap .head', el);
    head.hidden = hp == null;
    if (hp != null) head.style.left = 'calc(' + Math.max(0, Math.min(100, hp)).toFixed(3) + '% - 1px)';
    const f = ripFigures(s);
    put(q('figs'), f.figs);
    put(q('pills'), f.pills);
  }
  put(q('banners'), bannersHtml(s));
  const acts = q('actions');
  if (!acts.querySelector('.btn.confirm')) put(acts, actionsHtml(dev, s));
}

// ── Pipeline: mux, move, review ───────────────────────────────────────────

function barRow(name, sub, pct) {
  return '<div class="pipe-row"><span class="pulse"></span><div class="grow"><div class="name">' + esc(name) + '</div>'
    + '<div class="bar" style="margin:.35rem 0 .2rem"><i style="width:' + (pct || 0) + '%"></i></div><div class="sub">' + esc(sub) + '</div></div></div>';
}
function queueRows(list) {
  return (list || []).map(m => '<div class="pipe-row"><span class="dot dot-warn"></span><div class="grow"><div class="name" style="font-weight:500">' + esc(m) + '</div></div></div>').join('');
}
function errorRows(list, kind) {
  if (!list || !list.length) return '';
  return '<div class="card-head" style="margin:1rem 0 .25rem"><span class="small muted" style="text-transform:uppercase;letter-spacing:.05em;font-weight:700">Needs action</span>'
    + '<span class="actions"><button class="btn btn-ghost btn-sm" data-refresh>Refresh</button><button class="btn btn-ghost btn-sm" data-clearall="' + kind + '">Clear all</button></span></div>'
    + list.map(e => '<div class="pipe-row pipe-err"><span class="dot dot-bad"></span><div class="grow"><div class="name mono">' + esc(e.path) + '</div>'
      + '<div class="sub">' + esc(e.reason || '') + '</div>' + (e.hint ? '<div class="sub">' + esc(e.hint) + '</div>' : '') + '</div>'
      + '<button class="x" data-clear="' + kind + '" data-path="' + esc(e.path) + '" title="Clear this error" aria-label="Clear this error">×</button></div>').join('');
}

function muxHtml(state, sys) {
  const mx = state._mux;
  let h = '';
  if (mx && mx.status === 'ripping' && mx.disc_name) {
    h += barRow(mx.disc_name, [mx.progress_pct + '%', mx.speed_mbs > 0 ? fmtSpeed(mx.speed_mbs) : '', mx.eta ? mx.eta + ' remaining' : ''].filter(Boolean).join(' · '), mx.progress_pct);
  }
  h += queueRows(state._mux_queue != null ? state._mux_queue : sys.mux_queue);
  if (!h) h = '<div class="muted small">Nothing waiting.</div>';
  return h + errorRows(sys.mux_errors, 'mux');
}
function moveHtml(state, sys) {
  const moves = Array.isArray(state._move) ? state._move : (state._move && state._move.name ? [state._move] : []);
  let h = moves.filter(m => m && m.name).map(m => barRow(m.name + (m.artifact ? ' (' + m.artifact + ')' : ''),
    [m.progress_pct + '%', m.speed_mbs > 0 ? fmtSpeed(m.speed_mbs) : '', m.eta ? m.eta + ' remaining' : ''].filter(Boolean).join(' · '), m.progress_pct)).join('');
  h += queueRows(state._move_queue != null ? state._move_queue : sys.move_queue);
  if (!h) h = '<div class="muted small">Nothing waiting.</div>';
  return h + errorRows(sys.move_errors, 'move');
}
function reviewHtml(items) {
  return items.map((it, i) => '<div class="pipe-row"><span class="dot dot-teal"></span><div class="grow"><div class="name">' + esc(it.title || it.dir) + (it.year ? ' (' + it.year + ')' : '') + '</div>'
    + '<div class="sub">' + esc(it.reason || '') + '</div><div class="sub mono">' + esc(it.file || '') + '</div></div>'
    + '<button class="btn btn-secondary btn-sm" data-review="' + i + '">Review…</button></div>').join('');
}

// ── Dialogs ────────────────────────────────────────────────────────────────

/* A trailing "(YYYY)" becomes the year; else `fallbackYear`. */
function parseName(raw, fallbackYear) {
  raw = (raw || '').trim();
  if (!raw) return null;
  const m = raw.match(/^(.*?)\s*\((\d{4})\)\s*$/);
  const title = m ? m[1].trim() : raw;
  return title ? { title, year: m ? parseInt(m[2], 10) : (fallbackYear || 0) } : null;
}

/** Search TMDB or type a name. `onPick({title, year, ...})` files it. */
function titlePicker({ heading, initial, sub, extraFoot = '', onPick, onExtra }) {
  const m = modal({
    title: esc(heading), wide: true,
    body: (sub ? '<p class="small muted" style="margin:0 0 .9rem">' + sub + '</p>' : '')
      + '<div class="hook"><input class="txt" id="tp-q" placeholder="Type an exact name, or a search term" value="' + esc(initial || '') + '">'
      + '<button class="btn btn-secondary btn-sm" id="tp-search">Search TMDB</button></div>'
      + '<div id="tp-res" class="stack" style="margin-top:.5rem"></div>',
    foot: extraFoot + '<button class="btn btn-primary btn-sm" id="tp-manual">Use this exact name</button>',
  });
  const qi = m.el.querySelector('#tp-q');
  const res = m.el.querySelector('#tp-res');
  let found = [];
  const search = async (btn) => {
    const q = qi.value.trim();
    if (!q) { toast('Type something to search for', 'info'); return; }
    res.innerHTML = '<div class="muted small">Searching…</div>';
    const cs = await act(btn, () => api('GET', '/api/tmdb/search?q=' + encodeURIComponent(q)), 'TMDB search');
    if (!cs) { res.innerHTML = '<div class="muted small">Search failed.</div>'; return; }
    found = cs;
    res.innerHTML = cs.length ? cs.map((c, i) => '<div class="pipe-row">' + (c.poster_url ? '<img src="' + esc(c.poster_url) + '" alt="" style="width:40px;height:60px;object-fit:cover;border-radius:6px">' : '')
      + '<div class="grow"><div class="name">' + esc(c.title) + (c.year ? ' (' + c.year + ')' : '') + ' <span class="badge badge-muted">' + esc(c.media_type || '') + '</span></div>'
      + '<div class="sub" style="display:-webkit-box;-webkit-line-clamp:2;-webkit-box-orient:vertical;overflow:hidden">' + esc(c.overview || '') + '</div></div>'
      + '<button class="btn btn-secondary btn-sm" data-pick="' + i + '">Use this</button></div>').join('') : '<div class="muted small">No matches.</div>';
  };
  m.el.querySelector('#tp-search').onclick = (e) => search(e.currentTarget);
  qi.addEventListener('keydown', (e) => { if (e.key === 'Enter') search(m.el.querySelector('#tp-search')); });
  res.addEventListener('click', async (e) => {
    const b = e.target.closest('[data-pick]');
    if (b && await onPick(found[+b.dataset.pick], b)) m.close();
  });
  m.el.querySelector('#tp-manual').onclick = async (e) => {
    const p = parseName(qi.value, 0);
    if (!p) { toast('Type a name first', 'info'); qi.focus(); return; }
    if (await onPick({ ...p, tmdb_id: 0 }, e.currentTarget)) m.close();
  };
  if (onExtra) onExtra(m);
  qi.select();
  return m;
}

function changeTitle(dev, s) {
  titlePicker({
    heading: 'Title for the disc in ' + dev,
    initial: s.tmdb_title || s.disc_name,
    sub: 'Fix the name before ripping. Pick a TMDB match or type a name; "(YYYY)" at the end is the year. It applies to this rip only.',
    onPick: async (c, btn) => {
      const r = await act(btn, () => api('POST', '/api/title/' + dev, { ...c, year: c.year || 0 }), 'Change title');
      if (r === undefined) return false;
      toast('The disc in ' + dev + ' will be filed as ' + c.title + (c.year ? ' (' + c.year + ')' : ''), 'ok');
      return true;
    },
  });
}

function reviewDialog(it, reload) {
  titlePicker({
    heading: 'Which title is this? ' + (it.title || it.dir) + (it.year ? ' (' + it.year + ')' : ''),
    initial: it.title || '',
    sub: esc(it.reason || '') + (it.file ? '<br><span class="mono">' + esc(it.file) + '</span>' : ''),
    extraFoot: '<button class="btn btn-ghost btn-sm" id="rv-cancel">Delete this rip</button><button class="btn btn-secondary btn-sm" id="rv-proceed">Keep this name</button>',
    onPick: async (c, btn) => resolve(btn, { action: 'retitle', title: c.title, year: c.year || 0 }),
    onExtra: (m) => {
      m.el.querySelector('#rv-proceed').onclick = async (e) => { if (await resolve(e.currentTarget, { action: 'proceed' })) m.close(); };
      const cb = m.el.querySelector('#rv-cancel');
      cb.onclick = (e) => twoStep(e.currentTarget, async (b) => { if (await resolve(b, { action: 'cancel' })) m.close(); }, 'Confirm delete');
      void cb;
    },
  });
  async function resolve(btn, body) {
    const r = await act(btn, () => api('POST', '/api/review/resolve', { dir: it.dir, ...body }), 'Resolve');
    if (r === undefined) return false;
    toast(body.action === 'cancel' ? 'Discarded' : body.action === 'proceed' ? 'Filing as-is' : 'Filing as ' + body.title, 'ok');
    reload();
    return true;
  }
}

/* A device log line: "[2026-09-27T21:04:05Z] msg". */
function parseLogLine(line) {
  const m = line.match(/^\[(\d{4}-\d\d-\d\dT\d\d:\d\d:\d\dZ?)\] ?(.*)$/);
  const text = m ? m[2] : line;
  const ts = m ? Date.parse(m[1].endsWith('Z') ? m[1] : m[1] + 'Z') / 1000 : 0;
  const kind = /\b(error|failed|fatal|abort)/i.test(text) ? 'err' : /\bwarn/i.test(text) ? 'warn' : /\b(complete|finished|done)\b/i.test(text) ? 'ok' : text.startsWith('▸') ? 'cmd' : 'out';
  return { ts, kind, text };
}
function parseDebugLine(line) {
  try {
    const o = JSON.parse(line);
    const f = o.fields || {};
    const extra = Object.keys(f).filter(k => k !== 'message' && k !== 'build').map(k => k + '=' + f[k]).join(' ');
    const lvl = (o.level || '').toLowerCase();
    return { ts: Date.parse(o.timestamp) / 1000 || 0, kind: lvl === 'error' ? 'err' : lvl === 'warn' ? 'warn' : 'debug', text: (o.level || '') + ' ' + (f.message || '') + (extra ? '  ' + extra : '') };
  } catch (e) { return { ts: 0, kind: 'debug', text: line }; }
}

/** A terminal on one drive's log, with its live rip status. `dev` may be
    "system" for the daemon's own log. */
export function openDeviceTerminal(dev, debugOn) {
  const tools = debugOn && dev !== 'system' ? '<button class="tb" data-mode="log">Log</button><button class="tb" data-mode="debug">Debug</button>' : '';
  const t = terminal({ title: 'freemkv — ' + (dev === 'system' ? 'system log' : dev), tools });
  // Log mode follows the ring by sequence number (`since=`), so it keeps
  // up after the server's 500-line ring wraps. Debug mode re-reads its
  // window and appends whatever follows the last line it showed.
  let mode = 'log', since = 0, lastDebug = null, timer = null, closed = false;
  const reset = () => { since = 0; lastDebug = null; t.clear(); };
  const paintTools = () => t.box.querySelectorAll('.tb').forEach(b => b.classList.toggle('on', b.dataset.mode === mode));
  t.box.querySelectorAll('.tb').forEach(b => b.addEventListener('click', () => { mode = b.dataset.mode; reset(); paintTools(); poll(); }));
  paintTools();
  async function poll() {
    clearTimeout(timer);
    if (closed) return;
    try {
      if (mode === 'debug') {
        const text = await api('GET', '/api/debug?device=' + encodeURIComponent(dev) + '&n=1000');
        const raw = String(text || '').split('\n').filter(Boolean);
        let from = 0;
        if (lastDebug != null) {
          const at = raw.lastIndexOf(lastDebug);
          if (at >= 0) from = at + 1; else t.clear();
        }
        t.append(raw.slice(from).map(parseDebugLine));
        if (raw.length) lastDebug = raw[raw.length - 1];
        t.idle(t.lines().length ? null : 'no debug lines yet');
      } else {
        const r = await api('GET', '/api/logs/' + encodeURIComponent(dev) + '?since=' + since);
        t.append(r.lines.map(([, line]) => parseLogLine(line)));
        since = r.seq;
        t.idle(t.lines().length ? null : 'no log lines yet');
      }
    } catch (e) {
      t.idle('could not load the log: ' + e.message);
    }
    timer = setTimeout(poll, 2000);
  }
  const off = subscribe('state', (state) => {
    const s = state[dev];
    if (!s || s.status !== 'ripping') { t.status(s ? '<b>' + esc(stateLabel(s)) + '</b>' + (s.tmdb_title || s.disc_name ? ' · ' + esc(s.tmdb_title || s.disc_name) : '') : null, 0); return; }
    const pct = typeof s.pass_progress_pct === 'number' ? s.pass_progress_pct : s.progress_pct;
    t.status('<b>' + esc(s.tmdb_title || s.disc_name) + '</b> · ' + esc(passLabel(s) || 'ripping') + ' · <b>' + pct + '%</b>'
      + (s.speed_mbs != null ? ' · ' + fmtSpeed(s.speed_mbs) : '') + (s.pass_eta ? ' · ETA ' + esc(s.pass_eta) : ''), pct);
  });
  t.onClose(() => { closed = true; clearTimeout(timer); off(); });
  poll();
  return t;
}

// ── The page ───────────────────────────────────────────────────────────────

export default {
  title: 'Drives',
  mount(view, ctx) {
    view.innerHTML = `<div id="rp">
      <div class="page-head">
        <div><h1>Drives</h1><p class="lede" id="lede">Waiting for the drives…</p></div>
        <div class="actions"><button class="btn btn-ghost" id="syslog">${ICON.term} System log</button></div>
      </div>
      <div class="drives" id="drives"></div>
      <h2 style="margin:2.25rem 0 1rem;font-size:1.15rem;letter-spacing:-.01em">After the rip</h2>
      <div class="grid grid-2">
        <section class="card"><div class="card-head"><h2>Making video files</h2></div><div id="mux"></div></section>
        <section class="card"><div class="card-head"><h2>Moving to your library</h2></div><div id="move"></div></section>
      </div>
      <section class="card" id="review-card" style="margin-top:1.25rem" hidden><div class="card-head"><h2>Waiting for a title <span class="count" id="review-n"></span></h2></div>
        <p class="small muted" style="margin:-.4rem 0 .6rem">These rips are done, but the movie's name wasn't certain. Pick the right one to file them.</p><div id="review"></div></section></div>`;
    const root = $('#rp', view);
    const cards = new Map();
    let state = {};
    let sys = {};
    let reviews = [];
    const drivesEl = $('#drives', view);

    const render = (s) => {
      if (ctx.stale()) return;
      state = s || {};
      const devs = Object.keys(state).filter(k => !k.startsWith('_')).sort();
      put($('#lede', view), devs.length
        ? plural(devs.length, 'drive') + ' · ' + devs.filter(d => ACTIVE.includes(state[d].status)).length + ' busy'
        : 'No drives found. A drive appears here about a minute after it is plugged in.');
      for (const [dev, el] of cards) if (!devs.includes(dev)) { el.remove(); cards.delete(dev); }
      const emptyEl = drivesEl.querySelector(':scope > .empty');
      if (!devs.length && !emptyEl) drivesEl.insertAdjacentHTML('beforeend', '<div class="card empty" style="grid-column:1/-1">' + ICON.disc + 'No drives found</div>');
      if (devs.length && emptyEl) emptyEl.remove();
      devs.forEach((dev, i) => {
        let el = cards.get(dev);
        if (!el) { el = makeCard(dev); cards.set(dev, el); }
        if (drivesEl.children[i] !== el) drivesEl.insertBefore(el, drivesEl.children[i] || null);
        paintCard(el, dev, state[dev]);
      });
      put($('#mux', view), muxHtml(state, sys));
      put($('#move', view), moveHtml(state, sys));
    };
    ctx.onState(render);

    const loadSys = () => api('GET', '/api/system').then(d => { if (!ctx.stale()) { sys = d; render(state); } }).catch(() => {});
    const loadReview = () => api('GET', '/api/review').then(items => {
      if (ctx.stale()) return;
      reviews = items || [];
      $('#review-card', view).hidden = !reviews.length;
      put($('#review-n', view), reviews.length ? '(' + reviews.length + ')' : '');
      put($('#review', view), reviewHtml(reviews));
    }).catch(() => {});
    loadSys(); loadReview();
    ctx.every(5000, () => { loadSys(); loadReview(); });
    ctx.every(1000, () => {
      const now = Math.floor(Date.now() / 1000);
      view.querySelectorAll('.elapsed[data-started]').forEach(el => {
        const st = +el.dataset.started || 0;
        el.textContent = st > 0 ? fmtElapsed(now - st) : '';
      });
    });

    $('#syslog', view).addEventListener('click', () => openDeviceTerminal('system', false));
    drivesEl.addEventListener('click', (e) => {
      const card = e.target.closest('.drive');
      if (!card) return;
      const dev = card.dataset.dev;
      if (e.target.closest('[data-console]')) { openDeviceTerminal(dev, !!sys.debug_enabled); return; }
      if (e.target.closest('[data-title]')) { changeTitle(dev, state[dev] || {}); return; }
      const b = e.target.closest('button[data-url]');
      if (!b) return;
      const go = (btn) => driveAction(btn, dev, b.dataset.label, b.dataset.url);
      if (b.dataset.two != null) twoStep(b, go); else go(b);
    });
    root.addEventListener('click', async (e) => {
      const c = e.target.closest('[data-clear]');
      if (c) {
        const kind = c.dataset.clear;
        if (await act(c, () => api('POST', '/api/' + kind + '-errors/clear?path=' + encodeURIComponent(c.dataset.path)), 'Clear') !== undefined) {
          toast('Cleared. It comes back if the problem is still there.', 'info');
        }
        loadSys();
        return;
      }
      const ca = e.target.closest('[data-clearall]');
      if (ca) {
        if (await act(ca, () => api('POST', '/api/' + ca.dataset.clearall + '-errors/clear-all'), 'Clear all') !== undefined) {
          toast('Cleared all', 'info');
        }
        loadSys();
        return;
      }
      if (e.target.closest('[data-refresh]')) {
        const ok = await api('GET', '/api/system').then(d => { sys = d; render(state); return true; }).catch(err => { toast('Recheck failed: ' + err.message, 'bad'); return false; });
        if (ok) toast('Rechecked', 'info');
        return;
      }
      const rv = e.target.closest('[data-review]');
      if (rv) reviewDialog(reviews[+rv.dataset.review], loadReview);
    });
  },
};
