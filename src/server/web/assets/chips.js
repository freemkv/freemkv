// The filter chips both lists use: one is selected at a time, "All" resets.
// One component so the Library and Remux pages cannot drift apart.

import { esc, put } from './ui.js';

/**
 * host: the element to fill. store: sessionStorage key for the selection.
 * onChange(id): called after the selection changes.
 * Returns { update(defs), selected() } where defs is
 * [{ id, label, count, tone?: 'warn' | 'bad' | 'ok', tip?, hideEmpty? }].
 */
export function chipFilter(host, { store, onChange }) {
  let current = sessionStorage.getItem(store) || 'all';
  let ids = [];
  host.addEventListener('click', (e) => {
    const b = e.target.closest('button[data-f]');
    if (!b) return;
    current = b.dataset.f;
    sessionStorage.setItem(store, current);
    onChange(current);
  });
  return {
    update(defs) {
      const shown = defs.filter(d => !(d.hideEmpty && !d.count) || d.id === current);
      ids = shown.map(d => d.id);
      if (!ids.includes(current)) current = 'all';
      put(host, shown.map(d => {
        const tone = d.count && d.tone ? ' ' + d.tone : '';
        const on = d.id === current;
        return '<button class="stat' + tone + (on ? ' on' : '') + '" data-f="' + esc(d.id) + '" aria-pressed="' + on + '"'
          + (d.tip ? ' title="' + esc(d.tip) + '"' : '') + '><b>' + d.count + '</b> ' + esc(d.label) + '</button>';
      }).join(''));
    },
    selected() { return current; },
  };
}
