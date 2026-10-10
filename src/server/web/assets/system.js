// System: versions, the folders and their space, and
// the diagnostics (debug logging, logs, the bundle). Keys live in Settings.

import { esc, $, put, api, act, toast, bytes, ago } from './ui.js';
import { openDeviceTerminal } from './ripper.js';
import { stagedLine } from './folders.js';

function storageFolderRow(m) {
  const used = m.total_bytes ? 100 - (m.free_bytes / m.total_bytes) * 100 : null;
  const state = m.state || (m.ok ? 'ok' : 'unhealthy');
  const tone = state === 'ok' ? 'ok' : state === 'unresponsive' ? 'warn' : 'bad';
  const health = state === 'ok' ? 'ok' : (m.message || state);
  return '<tr><td class="title"><b>' + esc(m.role) + '</b><span class="path mono" title="' + esc(m.path) + '">' + esc(m.path) + '</span></td>'
    + '<td><span class="dot dot-' + tone + '"></span> ' + esc(health) + '</td>'
    + '<td>' + (used != null ? '<div class="bar"><i style="width:' + used.toFixed(1) + '%;' + (tone === 'warn' ? 'background:#d97706' : '') + '"></i></div><span class="note">' + bytes(m.free_bytes) + ' free of ' + bytes(m.total_bytes) + '</span>' : '<span class="muted small">–</span>') + '</td>'
    + '<td class="nowrap"><span class="muted small">' + esc(m.last_ok ? ago(m.last_ok) : 'never') + '</span></td>'
    + '<td>' + (m.last_error ? '<span class="small">' + esc(m.last_error) + '</span><span class="note">' + esc(ago(m.last_error_at)) + '</span>' : '<span class="muted small">–</span>') + '</td>'
    + '<td class="num"><span class="muted small">' + (m.latency_ms != null ? m.latency_ms + ' ms' : '') + '</span></td></tr>';
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
      <section class="table-card" style="margin-top:1.25rem"><div class="toolbar"><b>Storage &amp; folders</b><span class="muted small" id="mounts-note">checked every 30 s, and before each remux and move</span></div>
        <div class="table-scroll"><table class="list"><thead><tr><th>Folder</th><th>Health</th><th>Space</th><th>Last good access</th><th>Last error</th><th class="num">Response</th></tr></thead><tbody id="mounts"></tbody></table></div></section>
      <section class="card" style="margin-top:1.25rem"><div class="toolbar"><b>Remux staging</b><button class="btn btn-ghost btn-sm" id="clear-staging">Clear staging</button></div><p class="small muted" id="staged-kept" style="margin:.6rem 0 0"></p><p class="small muted" style="margin:.4rem 0 0">Removes completed remux files left behind after a stopped or retitled job. Active work and rip-recovery staging are not touched.</p></section>`;
    let sys = null;
    const paint = () => {
      const d = sys;
      if (!d || ctx.stale()) return;
      put($('#lede', view), 'freemkv <b>' + esc(d.version_label) + '</b> · '
        + ((d.mounts || []).every(m => m.ok) ? 'every folder answering' : '<span style="color:var(--bad)">a folder needs attention</span>'));
      put($('#about', view), '<dt>freemkv</dt><dd class="mono">' + esc(d.version_label) + '</dd>'
        + '<dt>Rip library</dt><dd class="mono">' + esc(d.libfreemkv) + '</dd>'
        + '<dt>Debug logging</dt><dd>' + (d.debug_enabled ? 'on' : 'off') + '</dd>');
      put($('#mounts', view), (d.mounts || []).map(storageFolderRow).join('') || '<tr><td colspan="6" class="muted">Checking the folders…</td></tr>');
      put($('#staged-kept', view), stagedLine(d.staged_kept));
      const dbg = $('#debug', view);
      if (document.activeElement !== dbg) { dbg.checked = !!d.debug_enabled; dbg.nextElementSibling.textContent = d.debug_enabled ? 'On' : 'Off'; }
      put($('#logdir', view), 'Logs live in <span class="mono">' + esc(d.log_dir) + '</span>. The download bundles every one of them.');
    };
    const load = () => api('GET', '/api/system').then(d => { sys = d; paint(); }).catch(e => ctx.stale() || put($('#lede', view), '<span style="color:var(--bad)">Could not load: ' + esc(e.message) + '</span>'));
    load();
    ctx.every(10000, load);

    $('#debug', view).addEventListener('change', async (e) => {
      const on = e.target.checked;
      const r = await act(null, () => api('POST', '/api/debug', { enabled: on }), 'Debug logging');
      if (r) { toast(r.enabled ? 'Debug logging on' : 'Debug logging off', 'info'); load(); } else e.target.checked = !on;
    });
    $('#syslog', view).addEventListener('click', () => openDeviceTerminal('system', false));
    $('#bundle', view).addEventListener('click', () => toast('Preparing the log bundle…', 'info'));
    $('#reboot', view).addEventListener('click', async (e) => {
      if (!window.confirm('Reboot the library? Active work will finish its current safe boundary; queued jobs and audit state are preserved.')) return;
      const r = await act(e.currentTarget, () => api('POST', '/api/system/reboot'), 'Reboot library');
      if (r) {
        put($('#lede', view), '<span class="muted">Library restarting… reconnecting shortly.</span>');
        e.currentTarget.disabled = true;
      }
    });
    $('#clear-staging', view).addEventListener('click', async (e) => {
      const count = sys?.staged_kept?.count || 0;
      if (!count) { toast('Remux staging is already empty', 'info'); return; }
      if (!window.confirm('Clear ' + count + ' completed remux file' + (count === 1 ? '' : 's') + ' from staging?')) return;
      const r = await act(e.currentTarget, () => api('POST', '/api/library/staged/clear'), 'Clear staging');
      if (r) {
        toast(r.discarded + ' staged remux file' + (r.discarded === 1 ? '' : 's') + ' cleared', r.failed ? 'warn' : 'info');
        load();
      }
    });
  },
};
