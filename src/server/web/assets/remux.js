// Remux: every title matched against its source ISO, which freemkv muxed it,
// and the queue that remuxes out-of-date titles through the engine. The MKV
// Remux prototype's page, in the brand.

import { esc, $, put, fill, api, act, toast, twoStep, confirmDialog, modal, bytes, runtime, when, hms, speed, plural, ICON, menu, updated } from './ui.js';
import { watch, refreshNow } from './libdata.js';
import { mediaList } from './medialist.js';
import { chipFilter } from './chips.js';
import { muxedHtml, openDetails, noteText, queueOne, outdated } from './details.js';
import { dotHtml } from './library.js';
import { openConsole, openTitleLog } from './console.js';
import { folderBanner, queuedNote, stagedPill, stagedActions } from './folders.js';

const can = r => r.kind === 'remux' || r.kind === 'iso_only';
const active = r => r.job && (r.job.state === 'queued' || r.job.state === 'running');
// The row's state as pills: live progress, queued (and why it waits), done or failed.
function statePills(r, hold) {
  const j = r.job, res = r.result;
  const log = (j || res) ? '<button class="pill link" type="button" data-log>' + (j && j.state === 'running' ? 'console' : 'log') + '</button>' : '';
  const outputs = j && j.outputs && j.outputs.length > 1
    ? '<span class="pill info" data-keep="1">TV · ' + j.outputs.filter(o => o.state === 'done').length + '/' + j.outputs.length + ' episodes</span>'
    : '';
  if (j && j.state === 'running') {
    // Static markup: the bar and its text are filled in place by paintLive.
    return outputs + '<span class="pill live" data-keep="1"><span class="cellbar" data-live="' + j.id + '"><span class="bar"><i></i></span><span class="txt">starting…</span></span></span>' + log;
  }
  if (j && j.staged) return outputs + stagedPill(j, Date.now() / 1000) + log;
  if (j && j.state === 'queued') {
    const why = queuedNote(j, hold, Date.now() / 1000);
    const tip = j.failure ? ' title="' + esc(j.failure.message) + '"' : '';
    return outputs + '<span class="pill warn" data-keep="1"' + tip + '>Queued' + (why ? ' · ' + esc(why) : '') + '</span>' + log;
  }
  if (res && res.outcome === 'done') {
    return outputs + '<span class="pill ok" data-keep="1" title="Remuxed in ' + esc(runtime(res.secs) || res.secs + 's') + ', ' + esc(when(res.finished_at)) + '">✓ Done · ' + bytes(res.size_bytes) + '</span>' + log;
  }
  if (res && res.outcome === 'failed') {
    const f = (j && j.failure) || res;
    const msg = (f.code != null && !f.message.includes('E' + f.code) ? 'E' + f.code + ' ' : '') + f.message;
    const short = msg.replace(/^E(\d+)\s+(Error:\s*)?/, 'E$1 ');
    return outputs + '<span class="pill bad" data-keep="1" title="' + esc(msg) + '">✗ Failed · ' + esc(short.length > 48 ? short.slice(0, 48) + '…' : short) + '</span>' + log;
  }
  return outputs;
}

// A row's pills in order. The first one never folds into "+N", so a kept finished file
// leads: on a narrow row the version and the "no MKV yet" pills fold instead of it.
function rowPills(r, hold) {
  const lead = r.mkv ? '<span class="mux-pill" data-keep="1">' + muxedHtml(r) + '</span>'
    : r.kind === 'iso_only' && !active(r) ? '<span class="pill warn" data-keep="1">no MKV yet</span>' : '';
  const match = r.source_match ? '<span class="pill info">Matched: ' + esc(r.source_match.media.kind === 'movie' ? 'Movie' : 'TV') + '</span>' : '';
  const state = match + statePills(r, hold);
  return r.job && r.job.staged && r.job.state !== 'running' ? state + lead : lead + state;
}

