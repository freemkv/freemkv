// Library: every MKV in the library with its audit verdict, runtime, tracks,
// who muxed it and when it landed. The mkv-audit status page, in the brand.

import { esc, $, put, api, act, toast, runtime, bytes, date, ago, plural, ICON, menu } from './ui.js';
import { watch, refreshNow } from './libdata.js';
import { mediaList } from './medialist.js';
import { muxedHtml, auditState, openDetails, outdated, videoLabel, issueText } from './details.js';

const FILTERS = [
  ['all', 'All'],
  ['issues', 'Issues'],
  ['outdated', 'Out of date'],
  ['pending', 'Not audited'],
];

/** The status dot, with what it means in its tooltip. */
export function dotHtml(r) {
  const [cls, glyph, tip] = auditState(r);
  if (!r.mkv) return '<span class="sdot dot-idle" title="No MKV yet"></span>';
  return '<span class="sdot ' + cls + (glyph === '✓' ? ' tick' : '') + '" title="' + esc(tip) + '" aria-label="' + esc(tip) + '">' + (glyph === '✓' ? '✓' : '') + '</span>';
}

/** Runtime and size, muted, after the title. */
export function metaHtml(r) {
  const bits = [];
  if (r.audit && r.audit.duration_secs) bits.push(runtime(r.audit.duration_secs));
  if (r.size_bytes) bits.push(bytes(r.size_bytes));
  return bits.map(b => '<span>' + esc(b) + '</span>').join('');
}

/** The track pills: video, audio codecs and count, subtitles, languages. */
export function trackPills(r) {
  const a = r.audit;
  if (!r.mkv) return '';
  if (!a) return '<span class="pill ghost">' + (r.probed ? 'auditing…' : 'probing…') + '</span>';
  const out = [];
  videoLabel(a.video).forEach(v => out.push('<span class="pill strong">' + esc(v) + '</span>'));
  [...new Set(a.audio.map(t => t.codec))].forEach(c => out.push('<span class="pill">' + esc(c) + '</span>'));
  if (a.audio.length) out.push('<span class="pill ghost" title="' + esc(a.audio.map(t => t.codec + ' ' + t.language).join(', ')) + '">' + plural(a.audio.length, 'audio', 'audio') + '</span>');
  if (a.subtitles.length) out.push('<span class="pill ghost" title="' + esc(a.subtitles.join(', ')) + '">' + a.subtitles.length + ' subs</span>');
  const langs = [...new Set(a.audio.map(t => t.language).filter(Boolean))];
  if (langs.length) out.push('<span class="pill text" title="Audio languages: ' + esc(langs.join(', ')) + '">' + esc(langs.slice(0, 3).join(' ') + (langs.length > 3 ? ' +' + (langs.length - 3) : '')) + '</span>');
  a.issues.forEach(i => out.push('<span class="pill ' + (i.kind === 'no_cues' ? 'warn' : 'bad') + '" title="' + esc(issueText(i)) + '">' + esc(i.kind === 'runtime_mismatch' ? 'truncated?' : i.kind.replace(/_/g, ' ')) + '</span>'));
  return out.join('');
}

function matches(r, f, q) {
  if (q && !r.title.toLowerCase().includes(q)) return false;
  if (f === 'issues') return auditState(r)[0] === 'dot-bad';
  if (f === 'outdated') return outdated(r);
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
          <button class="icon-btn legend-btn" id="legend-btn" aria-label="What the status dots mean" aria-expanded="false">i</button>
          <div class="legend" id="legend">
            <span><span class="sdot dot-ok tick">✓</span> checks out</span>
            <span><span class="sdot dot-warn"></span> pending or a note</span>
            <span><span class="sdot dot-bad"></span> issue</span>
          </div>
          <div class="sort" id="sort"></div>
        </div>
        <div id="list"></div>
      </div>
      <p class="foot-note">The audit is the fast structural pass: EBML header, tracks, declared duration, and the seek index agreeing with it. It never reads the video itself. Click a title for its full track list, findings and files.</p>`;
    let filter = sessionStorage.getItem('libFilter') || 'all';
    let q = '';
    let last = null;
    const list = mediaList($('#list', view), {
      key: r => r.mkv,
      store: 'libSort2',
      sortHost: $('#sort', view),
      defaultSort: { id: 'title', dir: 1 },
      sorts: {
        title: ['Title', r => r.title.toLowerCase()],
        runtime: ['Runtime', r => (r.audit && r.audit.duration_secs) || 0],
        size: ['Size', r => r.size_bytes || 0],
        muxed: ['Muxed with', r => (outdated(r) ? '0' : '1') + (r.muxed_label || '')],
        status: ['Status', r => -auditState(r)[3]],
        ripped: ['Ripped', r => r.modified || 0],
      },
      onRow: (r) => openDetails(r, { remux: false }),
      render: (r) => ({
        dot: dotHtml(r),
        title: esc(r.title),
        meta: metaHtml(r),
        pills: '<span class="m-only">' + muxedHtml(r) + '</span>' + trackPills(r) + (r.modified ? '<span class="pill text" title="Ripped">' + esc(date(r.modified)) + '</span>' : ''),
        side: muxedHtml(r),
        act: '<button class="icon-btn row-more" aria-label="Details for ' + esc(r.title) + '">' + ICON.more + '</button>',
      }),
    });
    ctx.cleanup.push(() => list.destroy());
    $('#list', view).addEventListener('click', (e) => {
      const b = e.target.closest('.row-more');
      if (b) { e.stopPropagation(); openDetails(b.closest('.mrow')._row, { remux: false }); }
    });
    menu($('#more', view), $('#more-list', view));
    $('#more-list', view).addEventListener('click', async (e) => {
      const b = e.target.closest('button');
      if (!b) return;
      if (b.dataset.a === 'rescan') {
        if (await act(null, () => api('POST', '/api/library/rescan'), 'Rescan') !== undefined) {
          toast('Rescanning the library folders', 'info');
        }
      } else if (b.dataset.a === 'reaudit') {
        const r = await act(null, () => api('POST', '/api/library/reaudit', { all: true }), 'Re-audit');
        if (r) { toast('Re-auditing ' + plural(r.requeued, 'file'), 'info'); refreshNow(); }
      } else if (b.dataset.a === 'json') {
        window.open('/api/library', '_blank');
      }
    });
    $('#legend-btn', view).addEventListener('click', (e) => {
      const open = $('#legend', view).classList.toggle('open');
      e.currentTarget.setAttribute('aria-expanded', String(open));
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
        const tip = f === 'outdated' ? ' title="MKVs not muxed by this freemkv. The same count as the Remux page."' : '';
        return '<button class="stat' + tone + (filter === f ? ' on' : '') + '" data-f="' + f + '" aria-pressed="' + (filter === f) + '"' + tip + '><b>' + count[f] + '</b> ' + label + '</button>';
      }).join(''));
      const empty = d.scanning ? 'Scanning the library…'
        : !files.length ? 'No MKVs found in ' + esc(d.library_dir) + '. Set the Library folder in <a href="/settings" data-link>Settings</a>.'
        : 'No titles match.';
      list.update(files.filter(r => matches(r, filter, q)), empty);
    }
    ctx.cleanup.push(watch((d, err, liveOnly) => {
      if (err && !d) { put($('#lede', view), '<span style="color:var(--bad)">Could not load the library: ' + esc(err.message) + '</span>'); return; }
      if (liveOnly) return;
      last = d;
      paint();
    }));
  },
};
