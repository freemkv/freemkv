// A keyed table: rows are matched by key and only changed cells are
// rewritten, so live updates never flicker, lose a hover or disarm a
// Confirm button. Column sort is remembered per table.

import { esc, put } from './ui.js';

/**
 * cols: [{ id, label, cls, sort: r => value, html: r => string, hideSm }]
 * opts: { key: r => string, rowClass: r => string, onRow: (r, ev) => void,
 *         store: 'localStorage key', defaultSort: { id, dir } }
 */
export function keyedTable(host, cols, opts) {
  host.innerHTML = '<div class="table-scroll"><table class="list"><thead><tr>'
    + cols.map(c => '<th class="' + (c.sort ? 'sortable ' : '') + (c.hideSm ? 'hide-sm ' : '') + (c.cls || '') + '" data-col="' + c.id + '"'
      + (c.sort ? ' tabindex="0" aria-sort="none"' : '') + '>' + esc(c.label) + (c.sort ? '<span class="ar"></span>' : '') + '</th>').join('')
    + '</tr></thead><tbody></tbody></table></div><div class="empty" hidden></div>';
  const tbody = host.querySelector('tbody');
  const emptyEl = host.querySelector('.empty');
  const rows = new Map();
  let sort = opts.defaultSort || { id: cols[0].id, dir: 1 };
  try { sort = JSON.parse(localStorage.getItem(opts.store)) || sort; } catch (e) { /* default */ }
  let last = [];

  const paintHeads = () => {
    host.querySelectorAll('th[data-col]').forEach(th => {
      const on = th.dataset.col === sort.id;
      const ar = th.querySelector('.ar');
      if (ar) ar.textContent = on ? (sort.dir > 0 ? ' ▲' : ' ▼') : '';
      if (th.hasAttribute('aria-sort')) th.setAttribute('aria-sort', on ? (sort.dir > 0 ? 'ascending' : 'descending') : 'none');
    });
  };
  const clickSort = (id) => {
    sort = { id, dir: sort.id === id ? -sort.dir : 1 };
    try { localStorage.setItem(opts.store, JSON.stringify(sort)); } catch (e) { /* ok */ }
    paintHeads();
    api.update(last);
  };
  host.querySelectorAll('th.sortable').forEach(th => {
    th.addEventListener('click', () => clickSort(th.dataset.col));
    th.addEventListener('keydown', (e) => { if (e.key === 'Enter' || e.key === ' ') { e.preventDefault(); clickSort(th.dataset.col); } });
  });
  paintHeads();

  const cmp = (a, b) => {
    const col = cols.find(c => c.id === sort.id) || cols[0];
    const x = col.sort ? col.sort(a) : '', y = col.sort ? col.sort(b) : '';
    const r = typeof x === 'number' && typeof y === 'number' ? x - y : String(x).localeCompare(String(y), undefined, { numeric: true, sensitivity: 'base' });
    return (r || String(opts.key(a)).localeCompare(String(opts.key(b)))) * sort.dir;
  };

  const api = {
    /** Show `list` (already filtered). */
    update(list, emptyHtml) {
      last = list;
      const sorted = (opts.group ? [...list].sort((a, b) => (opts.group(a) - opts.group(b)) || cmp(a, b)) : [...list].sort(cmp));
      const seen = new Set();
      let prev = null;
      for (const r of sorted) {
        const k = opts.key(r);
        seen.add(k);
        let tr = rows.get(k);
        if (!tr) {
          tr = document.createElement('tr');
          tr.innerHTML = cols.map(c => '<td class="' + (c.hideSm ? 'hide-sm ' : '') + (c.cls || '') + '"></td>').join('');
          if (opts.onRow) {
            tr.classList.add('clickable');
            tr.tabIndex = 0;
            tr.addEventListener('click', (e) => { if (!e.target.closest('button, a, input')) opts.onRow(tr._row, e); });
            tr.addEventListener('keydown', (e) => { if (e.key === 'Enter' && e.target === tr) opts.onRow(tr._row, e); });
          }
          rows.set(k, tr);
        }
        tr._row = r;
        const rc = opts.rowClass ? opts.rowClass(r) : '';
        const want = (opts.onRow ? 'clickable ' : '') + rc;
        if (tr.className !== want.trim()) tr.className = want.trim();
        cols.forEach((c, i) => {
          put(tr.children[i], c.html(r));
        });
        const next = prev ? prev.nextSibling : tbody.firstChild;
        if (next !== tr) tbody.insertBefore(tr, next);
        prev = tr;
      }
      for (const [k, tr] of rows) {
        if (!seen.has(k)) { tr.remove(); rows.delete(k); }
      }
      emptyEl.hidden = sorted.length > 0;
      if (!sorted.length) put(emptyEl, emptyHtml || 'Nothing to show.');
    },
    row(key) { return rows.get(key); },
    rows() { return rows; },
  };
  return api;
}
