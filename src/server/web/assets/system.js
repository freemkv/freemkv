// System: versions, the folders and their space, and
// the diagnostics (debug logging, logs, the bundle). Keys live in Settings.

import { esc, $, put, api, act, toast, bytes, ago, twoStep } from './ui.js';
import { openDeviceTerminal } from './ripper.js';
import { stagedLine } from './folders.js';

function storageSummary(d) {
  if ((d.move_errors || []).length) return '<span style="color:var(--bad)">file delivery needs attention — see <a href="/drives">Drives</a></span>';
  if (!(d.mounts || []).length) return 'checking folders';
  return d.mounts.every(m => m.ok) ? 'every folder answering' : '<span style="color:var(--bad)">a folder needs attention</span>';
}

function storageFolderRow(m) {
  const used = m.total_bytes ? 100 - (m.free_bytes / m.total_bytes) * 100 : null;
  const state = m.state;
  const tone = state === 'ok' ? 'ok' : state === 'unresponsive' ? 'warn' : 'bad';
  const health = state === 'ok' ? 'Accessible' : (m.message || state);
  return '<tr><td class="title"><b>' + esc(m.role) + '</b><span class="path mono" title="' + esc(m.path) + '">' + esc(m.path) + '</span></td>'
    + '<td><span class="health"><span class="dot dot-' + tone + '"></span><span>' + esc(health) + '</span></span></td>'
    + '<td>' + (used != null ? '<div class="bar"><i style="width:' + used.toFixed(1) + '%;' + (tone === 'warn' ? 'background:#d97706' : '') + '"></i></div><span class="note">' + bytes(m.free_bytes) + ' free of ' + bytes(m.total_bytes) + '</span>' : '<span class="muted small">–</span>') + '</td>'
    + '<td class="nowrap"><span class="muted small">' + esc(m.last_ok ? ago(m.last_ok) : 'never') + '</span></td>'
    + '<td>' + (m.last_error ? '<span class="small">' + esc(m.last_error) + '</span><span class="note">' + esc(ago(m.last_error_at)) + '</span>' : '<span class="muted small">–</span>') + '</td>'
    + '<td class="num"><span class="muted small">' + (m.latency_ms != null ? m.latency_ms + ' ms' : '') + '</span></td></tr>';
}

const NO_ANSWER = 'the server did not answer';
const RESTARTING = 'Restarting… running work stops at its next safe point, then the server restarts. This page reconnects by itself.';
const RESTART_NOTES = {
  slow: 'Still restarting. If freemkv was started by hand rather than as a service, start it again.',
  lost: 'The server has not come back; check the service logs.',
};

// Ask for a reboot and resolve to the /api/version answer read just before it, which the
// wait compares against. A reboot answer lost to the restart itself still counts.
async function requestReboot(call) {
  const before = await call('GET', '/api/version').catch(() => null);
  try {
    const r = await call('POST', '/api/system/reboot');
    return before || {boot: r && r.boot};
  } catch (e) {
    if (before && e.message === NO_ANSWER) return before;
    throw e;
  }
}

// Poll every 2 s until a server with a boot id other than `boot` answers (null if the
// page is left first); `note` hears 'slow' after a minute and 'lost' after six.
async function awaitRestart(boot, {get, sleep, now, stale, note}) {
  const started = now();
  let stage = '';
  while (!stale()) {
    await sleep(2000);
    if (stale()) break;
    const v = await get().catch(() => null);
    if (v && v.boot && v.boot !== boot) return v;
    const waited = now() - started;
    const next = waited >= 360000 ? 'lost' : waited >= 60000 ? 'slow' : '';
    if (next !== stage) { stage = next; note(stage); }
  }
  return null;
}

// GET /api/version, given up after 2 s so hung requests to a stopping server do not
// fill the browser's connection pool.
function versionWithin2s() {
  const abort = new AbortController();
  const timer = setTimeout(() => abort.abort(), 2000);
  return api('GET', '/api/version', undefined, abort.signal).finally(() => clearTimeout(timer));
}

