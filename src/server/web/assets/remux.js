// Remux: every title matched against its source ISO, which freemkv muxed it,
// and the queue that remuxes out-of-date titles through the engine. The MKV
// Remux prototype's page, in the brand.

import { esc, $, put, fill, api, act, toast, twoStep, bytes, runtime, when, hms, speed, plural, ICON, menu } from './ui.js';
import { watch, refreshNow } from './libdata.js';
import { mediaList } from './medialist.js';
import { muxedHtml, openDetails, noteText, queueOne, outdated } from './details.js';
import { dotHtml, metaHtml } from './library.js';
import { openConsole, openTitleLog } from './console.js';

const can = r => r.kind === 'remux' || r.kind === 'iso_only';
const active = r => r.job && (r.job.state === 'queued' || r.job.state === 'running');
const NOTE = { restarted: 'after a restart', preempted: 'a rip took the slot', interrupted: 'interrupted', stalled: 'stalled' };

// The row's state as pills: live progress, queued, done or failed.
function statePills(r) {
  const j = r.job, res = r.result;
  const log = (j || res) ? '<button class="pill link" type="button" data-log>' + (j && j.state === 'running' ? 'console' : 'log') + '</button>' : '';
  if (j && j.state === 'running') {
    // Static markup: the bar and its text are filled in place by paintLive.
    return '<span class="pill live" data-keep="1"><span class="cellbar" data-live="' + j.id + '"><span class="bar"><i></i></span><span class="txt">starting…</span></span></span>' + log;
  }
  if (j && j.state === 'queued') {
    return '<span class="pill warn" data-keep="1">Queued' + (j.note ? ' · ' + esc(NOTE[j.note] || j.note) : '') + '</span>';
  }
  if (res && res.outcome === 'done') {
    return '<span class="pill ok" data-keep="1" title="Remuxed in ' + esc(runtime(res.secs) || res.secs + 's') + ', ' + esc(when(res.finished_at)) + '">✓ Done · ' + bytes(res.size_bytes) + '</span>' + log;
  }
  if (res && res.outcome === 'failed') {
    const f = (j && j.failure) || res;
    const msg = (f.code != null && !f.message.includes('E' + f.code) ? 'E' + f.code + ' ' : '') + f.message;
    const short = msg.replace(/^E(\d+)\s+(Error:\s*)?/, 'E$1 ');
    return '<span class="pill bad" data-keep="1" title="' + esc(msg) + '">✗ Failed · ' + esc(short.length > 48 ? short.slice(0, 48) + '…' : short) + '</span>' + log;
  }
  return '';
}

function actHtml(r) {
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
  if (p.pct == null) return p.phase === 'start' ? 'starting…' : p.phase + '…';
  return p.pct.toFixed(1) + '% · ' + speed(p.speed_bps) + (p.eta_secs != null ? ' · ETA ' + hms(p.eta_secs) : '')
    + (p.phase !== 'mux' ? ' · ' + p.phase : '');
}

