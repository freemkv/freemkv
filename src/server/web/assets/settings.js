// Settings: drawn entirely from GET /api/settings/schema, the same table the
// server loads, validates and redacts with. Nothing here names a setting,
// except the webhook list's own editor.

import { esc, $, $$, api, act, toast } from './ui.js';

function ctlHtml(f, v) {
  const id = 'f-' + f.key;
  const ph = f.placeholder ? ' placeholder="' + esc(f.placeholder) + '"' : '';
  switch (f.type) {
    case 'bool':
      return '<label class="switch"><input type="checkbox" id="' + id + '" data-key="' + f.key + '"' + (v ? ' checked' : '') + '><span>' + (v ? 'On' : 'Off') + '</span></label>';
    case 'choice': {
      // The selected option's one line sits under the control and follows it.
      const cur = f.options.find(o => o.value === v);
      return '<div class="seg" role="radiogroup" aria-labelledby="l-' + f.key + '">' + f.options.map(o =>
        '<label title="' + esc(o.help || '') + '"><input type="radio" name="' + f.key + '" data-key="' + f.key + '" value="' + esc(o.value) + '" data-help="' + esc(o.help || '') + '"' + (v === o.value ? ' checked' : '') + '><span>' + esc(o.label) + '</span></label>').join('') + '</div>'
        + '<div class="opt-help" id="oh-' + f.key + '">' + esc(cur && cur.help ? cur.help : '') + '</div>';
    }
    case 'number':
      return '<input class="txt num" type="number" min="0" max="' + (f.max || '') + '" id="' + id + '" data-key="' + f.key + '" value="' + esc(v == null ? '' : v) + '">';
    case 'secret':
      return '<input class="txt" type="password" autocomplete="new-password" id="' + id + '" data-key="' + f.key + '" value="' + esc(v || '') + '"' + (v ? '' : ' placeholder="Not set"') + '>';
    case 'info':
      return '<div class="info-line" id="' + id + '">' + esc(v || '') + '</div>';
    case 'action':
      return '<button type="button" class="btn btn-secondary btn-sm" data-endpoint="' + esc(f.endpoint) + '">' + esc(f.button) + '</button> <span class="small muted" data-status="' + f.key + '"></span>';
    case 'webhooks':
      return '<div id="hooks">' + (v || []).map(hookRow).join('') + '</div><button type="button" class="btn btn-ghost btn-sm" id="addhook">+ Add a webhook</button>';
    default:
      return '<input class="txt" type="text" spellcheck="false" id="' + id + '" data-key="' + f.key + '" value="' + esc(v == null ? '' : v) + '"' + ph + '>';
  }
}

