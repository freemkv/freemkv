// The media list: one two-line row per title (status dot, title with its
// facts, a line of pills; what muxed it and the row's action on the right).
// Rows are keyed and patched part by part, so live updates never flicker or
// disarm a Confirm button. The pill line never wraps: pills that do not fit
// fold into a "+N" pill that opens the details.

import { esc, put } from './ui.js';

const PARTS = ['dot', 'title', 'meta', 'pills', 'side', 'act'];

function makeRow() {
  const el = document.createElement('div');
  el.className = 'mrow';
  el.tabIndex = 0;
  el.setAttribute('role', 'button');
  el.innerHTML = '<span class="mdot" data-p="dot"></span>'
    + '<div class="mmain"><div class="mline"><b class="mtitle" data-p="title"></b><span class="mmeta" data-p="meta"></span></div>'
    + '<div class="mpills" data-p="pills"></div></div>'
    + '<div class="mside" data-p="side"></div><div class="mact" data-p="act"></div>';
  el._p = {};
  PARTS.forEach(p => { el._p[p] = el.querySelector('[data-p="' + p + '"]'); });
  return el;
}

// Whether the line's last shown pill ends past the line's own right edge.
function overflows(line) {
  const shown = [...line.children].filter(c => !c.hidden && c.getClientRects().length);
  if (!shown.length) return false;
  return shown[shown.length - 1].getBoundingClientRect().right > line.getBoundingClientRect().right - 0.5;
}

/** Fold the pills that overflow their line into a "+N" pill. */
export function fitPills(line) {
  const pills = [...line.children].filter(c => !c.classList.contains('more'));
  pills.forEach(p => { p.hidden = false; });
  let more = line.querySelector('.more');
  if (more) more.hidden = true;
  if (!overflows(line)) return;
  if (!more) {
    more = document.createElement('button');
    more.type = 'button';
    more.className = 'pill more';
    more.dataset.more = '1';
    line.appendChild(more);
  }
  more.hidden = false;
  let hidden = 0;
  // Unpinned pills go first, from the end; pinned ones (the state) only if
  // the line still does not fit. The first pill always stays.
  for (const pinned of [false, true]) {
    for (let i = pills.length - 1; i > 0 && overflows(line); i--) {
      if (pills[i].hidden || !!pills[i].dataset.keep !== pinned) continue;
      pills[i].hidden = true;
      hidden++;
      more.textContent = '+' + hidden;
    }
  }
  more.title = hidden + ' more: open the details';
  more.setAttribute('aria-label', more.title);
}

/**
 * host: element to fill. opts: {
 *   key: r => string, render: r => { dot, title, meta, pills, side, act, cls },
 *   onRow: r => void, onMore: r => void,
 *   sorts: { id: [label, r => value] }, store, defaultSort: { id, dir },
 *   sortHost: element for the Sort control, group: r => number (sorts first) }
 */
export function mediaList(host, opts) {
  host.innerHTML = '<div class="mlist" role="list"></div><div class="empty" hidden></div>';
  const list = host.querySelector('.mlist');
  const emptyEl = host.querySelector('.empty');
  const rows = new Map();
  let sort = opts.defaultSort;
  try { sort = JSON.parse(localStorage.getItem(opts.store)) || sort; } catch (e) { /* default */ }
  if (!opts.sorts[sort.id]) sort = opts.defaultSort;
  let last = [];

  if (opts.sortHost) {
    opts.sortHost.innerHTML = '<label class="sortsel"><span>Sort</span><select aria-label="Sort by">'
      + Object.entries(opts.sorts).map(([id, [label]]) => '<option value="' + id + '">' + esc(label) + '</option>').join('')
      + '</select></label><button class="icon-btn sortdir" type="button"></button>';
    const sel = opts.sortHost.querySelector('select');
    const dir = opts.sortHost.querySelector('.sortdir');
    const paint = () => {
      sel.value = sort.id;
      dir.textContent = sort.dir > 0 ? '↑' : '↓';
      dir.setAttribute('aria-label', sort.dir > 0 ? 'Ascending; switch to descending' : 'Descending; switch to ascending');
    };
    const save = () => { try { localStorage.setItem(opts.store, JSON.stringify(sort)); } catch (e) { /* ok */ } paint(); api.update(last); };
    sel.addEventListener('change', () => { sort = { id: sel.value, dir: 1 }; save(); });
    dir.addEventListener('click', () => { sort = { id: sort.id, dir: -sort.dir }; save(); });
    paint();
  }

  const cmp = (a, b) => {
    const f = opts.sorts[sort.id][1];
    const x = f(a), y = f(b);
    const r = typeof x === 'number' && typeof y === 'number' ? x - y : String(x).localeCompare(String(y), undefined, { numeric: true, sensitivity: 'base' });
    return (r || String(opts.key(a)).localeCompare(String(opts.key(b)))) * sort.dir;
  };

  list.addEventListener('click', (e) => {
    const row = e.target.closest('.mrow');
    if (!row || !row._row) return;
    if (e.target.closest('[data-more]')) { e.stopPropagation(); (opts.onMore || opts.onRow)(row._row); return; }
    if (e.target.closest('button, a, input, label')) return;
    opts.onRow(row._row);
  });
  list.addEventListener('keydown', (e) => {
    const row = e.target.closest('.mrow');
    if (row && e.target === row && (e.key === 'Enter' || e.key === ' ')) { e.preventDefault(); opts.onRow(row._row); }
  });

  let resizeTimer = null;
  const ro = new ResizeObserver(() => {
    clearTimeout(resizeTimer);
    resizeTimer = setTimeout(() => rows.forEach(r => fitPills(r._p.pills)), 80);
  });
  ro.observe(list);

  const api = {
    update(items, emptyHtml) {
      last = items;
      const sorted = [...items].sort((a, b) => (opts.group ? opts.group(a) - opts.group(b) : 0) || cmp(a, b));
      const seen = new Set();
      let prev = null;
      for (const r of sorted) {
        const k = opts.key(r);
        seen.add(k);
        let el = rows.get(k);
        if (!el) { el = makeRow(); rows.set(k, el); }
        el._row = r;
        const v = opts.render(r);
        const cls = ('mrow ' + (v.cls || '')).trim();
        if (el.className !== cls) el.className = cls;
        el.setAttribute('aria-label', r.title);
        if (v.tip) el.title = v.tip; else el.removeAttribute('title');
        for (const p of PARTS) {
          if (p === 'act' && el._p.act.querySelector('.btn.confirm')) continue;
          const before = el._p[p]._html;
          put(el._p[p], v[p] || '');
          if (p === 'pills' && before !== el._p[p]._html) el._fit = true;
        }
        const next = prev ? prev.nextSibling : list.firstChild;
        if (next !== el) list.insertBefore(el, next);
        prev = el;
      }
      for (const [k, el] of rows) if (!seen.has(k)) { el.remove(); rows.delete(k); }
      rows.forEach(el => { if (el._fit) { el._fit = false; fitPills(el._p.pills); } });
      emptyEl.hidden = sorted.length > 0;
      if (!sorted.length) put(emptyEl, emptyHtml || 'Nothing to show.');
    },
    rows() { return rows; },
    destroy() { ro.disconnect(); },
  };
  return api;
}
