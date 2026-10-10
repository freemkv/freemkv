// Remux: every title matched against its source ISO, which freemkv muxed it,
// and the queue that remuxes out-of-date titles through the engine. The MKV
// Remux prototype's page, in the brand.

import { esc, $, put, fill, api, act, toast, twoStep, confirmDialog, bytes, runtime, when, hms, speed, plural, ICON, menu, updated } from './ui.js';
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
  const state = statePills(r, hold);
  return r.job && r.job.staged && r.job.state !== 'running' ? state + lead : lead + state;
}

function actHtml(r) {
  if (r.job && r.job.staged && r.job.state !== 'running') return stagedActions(r);
  if (r.job && r.job.state === 'queued') {
    return '<button class="btn btn-ghost btn-sm" data-unqueue title="Take it out of the queue" aria-label="Take ' + esc(r.title) + ' out of the queue">× Unqueue</button>';
  }
  if (!can(r) || active(r)) return '';
  const failed = r.result && r.result.outcome === 'failed';
  const label = failed ? 'Retry' : r.kind === 'iso_only' ? 'Create' : 'Remux';
  const tone = failed || r.needs_remux ? 'btn-primary' : 'btn-ghost';
  return '<button class="btn btn-sm ' + tone + '" data-remux>' + label + '</button>';
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