function hookRow(h) {
  h = typeof h === 'string' ? { url: h, post_rip: true, post_mux: true, post_move: true } : (h || { url: '', post_rip: true, post_mux: true, post_move: true });
  const savedUrl = h.url || '';
  const displayUrl = savedUrl.replace(/(\*{8})#\d+$/, '$1');
  const cb = (k, label) => '<label><input type="checkbox" data-flag="' + k + '"' + (h[k] !== false ? ' checked' : '') + '> ' + label + '</label>';
  return '<div class="hook-entry"><div class="hook"><input class="txt" type="text" data-hook data-saved-url="' + esc(savedUrl) + '" placeholder="https://discord.com/api/webhooks/…" value="' + esc(displayUrl) + '" aria-label="Webhook URL">'
    + '<span class="flags">' + cb('post_rip', 'Rip') + cb('post_mux', 'Mux') + cb('post_move', 'Move') + '</span>'
    + '<button type="button" class="x" data-rmhook aria-label="Remove this webhook">×</button></div>'
    + '<details class="hook-auth"' + (Object.keys(h.headers || {}).length ? ' open' : '') + '><summary>Headers &amp; test</summary>'
    + '<div data-hook-headers>' + Object.entries(h.headers || {}).map(([name, value]) => headerRow(name, value)).join('') + '</div>'
    + '<div class="hook-auth-fields"><button type="button" class="btn btn-ghost btn-sm" data-addheader>+ Add a header</button>'
    + '<button type="button" class="btn btn-secondary btn-sm" data-testhook>Test</button><span class="small muted" data-hook-status role="status"></span></div>'
    + '<p class="small muted">Test sends a request using these fields without saving. Header values are hidden after saving.</p></details></div>';
}

function headerRow(name = '', value = '') {
  return '<div class="hook-auth-fields" data-header-row><label>Header name<input class="txt" data-header-name placeholder="Authorization" value="' + esc(name) + '"></label>'
    + '<label>Value<input class="txt" type="password" autocomplete="new-password" data-header-value value="' + esc(value) + '"></label>'
    + '<button type="button" class="x" data-rmheader aria-label="Remove this header">×</button></div>';
}

function collectHook(row) {
  const flag = k => row.querySelector('[data-flag="' + k + '"]').checked;
  const input = row.querySelector('[data-hook]');
  const entered = input.value.trim(), saved = input.dataset.savedUrl || '';
  // Preserve the masked identity in requests, without displaying its internal index.
  const url = entered === saved.replace(/(\*{8})#\d+$/, '$1') ? saved : entered;
  return { url,
    post_rip: flag('post_rip'), post_mux: flag('post_mux'), post_move: flag('post_move'),
    headers: Object.fromEntries($$('[data-header-row]', row).map(r => [r.querySelector('[data-header-name]').value.trim(), r.querySelector('[data-header-value]').value]).filter(([name]) => name)) };
}

function fieldHtml(f, v, sub) {
  const labelled = f.type !== 'action' && f.type !== 'webhooks';
  const lbl = f.label ? (labelled && f.type !== 'choice' ? '<label id="l-' + f.key + '" for="f-' + f.key + '">' + esc(f.label) + '</label>' : '<span class="lbl" id="l-' + f.key + '">' + esc(f.label) + '</span>') : '<span class="lbl"></span>';
  return '<div class="field' + (sub ? ' sub' : '') + '" data-field="' + f.key + '"'
    + (f.show_if ? ' data-show="' + f.show_if.key + '=' + esc(f.show_if.value) + '"' : '')
    + (f.hide_if ? ' data-hide="' + f.hide_if.key + '=' + esc(f.hide_if.value) + '"' : '') + '>'
    + lbl + '<div class="ctl">' + ctlHtml(f, v) + '</div>' + (f.help ? '<div class="help">' + esc(f.help) + '</div>' : '') + '</div>';
}

async function mount(view, ctx) {
    view.innerHTML = '<div class="page-head"><div><h1>Settings</h1><p class="lede">Saved to <span class="mono">settings.json</span> in the config folder. Changes apply to the next rip.</p></div></div><div id="body" class="muted">Loading…</div>';
    let schema, values;
    try {
      [schema, values] = await Promise.all([api('GET', '/api/settings/schema'), api('GET', '/api/settings')]);
    } catch (e) {
      if ((ctx && ctx.stale()) || !$('#body', view)) return;
      $('#body', view).innerHTML = '<div class="banner bad" style="margin:0">Could not load the settings: ' + esc(e.message) + '</div>';
      return;
    }
    // The user navigated away while this loaded: the view is someone else's now.
    if ((ctx && ctx.stale()) || !$('#body', view)) return;
    const groups = schema.groups.filter(g => schema.fields.some(f => f.group === g.id));
    const html = groups.map(g => {
      const fields = schema.fields.filter(f => f.group === g.id);
      const inner = fields.map(f => fieldHtml(f, values[f.key], !!f.show_if || !!f.hide_if)).join('');
      if (g.id === 'Advanced') {
        return '<section class="card" id="' + g.id + '"><details class="adv"><summary>' + esc(g.title) + ' <span class="muted small" style="font-weight:400">time limits most setups never change</span></summary><div style="margin-top:.8rem">' + inner + '</div></details></section>';
      }
      return '<section class="card" id="' + g.id + '"><h2>' + esc(g.title) + '</h2>' + inner + '</section>';
    }).join('');
    $('#body', view).outerHTML = '<div class="settings-layout"><nav class="settings-nav" aria-label="Settings sections">'
      + groups.map(g => '<a href="#' + g.id + '">' + esc(g.title) + '</a>').join('') + '</nav>'
      + '<form id="form" class="stack" novalidate>' + html
      + '<div class="savebar"><button type="submit" class="btn btn-primary" id="save">Save changes</button><button type="button" class="btn btn-ghost" id="revert">Discard</button><span class="msg" id="msg">No changes</span></div></form></div>';
    const connections = document.createElement('section');
    connections.className = 'card';
    connections.id = 'Connections';
    connections.innerHTML = `<h2>Connected Libraries</h2>
      <div id="connected-libraries"></div>
      <div class="field"><span class="lbl">Libraries</span><div class="ctl"><button type="button" class="btn btn-ghost btn-sm" id="add-library" aria-expanded="false" aria-controls="connect-library">+ Add a Library</button></div>
        <div class="help">See and control another Library’s drives here. Rips and files stay on the machine with the drive.</div></div>
      <form id="connect-library" hidden>
        <div class="field"><label for="peer-name">Library name</label><div class="ctl"><input id="peer-name" class="txt" name="name" required maxlength="100" placeholder="Ripping PC"></div></div>
        <div class="field"><label for="peer-url">Remote Library URL</label><div class="ctl"><input id="peer-url" class="txt" name="url" type="url" required placeholder="http://library-pc:8080"></div></div>
        <div class="field"><label for="peer-return">This Library’s URL</label><div class="ctl"><input id="peer-return" class="txt" name="return_url" type="url" placeholder="http://this-computer:8080"></div>
          <div class="help">Optional: this machine’s reachable network URL, so both UIs show both machines’ drives. Both Libraries need the connection feature.</div></div>
        <div class="field"><span class="lbl"></span><div class="ctl"><button class="btn btn-secondary btn-sm" type="submit">Connect to Remote Library</button> <button class="btn btn-ghost btn-sm" id="cancel-library" type="button">Cancel</button></div></div>
      </form>
      <p id="connect-status" role="status" class="small muted"></p>`;
    const connectionForm = $('#connect-library', connections);
    const addLibrary = $('#add-library', connections);
    const showConnectionForm = show => {
      connectionForm.hidden = !show;
      addLibrary.setAttribute('aria-expanded', String(show));
      if (show) $('#peer-name', connections).focus();
      else addLibrary.focus();
    };
    addLibrary.addEventListener('click', () => showConnectionForm(true));
    $('#cancel-library', connections).addEventListener('click', () => { connectionForm.reset(); showConnectionForm(false); });
    const content = document.createElement('div');
    content.className = 'stack';
    const settingsForm = $('#form', view);
    settingsForm.replaceWith(content);
    content.append(settingsForm, connections);
    $('.settings-nav', view).insertAdjacentHTML('beforeend', '<a href="#Connections">Connected Libraries</a>');
    const loadConnections = async () => {
      const peers = await api('GET', '/api/peers');
      if (ctx.stale()) return;
      $('#connected-libraries', connections).innerHTML = peers.map(p => '<div class="field"><span class="lbl">' + esc(p.name) + '</span><div class="ctl"><div class="hook"><input class="txt" type="text" readonly aria-label="' + esc(p.name) + ' URL" value="' + esc(p.url) + '"><button type="button" class="btn btn-ghost btn-sm" data-disconnect="' + esc(p.id) + '">Disconnect here</button></div></div></div>').join('');
    };
    loadConnections().catch(e => { if (!ctx.stale()) $('#connect-status', connections).textContent = e.message; });
    connections.addEventListener('click', async e => {
      const b = e.target.closest('[data-disconnect]');
      if (!b) return;
      await act(b, async () => { await api('POST', '/api/peers', { remove: b.dataset.disconnect }); await loadConnections(); }, 'Disconnect');
    });
    $('#connect-library', connections).addEventListener('submit', async e => {
      e.preventDefault();
      const fields = new FormData(e.target);
      const button = e.target.querySelector('button');
      await act(button, async () => {
        const result = await api('POST', '/api/peers', Object.fromEntries(fields));
        if (ctx.stale()) return;
        $('#connect-status', connections).textContent = result.warning || 'Connected. Open Drives to see both Libraries.';
        await loadConnections();
        if (!result.warning) { connectionForm.reset(); showConnectionForm(false); }
      }, 'Connect');
    });
    const form = $('#form', view);
    const initial = JSON.stringify(collect());

    function current(key) {
      const on = form.querySelector('input[data-key="' + key + '"]:checked');
      return on ? on.value : values[key];
    }
    function applyConditions() {
      $$('[data-show],[data-hide]', form).forEach(el => {
        let visible = true;
        if (el.dataset.show) { const [k, v] = el.dataset.show.split('='); if (current(k) !== v) visible = false; }
        if (el.dataset.hide) { const [k, v] = el.dataset.hide.split('='); if (current(k) === v) visible = false; }
        el.hidden = !visible;
      });
    }
    function collect() {
      const out = {};
      $$('[data-key]', form).forEach(el => {
        const k = el.dataset.key;
        if (el.type === 'radio') { if (el.checked) out[k] = el.value; }
        else if (el.type === 'checkbox') out[k] = el.checked;
        else if (el.type === 'number') {
          // Empty means "the default", not 0.
          const f = schema.fields.find(x => x.key === k);
          out[k] = el.value.trim() === '' ? (f && f.default != null ? f.default : undefined) : Math.max(0, parseInt(el.value, 10) || 0);
          if (out[k] === undefined) delete out[k];
        }
        else out[k] = el.value;
      });
      const hooks = $('#hooks', form);
      if (hooks) {
        out.webhook_urls = $$('.hook-entry', hooks).map(collectHook).filter(h => h.url);
      }
      return out;
    }
    const dirty = () => JSON.stringify(collect()) !== initial;
    function paintDirty() {
      const d = dirty();
      const msg = $('#msg', form);
      if (!msg.classList.contains('bad')) msg.textContent = d ? 'Unsaved changes' : 'No changes';
      $('#save', form).disabled = !d;
      $('#revert', form).disabled = !d;
    }
    form.addEventListener('input', (e) => {
      if (e.target.matches('.seg input')) {
        const oh = form.querySelector('#oh-' + e.target.dataset.key);
        if (oh) oh.textContent = e.target.dataset.help || '';
      }
      if (e.target.matches('.switch input')) e.target.nextElementSibling.textContent = e.target.checked ? 'On' : 'Off';
      $('#msg', form).classList.remove('bad');
      applyConditions();
      paintDirty();
    });
    form.addEventListener('click', async (e) => {
      if (e.target.closest('#addhook')) {
        $('#hooks', form).insertAdjacentHTML('beforeend', hookRow(null));
        $('#hooks .hook-entry:last-child [data-hook]', form).focus();
        paintDirty();
      } else if (e.target.closest('[data-rmhook]')) {
        e.target.closest('.hook-entry').remove();
        paintDirty();
      } else if (e.target.closest('[data-addheader]')) {
        const headers = e.target.closest('.hook-entry').querySelector('[data-hook-headers]');
        headers.insertAdjacentHTML('beforeend', headerRow());
        headers.lastElementChild.querySelector('input').focus();
        paintDirty();
      } else if (e.target.closest('[data-rmheader]')) {
        e.target.closest('[data-header-row]').remove();
        paintDirty();
      } else if (e.target.closest('[data-testhook]')) {
        const b = e.target.closest('[data-testhook]'), row = b.closest('.hook-entry');
        const status = row.querySelector('[data-hook-status]');
        status.textContent = 'Testing…';
        let failure = 'Test failed';
        const r = await act(b, () => api('POST', '/api/webhook/test', collectHook(row)).catch(err => { failure = err.message; throw err; }), 'Webhook test');
        status.textContent = r ? 'Success · HTTP ' + r.status : failure;
      } else if (e.target.closest('[data-endpoint]')) {
        const b = e.target.closest('[data-endpoint]');
        const st = b.parentElement.querySelector('[data-status]');
        if (dirty()) toast('Save first: the update uses the saved settings', 'info');
        const r = await act(b, () => api('POST', b.dataset.endpoint), b.textContent);
        if (r) {
          const bad = r.reachable === false;
          const t = r.entries != null ? 'Updated: ' + r.entries.toLocaleString() + ' entries'
            : r.reachable != null ? (r.reachable ? 'The keyserver answered' : 'No proper answer: ' + r.result) : 'Done';
          if (st) { st.textContent = t; st.style.color = bad ? 'var(--bad)' : ''; }
          toast(t, bad ? 'bad' : 'ok');
        } else if (st) st.textContent = '';
      }
    });
    $('#revert', form).addEventListener('click', () => mount(view, ctx));
    form.addEventListener('submit', async (e) => {
      e.preventDefault();
      const btn = $('#save', form);
      const msg = $('#msg', form);
      const body = collect();
      let lastError = '';
      const r = await act(btn, () => api('POST', '/api/settings', body).catch(err => { lastError = err.message; throw err; }), 'Save');
      if (r) {
        const y = window.scrollY;
        await mount(view, ctx);
        window.scrollTo(0, y);
        toast('Settings saved', 'ok');
      } else {
        msg.classList.add('bad');
        msg.textContent = 'Not saved: ' + (lastError || 'the server refused it');
      }
    });
    applyConditions();
    paintDirty();
    // Scroll-spy: the nav marks the section at the top of the view.
    document.documentElement.style.setProperty('--nav-h', document.querySelector('.nav').offsetHeight + 'px');
    const links = new Map([...view.querySelectorAll('.settings-nav a')].map(a => [a.getAttribute('href').slice(1), a]));
    const spy = new IntersectionObserver((entries) => {
      const top = entries.filter(e => e.isIntersecting).sort((a, b) => a.boundingClientRect.top - b.boundingClientRect.top)[0];
      if (!top) return;
      links.forEach((a, id) => a.classList.toggle('on', id === top.target.id));
    }, { rootMargin: '-' + (document.querySelector('.nav').offsetHeight + 8) + 'px 0px -60% 0px' });
    form.querySelectorAll(':scope > section.card').forEach(sec => spy.observe(sec));
    if (ctx) ctx.cleanup.push(() => spy.disconnect());
    if (location.hash) { const t = document.getElementById(location.hash.slice(1)); if (t) t.scrollIntoView(); }
}

export default { title: 'Settings', mount };
