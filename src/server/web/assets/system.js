// System: what is running, the drives, the folders and their health, and
// the diagnostics (debug logging, logs, the bundle). Keys live in Settings.

import { esc, $, put, api, act, toast, bytes, ago, plural } from './ui.js';
import { openDeviceTerminal } from './ripper.js';

function mountRow(m) {
  const used = m.total_bytes ? 100 - (m.free_bytes / m.total_bytes) * 100 : null;
  const tone = !m.ok ? 'bad' : used != null && used > 95 ? 'warn' : 'ok';
  return '<tr><td class="title"><b>' + esc(m.role) + '</b><span class="path mono" title="' + esc(m.path) + '">' + esc(m.path) + '</span></td>'
    + '<td class="nowrap"><span class="dot dot-' + tone + '"></span> ' + (m.ok ? (m.writable === false ? 'read-only' : 'ok') : '<span style="color:var(--bad)">' + esc(m.problem || 'problem') + '</span>') + '</td>'
    + '<td style="min-width:10rem">' + (used != null ? '<div class="bar"><i style="width:' + used.toFixed(1) + '%;' + (tone === 'warn' ? 'background:#d97706' : '') + '"></i></div><span class="note">' + bytes(m.free_bytes) + ' free of ' + bytes(m.total_bytes) + '</span>' : '<span class="muted small">–</span>') + '</td>'
    + '<td class="num"><span class="muted small">' + (m.latency_ms != null ? m.latency_ms + ' ms' : '') + '</span></td></tr>';
}

export default {
  title: 'System',
  mount(view, ctx) {
    view.innerHTML = `
      <div class="page-head"><div><h1>System</h1><p class="lede" id="lede">Loading…</p></div>
        <div class="actions"><a class="btn btn-secondary" href="/api/logs/download" id="bundle">Download all logs</a></div></div>
      <div class="grid grid-2">
        <section class="card"><h2>About</h2><dl class="kv" id="about"></dl></section>
<section class="card" id="diag-card"><h2>Diagnostics</h2>
          <div style="display:flex;align-items:center;justify-content:space-between;gap:1rem"><b>Debug logging</b><label class="switch" style="padding:0"><input type="checkbox" id="debug"><span>Off</span></label></div>
          <p class="small muted" style="margin:.4rem 0 1rem">Extra-detailed logs, for bug reports. Off by default. When on, each drive's console also has a Debug view.</p>
          <div class="actions"><button class="btn btn-ghost btn-sm" id="syslog">System log</button><a class="btn btn-ghost btn-sm" href="/api/debug?n=5000" target="_blank">Event log (JSON lines)</a><a class="btn btn-ghost btn-sm" href="/api/state" target="_blank">Live state (JSON)</a></div>
          <p class="small muted" id="logdir" style="margin:.9rem 0 0"></p></section>
      </div>
      <section class="table-card" style="margin-top:1.25rem"><div class="toolbar"><b>Storage</b><span class="muted small" id="mounts-note"></span></div>
        <div class="table-scroll"><table class="list"><thead><tr><th>Folder</th><th>State</th><th>Space</th><th class="num">Response</th></tr></thead><tbody id="mounts"></tbody></table></div></section>
      <div style="margin-top:1.25rem">
        <section class="table-card"><div class="toolbar"><b>Drives</b></div>
          <div class="table-scroll"><table class="list"><thead><tr><th>Drive</th><th>State</th><th>Disc</th><th></th></tr></thead><tbody id="drives"></tbody></table></div></section>
      </div>`;
    let sys = null;
    const paint = () => {
      const d = sys;
      if (!d || ctx.stale()) return;
      put($('#lede', view), 'freemkv <b>' + esc(d.version_label) + '</b> · ' + plural((d.drives || []).length, 'drive') + ' · '
        + ((d.mounts || []).every(m => m.ok) ? 'every folder answering' : '<span style="color:var(--bad)">a folder needs attention</span>'));
      put($('#about', view), '<dt>freemkv</dt><dd class="mono">' + esc(d.version_label) + '</dd>'
        + '<dt>Rip library</dt><dd class="mono">' + esc(d.libfreemkv) + '</dd>'
        + '<dt>Debug logging</dt><dd>' + (d.debug_enabled ? 'on' : 'off') + '</dd>');
      put($('#mounts', view), (d.mounts || []).map(mountRow).join('') || '<tr><td colspan="4" class="muted">Checking the folders…</td></tr>');
      const checked = (d.mounts || []).map(m => m.checked_at).sort()[0];
      put($('#mounts-note', view), checked ? 'checked ' + ago(checked) : '');
      put($('#drives', view), (d.drives || []).map(v => '<tr><td class="mono"><b>' + esc(v.device) + '</b></td><td>' + esc(v.status) + (v.disc_present ? '' : ' <span class="muted small">· empty</span>') + '</td>'
        + '<td>' + esc(v.disc || '') + (v.format ? ' <span class="badge fmt-' + esc(v.format) + '">' + esc(v.format.toUpperCase()) + '</span>' : '') + '</td>'
        + '<td class="act"><button class="btn btn-ghost btn-sm" data-dev="' + esc(v.device) + '">Log</button></td></tr>').join('') || '<tr><td colspan="4" class="muted">No drives detected.</td></tr>');
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
    $('#drives', view).addEventListener('click', (e) => {
      const b = e.target.closest('[data-dev]');
      if (b) openDeviceTerminal(b.dataset.dev, !!(sys && sys.debug_enabled));
    });
    $('#bundle', view).addEventListener('click', () => toast('Preparing the log bundle…', 'info'));
  },
};
