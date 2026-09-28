// Library: every MKV in the library with its audit verdict, runtime, tracks,
// who muxed it and when it landed. The mkv-audit status page, in the brand.

import { esc, $, put, api, act, toast, runtime, bytes, date, ago, plural, ICON, menu } from './ui.js';
import { watch, refreshNow } from './libdata.js';
import { keyedTable } from './table.js';
import { muxedHtml, auditHtml, auditState, openDetails } from './details.js';

const FILTERS = [
  ['all', 'All'],
  ['issues', 'Issues'],
  ['outdated', 'Out of date'],
  ['pending', 'Not audited'],
];

function langs(list, cap = 6) {
  const seen = [...new Set(list.filter(Boolean))];
  return seen.slice(0, cap).join(' ') + (seen.length > cap ? ' +' + (seen.length - cap) : '');
}

const cols = [
  { id: 'title', label: 'Title', cls: 'title', sort: r => r.title.toLowerCase(),
    html: r => '<b>' + esc(r.title) + '</b>' },
  { id: 'status', label: 'Status', sort: r => auditState(r)[3], html: auditHtml },
  { id: 'runtime', label: 'Runtime', cls: 'nowrap', sort: r => (r.audit && r.audit.duration_secs) || 0,
    html: r => r.audit ? '<span class="muted small">' + runtime(r.audit.duration_secs) + '</span>' : '<span class="muted small">…</span>' },
  { id: 'video', label: 'Video', hideSm: true, sort: r => (r.audit && r.audit.video.join(' ')) || '',
    html: r => r.audit ? '<span class="chips">' + r.audit.video.map(v => '<span class="tag">' + esc(v) + '</span>').join('') + '</span>' : '' },
  { id: 'audio', label: 'Audio', hideSm: true, sort: r => (r.audit && r.audit.audio.length) || 0,
    html: r => {
      if (!r.audit) return '';
      const a = r.audit.audio;
      if (!a.length) return '<span class="muted small">none</span>';
      const codecs = [...new Set(a.map(t => t.codec))];
      return '<span class="chips">' + codecs.map(c => '<span class="tag">' + esc(c) + '</span>').join('') + '</span>'
        + '<div class="note" title="' + esc(a.map(t => t.codec + ' ' + t.language).join(' | ')) + '">' + plural(a.length, 'track') + ' · ' + esc(langs(a.map(t => t.language))) + '</div>';
    } },
  { id: 'subs', label: 'Subtitles', hideSm: true, sort: r => (r.audit && r.audit.subtitles.length) || 0,
    html: r => r.audit ? (r.audit.subtitles.length ? '<span class="small">' + r.audit.subtitles.length + '</span> <span class="note">' + esc(langs(r.audit.subtitles)) + '</span>' : '<span class="muted small">none</span>') : '' },
  { id: 'muxed', label: 'Muxed with', cls: 'nowrap', sort: r => r.muxed_label || '', html: muxedHtml },
  { id: 'size', label: 'Size', cls: 'num', hideSm: true, sort: r => r.size_bytes || 0, html: r => '<span class="small">' + bytes(r.size_bytes) + '</span>' },
  { id: 'modified', label: 'Ripped', cls: 'nowrap', sort: r => r.modified || 0, html: r => '<span class="muted small">' + date(r.modified) + '</span>' },
];

function matches(r, f, q) {
  if (q && !r.title.toLowerCase().includes(q)) return false;
  if (f === 'issues') return auditState(r)[0] === 'dot-bad';
  if (f === 'outdated') return r.probed && r.muxed_with && (r.muxed_with.state === 'older' || r.muxed_with.state === 'other');
  if (f === 'pending') return !r.audit;
  return true;
}