function actHtml(r) {
  if (r.job && r.job.staged && r.job.state !== 'running') return stagedActions(r);
  if (r.job && r.job.state === 'queued') {
    return '<button class="btn btn-ghost btn-sm" data-unqueue title="Take it out of the queue" aria-label="Take ' + esc(r.title) + ' out of the queue">× Unqueue</button>';
  }
  if (active(r)) return '';
  if (!can(r)) return r.iso ? '<button class="btn btn-ghost btn-sm" data-match>Change match</button>' : '';
  const failed = r.result && r.result.outcome === 'failed';
  const label = failed ? 'Retry' : r.kind === 'iso_only' ? 'Create' : 'Remux';
  const tone = failed || r.needs_remux ? 'btn-primary' : 'btn-ghost';
  return '<button class="btn btn-ghost btn-sm" data-match>Change match</button><button class="btn btn-sm ' + tone + '" data-remux>' + label + '</button>';
}

async function changeMatch(row, button) {
  const current = await act(button, () => api('GET', '/api/library/match?source=' + encodeURIComponent(row.iso)), 'Load match');
  if (!current) return;
  let saved = current.saved;
  const owned = current.owned_outputs || [];
  const candidates = current.candidates || [];
  const m = modal({ title: 'Change movie or TV match', wide: true,
    body: '<p class="small muted">Choose the correct TMDB entry and remux this ISO. The complete new output set is verified before the old files are replaced or removed. The ISO is kept.</p>'
      + '<p class="mono small">' + esc(row.iso) + '</p>'
      + (owned.length ? '<p>Source-linked MKVs to replace:</p><ul>' + owned.map(path => '<li class="mono small">' + esc(path) + '</li>').join('') + '</ul>' : '<p>No existing MKVs have proven source links.</p>')
      + '<p>Unlinked files are left alone unless you explicitly confirm them below. Names are suggestions, not source proof.</p>'
      + (current.omitted_candidates ? '<p>Some indexed files are unavailable, unsafe, or aliases and were omitted. They cannot be authorized here and will be left alone.</p>' : '')
      + (candidates.length ? '<details><summary>Confirm legacy outputs belonging to this ISO</summary><p>Select only files you know came from this ISO. All start unchecked.</p>'
        + candidates.map(c => '<label class="hook"><input type="checkbox" data-legacy-output="' + c.id + '"><span class="mono small">' + esc(c.path) + '</span><span>' + bytes(c.size_bytes) + '</span></label>').join('')
        + '<label><input type="checkbox" data-confirm-ownership> I confirm the selected files belong to this ISO and may be removed after all replacement outputs are verified.</label></details>' : '')
      + '<div class="hook"><input class="txt" data-query aria-label="Search TMDB" value="' + esc(saved?.media.title || row.title) + '"><button class="btn btn-secondary" data-search>Search TMDB</button></div>'
      + '<label>Show <select data-kind><option value="">Movies and TV</option><option value="movie">Movies</option><option value="tv">TV</option></select></label>'
      + '<div data-results class="stack"></div>'
      + '<div class="hook"><label>TV season <input type="number" min="1" max="65535" data-season value="' + (saved?.media.season || 1) + '"></label>'
      + '<label>Disc number (optional) <input type="number" min="1" max="65535" data-disc value="' + (saved?.media.disc || '') + '"></label>'
      + '<label>TV First episode (optional) <input type="number" min="1" max="65535" step="1" data-episode-start value="' + esc(saved?.media.episode_start ?? '') + '"></label></div>',
  });
  let found = [], closed = false, searchRevision = 0;
  m.onClose(() => { closed = true; });
  const results = m.el.querySelector('[data-results]');
  const filter = m.el.querySelector('[data-kind]');
  const render = () => {
    results.innerHTML = found.map((c, i) => !filter.value || c.media_type === filter.value
      ? '<div class="pipe-row"><div class="grow">' + esc(c.title) + ' (' + esc(c.year || '') + ') <span class="badge">' + esc(c.media_type) + '</span></div><button class="btn btn-secondary" data-pick="' + i + '">Remux as ' + (c.media_type === 'tv' ? 'TV' : 'movie') + '</button></div>' : '').join('') || '<p>No matching results. Try a more specific search.</p>';
  };
  filter.onchange = render;
  const search = async () => {
    const revision = ++searchRevision;
    const value = m.el.querySelector('[data-query]').value.trim();
    found = [];
    results.innerHTML = '';
    if (!value) return;
    const response = await act(m.el.querySelector('[data-search]'), () => api('GET', '/api/tmdb/search?q=' + encodeURIComponent(value)), 'Search TMDB');
    if (closed || revision !== searchRevision || !Array.isArray(response)) return;
    found = response.filter(c => c.media_type === 'movie' || c.media_type === 'tv');
    render();
  };
  m.el.querySelector('[data-search]').onclick = search;
  m.el.querySelector('[data-query]').onkeydown = e => { if (e.key === 'Enter') { e.preventDefault(); search(); } };
  results.onclick = async e => {
    if (closed) return;
    const pick = e.target.closest('[data-pick]');
    if (!pick) return;
    const choice = found[Number(pick.dataset.pick)];
    if (!choice) return;
    const season = choice.media_type === 'tv' ? Number(m.el.querySelector('[data-season]').value) : null;
    const discValue = m.el.querySelector('[data-disc]').value;
    const disc = choice.media_type === 'tv' && discValue ? Number(discValue) : null;
    if ([season, disc].some(n => n !== null && (!Number.isInteger(n) || n < 1 || n > 65535))) { toast('Use valid season and disc numbers', 'info'); return; }
    const episodeInput = m.el.querySelector('[data-episode-start]');
    const episodeValue = episodeInput.value.trim();
    const episode_start = choice.media_type === 'tv' && episodeValue ? Number(episodeValue) : null;
    if (choice.media_type === 'tv' && (episodeInput.validity?.badInput || (episode_start !== null && (!Number.isInteger(episode_start) || episode_start < 1 || episode_start > 65535)))) {
      toast('First episode must be a whole number from 1 to 65535, or blank', 'info'); return;
    }
    const selected_candidates = [...m.el.querySelectorAll('[data-legacy-output]:checked')].map(node => Number(node.dataset.legacyOutput));
    const confirm_ownership = !!m.el.querySelector('[data-confirm-ownership]')?.checked;
    if (selected_candidates.length && !confirm_ownership) {
      toast('Confirm ownership of the selected legacy files before remuxing.', 'info'); return;
    }
    const response = await act(pick, () => api('POST', '/api/library/match/remux', {
      source: row.iso, expected_revision: saved?.revision || 0,
      media: { title: choice.title, year: choice.year || 0, tmdb_id: choice.tmdb_id, kind: choice.media_type, season, disc, episode_start },
      ownership: { preview_token: current.preview_token, selected_candidates, confirm_ownership },
    }), 'Save match');
    if (!response) { m.close(); refreshNow(); return; }
    saved = response.saved;
    if (response.queued !== 1) {
      toast('Match saved, but no remux was queued. Refresh the Library to check its status.', 'info');
      m.close();
      refreshNow();
      return;
    }
    toast('Queued remux as ' + choice.title, 'ok');
    m.close(); refreshNow();
  };
}