export default {
  title: 'System',
  mount(view, ctx) {
    view.innerHTML = `
      <div class="page-head"><div><h1>System</h1><p class="lede" id="lede">Loading…</p></div>
        <div class="actions"><button class="btn btn-ghost" id="reboot">Reboot library</button><a class="btn btn-secondary" href="/api/logs/download" id="bundle">Download all logs</a></div></div>
      <div class="grid grid-2">
        <section class="card"><h2>About</h2><dl class="kv" id="about"></dl></section>
<section class="card" id="diag-card"><h2>Diagnostics</h2>
          <div style="display:flex;align-items:center;justify-content:space-between;gap:1rem"><b>Debug logging</b><label class="switch" style="padding:0"><input type="checkbox" id="debug"><span>Off</span></label></div>
          <p class="small muted" style="margin:.4rem 0 1rem">Extra-detailed logs, for bug reports. Off by default. When on, each drive's console also has a Debug view.</p>
          <div class="actions"><button class="btn btn-ghost btn-sm" id="syslog">System log</button><a class="btn btn-ghost btn-sm" href="/api/debug?n=5000" target="_blank">Event log (JSON lines)</a><a class="btn btn-ghost btn-sm" href="/api/state" target="_blank">Live state (JSON)</a></div>
          <p class="small muted" id="logdir" style="margin:.9rem 0 0"></p></section>
      </div>
      <section class="table-card" style="margin-top:1.25rem"><div class="toolbar"><b>Storage &amp; folders</b><span class="muted small" id="mounts-note">Access checked every 30 s. Access alone does not confirm successful file delivery.</span></div>
        <div class="table-scroll"><table class="list"><thead><tr><th>Folder</th><th>Health</th><th>Space</th><th>Last good access</th><th>Last error</th><th class="num">Response</th></tr></thead><tbody id="mounts"></tbody></table></div></section>
      <section class="card" style="margin-top:1.25rem"><div class="toolbar"><b>Staging</b><button class="btn btn-ghost btn-sm" id="clear-staging">Clear staging</button></div><p class="small muted" id="staged-kept" style="margin:.6rem 0 0"></p><p class="small muted" style="margin:.4rem 0 0">Removes failed rips (including their saved disc images) and empty leftover entries. Anything ripping, muxing, held for review, resumable, corrected or not yet delivered is kept.</p></section>`;
    let sys = null;
    let restarting = false;
    const paint = () => {
      const d = sys;
      if (!d || ctx.stale()) return;
      if (!restarting) put($('#lede', view), 'freemkv <b>' + esc(d.version_label) + '</b> · '
        + storageSummary(d));
      put($('#about', view), '<dt>freemkv</dt><dd class="mono">' + esc(d.version_label) + '</dd>'
        + '<dt>Rip library</dt><dd class="mono">' + esc(d.libfreemkv) + '</dd>'
        + '<dt>Debug logging</dt><dd>' + (d.debug_enabled ? 'on' : 'off') + '</dd>');
      put($('#mounts', view), (d.mounts || []).map(storageFolderRow).join('') || '<tr><td colspan="6" class="muted">Checking the folders…</td></tr>');
      put($('#staged-kept', view), stagedLine(d.staged_kept));
      const dbg = $('#debug', view);
      if (document.activeElement !== dbg) { dbg.checked = !!d.debug_enabled; dbg.nextElementSibling.textContent = d.debug_enabled ? 'On' : 'Off'; }
      put($('#logdir', view), 'Logs live in <span class="mono">' + esc(d.log_dir) + '</span>. The download bundles every one of them.');
    };
    const load = () => restarting ? Promise.resolve() : api('GET', '/api/system').then(d => { sys = d; paint(); }).catch(e => ctx.stale() || restarting || put($('#lede', view), '<span style="color:var(--bad)">Could not load: ' + esc(e.message) + '</span>'));
    load();
    ctx.every(10000, load);

    $('#debug', view).addEventListener('change', async (e) => {
      const on = e.target.checked;
      const r = await act(null, () => api('POST', '/api/debug', { enabled: on }), 'Debug logging');
      if (r) { toast(r.enabled ? 'Debug logging on' : 'Debug logging off', 'info'); load(); } else e.target.checked = !on;
    });
    $('#syslog', view).addEventListener('click', () => openDeviceTerminal('system', false));
    $('#bundle', view).addEventListener('click', () => toast('Preparing the log bundle…', 'info'));
    $('#reboot', view).addEventListener('click', (e) => twoStep(e.currentTarget, async (btn) => {
      const before = await act(btn, () => requestReboot(api), 'Reboot library');
      if (!before) return;
      restarting = true;
      btn.disabled = true;
      put($('#lede', view), '<span class="muted">' + RESTARTING + '</span>');
      const v = await awaitRestart(before.boot, {
        get: versionWithin2s,
        sleep: ms => new Promise(done => setTimeout(done, ms)),
        now: Date.now,
        stale: ctx.stale,
        note: stage => put($('#lede', view), '<span class="muted">' + RESTARTING + ' ' + RESTART_NOTES[stage] + '</span>'),
      });
      if (!v || ctx.stale()) return;
      if (before.version && v.version !== before.version) return location.reload();
      restarting = false;
      btn.disabled = false;
      toast('Library restarted', 'info');
      load();
    }, 'Confirm reboot'));
    $('#clear-staging', view).addEventListener('click', (e) => twoStep(e.currentTarget, async (btn) => {
      const r = await act(btn, () => api('POST', '/api/library/staged/clear'), 'Clear staging');
      if (r) {
        toast(r.discarded + ' staging item' + (r.discarded === 1 ? '' : 's') + ' cleared', r.failed ? 'warn' : 'info');
        load();
      }
    }, 'Confirm: remove failed rips'));
  },
};