export default {
  title: 'Library',
  mount(view, ctx) {
    view.innerHTML = `
      <div class="page-head">
        <div><h1>Library</h1><p class="lede" id="lede">Loading…</p></div>
        <div class="actions">
          <a class="btn btn-secondary" href="/remux" data-link>Remux…</a>
          <div class="menu"><button class="icon-btn" id="more" aria-label="More actions">${ICON.more}</button>
            <div class="menu-list" id="more-list" hidden>
              <button data-a="rescan">Rescan the folders now</button>
              <button data-a="reaudit">Re-audit every file</button>
              <hr><button data-a="json">Open the JSON</button>
            </div></div>
        </div>
      </div>
      <div class="stats" id="stats"></div>
      <div class="table-card">
        <div class="toolbar">
          <label class="search">${ICON.search}<span class="sr-only">Search titles</span><input id="q" type="search" placeholder="Search titles" autocomplete="off"></label>
          <div class="legend">
            <span><span style="color:var(--ok)">✓</span> structure checks out</span>
            <span><span style="color:#d97706">●</span> pending, or a note</span>
            <span><span style="color:var(--bad)">●</span> issue</span>
          </div>
        </div>
        <div id="tbl"></div>
      </div>
      <p class="foot-note">The audit is the fast structural pass: EBML header, tracks, declared duration, and the seek index agreeing with it. It never reads the video itself. Click a title for its tracks, findings and files.</p>`;
    let filter = sessionStorage.getItem('libFilter') || 'all';
    let q = '';
    let last = null;
    const table = keyedTable($('#tbl', view), cols, {
      key: r => r.mkv,
      store: 'libSort',
      onRow: (r) => openDetails(r),
    });
    menu($('#more', view), $('#more-list', view));
    $('#more-list', view).addEventListener('click', async (e) => {
      const b = e.target.closest('button');
      if (!b) return;
      if (b.dataset.a === 'rescan') {
        await act(null, () => api('POST', '/api/library/rescan'), 'Rescan');
        toast('Rescanning the library folders', 'info');
      } else if (b.dataset.a === 'reaudit') {
        const r = await act(null, () => api('POST', '/api/library/reaudit', { all: true }), 'Re-audit');
        if (r) { toast('Re-auditing ' + plural(r.requeued, 'file'), 'info'); refreshNow(); }
      } else if (b.dataset.a === 'json') {
        window.open('/api/library', '_blank');
      }
    });
    $('#q', view).addEventListener('input', (e) => { q = e.target.value.trim().toLowerCase(); paint(); });
    $('#stats', view).addEventListener('click', (e) => {
      const b = e.target.closest('button[data-f]');
      if (!b) return;
      filter = b.dataset.f;
      sessionStorage.setItem('libFilter', filter);
      paint();
    });

    function paint() {
      const d = last;
      if (!d) return;
      const files = d.rows.filter(r => r.mkv);
      const count = { all: files.length };
      for (const [f] of FILTERS.slice(1)) count[f] = files.filter(r => matches(r, f, '')).length;
      if (d.scanning) {
        put($('#lede', view), 'Scanning <b>' + esc(d.library_dir) + '</b> for the first time…');
      } else {
        put($('#lede', view), '<b>' + esc(d.library_dir) + '</b> · ' + plural(files.length, 'file')
          + (d.probing ? ' · reading ' + d.probing + ' headers' : '')
          + (d.auditing ? ' · auditing ' + d.auditing : '')
          + ' · scanned ' + ago(d.scanned_at)
          + (d.incomplete ? ' · <span style="color:var(--warn)">a folder could not be fully read</span>' : ''));
      }
      put($('#stats', view), FILTERS.map(([f, label]) => {
        const tone = f === 'issues' && count[f] ? ' bad' : f === 'outdated' && count[f] ? ' warn' : '';
        return '<button class="stat' + tone + (filter === f ? ' on' : '') + '" data-f="' + f + '" aria-pressed="' + (filter === f) + '"><b>' + count[f] + '</b> ' + label + '</button>';
      }).join(''));
      const empty = d.scanning ? 'Scanning the library…'
        : !files.length ? 'No MKVs found in ' + esc(d.library_dir) + '. Set the Library folder in <a href="/settings" data-link>Settings</a>.'
        : 'No titles match.';
      table.update(files.filter(r => matches(r, filter, q)), empty);
    }
    ctx.cleanup.push(watch((d, err, liveOnly) => {
      if (err && !d) { put($('#lede', view), '<span style="color:var(--bad)">Could not load the library: ' + esc(err.message) + '</span>'); return; }
      if (liveOnly) return;
      last = d;
      paint();
    }));
  },
};