function liveText(p) {
  if (!p) return 'starting…';
  if (p.stopping) return 'Stopping — waiting for storage to return';
  if (p.pct == null) return p.phase === 'start' ? 'starting…' : p.phase + '…';
  return p.pct.toFixed(1) + '% · ' + speed(p.speed_bps) + (p.eta_secs != null ? ' · ETA ' + hms(p.eta_secs) : '')
    + (p.phase !== 'mux' ? ' · ' + p.phase : '');
}

// The ISO list: every row that has an ISO, or several that match one title.
const isIsoRow = r => !!r.iso || r.kind === 'ambiguous';

function isoName(r) {
  return r.iso ? String(r.iso).split('/').pop() : noteText(r);
}

// The MKV's file name, only when it is not the expected Title/Title.mkv.
function mkvHtml(r) {
  if (!r.mkv) return '';
  const parts = String(r.mkv).split('/');
  const name = parts.pop();
  if (name === parts.pop() + '.mkv') return '';
  return '<span class="one" title="' + esc(r.mkv) + '"><span class="ell iso">' + esc(name) + '</span></span>';
}

export default {
  title: 'Remux',
  mount(view, ctx) {
    view.innerHTML = `
      <div class="page-head">
        <div><h1>Remux</h1><p class="lede" id="lede">Loading…</p></div>
        <div class="actions">
          <button class="btn btn-primary" id="ood" disabled>Remux out of date</button>
          <button class="btn btn-secondary" id="all" disabled>Remux all</button>
          <button class="btn btn-ghost" id="console">${ICON.term} Console</button>
          <div class="menu"><button class="icon-btn" id="more" aria-label="More queue actions">${ICON.more}</button>
            <div class="menu-list" id="more-list" hidden>
              <button data-a="clear">Clear finished…</button>
              <button data-a="rescan">Rescan the folders now</button>
              <hr>
              <label title="Write extra detail into each remux's log, for bug reports"><input type="checkbox" id="debug"> Detailed logs</label>
            </div></div>
        </div>
      </div>
      <div id="folders" class="folder-banners"></div>
      <div class="stats" id="stats"></div>
      <div class="now" id="now" hidden>
        <span class="now-ico" id="now-ico"></span>
        <div class="now-main"><b id="now-t"></b><span class="muted small" id="now-s"></span></div>
        <div class="bar" id="now-barbox"><i id="now-bar"></i></div>
        <div class="actions"><button class="btn btn-ghost btn-sm" id="now-console">${ICON.term} Console</button><button class="btn btn-ghost btn-sm" id="now-pause">Pause</button><button class="btn btn-ghost btn-sm" id="now-stop">Stop all</button></div>
      </div>
      <div class="table-card">
        <div class="toolbar">
          <label class="search">${ICON.search}<span class="sr-only">Search titles</span><input id="q" type="search" placeholder="Search titles" autocomplete="off"></label>
          <div class="sort" id="sort"></div>
          <span class="upd" id="upd"></span>
          <a class="dl-json" href="/api/library?download=1" download title="Download the ISO and queue listing as a JSON file">↓ JSON</a>
        </div>
        <div id="tbl"></div>
      </div>
      <p class="foot-note">Remux rebuilds media from its disc copy (ISO) with this version of freemkv, and only replaces existing files once the new output checks out. TV discs may produce several episode files. One job at a time; rips always go first. Titles that match several disc copies are greyed out and left alone.</p>`;
    let last = null, q = '';
    const chips = chipFilter($('#stats', view), { store: 'remuxFilter', onChange: () => paint() });
    $('#tbl', view).classList.add('wrap-m');
    const list = mediaList($('#tbl', view), {
      key: r => r.key + '|' + (r.target || r.mkv || r.iso || ''),
      store: 'remuxSort3',
      sortHost: $('#sort', view),
      defaultSort: { id: 'need', dir: 1 },
      group: r => can(r) ? 0 : 1,
      sorts: {
        need: ['Needs a remux', r => (outdated(r) ? '0' : !r.mkv ? '1' : '2') + r.title.toLowerCase()],
        title: ['Title', r => r.title.toLowerCase()],
        state: ['State', r => r.job ? ({ running: 0, queued: 1 }[r.job.state] ?? 3) : r.result ? (r.result.outcome === 'failed' ? 2 : 4) : 5],
        muxed: ['Muxed with', r => (r.needs_remux ? '0' : '1') + (r.muxed_label || '')],
        finished: ['Finished', r => (r.result && r.result.finished_at) || 0],
        iso: ['ISO name', r => isoName(r).toLowerCase()],
      },
      onRow: (r) => openDetails(r),
      render: (r) => ({
        cls: !can(r) ? 'grey' : r.needs_remux ? 'need' : '',
        dot: dotHtml(r),
        title: esc(r.title),
        tip: r.iso ? 'ISO: ' + r.iso : noteText(r),
        meta: r.kind === 'ambiguous' ? '<span>' + esc(noteText(r)) + '</span>' : '',
        pills: rowPills(r, last && last.hold),
        side: mkvHtml(r),
        act: actHtml(r),
      }),
    });
    ctx.cleanup.push(() => list.destroy());
    $('#tbl', view).addEventListener('click', async (e) => {
      const tr = e.target.closest('.mrow');
      const r = tr && tr._row;
      if (!r) return;
      const match = e.target.closest('button[data-match]');
      if (match) { e.stopPropagation(); await changeMatch(r, match); return; }
      const sr = e.target.closest('button[data-staged-retry]');
      if (sr) {
        e.stopPropagation();
        const res = await act(sr, () => api('POST', '/api/library/staged/retry', { target: r.target }), 'Retry now');
        if (res) { toast('Copying ' + r.title + ' in next', 'ok'); refreshNow(); }
        return;
      }
      const sd = e.target.closest('button[data-staged-discard]');
      if (sd) {
        e.stopPropagation();
        const size = r.job && r.job.staged_bytes != null ? ' (' + bytes(r.job.staged_bytes) + ')' : '';
        const ok = await confirmDialog({
          title: 'Discard the finished file?',
          body: 'Deletes the finished MKV of ' + r.title + size + ' from local staging. '
            + (r.mkv ? 'The MKV in the library is untouched.' : 'Nothing was written to the library.')
            + ' A fresh remux muxes it again from the ISO.',
          action: 'Discard',
        });
        if (!ok) return;
        const res = await act(sd, () => api('POST', '/api/library/staged/discard', { target: r.target }), 'Discard');
        if (res) { toast('Discarded the finished file of ' + r.title, 'info'); refreshNow(); }
        return;
      }
      const rb = e.target.closest('button[data-remux]');
      if (rb) { e.stopPropagation(); twoStep(rb, (b) => queueOne(r, b)); return; }
      const ub = e.target.closest('button[data-unqueue]');
      if (ub) {
        e.stopPropagation();
        act(ub, async () => {
          const res = await api('POST', '/api/library/queue/remove', { target: r.target });
          toast(res.removed ? 'Took ' + r.title + ' out of the queue' : r.title + ' had already started', res.removed ? 'ok' : 'info');
          refreshNow();
        }, 'Remove');
        return;
      }
      const lg = e.target.closest('[data-log]');
      if (lg) {
        e.preventDefault();
        e.stopPropagation();
        if (r.job && r.job.state === 'running') openConsole(); else openTitleLog(r.title);
      }
    });

    // ── Header actions ──
    $('#ood', view).addEventListener('click', (e) => bulk(e.currentTarget, '/api/library/queue/out-of-date', 'out of date'));
    $('#all', view).addEventListener('click', (e) => twoStep(e.currentTarget, (b) => bulk(b, '/api/library/queue/all', 'with an ISO')));
    $('#console', view).addEventListener('click', () => openConsole());
    $('#now-console', view).addEventListener('click', () => openConsole());
    $('#now-pause', view).addEventListener('click', async (e) => {
      const paused = last && last.queue && last.queue.paused;
      const r = await act(e.currentTarget, () => api('POST', paused ? '/api/library/queue/resume' : '/api/library/queue/pause'), paused ? 'Resume' : 'Pause');
      if (r) { toast(r.paused ? 'Paused. The title being remuxed finishes first.' : 'Resumed', 'info'); refreshNow(); }
    });
    $('#now-stop', view).addEventListener('click', async (e) => {
      const qd = (last && last.queue) || {};
      const run = last && last.live;
      const n = qd.queued || 0;
      const parts = [];
      if (run) parts.push('Cancels ' + run.title);
      if (n) parts.push((run ? 'removes ' : 'Removes ') + plural(n, 'queued title'));
      const ok = await confirmDialog({
        title: 'Stop all remuxes?',
        body: (parts.join(' and ') || 'Nothing is running') + '. Existing MKVs are untouched.',
        action: 'Stop all',
      });
      if (!ok) return;
      const r = await act(e.currentTarget, () => api('POST', '/api/library/queue/stop-all'), 'Stop all');
      if (r) { toast((r.stopped ? 'Stop requested; waiting for the running job to finish cancelling' : 'Stopped') + (r.removed ? '; removed ' + plural(r.removed, 'queued title') : ''), 'info'); refreshNow(); }
    });
    $('#q', view).addEventListener('input', (e) => { q = e.target.value.trim().toLowerCase(); paint(); });
    menu($('#more', view), $('#more-list', view));
    $('#debug', view).addEventListener('change', async (e) => {
      const on = e.target.checked;
      const r = await act(null, () => api('POST', '/api/library/debug', { enabled: on }), 'Debug log');
      if (r) toast(r.debug_log ? 'Remux logs now include debug lines' : 'Debug lines off', 'info');
      else e.target.checked = !on;
    });
    $('#more-list', view).addEventListener('click', async (e) => {
      const b = e.target.closest('button');
      if (!b) return;
      if (b.dataset.a === 'clear') {
        const n = ((last && last.queue && last.queue.done) || 0) + ((last && last.queue && last.queue.failed) || 0);
        const ok = await confirmDialog({
          title: 'Clear finished jobs?',
          body: (n ? 'Removes ' + plural(n, 'done or failed job') + ' from the list.' : 'There are no finished jobs.') + ' Each title keeps its last result.',
          action: 'Clear finished',
        });
        if (!ok) return;
        const r = await act(null, () => api('POST', '/api/library/queue/clear'), 'Clear finished');
        if (r) { toast(r.cleared ? 'Cleared ' + plural(r.cleared, 'finished job') : 'Nothing finished to clear', 'info'); refreshNow(); }
      } else if (b.dataset.a === 'rescan') {
        if (await act(null, () => api('POST', '/api/library/rescan'), 'Rescan')) toast('Rescanning the folders', 'info');
      }
    });

    async function bulk(btn, url, what) {
      const r = await act(btn, () => api('POST', url), 'Queue');
      if (!r) return;
      if (r.queued) {
        const already = r.eligible - r.queued;
        toast('Queued ' + plural(r.queued, 'title') + (already > 0 ? ' (' + already + ' already queued)' : ''), 'ok');
      } else {
        toast(r.eligible ? 'All ' + plural(r.eligible, 'title') + ' ' + what + ' are already queued' : 'Nothing to queue: every title with an ISO is current', 'info');
      }
      refreshNow();
    }

    function paint() {
      const d = last;
      if (!d) return;
      const rows = d.rows;
      const have = rows.filter(can);
      const qd = d.queue || {};
      put($('#lede', view), d.scanning ? 'Scanning the library and ISO folders…'
        : 'freemkv <b>' + esc(d.version_label) + '</b>'
          + (d.iso_dir ? ' · ISOs from <b>' + esc(d.iso_dir) + '</b>' + (d.iso_subfolders ? ' (and sub-folders)' : '') : ' · <span style="color:var(--warn)">no ISO folder set: <a href="/settings#Library" data-link>set one</a></span>')
          + (d.probing ? ' · reading ' + d.probing + ' headers' : ''));
      // Chips count ISOs and the queue. "Need a remux" is exactly what the
      // bulk button can queue: out of date (one definition, shared with the
      // Library) or no MKV yet.
      const isoRows = rows.filter(isIsoRow);
      const need = have.filter(r => r.needs_remux);
      const todo = need.filter(r => !active(r)).length;
      const waiting = need.length - todo;
      const stale = have.filter(outdated).length;
      const missing = have.filter(r => !r.mkv).length;
      const ambiguous = rows.filter(r => r.kind === 'ambiguous').length;
      const is = {
        all: () => true,
        need: r => can(r) && r.needs_remux,
        nomkv: r => can(r) && !r.mkv,
        queued: r => active(r),
        done: r => !!(r.result && r.result.outcome === 'done') && !active(r),
        failed: r => !!(r.result && r.result.outcome === 'failed') && !active(r),
        ambiguous: r => r.kind === 'ambiguous',
      };
      const n = (f) => isoRows.filter(is[f]).length;
      chips.update([
        { id: 'all', label: 'ISOs', count: isoRows.length },
        { id: 'need', label: 'need a remux', count: need.length, tone: 'warn', tip: stale + ' out of date, ' + missing + ' with no MKV yet' },
        { id: 'nomkv', label: 'no MKV yet', count: missing, tip: 'A remux creates the MKV' },
        { id: 'queued', label: 'queued', count: n('queued'), tip: 'Queued or running' },
        { id: 'done', label: 'done', count: n('done'), tone: 'ok' },
        { id: 'failed', label: 'failed', count: n('failed'), tone: 'bad' },
        { id: 'ambiguous', label: 'ambiguous', count: ambiguous, tone: 'warn', hideEmpty: true, tip: 'Several ISOs match one title: rename one' },
      ]);
      const filter = is[chips.selected()] || is.all;
      const ood = $('#ood', view), all = $('#all', view);
      if (!ood.classList.contains('busy')) {
        ood.disabled = !todo || d.scanning;
        put(ood, todo ? (waiting ? 'Queue ' + todo + ' more' : 'Remux ' + todo + ' out of date') : (waiting ? 'All out of date queued' : 'Nothing out of date'));
        ood.title = todo + ' of the ' + need.length + ' that need a remux, not yet queued' + (waiting ? '; ' + waiting + ' already queued or running' : '');
      }
      if (!all.classList.contains('busy') && !all.classList.contains('confirm')) {
        all.disabled = !have.length || d.scanning;
        put(all, 'Remux all ' + have.length);
        all._label = all.innerHTML;
      }
      $('#debug', view).checked = !!qd.debug_log;
      const shown = isoRows.filter(r => filter(r) && (!q || r.title.toLowerCase().includes(q) || isoName(r).toLowerCase().includes(q)));
      list.update(shown, d.scanning ? 'Scanning…' : isoRows.length ? 'No ISOs match.'
        : d.iso_dir ? 'No ISOs in ' + esc(d.iso_dir) + '.' : 'Set the source ISO folder in <a href="/settings#Library" data-link>Settings</a>.');
      paintLive(d.live);
    }

    // Progress straight off the SSE frame, into the existing bars: no re-render.
    // The one activity strip: shown only while something is running or queued.
    function paintLive(live) {
      const qd = (last && last.queue) || {};
      const queued = qd.queued || 0;
      $('#now', view).hidden = !live && !queued;
      const paused = !!qd.paused;
      put($('#now-ico', view), paused ? '⏸' : '<span class="pulse"></span>');
      if (paused) {
        put($('#now-t', view), 'Paused');
        put($('#now-s', view), plural(queued, 'title') + ' queued' + (live ? ' · ' + esc(live.title) + ' finishes first' : ''));
      } else if (live) {
        put($('#now-t', view), 'Remuxing ' + esc(live.title));
        put($('#now-s', view), esc(liveText(live)) + (queued ? ' · ' + queued + ' more queued' : ''));
      } else if (last && last.hold) {
        put($('#now-t', view), 'Waiting for the ' + esc(last.hold.role) + ' folder');
        put($('#now-s', view), plural(queued, 'title') + ' queued · ' + esc(last.hold.message) + ' Starts on its own once the folder answers.');
      } else {
        put($('#now-t', view), 'Waiting to start');
        put($('#now-s', view), plural(queued, 'title') + ' queued · starts when no rip needs the drive');
      }
      $('#now-barbox', view).hidden = !live;
      if (live) fill($('#now-bar', view), live.pct);
      const pb = $('#now-pause', view);
      if (!pb.classList.contains('busy')) pb.textContent = paused ? 'Resume' : 'Pause';
      const cell = view.querySelector('.cellbar[data-live]');
      if (!cell) return;
      const p = live && String(live.job_id) === cell.dataset.live ? live : null;
      fill(cell.querySelector('.bar i'), p ? p.pct : 0);
      const txt = cell.querySelector('.txt');
      if (txt && txt.firstChild && txt.firstChild.nodeType === 3) txt.firstChild.nodeValue = liveText(p);
    }

    ctx.cleanup.push(watch((d, err, liveOnly) => {
      if (err && !d) { put($('#lede', view), '<span style="color:var(--bad)">Could not load the library: ' + esc(err.message) + '</span>'); return; }
      const held = !!(last && last.hold) !== !!d.hold;
      last = d;
      if (liveOnly && !held) paintLive(d.live); else paint();
      put($('#folders', view), folderBanner(d.folders, d.hold, true));
    }));
  },
};