function isoHtml(r) {
  if (!r.iso) return '<span class="note' + (r.kind === 'ambiguous' ? ' warn' : '') + '">' + esc(noteText(r)) + '</span>';
  const name = String(r.iso).split('/').pop();
  return '<span class="one" title="' + esc(r.iso) + (r.linked ? ' (recorded when this app ripped it)' : '') + '">' + (r.linked ? '<span class="badge badge-teal">linked</span>' : '') + '<span class="ell iso">' + esc(name) + '</span></span>';
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
          <button class="btn btn-ghost" id="pause">Pause queue</button>
          <button class="btn btn-ghost" id="console">${ICON.term} Console</button>
          <div class="menu"><button class="icon-btn" id="more" aria-label="More queue actions">${ICON.more}</button>
            <div class="menu-list" id="more-list" hidden>
              <button data-a="clear">Clear finished</button>
              <button data-a="clearq" data-two>Clear the queue</button>
              <button data-a="rescan">Rescan the folders now</button>
              <hr>
              <label title="Also log the engine's debug lines for each remux (shown in its log)"><input type="checkbox" id="debug"> Debug log</label>
            </div></div>
        </div>
      </div>
      <div class="stats" id="stats"></div>
      <div class="banner warn" id="paused" hidden style="margin:0 0 1rem">The queue is paused: the running title finishes, then nothing new starts until you resume.</div>
      <div class="table-card">
        <div class="toolbar">
          <label class="search">${ICON.search}<span class="sr-only">Search titles</span><input id="q" type="search" placeholder="Search titles" autocomplete="off"></label>
          <label class="small muted" style="display:inline-flex;gap:.4rem;align-items:center"><input type="checkbox" id="hide" style="accent-color:var(--teal)"> Hide titles with no ISO</label>
          <div class="sort" id="sort"></div>
        </div>
        <div id="tbl"></div>
      </div>
      <p class="foot-note">A remux re-muxes the main title from its ISO with this freemkv and only replaces the MKV once the new one verifies (or creates it when missing). Greyed titles have no single ISO and are never touched. One title at a time; a rip always takes priority. <a href="/api/library" target="_blank">JSON</a></p>`;
    let last = null, q = '';
    let hide = localStorage.getItem('remuxHide') === '1';
    $('#hide', view).checked = hide;
    const list = mediaList($('#tbl', view), {
      key: r => r.key + '|' + (r.target || r.mkv || r.iso || ''),
      store: 'remuxSort2',
      sortHost: $('#sort', view),
      defaultSort: { id: 'title', dir: 1 },
      group: r => can(r) ? 0 : 1,
      sorts: {
        title: ['Title', r => r.title.toLowerCase()],
        state: ['State', r => r.job ? ({ running: 0, queued: 1 }[r.job.state] ?? 3) : r.result ? (r.result.outcome === 'failed' ? 2 : 4) : 5],
        muxed: ['Muxed with', r => (r.needs_remux ? '0' : '1') + (r.muxed_label || '')],
        finished: ['Finished', r => (r.result && r.result.finished_at) || 0],
        size: ['Size', r => r.size_bytes || 0],
      },
      onRow: (r) => openDetails(r),
      render: (r) => ({
        cls: can(r) ? '' : 'grey',
        dot: dotHtml(r),
        title: esc(r.title),
        meta: metaHtml(r),
        pills: '<span class="mux-pill" data-keep="1">' + muxedHtml(r) + '</span>' + statePills(r),
        side: isoHtml(r),
        act: actHtml(r),
      }),
    });
    ctx.cleanup.push(() => list.destroy());
    $('#tbl', view).addEventListener('click', (e) => {
      const tr = e.target.closest('.mrow');
      const r = tr && tr._row;
      if (!r) return;
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
    $('#pause', view).addEventListener('click', async (e) => {
      const paused = last && last.queue && last.queue.paused;
      const r = await act(e.currentTarget, () => api('POST', paused ? '/api/library/queue/resume' : '/api/library/queue/pause'), paused ? 'Resume' : 'Pause');
      if (r) { toast(r.paused ? 'Queue paused: the running title finishes, then it waits' : 'Queue resumed', 'info'); refreshNow(); }
    });
    $('#hide', view).addEventListener('change', (e) => { hide = e.target.checked; localStorage.setItem('remuxHide', hide ? '1' : '0'); paint(); });
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
      if (b.dataset.two != null) {
        e.stopPropagation();
        twoStep(b, async () => {
          $('#more-list', view).hidden = true;
          const r = await act(null, () => api('POST', '/api/library/queue/clear-queued'), 'Clear the queue');
          if (r) { toast(r.removed ? 'Removed ' + plural(r.removed, 'queued title') : 'Nothing was queued', 'info'); refreshNow(); }
        });
        return;
      }
      if (b.dataset.a === 'clear') {
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
      // One definition of out of date everywhere (see details.outdated); the
      // button says exactly what it will queue.
      const stale = rows.filter(outdated);
      const staleNoIso = stale.filter(r => !can(r)).length;
      const missing = rows.filter(r => r.kind === 'iso_only' && !r.mkv).length;
      const todo = rows.filter(r => r.needs_remux && !active(r)).length;
      const waiting = rows.filter(r => r.needs_remux && active(r)).length;
      const stats = [
        [stale.length, 'out of date', stale.length ? 'warn' : '', staleNoIso ? staleNoIso + ' of them have no single ISO, so they cannot be remuxed' : 'All of them have an ISO'],
        [missing, 'with no MKV yet', '', 'An ISO with no MKV: a remux creates it'],
        [(qd.queued || 0) + (qd.running ? 1 : 0), 'queued', '', 'Queued or running'],
        [qd.done || 0, 'done', 'ok', ''],
        [qd.failed || 0, 'failed', qd.failed ? 'bad' : '', ''],
        [have.length, 'with an ISO', '', ''],
      ];
      put($('#stats', view), stats.map(([n, l, tone, tip]) => '<span class="stat ' + tone + '"' + (tip ? ' title="' + esc(tip) + '"' : '') + '><b>' + n + '</b> ' + l + '</span>').join(''));
      const ood = $('#ood', view), all = $('#all', view);
      if (!ood.classList.contains('busy')) {
        ood.disabled = !todo || d.scanning;
        put(ood, todo ? (waiting ? 'Queue ' + todo + ' more' : 'Remux ' + todo + ' out of date') : (waiting ? 'All out of date queued' : 'Nothing out of date'));
        ood.title = todo + ' out of date or with no MKV, not yet queued' + (waiting ? '; ' + waiting + ' already queued or running' : '');
      }
      if (!all.classList.contains('busy') && !all.classList.contains('confirm')) {
        all.disabled = !have.length || d.scanning;
        put(all, 'Remux all ' + have.length);
        all._label = all.innerHTML;
      }
      const pb = $('#pause', view);
      if (!pb.classList.contains('busy')) pb.textContent = qd.paused ? 'Resume queue' : 'Pause queue';
      $('#paused', view).hidden = !qd.paused;
      $('#debug', view).checked = !!qd.debug_log;
      const shown = rows.filter(r => (!hide || can(r)) && (!q || r.title.toLowerCase().includes(q)));
      list.update(shown, d.scanning ? 'Scanning…' : rows.length ? 'No titles match.' : 'Nothing in the library or ISO folders yet.');
      paintLive(d.live);
    }

    // Progress straight off the SSE frame, into the existing bar: no re-render.
    function paintLive(live) {
      const cell = view.querySelector('.cellbar[data-live]');
      if (!cell) return;
      const p = live && String(live.job_id) === cell.dataset.live ? live : null;
      fill(cell.querySelector('.bar i'), p ? p.pct : 0);
      const txt = cell.querySelector('.txt');
      if (txt && txt.firstChild && txt.firstChild.nodeType === 3) txt.firstChild.nodeValue = liveText(p);
    }

    ctx.cleanup.push(watch((d, err, liveOnly) => {
      if (err && !d) { put($('#lede', view), '<span style="color:var(--bad)">Could not load the library: ' + esc(err.message) + '</span>'); return; }
      last = d;
      if (liveOnly) paintLive(d.live); else paint();
    }));
  },
};
