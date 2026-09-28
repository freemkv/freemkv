use crate::server::config::{self, Config, WebhookEntry};
use crate::server::ripper;
use once_cell::sync::Lazy;
use std::io::{Read as _, Write as _};
use std::net::{IpAddr, SocketAddr, ToSocketAddrs};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, RwLock};
use tiny_http::{Header, Method, Response, Server, StatusCode};

/// Runtime debug flag - toggled via /api/debug POST.
pub static DEBUG_ENABLED: Lazy<Arc<RwLock<bool>>> = Lazy::new(|| Arc::new(RwLock::new(false)));

/// Check if debug logging is enabled.
///
/// Poison-tolerant: this runs on the mux hot path, so a panic elsewhere
/// while the write guard is held must not turn every later call into a
/// panic (which would kill the mux thread). Recover the inner value
/// instead of unwrapping.
pub fn debug_enabled() -> bool {
    *DEBUG_ENABLED
        .read()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Embedded single-page HTML dashboard — full parity with Python autorip web UI.
// The freemkv brand favicon, shared verbatim with the marketing site
// (freemkv.org's public/favicon.svg). Served at /favicon.svg so the dashboard
// tab shows the same icon as the website.
const FAVICON_SVG: &str = r##"<svg width="256" height="256" viewBox="0 0 256 256" xmlns="http://www.w3.org/2000/svg">
<g transform="translate(128, 128)">
<circle cx="0" cy="0" r="120" fill="#0D9488" />
<circle cx="0" cy="0" r="95" fill="#0F766E" />
<circle cx="0" cy="0" r="70" fill="#0D9488" />
<circle cx="0" cy="0" r="28" fill="#F0FDFA" />
<circle cx="0" cy="0" r="15" fill="#0D9488" />
<path d="M0,-120 A120,120 0 0,1 103.9,60 L77.9,45 A90,90 0 0,0 0,-90 Z" fill="#14B8A6" opacity="0.6"/>
<path d="M-15,-10 L-2,-10 L-2,10 L-15,10 Z" fill="#F0FDFA" opacity="0.9" transform="translate(55, 0)"/>
<path d="M0,-9 L16,0 L0,9 Z" fill="#F0FDFA" opacity="0.9" transform="translate(58, 0)"/>
</g>
</svg>
"##;

const DASHBOARD_HTML: &str = r##"<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<link rel="icon" href="/favicon.svg" type="image/svg+xml">
<title>AutoRip</title>
<style>
:root {
  --bg:#f6f8fa; --border:#d0d7de; --text:#1f2328; --text2:#4d5560; --text3:#656d76;
  --accent:#0969da; --green:#1a7f37; --yellow:#9a6700; --red:#cf222e; --blue:#0969da;
  --card:#fff; --log-bg:#fff; --log-text:#24292f; --log-border:#d0d7de; --chip:#eaeef2; --poster-bg:#e1e4e8;
  /* Light-mode pill backgrounds (--green / --yellow / --red) are all
     saturated/dark — pair them with white pill text. Dark mode flips
     to lighter pill backgrounds, which want black text instead. */
  --pill-fg:#fff;
}
body.dark {
  --bg:#0d1117; --border:#3d444d; --text:#f0f6fc; --text2:#d1d9e0; --text3:#9198a1;
  --accent:#79c0ff; --green:#56d364; --yellow:#e3b341; --red:#ff7b72; --blue:#79c0ff;
  --card:#151b23; --log-bg:#151b23; --log-text:#d1d9e0; --log-border:#3d444d; --chip:#262c36; --poster-bg:#262c36;
  --pill-fg:#000;
}
* { margin:0; padding:0; box-sizing:border-box; }
/* Always reserve the vertical scrollbar gutter. Without this, switching
   between key sources (Local is taller than Online) makes the page scrollbar
   appear/disappear, which changes the viewport width and shifts the centered
   .c container sideways. overflow-y:scroll keeps the gutter present always. */
html { overflow-y:scroll; scrollbar-gutter:stable; }
body { font-family:-apple-system,system-ui,"Segoe UI",Roboto,sans-serif; background:var(--bg); color:var(--text); min-height:100vh; display:flex; flex-direction:column; }
.c { max-width:900px; margin:0 auto; padding:20px; width:100%; flex:1; display:flex; flex-direction:column; }
@keyframes p { 0%,100%{opacity:1} 50%{opacity:.3} }
@keyframes ph { 0%,100%{opacity:1} 50%{opacity:.45} }
.card { background:var(--card); border:1px solid var(--border); border-radius:12px; padding:16px; margin-bottom:16px; }
.card h2 { font-size:.7rem; color:var(--text3); margin-bottom:10px; text-transform:uppercase; font-weight:600; letter-spacing:1px; }
.log { background:var(--log-bg); border:1px solid var(--log-border); border-radius:8px; padding:12px; font-family:'SF Mono','Fira Code',monospace; font-size:.75rem; max-height:280px; overflow-y:auto; white-space:pre-wrap; word-break:break-all; line-height:1.6; color:var(--log-text); }
.log::-webkit-scrollbar { width:5px; } .log::-webkit-scrollbar-thumb { background:var(--border); border-radius:3px; }
.btn { background:var(--chip); border:1px solid var(--border); color:var(--text); padding:5px 12px; border-radius:6px; cursor:pointer; font-size:.78rem; text-decoration:none; }
.btn:hover { background:var(--border); }
.ok { color:var(--green); } .warn { color:var(--red); }
.headerbar { display:flex; align-items:center; gap:8px 12px; padding:12px 16px; flex-wrap:wrap; border-bottom:1px solid var(--border); background:var(--card); position:sticky; top:0; z-index:10; }
.nav { text-decoration:none; font-size:.85rem; color:var(--text3); padding:4px 0; border-bottom:2px solid transparent; cursor:pointer; background:none; border-top:none; border-left:none; border-right:none; }
.nav:hover { color:var(--text); } .nav.active { color:var(--text); border-bottom-color:var(--accent); font-weight:500; }
.brand { font-size:1.1rem; color:var(--text3); font-weight:400; letter-spacing:3px; text-transform:uppercase; }
/* Now Playing card */
.np { display:flex; align-items:flex-start; gap:20px; background:var(--card); border:1px solid var(--border); border-radius:12px; padding:20px; margin-bottom:16px; min-height:180px; }
.poster { width:120px; height:180px; border-radius:8px; background:var(--poster-bg); flex-shrink:0; align-self:flex-start; object-fit:cover; box-shadow:0 2px 8px rgba(0,0,0,.1); }
.ph { width:120px; min-height:170px; border-radius:8px; background:var(--poster-bg); flex-shrink:0; display:flex; align-items:center; justify-content:center; }
.ph svg { width:40px; height:40px; opacity:.4; }
.nfo { flex:1; display:flex; flex-direction:column; justify-content:center; }
.mt { font-size:1.5rem; font-weight:600; color:var(--text); line-height:1.2; }
.my { font-size:.9rem; color:var(--text2); margin-top:4px; }
.mo { font-size:.8rem; color:var(--text2); margin-top:8px; line-height:1.5; display:-webkit-box; -webkit-line-clamp:3; -webkit-box-orient:vertical; overflow:hidden; }
.b { display:inline-block; padding:2px 8px; border-radius:4px; font-size:.7rem; font-weight:600; text-transform:uppercase; margin-left:8px; }
.b.uhd { background:#0969da18; color:var(--blue); border:1px solid #0969da33; }
.b.bluray { background:#1a7f3718; color:var(--green); border:1px solid #1a7f3733; }
.b.dvd { background:#9a670018; color:var(--yellow); border:1px solid #9a670033; }
.btn-stop, .btn-eject { font-size:.78rem; }
.idle-msg { display:flex; flex-direction:column; align-items:center; justify-content:center; width:100%; min-height:160px; color:var(--text3); }
.idle-msg svg { width:48px; height:48px; opacity:.4; margin-bottom:12px; }
.idle-msg p { font-size:.85rem; }
/* Device tabs */
.dtab { display:inline-block; padding:6px 16px; font-size:.8rem; cursor:pointer; border:1px solid var(--border); border-bottom:none; border-radius:8px 8px 0 0; background:var(--chip); color:var(--text3); margin-right:4px; }
.dtab.active { background:var(--card); color:var(--text); font-weight:500; border-bottom:1px solid var(--card); margin-bottom:-1px; position:relative; z-index:1; }
.dtabs { border-bottom:1px solid var(--border); margin-bottom:16px; padding:0 4px; }
.actions { display:flex; gap:8px; align-items:center; margin-bottom:12px; }
/* History table */
table { width:100%; border-collapse:collapse; font-size:.8rem; margin-top:16px; display:block; overflow-x:auto; }
th { text-align:left; color:var(--text3); font-weight:600; font-size:.7rem; text-transform:uppercase; letter-spacing:.5px; padding:8px 10px; border-bottom:2px solid var(--border); }
td { padding:8px 10px; border-bottom:1px solid var(--border); }
tr:hover { background:var(--chip); }
/* System page */
.files { font-size:.8rem; line-height:1.8; }
.files span { color:var(--text2); }
/* Settings */
.setting { margin-bottom:18px; }
.setting label { display:block; font-size:13px; color:var(--text2); font-weight:500; margin-bottom:5px; }
.setting input[type=text], .setting input[type=number] { padding:8px 10px; border:1px solid var(--border); border-radius:6px; background:var(--log-bg); color:var(--text); font-size:13px; font-family:inherit; box-sizing:border-box; }
.setting input[type=text] { width:100%; }
.setting input[type=number] { width:120px; }
.setting input:focus { outline:none; border-color:var(--accent); }
.setting .hint { font-size:12px; color:var(--text3); margin-top:3px; line-height:1.4; }
.toggle { display:flex; align-items:center; gap:6px; font-size:13px; cursor:pointer; font-weight:400; color:var(--text); line-height:1; }
.toggle input[type=checkbox] { width:13px; height:13px; margin:0; flex-shrink:0; accent-color:var(--accent); }
#settings-form .card { margin-bottom:12px; }
#settings-form .card h2 { margin-bottom:14px; }
.section { display:none; } .section.active { display:flex; flex-direction:column; flex:1; }
/* ── Mobile / narrow viewports ─────────────────────────────────────────
   Additive: these rules only take effect below the breakpoints, so the
   desktop layout above is untouched. Keep the Now-Playing card a COMPACT
   horizontal row (small poster + info) rather than a full-width stacked
   poster, tighten the sticky header so the wordmark stops crowding the
   nav, wrap the action buttons, and enlarge tap targets for touch. */
@media(max-width:600px){
  .c{padding:10px 10px 20px}
  .headerbar{padding:10px 12px;gap:6px 10px}
  .brand{font-size:.95rem;letter-spacing:2px}
  .nav{font-size:.9rem;padding:7px 0}
  .card{padding:13px;margin-bottom:12px}
  .np{gap:14px;padding:14px;min-height:0}
  .poster,.ph{width:84px;height:126px;min-height:0;max-height:none}
  .ph svg{width:28px;height:28px}
  .mt{font-size:1.2rem}
  .mo{-webkit-line-clamp:4}
  .actions{flex-wrap:wrap}
  .btn{padding:7px 12px}
  .log{font-size:.72rem;padding:10px}
  table{font-size:.75rem}
  th,td{padding:6px 8px}
  .dtab{padding:6px 12px}
}
@media(max-width:400px){
  .brand{display:none}
  .mt{font-size:1.12rem}
  .poster,.ph{width:72px;height:108px}
}
</style>
</head>
<body>
<div class="c">
<div class="headerbar">
  <span class="brand">AUTORIP</span>
  <button class="nav active" data-tab="ripper">Ripper</button>
  <button class="nav" data-tab="library">Library</button>
  <button class="nav" data-tab="system">System</button>
  <button class="nav" data-tab="settings">Settings</button>
  <button class="btn" style="margin-left:auto" onclick="toggleTheme()" id="thm"></button>
</div>

<!-- Ripper page -->
<div id="ripper" class="section active">
  <div id="dtabs"></div>
  <div id="muxbanner"></div>
  <div id="np"></div>
  <div id="actions"></div>
  <div id="steps" style="margin-bottom:16px"></div>
  <div id="err"></div>
  <details style="margin-top:16px"><summary style="font-size:.7rem;color:var(--text3);text-transform:uppercase;font-weight:600;letter-spacing:1px;cursor:pointer;user-select:none">Log</summary>
  <div id="log" class="log" style="flex:1;max-height:none;margin-top:8px"></div></details>
  <details id="debugBox" open style="margin-top:12px;display:none"><summary style="font-size:.7rem;color:var(--accent);text-transform:uppercase;font-weight:600;letter-spacing:1px;cursor:pointer;user-select:none">Debug Log (live) — the patch walk + timings</summary>
  <div id="debuglog" class="log" style="flex:1;max-height:none;margin-top:8px"></div></details>
</div>

<!-- Library page -->
<div id="library" class="section">
  <div class="card" style="margin-top:16px">
    <div id="libsum" style="font-size:.85rem;color:var(--text2)"></div>
    <div style="display:flex;gap:8px;flex-wrap:wrap;align-items:center;margin-top:10px">
      <button class="btn" onclick="libPost('/api/library/queue/out-of-date',null,this)">Remux out of date</button>
      <button class="btn" onclick="if(confirm('Remux every title that has an ISO?'))libPost('/api/library/queue/all',null,this)">Remux all</button>
      <button class="btn" id="libpause" onclick="libPost(this.dataset.paused==='1'?'/api/library/queue/resume':'/api/library/queue/pause',null,this)">Pause queue</button>
      <button class="btn" onclick="libPost('/api/library/queue/clear',null,this)">Clear finished</button>
      <label class="toggle" style="margin-left:auto;font-size:.8rem"><input type="checkbox" id="libdebug" onchange="libPost('/api/library/debug',{enabled:this.checked},null)"> Debug log</label>
    </div>
  </div>
  <div id="librun"></div>
  <div id="libtable"></div>
  <details open style="margin-top:12px"><summary style="font-size:.7rem;color:var(--text3);text-transform:uppercase;font-weight:600;letter-spacing:1px;cursor:pointer">Console</summary>
  <div id="libconsole" class="log" style="margin-top:8px;max-height:320px"></div></details>
</div>

<!-- System page -->
<div id="system" class="section">
  <div id="review"></div>
  <div class="card" style="margin-top:16px"><h2>Mux Queue</h2><div id="muxes"></div></div>
  <div class="card"><h2>Move Queue</h2><div id="moves"></div></div>
  <div class="card"><div class="setting"><label class="toggle"><input type="checkbox" id="debugToggle" onchange="toggleDebug(this.checked)"> Debug logging</label><div class="hint">Verbose logs for bug reports (autorip + rip library). Off by default.</div></div></div>
  <div><h2 style="font-size:.7rem;color:var(--text3);text-transform:uppercase;font-weight:600;letter-spacing:1px;margin-bottom:8px">System Log</h2><div id="syslog" class="log" style="max-height:400px"></div></div>
</div>

<!-- Settings page -->
<div id="settings" class="section">
  <div style="margin-top:16px">
  <div id="settings-form"></div>
  <div style="position:sticky;bottom:0;padding:12px 0;background:var(--bg)">
  <button class="btn" id="savebtn" onclick="saveSettings()">Save</button>
  <span id="save-status" style="margin-left:8px;font-size:.8rem;color:var(--green)"></span>
  </div>
  </div>
</div>
</div>

<div style="text-align:center;padding:16px;font-size:.7rem"><a href="https://github.com/freemkv/autorip" style="color:var(--text3);text-decoration:none" target="_blank">autorip v{VERSION}</a></div>

<script>
/* ---- Theme ---- */
const _sun='<svg width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><circle cx="12" cy="12" r="4"/><path d="M12 2v2M12 20v2M4.93 4.93l1.41 1.41M17.66 17.66l1.41 1.41M2 12h2M20 12h2M6.34 17.66l-1.41 1.41M19.07 4.93l-1.41 1.41"/></svg>';
const _moon='<svg width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M21 12.79A9 9 0 1 1 11.21 3 7 7 0 0 0 21 12.79z"/></svg>';
function toggleTheme(){document.body.classList.toggle('dark');localStorage.setItem('theme',document.body.classList.contains('dark')?'dark':'light');document.getElementById('thm').innerHTML=document.body.classList.contains('dark')?_sun:_moon}
(function(){
  const saved=localStorage.getItem('theme');
  if(saved==='dark'||(saved==null&&window.matchMedia('(prefers-color-scheme:dark)').matches))document.body.classList.add('dark');
  document.getElementById('thm').innerHTML=document.body.classList.contains('dark')?_sun:_moon;
})();

/* ---- Util ---- */
function esc(s){if(s==null)return'';return String(s).replace(/&/g,'&amp;').replace(/</g,'&lt;').replace(/>/g,'&gt;').replace(/"/g,'&quot;').replace(/'/g,'&#39;')}
/* Turn bare https:// URLs into anchors. Matched on the RAW text and each
   piece escaped separately, so a URL next to a quote can't swallow its
   &#39;/&quot; entity. Trailing sentence punctuation stays outside the link. */
function escLinks(s){if(s==null)return'';s=String(s);let out='',last=0,m;const re=/https:\/\/[^\s<>"']+/g;
  while((m=re.exec(s))!==null){let u=m[0],tail='';const t=u.match(/[.,;:)\]]+$/);if(t){tail=t[0];u=u.slice(0,-tail.length)}
    out+=esc(s.slice(last,m.index))+'<a href="'+esc(u)+'" target="_blank" rel="noopener noreferrer" style="color:inherit">'+esc(u)+'</a>'+esc(tail);last=m.index+m[0].length}
  return out+esc(s.slice(last))}
function upd(id,html){const el=document.getElementById(id);if(el&&el._last!==html){el.innerHTML=html;el._last=html}}
/* Every device action button goes through this, and none of them may be
   fire-and-forget. The drive-card buttons used to call fetch() bare, with no
   .then and no .catch: the server's answer was DISCARDED. Two of them are
   rendered exactly in the window where the server now answers 409 — Eject
   renders on discIn && !active, and "Accept & deliver" renders on
   lossAborted && !active — and both of those windows can overlap a worker that
   is still unwinding, which the claim refuses. So the operator clicked Eject
   and the disc stayed in the drive; or clicked "Accept & deliver", watched the
   button grey out (it set this.disabled=true first, which READS as success),
   and no `.accept-loss` marker was ever written. A 409 rendered as a success.
   Report the failure, and put the button back the way it was. */
function apiPost(u,btn,label){
  if(btn)btn.disabled=true;
  return fetch(u,{method:'POST'}).then(function(r){
    if(r.ok)return null;
    return r.text().then(function(t){
      let msg='';
      try{const j=JSON.parse(t);if(j&&j.error)msg=j.error}catch(e){}
      throw new Error(msg||('HTTP '+r.status));
    });
  }).catch(function(e){
    alert((label||'Request')+' failed: '+e.message);
  }).then(function(){
    /* Re-enable even on success: the poll re-renders the card from server
       state a moment later, and a button left disabled after a failure is
       exactly the "it looked like it worked" bug. */
    if(btn)btn.disabled=false;
  });
}

/* ---- Library ---- */
let _libGen=-1,_libBusy=false;
function libActive(){return document.getElementById('library').classList.contains('active')}
function libGB(b){return b==null?'':(b/1e9).toFixed(1)+' GB'}
function libHms(s){if(s==null)return'-';s=Math.round(s);return Math.floor(s/3600)+':'+String(Math.floor(s/60)%60).padStart(2,'0')+':'+String(s%60).padStart(2,'0')}
function libWhen(t){return t?new Date(t*1000).toLocaleString([],{month:'short',day:'numeric',hour:'2-digit',minute:'2-digit'}):''}
function libPost(u,body,btn){
  if(btn)btn.disabled=true;
  const opt={method:'POST'};if(body){opt.body=JSON.stringify(body);opt.headers={'Content-Type':'application/json'}}
  return fetch(u,opt).then(r=>r.ok?null:r.text().then(t=>{throw new Error(t||('HTTP '+r.status))}))
    .catch(e=>alert('Request failed: '+e.message)).then(()=>{if(btn)btn.disabled=false;loadLibrary(false)});
}
function libRunHtml(r){
  if(!r)return'';
  const pct=r.pct==null?null:r.pct;
  const bar=pct==null?'':'<div style="height:6px;background:var(--border);border-radius:3px;overflow:hidden;margin:8px 0"><div style="height:100%;width:'+pct.toFixed(1)+'%;background:var(--accent)"></div></div>';
  const stats=pct==null?esc(r.phase):(pct.toFixed(1)+'% \u00b7 '+(r.speed_bps/1e6).toFixed(1)+' MB/s \u00b7 ETA '+libHms(r.eta_secs)+' \u00b7 '+esc(r.phase));
  const stall=r.stalled_secs>=60?' <span class="warn">no progress for '+libHms(r.stalled_secs)+'</span>':'';
  return '<div class="card"><b>'+esc(r.title)+'</b>'+bar+'<div style="font-size:.8rem;color:var(--text2)">'+stats+stall+'</div></div>';
}
function libConsole(lines,reset){
  const el=document.getElementById('libconsole');if(!el)return;
  const atEnd=el.scrollTop+el.clientHeight>=el.scrollHeight-20;
  if(reset)el.textContent='';
  el.textContent+=lines.map(l=>l.text+'\n').join('');
  if(atEnd)el.scrollTop=el.scrollHeight;
}
function libEvent(f){
  if(!libActive())return;
  libConsole(f.lines||[],false);
  upd('librun',libRunHtml(f.running));
  if(f.queue_generation!==_libGen){_libGen=f.queue_generation;loadLibrary(false)}
}
function libMuxed(r){
  const m=r.muxed_with;
  if(!r.mkv)return r.kind==='iso_only'?'<span class="warn">no MKV</span>':'';
  if(m.state==='current')return '<span class="ok">'+esc(m.version)+'</span>';
  if(m.state==='older')return '<span class="warn" title="not muxed with this version">'+esc(m.version)+'</span>';
  if(m.state==='other')return '<span class="warn">'+esc(m.app)+'</span>';
  return '-';
}
function libAudit(r){
  if(!r.mkv)return'';
  const a=r.audit;if(!a)return '<span style="color:var(--text3)">pending</span>';
  if(a.ok&&!a.issues.length)return '<span class="ok">ok</span>';
  const t=a.issues.map(i=>i.kind.replace(/_/g,' ')).join(', ');
  return '<span class="'+(a.ok?'':'warn')+'" title="'+esc(t)+'">'+esc(t)+'</span>';
}
function libStatus(r){
  const j=r.job,res=r.result;let s='';
  if(j&&j.state==='queued')s='<span style="color:var(--yellow)">queued'+(j.note?' ('+esc(j.note)+')':'')+'</span>';
  else if(j&&j.state==='running')s='<span style="color:var(--accent)">running</span>';
  else if(res&&res.outcome==='done')s='<span class="ok">\u2713 '+libGB(res.size_bytes)+' in '+libHms(res.secs)+'</span> <span style="color:var(--text3)">'+libWhen(res.finished_at)+'</span>';
  else if(res&&res.outcome==='failed')s='<span class="warn" title="'+esc(res.message)+'">\u2717 '+esc((res.code!=null&&res.message.indexOf('E'+res.code)<0?'E'+res.code+' ':'')+res.message).slice(0,90)+'</span>';
  if(j||res)s+=' <a href="/api/library/log?title='+encodeURIComponent(r.title)+'" target="_blank" style="font-size:.75rem">log</a>';
  return s;
}
function renderLibrary(d){
  const rows=d.rows||[],q=d.queue||{};
  const n=rows.filter(r=>r.needs_remux).length,isos=rows.filter(r=>r.iso).length,mkvs=rows.filter(r=>r.mkv).length;
  upd('libsum','freemkv <b>'+esc(d.version)+'</b> \u00b7 '+mkvs+' MKVs \u00b7 '+isos+' ISOs \u00b7 '+n+' out of date or missing \u00b7 '+q.queued+' queued'+(q.paused?' \u00b7 <b class="warn">queue paused</b>':'')+(d.iso_dir?'':' \u00b7 <span class="warn">no ISO folder set</span>')+(d.incomplete?' \u00b7 <span class="warn">a folder could not be fully read</span>':''));
  const pb=document.getElementById('libpause');if(pb){pb.dataset.paused=q.paused?'1':'0';pb.textContent=q.paused?'Resume queue':'Pause queue'}
  const dbg=document.getElementById('libdebug');if(dbg)dbg.checked=!!q.debug_log;
  upd('librun',libRunHtml(d.live));
  const order=r=>(r.kind==='remux'||r.kind==='iso_only')?0:1;
  const sorted=rows.slice().sort((a,b)=>order(a)-order(b));
  let h='<table style="display:table"><thead><tr><th>Title</th><th>ISO</th><th>Muxed with</th><th>Audit</th><th>Remux</th><th></th></tr></thead><tbody>';
  sorted.forEach(r=>{
    const can=r.kind==='remux'||r.kind==='iso_only';
    const busy=r.job&&(r.job.state==='queued'||r.job.state==='running');
    const note=r.note?(r.note.kind==='several_isos'?r.note.count+' ISOs match; rename one':r.note.count+' MKVs match'):(r.iso?'':'no ISO');
    const iso=r.iso?esc(r.iso.split('/').pop())+(r.linked?' <span title="recorded at rip time">\u{1F517}</span>':''):'<span style="color:var(--text3)">'+esc(note)+'</span>';
    const btn=can?'<button class="btn" '+(busy?'disabled ':'')+'data-target="'+esc(r.target)+'" onclick="libPost(\'/api/library/queue/add\',{target:this.dataset.target},this)">Remux</button>':'';
    h+='<tr'+(can?'':' style="opacity:.5"')+'><td><b>'+esc(r.title)+'</b></td><td style="font-size:.75rem">'+iso+'</td><td>'+libMuxed(r)+'</td><td style="font-size:.75rem">'+libAudit(r)+'</td><td style="font-size:.75rem">'+libStatus(r)+'</td><td>'+btn+'</td></tr>';
  });
  h+='</tbody></table>';
  upd('libtable',h);
}
function loadLibrary(withConsole){
  if(_libBusy)return;_libBusy=true;
  fetch('/api/library',{cache:'no-store'}).then(r=>r.json()).then(d=>renderLibrary(d)).catch(()=>{}).then(()=>{_libBusy=false});
  if(withConsole)fetch('/api/library/console',{cache:'no-store'}).then(r=>r.json()).then(c=>libConsole(c.lines||[],true)).catch(()=>{});
}

/* ---- Navigation ---- */
document.querySelectorAll('.nav[data-tab]').forEach(btn=>{
  btn.addEventListener('click',function(){
    const tab=this.dataset.tab;
    document.querySelectorAll('.section').forEach(s=>s.classList.remove('active'));
    document.getElementById(tab).classList.add('active');
    document.querySelectorAll('.nav[data-tab]').forEach(b=>b.classList.remove('active'));
    this.classList.add('active');
    if(tab==='system')loadSystem();
    if(tab==='library')loadLibrary(true);
    if(tab==='settings')loadSettings();
  });
});

/* ---- Browser notifications ---- */
if(typeof Notification!=='undefined'&&Notification.permission==='default')Notification.requestPermission();
function notify(title,body,icon){
  if(typeof Notification!=='undefined'&&Notification.permission==='granted'){
    try{new Notification(title,{body:body,icon:icon||''})}catch(e){}
  }
}

/* ---- Disc SVG icon ---- */
const D='<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.5"><circle cx="12" cy="12" r="10"/><circle cx="12" cy="12" r="3"/></svg>';

/* ---- Codec/Resolution maps ---- */
/* ---- Step-by-step progress ---- */
const ACTIVE_STATES=['ripping','scanning','detecting'];
let _lastStatus={};
let _activeTab=null;

function renderBar(s,p){
  /* Two modes, one renderer:
     - Pass 1 (sequential sweep, s.pass<=1): a genuine left-to-right progress
       fill. x-axis = work done. Green grows to the read head. Only damage the
       sweep has ALREADY passed over shows red; the unread region ahead of the
       head stays blank (NonTried is unknown, not bad — see the overlay clip
       below).
     - Pass 2-N (retry/patch, s.pass>1): a POSITIONAL disc map ("the disc,
       coloured by status"). x-axis = DISC POSITION (0..bytes_total_disc), NOT
       work done. The whole bar is the disc: GREEN everywhere it's good, RED
       segments at each still-bad range's real offset. As patches recover
       sectors, bad_ranges shrink and the red heals to green IN PLACE. A blue
       PLAYHEAD marks the current read position (last_sector) and pulses so it
       reads as "actively working here". */
  const total=s&&s.bytes_total_disc||0;
  const ranges=s&&s.bad_ranges||[];
  const positional=!!(s&&s.pass>1);
  /* Same height in EVERY pass (pass 1 sweep + pass N defrag map) so the bar
     doesn't resize between phases. 20px reads as a disc map, not a hairline. */
  const barH=20;
  /* "You are here" caret ABOVE the bar — consistent in EVERY pass.
     pass 1 (sweep): at last_sector, the real sequential read head.
     pass N (patch): at the active section = the highest-LBA bad range still
     present (autorip patches in reverse, so it steps left as ranges recover).
     Derived from published state — in patch, last_sector is work-done (not a
     disc LBA), so the active-section position is used instead. */
  let caretPct=null;
  if(total>0&&s&&s.status==='ripping'){
    if(positional&&ranges.length){
      /* The handler-chain engine works the LARGEST bad range first
         (largest-first ordering), so the live position sits on the largest
         remaining range, not the highest-LBA one. Track that — otherwise the
         caret parks on a small high-LBA block while recovery is really chewing
         a big block elsewhere. When the largest range shrinks below another,
         the arrow jumps to the new largest. */
      let act=ranges[0];
      ranges.forEach(r=>{ if(r.count>act.count) act=r; });
      /* Align to the red block's RENDERED right edge — same offset + min-width
         0.5% clamp the overlay uses below — so the arrow sits exactly on the
         end of the red even for tiny ranges (where the true lba+count would
         fall short of the clamped-wider block). */
      let offPct=(act.lba*2048)/total*100;
      if(offPct<0)offPct=0; if(offPct>100)offPct=100;
      let wPct=Math.max((act.count*2048)/total*100,0.5);
      if(offPct+wPct>100)wPct=100-offPct;
      caretPct=offPct+wPct;
    }else if(!positional&&s.last_sector>0){
      caretPct=(s.last_sector*2048)/total*100;
    }
  }
  let caret='';
  if(caretPct!=null){
    if(caretPct<0)caretPct=0; if(caretPct>100)caretPct=100;
    caret='<div style="position:relative;height:14px">'
      +'<div style="position:absolute;left:'+caretPct+'%;bottom:0;transform:translateX(-50%);'
      +'font-size:.8rem;color:var(--blue);line-height:1;animation:ph 1.2s infinite">'
      +'▼</div></div>';
  }
  let html='<div style="background:var(--chip);border-radius:4px;height:'+barH+'px;overflow:hidden;position:relative">';
  if(positional){
    /* Positional map: the entire bar is green (the whole disc, good), then
       red bad ranges are punched in at their real offsets. If total is
       unknown (0) we can't place anything positionally — fall back to a
       neutral green fill so we never divide by zero. */
    html+='<div style="position:absolute;left:0;top:0;width:100%;height:100%;background:var(--green)"></div>';
  }else{
    /* Sequential sweep: green grows with the swept position. Use the
       FRACTIONAL position (last_sector advances every 250 ms) rather than the
       integer pass_progress_pct, otherwise the fill jumps a whole 1% at a time
       (~50 s per step on a UHD) and reads as "nothing, boom chunk". */
    let fillPct=(total>0&&s&&s.last_sector>0)?(s.last_sector*2048)/total*100:p;
    if(fillPct<0)fillPct=0; if(fillPct>100)fillPct=100;
    html+='<div style="background:var(--green);height:100%;width:'+fillPct+'%;transition:width 1s"></div>';
  }
  /* Red bad-range overlay. Drawn at each range's real LBA position. min-width
     0.5% keeps single-sector ranges visible on a 72GB UHD; clamp left+width so
     a range near the tail never overflows the bar.

     Pass 1 (sweep): the region AHEAD of the read head is UNREAD — NonTried, not
     bad. We don't know if it's good or bad until it's read, so it stays BLANK
     (same reason the "maybe" bucket shows no red pill: nothing is a verdict
     until it's been read). The live bad_ranges during a sweep is dominated by
     one giant NonTried range covering the whole un-swept tail; painting it red
     makes a pristine disc look 100% damaged. So CLIP the red to the swept
     portion [0, last_sector]: only damage the sweep has actually passed over
     shows red; the unread remainder is neutral track.
     Pass N (positional): every bad_range IS determined-bad, so draw it in full. */
  if(total>0&&ranges.length){
    const sweptPct=positional?100
      :((s&&s.last_sector>0)?(s.last_sector*2048)/total*100:0);
    ranges.forEach(r=>{
      let offPct=(r.lba*2048)/total*100;
      if(offPct<0)offPct=0; if(offPct>100)offPct=100;
      let wPct=Math.max((r.count*2048)/total*100,0.5);
      if(offPct+wPct>100)wPct=100-offPct;
      if(!positional){
        /* Blank until read: skip ranges entirely ahead of the head, trim the
           unread tail off one that straddles it. */
        if(offPct>=sweptPct)return;
        if(offPct+wPct>sweptPct)wPct=sweptPct-offPct;
        if(wPct<=0)return;
      }
      html+='<div style="position:absolute;left:'+offPct+'%;top:0;width:'+wPct+'%;height:100%;background:var(--red);opacity:0.9;transition:left 1s,width 1s"></div>';
    });
  }
  html+='</div>';
  return caret+html;
}
function passLabelFor(s){
   /* Resolve the current pass into a human-readable label for the Ripping
      step. During multipass we show pass number + phase; otherwise "Ripping". */
   if(s.pass>0&&s.total_passes>0){
     /* This is the RIP tab: mux is a 100% separate process and view, so it
        is NOT a rip pass and is excluded from the count. The backend's
        total_passes still includes the trailing mux pass (max_retries + 2);
        subtract it here so the operator sees "pass 2/6" (sweep + 5 retries),
        never "2/7". */
     const ripTotal=Math.max(s.total_passes-1,1); // drop the mux pass
     if(s.pass>=s.total_passes){
       /* Mux phase \u2014 from the rip tab's view, recovery is finished;
          muxing lives in its own view. */
       return 'recovery complete';
     }
     const phase=s.pass===1?'copying':'retrying';
     return 'pass '+s.pass+'/'+ripTotal+' \u00b7 '+phase;
   }
   /* If pass=1 and no total_passes set, this is a clean disc — skip to mux. */
   if(s.pass===1&&s.total_passes===0){
     return 'pass 1/1 · copying';
   }
   return '';
 }
function renderSteps(steps,progress,eta,speed,s){
  if(!steps||!steps.length)return'';
  const icons={done:'\u2713',active:'\u25cf',pending:'\u25cb'};
  const colors={done:'var(--green)',active:'var(--accent)',pending:'var(--text3)'};
  return steps.map(st=>{
    let detail=st.detail||'';
    if(st.status==='active'&&st.name==='Ripping'){
      /* v0.13.18: two distinct bars + their own text rows.
           [pass bar           ] X% \u00b7 ETA H:MM \u00b7 NN MB/s
           [total bar          ] Total Y% \u00b7 Total ETA H:MM \u00b7 Recovered A.B / C.D GB
         Both bars read pass_progress_pct / total_progress_pct directly from
         the server. JS does NO math. */
      const passPct=(typeof s.pass_progress_pct==='number')?s.pass_progress_pct:(parseInt(progress)||0);
      const totalPct=(typeof s.total_progress_pct==='number')?s.total_progress_pct:passPct;
      const passLbl=passLabelFor(s);
      const header=passLbl?' \u00b7 '+passLbl:'';
      /* Three fixed-width columns + tabular-nums so the per-pass and
         total rows align visually: digits stack vertically across rows
         instead of shifting as the value width changes ("9%" -> "10%",
         "ETA 1:30:45" -> "ETA 0:05"). Empty slots reserve their column
         width so the totalLine doesn't drift right when speed is blank. */
      const TAB='font-variant-numeric:tabular-nums;display:inline-block;';
      const col=(body,minPx,align)=>
        '<span style="'+TAB+'min-width:'+minPx+'px;text-align:'+align+'">'+body+'</span>';
      const passPctStr=col(passPct+'%',45,'right');
      const passEtaStr=col(s.pass_eta?'ETA '+s.pass_eta:'',95,'left');
      const spdStr=col(speed||'',85,'left');
      /* v0.13.19: wider text-row separators (em-spaces around the middle dot)
         + more vertical breathing room between bars and their text rows so
         the dashboard doesn't feel cramped. */
      const SEP=' \u2003\u00b7\u2003 ';
      /* Don't filter empty slots — they're already wrapped in fixed-width
         spans and need to keep their column position. Always join all 3. */
      const passLine=[passPctStr,passEtaStr,spdStr].join(SEP);
      /* 0.13.24: mirror the per-pass line's terse format. Drop the
         redundant "Total " prefix on ETA (the leading "Total N%" already
         makes the bar's identity obvious), and drop "Recovered X / Y GB"
         entirely — the green Good pill carries the same information
         without duplicating it. */
      /* No second "total" bar — mux is a separate view, and a bytes-recovered
         bar just sits at ~99% through the whole patch (the sweep already
         recovered nearly everything; the slow grind is the last <1%). The
         live patch signal is TEXTUAL instead: how many disc sections are
         still bad and how many sectors remain. */
      const fmtBytes = (b)=> b>=1073741824 ? (b/1073741824).toFixed(2)+' GB'
                          : b>=1048576    ? (b/1048576).toFixed(1)+' MB'
                          : b>=1024       ? (b/1024).toFixed(1)+' KB'
                          : b+' B';
      let sectorsLine='';
      {
        /* Same line in EVERY pass (consistency): bytes_maybe is the bad/not-yet-
           good set (NonTrimmed+NonScraped), so this reads as "damage left to
           recover" in both the sweep and the patch passes. The "N sections ·"
           prefix only appears once bad ranges exist. */
        const nSec=(typeof s.num_bad_ranges==='number'&&s.num_bad_ranges>0)
                    ?s.num_bad_ranges:((s.bad_ranges&&s.bad_ranges.length)||0);
        const remBytes=(s.bytes_maybe||0)+(s.bytes_lost||0);
        const remSect=Math.round(remBytes/2048);
        if(remSect>0){
          const secPrefix = nSec>0 ? (nSec+' '+(nSec===1?'section':'sections')+' · ') : '';
          sectorsLine='<div style="font-size:.75rem;color:var(--text2);margin-top:7px;font-variant-numeric:tabular-nums">'
            +secPrefix+remSect.toLocaleString()+' sectors ('+fmtBytes(remBytes)+') remaining</div>';
        }
      }
      let badLine='';
      /* TWO pills, ever \u2014 Good and Maybe. Never a third.
         GOOD  = whole-disc bytes successfully read off the disc (Finished).
         MAYBE = whole-disc bytes not-yet-good: pending, NonTrimmed, and
                 currently-unreadable/undecryptable all folded together.
                 NOTHING is "lost"/"no chance" mid-rip \u2014 a later pass (or a
                 freshly power-cycled drive) still recovers it, so there is no
                 terminal bucket here. "Bad" is a VERDICT, decided once after
                 the final pass (main-feature lost time vs abort_on_lost_secs),
                 not a live pill.
         The Maybe pill's BYTES are whole-disc, but its TIME is the MAIN-FEATURE
         lost time (main_lost_ms) \u2014 that is what the abort gate judges. So
         "Maybe 990 MB \u00b7 0:00" = 990 MB pending but zero movie time \u21d2 will pass;
         "Maybe 12 KB \u00b7 ~1 ms" = a few sectors of movie \u21d2 fails a 0 threshold.
         Time is rendered at ms precision (fmtMs): 6 sectors is 1 ms, never 0. */
      const bg=s.bytes_good||0, bm=(s.bytes_maybe||0)+(s.bytes_lost||0);
      if(bg>0 || bm>0){
        /* FIXED width (not min-width) + border-box so the pills never grow or
           shrink as the byte/time values change — the row stays rock-steady. */
        const pill = (label, color, body, wPx)=>
          '<span style="display:inline-block;box-sizing:border-box;padding:2px 8px;border-radius:10px;background:'+color
          +';color:var(--pill-fg);font-size:.65rem;font-weight:600;margin-right:6px;'
          +'width:'+wPx+'px;text-align:center;white-space:nowrap;overflow:hidden;font-variant-numeric:tabular-nums">'
          +label+' '+body+'</span>';
        let pills='';
        if(bg>0){
          pills+=pill('Good','var(--green,#3aaa55)', fmtBytes(bg), 150);
        }
        if(bm>0){
          /* Time = MAIN-FEATURE movie time at risk (main_at_risk_ms: pending +
             lost \u2229 feature), NOT the terminal Unreadable-only main_lost_ms which
             is structurally 0 until the final pass. So 0:00 honestly means "no
             movie impact" even mid-rip, and a real ms figure means the movie is
             affected. Melts toward 0 as retries recover pending sectors. */
          const atRiskMs = (s.main_at_risk_ms!=null && s.main_at_risk_ms>=0) ? s.main_at_risk_ms : 0;
          const t = atRiskMs>0 ? '~'+fmtMs(atRiskMs) : '0:00';
          pills+=pill('Maybe','var(--yellow,#f0c000)', fmtBytes(bm)+' \u00b7 '+t, 200);
        }
        if(pills) badLine='<div style="font-size:.7rem;margin-top:14px">'+pills+'</div>';
      }
      detail='<div style="margin-top:6px">'
        +renderBar(s,passPct)
        +'<div style="font-size:.75rem;color:var(--text2);margin-top:7px">'+passLine+'</div>'
        +sectorsLine
        +badLine+'</div>';
      /* Fold pass info into the step name so it's obvious at a glance. */
      if(passLbl){
        /* 0.13.25: flex:1 + min-width:0 on the content span pins it to
           the remaining row width regardless of inner text length. Without
           this the span sizes to its content, so a longer header
           ("Pass 2/7: retrying bad ranges") makes the bar inside `detail`
           wider than a shorter one ("pass 1/7 · copying"), producing
           visible width wobble as the rip moves through phases. */
        return '<div style="display:flex;align-items:flex-start;gap:8px;padding:4px 0;font-size:.8rem"><span style="color:'+colors[st.status]+';font-size:.7rem;width:14px;text-align:center;flex-shrink:0;animation:p 1.5s infinite">'+icons[st.status]+'</span><span style="color:var(--text);flex:1;min-width:0">Rip'+header+detail+'</span></div>';
      }
    }else if(detail){detail=' \u2014 '+escLinks(detail)}
    const anim=st.status==='active'?';animation:p 1.5s infinite':'';
    return '<div style="display:flex;align-items:flex-start;gap:8px;padding:4px 0;font-size:.8rem"><span style="color:'+colors[st.status]+';font-size:.7rem;width:14px;text-align:center'+anim+'">'+icons[st.status]+'</span><span style="color:'+(st.status==='pending'?'var(--text3)':'var(--text)')+'">'+st.name+detail+'</span></div>';
  }).join('');
}
function fmtMs(ms){
  /* 0.13.24: escalate to minutes / hours / H:MM:SS for large durations.
     "10817 s" by itself means nothing — render it as "3:00:17". Below
     1 s we still want millisecond precision for tight read traces. */
  if(ms==null||!isFinite(ms))return'';
  if(ms<1)return'<1 ms';
  if(ms<1000)return ms.toFixed(0)+' ms';
  const totalSecs=ms/1000;
  if(totalSecs<60)return totalSecs.toFixed(2)+' s';
  const h=Math.floor(totalSecs/3600);
  const m=Math.floor((totalSecs%3600)/60);
  const s=Math.floor(totalSecs%60);
  return h>0
    ? h+':'+String(m).padStart(2,'0')+':'+String(s).padStart(2,'0')
    : m+':'+String(s).padStart(2,'0');
}
/* ---- Build steps from state ---- */
function buildSteps(s){
  const steps=[];
  const st=s.status;
  if(st==='idle')return[];
  if(st==='scanning'){
    steps.push({name:'Scanning',status:'active',detail:''});
    steps.push({name:'Ripping',status:'pending',detail:''});
    steps.push({name:'Done',status:'pending',detail:''});
  }else if(st==='ripping'){
    steps.push({name:'Scanning',status:'done',detail:''});
    steps.push({name:'Ripping',status:'active',detail:''});
    steps.push({name:'Done',status:'pending',detail:''});
  }else if(st==='moving'||st==='done'){
    steps.push({name:'Scanning',status:'done',detail:''});
    steps.push({name:'Ripping',status:'done',detail:''});
    steps.push({name:'Done',status:'done',detail:''});
  }else if(st==='error'){
    steps.push({name:'Error',status:'active',detail:s.last_error||''});
  }
  return steps;
}

/* ---- Ripper page render ---- */
function handleState(data){
  /* Persist the latest payload + refresh the Move Queue first — the
     mover keeps running (and `_move` keeps changing) even when the
     drive list is empty (idle / state briefly cleared), so the
     no-devices early return below must not gate Move Queue updates. */
  window._stateData=data;
  renderMuxBanner(data);
  if(document.getElementById('system').classList.contains('active')){renderMuxes();renderMoves();}
  const devs=Object.keys(data).filter(k=>!k.startsWith('_'));
  if(!devs.length){
    upd('dtabs','');
    upd('np','<div class="np"><div class="idle-msg">'+D+'<p>No drives detected</p></div></div>');
    upd('actions','');upd('steps','');upd('err','');
    return;
  }
  const multi=devs.length>1;

  devs.forEach(dev=>{
    const s=data[dev];
    const prev=_lastStatus[dev];
    if(prev&&prev!==s.status){
      if(s.status==='done')notify('AutoRip',(s.tmdb_title||s.disc_name)+' \u2014 Complete',s.tmdb_poster);
      if(s.status==='error')notify('AutoRip',(s.tmdb_title||s.disc_name)+' \u2014 Error: '+(s.last_error||'unknown'),s.tmdb_poster);
    }
    _lastStatus[dev]=s.status;
  });

  if(!_activeTab||!devs.includes(_activeTab))_activeTab=devs[0];

  /* Device tabs */
  if(multi){
    const tabHtml=devs.map(dev=>{
      const s=data[dev];
      const active=ACTIVE_STATES.includes(s.status);
      const errState=s.status==='error';
      const dotColor=active?'var(--green)':errState?'var(--red)':'var(--text3)';
      const dotAnim=active?'animation:p 1.5s infinite;':'';
      const dot='<span style="display:inline-block;width:6px;height:6px;border-radius:50%;background:'+dotColor+';'+dotAnim+'margin-right:4px;vertical-align:middle"></span>';
      return '<span class="dtab'+(dev===_activeTab?' active':'')+'" onclick="_activeTab=\''+dev+'\';renderCurrent()">'+dot+dev+'</span>';
    }).join('');
    upd('dtabs','<div class="dtabs">'+tabHtml+'</div>');
  }else{upd('dtabs','')}

  renderCurrent();
}

/* Ripper-page banner: muxing/moving of PREVIOUS rips runs in the background
   on the System tab. When the drive is idle (No disc / done card) new users
   have no idea that work is still happening off-screen — so surface a hint
   between the header and the disc card whenever a mux or move is in flight or
   queued. It clears itself the moment both queues drain. */
function renderMuxBanner(data){
  const el=document.getElementById('muxbanner');
  if(!el)return;
  data=data||window._stateData||{};
  const mx=data._mux;
  const muxActive=!!(mx&&mx.status==='ripping'&&mx.disc_name);
  const muxQ=!!(data._mux_queue&&data._mux_queue.length);
  /* `_move` is an ARRAY of per-artifact bars (1.6.7+); tolerate the legacy
     single-object shape. An active move is any bar carrying a name. The old
     `mv.name` check silently missed the array form, so the banner never lit
     up while a move was running — mux showed, move didn't. */
  const mv=data._move;
  const moveActive=Array.isArray(mv)?mv.some(m=>m&&m.name):!!(mv&&mv.name);
  const moveQ=!!(data._move_queue&&data._move_queue.length);
  const muxing=muxActive||muxQ, moving=moveActive||moveQ;
  if(!muxing&&!moving){el.innerHTML='';return;}
  let what;
  if(muxing&&moving)what='Muxing &amp; moving of previous rips';
  else if(moving)what='Moving of previous rips';
  else what='Muxing of previous rips';
  el.innerHTML='<div onclick="goSystemTab()" '
    +'style="margin:0 0 16px;padding:10px 14px;background:var(--chip);border:1px solid var(--border);'
    +'border-radius:8px;font-size:.85rem;color:var(--text2);cursor:pointer;display:flex;align-items:center;gap:8px">'
    +'<span style="display:inline-block;width:8px;height:8px;border-radius:50%;background:var(--green);animation:p 1.5s infinite;flex-shrink:0"></span>'
    +'<span>'+what+' still in progress — see the <b>System</b> tab.</span></div>';
}
/* Programmatically activate the System tab (reuses the nav click handler, so
   loadSystem() fires). Used by the Ripper-page mux/move banner. */
function goSystemTab(){var b=document.querySelector('.nav[data-tab="system"]');if(b)b.click();}
function renderCurrent(){
  const data=window._stateData;
  if(!data)return;
  const dev=_activeTab;
  const s=data[dev];
  if(!s)return;

  /* Derived state */
  const active=ACTIVE_STATES.includes(s.status);
  const title=s.tmdb_title||s.disc_name;
  const scanned=!!title;
  const discIn=s.disc_present||scanned||active;

  /* Now Playing card */
  let card;
  if(!discIn){
    card='<div class="np"><div class="idle-msg">'+D+'<p>No disc</p></div></div>';
  }else if(!scanned){
    card='<div class="np"><div class="idle-msg">'+D+'<p>Disc detected</p></div></div>';
  }else{
    const img=s.tmdb_poster?'<img class="poster" src="'+esc(s.tmdb_poster)+'" alt="">':'<div class="ph">'+D+'</div>';
    const fmt=s.disc_format;
    const b=fmt&&fmt!=='unknown'?'<span class="b '+esc(fmt)+'">'+esc(fmt)+'</span>':'';
    const o=s.tmdb_overview?'<div class="mo">'+esc(s.tmdb_overview)+'</div>':'';
    const yr=s.tmdb_year>0?s.tmdb_year:'';
    const dur=s.duration?' \u00b7 '+esc(s.duration):'';
    const codecs=s.codecs?'<div class="mo" style="color:var(--text3);font-size:.75rem;margin-top:6px">'+esc(s.codecs)+'</div>':'';
    const ks=s.key_status||'';const rc=ks.indexOf('Missing')===0?'var(--yellow)':'var(--green)';const ready=s.status==='idle'?'<div class="mo" style="color:'+rc+'">'+esc(ks||'Ready to rip')+'</div>':'';
    /* Before ripping (idle), let the operator correct the matched title:
       search TMDB and pick — the choice overrides the auto-match for this rip. */
    const editable=s.status==='idle';
    /* ✎ change sits in a fixed row ABOVE the title (not appended to it, where it
       shifted with title length). */
    const editRow=editable?'<div style="margin-bottom:6px"><button class="btn" style="padding:1px 7px;font-size:.7rem" onclick="titleEdit(\''+dev+'\')">✎ change</button></div>':'';
    const editBox=editable?'<div id="tedit-'+dev+'" style="display:none;margin-top:8px"></div>':'';
    card='<div class="np">'+img+'<div class="nfo">'+editRow+'<div class="mt">'+esc(title)+'</div><div class="my">'+yr+dur+' '+b+'</div>'+o+codecs+ready+editBox+'</div></div>';
  }
  upd('np',card);

  /* Actions bar */
  let btns='';
  if(active){
    /* Elapsed counter goes BEFORE the Stop button: the action row is
       right-anchored, so the counter's growth (1m → 1h 02m 34s) extends
       LEFTWARD into empty space and never shoves the Stop button. tabular-nums
       keeps digits a fixed pixel width (no per-second jitter); text-align:right
       + a min-width wide enough for the "1h 02m 34s" form keeps it stable. */
    btns='<span id="rip-elapsed-'+dev+'" data-started="'+(s.started_epoch_secs||0)+'" style="margin-right:10px;font-size:.78rem;color:var(--text2);align-self:center;font-variant-numeric:tabular-nums;min-width:95px;text-align:right;display:inline-block"></span>';
    btns+='<button class="btn btn-stop" onclick="if(confirm(\'Stop?\')){apiPost(\'/api/stop/'+dev+'\',this,\'Stop\')}">Stop</button>';
  }else if(scanned){
    /* Keys resolved at scan time. If they're missing (and the operator
       hasn't opted into capture-without-keys), don't offer Rip at all —
       it would just error. Offer "Scan again" so a freshly-loaded KEYDB
       or a corrected key source can be re-checked without a page reload. */
    const notReady=(s.key_status||'').indexOf('Missing')===0;
    if(notReady){
      btns='<button class="btn" onclick="apiPost(\'/api/scan/'+dev+'\',this,\'Scan\')">Scan again</button>';
    }else if(s.resumable){
      /* A resumable partial exists. Design: the PRIMARY action (Resume —
         continue where it left off) is the filled accent button and comes
         first; "Start over" is the DESTRUCTIVE alternative (wipes the partial
         and re-sweeps from scratch), so it is de-emphasized as a red OUTLINE
         (not a green fill that competed with the primary) and confirmed. For
         "remux" Resume just re-muxes the staged ISO. */
      const rl=s.resumable==='remux'?'Resume (re-mux)':'Resume';
      btns='<button class="btn" style="background:var(--accent);color:#fff;border-color:var(--accent)" onclick="apiPost(\'/api/rip/'+dev+'?resume=yes\',this,\'Resume\')">'+rl+'</button>';
      btns+='<button class="btn" style="background:transparent;color:var(--red);border-color:var(--red)" onclick="if(confirm(\'Start over from scratch? This discards the resumable partial for this disc and re-rips the whole disc.\')){apiPost(\'/api/rip/'+dev+'?resume=no\',this,\'Start over\')}">Start over</button>';
    }else{
      btns='<button class="btn" style="background:var(--green);color:#fff;border-color:var(--green)" onclick="apiPost(\'/api/rip/'+dev+'?resume=no\',this,\'Rip\')">Rip</button>';
    }
  }else if(discIn){
    btns='<button class="btn" onclick="apiPost(\'/api/scan/'+dev+'\',this,\'Scan\')">Scan</button>';
  }
  /* Loss-aborted off-ramp: the rip aborted because main-movie loss exceeded the
     threshold, but the COMPLETE ISO is staged on disk. Offer exactly TWO clear
     choices and REPLACE the generic start-over (so the operator isn't offered a
     destructive fresh rip here):
       • "Run one more pass" — Resume (resume=yes): another recovery pass over
         the bad ranges (Pass N from the mapfile, recovering only the bad core).
       • "Accept & deliver"  — accept the recorded loss and deliver as-is
         (re-mux with the abort gate bypassed; one-shot, confirmed).
     Detected by the loss_aborted flag OR a loss-abort last_error; shown ONCE.
     (Previously two separate blocks each appended an "Accept damage & deliver",
     so it rendered twice with no Resume; --yellow is a dark brown, so black
     text on it was unreadable — Resume is accent/white, Accept is amber-outlined.) */
  const lossAborted=s.loss_aborted||(s.last_error||'').indexOf('lost in main movie')>=0||(s.last_error||'').indexOf('lost at mux')>=0;
  if(lossAborted&&!active){
    btns='<button class="btn" style="background:var(--accent);color:#fff;border-color:var(--accent)" onclick="apiPost(\'/api/rip/'+dev+'?resume=yes\',this,\'Run one more pass\')">Run one more pass</button>';
    btns+='<button class="btn" style="background:transparent;color:var(--yellow);border-color:var(--yellow);font-weight:500" onclick="if(confirm(\'Accept the recorded main-movie damage and deliver this rip as-is? The unreadable section will be missing, but the rest is intact.\')){apiPost(\'/api/accept-loss/\'+dev,this,\'Accept &amp; deliver\')}">Accept &amp; deliver</button>';
  }
  if(discIn&&!active)btns+='<button class="btn btn-eject" onclick="apiPost(\'/api/eject/'+dev+'\',this,\'Eject\')">Eject</button>';

  const dot=active?'var(--green)':scanned?'var(--accent)':discIn?'var(--yellow)':'var(--text3)';
  const pulse=active?'animation:p 1.5s infinite;':'';
  /* statusLabel intentionally not shown here \u2014 it's already in the
     Ripping step header below ("Rip \u00b7 pass N/M \u00b7 copying") and the tab
     strip identifies which device this panel is for. Keep just the
     colored dot + dev name + action buttons in this row. */
  upd('actions','<div class="actions"><span style="display:inline-block;width:8px;height:8px;border-radius:50%;background:'+dot+';vertical-align:middle;margin-right:6px;'+pulse+'"></span><span style="font-size:.8rem;color:var(--text2)">'+dev+'</span><span style="margin-left:auto;display:flex;gap:6px">'+btns+'</span></div>');

  /* Steps */
  const steps=buildSteps(s);
  const progressStr=s.progress_pct>0?s.progress_pct+'%':(s.progress_gb>0?s.progress_gb.toFixed(1)+' GB':'');
  const speedStr=fmtSpeed(s.speed_mbs);
  const etaStr=s.eta||'';
  upd('steps',renderSteps(steps,progressStr,etaStr,speedStr,s));

  /* Error + recovery banner */
  let errHtml='';
  if(s.errors>0&&s.last_error){
    errHtml='<div style="background:var(--red);color:#fff;padding:8px 12px;border-radius:6px;font-size:.8rem;margin-bottom:8px">\u26a0 '+escLinks(s.last_error)+'</div>';
  }
  /* The old "N sectors skipped (X MB) — Y at risk" yellow box was removed
     (2026-06-05): it duplicated the Good/Maybe/No-chance pills (which already
     show the byte + time breakdown) and the bad-range bar (which shows where
     the damage is). The red banner above still surfaces a real last_error. */
  /* Adaptive batch recovery state \u2014 only during an active rip.
     current_batch < preferred_batch means the library shrunk the read size
     after a failure and is working through a marginal zone. Show a blue
     banner so the user can tell "recovering" from "stalled". */
  if(s.status==='ripping'&&s.current_batch>0&&s.preferred_batch>0&&s.current_batch<s.preferred_batch){
    const lbaStr=s.last_sector>0?' \u00b7 LBA '+s.last_sector.toLocaleString():'';
    errHtml+='<div style="background:var(--blue);color:#fff;padding:8px 12px;border-radius:6px;font-size:.8rem;margin-bottom:8px">\u21ba Recovering \u00b7 batch '+s.current_batch+' / '+s.preferred_batch+lbaStr+'</div>';
  }
  /* (Pass/phase info lives inside the Ripping step \u2014 no separate banner.) */
  upd('err',errHtml);

  /* Device log */
  loadDeviceLog(dev);
}

/* ---- Local time conversion for log lines ---- */
function utcToLocal(line){
  return line.replace(/^\[(\d{2}):(\d{2}):(\d{2})\]/,function(_,h,m,s){
    const now=new Date();
    const d=new Date(Date.UTC(now.getUTCFullYear(),now.getUTCMonth(),now.getUTCDate(),+h,+m,+s));
    return '['+String(d.getHours()).padStart(2,'0')+':'+String(d.getMinutes()).padStart(2,'0')+':'+String(d.getSeconds()).padStart(2,'0')+']';
  });
}

/* ---- Device log viewer ---- */
let _logTimer=null;
/* Render one autorip.jsonl line as a compact human line for the debug box:
   "HH:MM:SS LEVEL message  k=v k=v" (the patch walk + timings). Falls back
   to the raw line if it isn't JSON. */
function fmtDebugLine(line){
  try{
    const o=JSON.parse(line);
    const t=(o.timestamp||'').replace('T',' ').replace('Z','').replace(/\.[0-9]+/,'');
    const f=o.fields||{};
    const msg=f.message||'';
    const extra=Object.keys(f).filter(k=>k!=='message'&&k!=='build').map(k=>k+'='+f[k]).join(' ');
    return t+' '+(o.level||'')+' '+msg+(extra?'  '+extra:'');
  }catch(e){return line;}
}
function loadDeviceLog(dev){
  clearTimeout(_logTimer);
  fetch('/api/logs/'+encodeURIComponent(dev)).then(r=>r.text()).then(text=>{
    const e=document.getElementById('log');
    const reversed=text.split('\n').filter(l=>l).map(utcToLocal).reverse().join('\n');
    if(e&&e._last!==reversed){
      e.textContent=reversed;
      e._last=reversed;
    }
  }).catch(()=>{});
  /* Debug Log box: only shown + polled when debug logging is ON. Reuses the
     existing /api/debug jsonl tailer, filtered to this device, newest-first. */
  const db=document.getElementById('debugBox');
  if(window._debugOn){
    if(db)db.style.display='';
    fetch('/api/debug?device='+encodeURIComponent(dev)+'&n=500').then(r=>r.text()).then(text=>{
      const el=document.getElementById('debuglog');
      const lines=text.split('\n').filter(l=>l).map(fmtDebugLine).reverse().join('\n');
      if(el&&el._last!==lines){el.textContent=lines;el._last=lines;}
    }).catch(()=>{});
  }else if(db){db.style.display='none';}
  _logTimer=setTimeout(()=>loadDeviceLog(dev),3000);
}

/* ---- SSE connection ---- */
let _es=null;
function connectSSE(){
  if(_es){_es.close();_es=null}
  _es=new EventSource('/events');
  _es.onmessage=function(e){try{handleState(JSON.parse(e.data))}catch(x){}};
  _es.addEventListener('library',function(e){try{libEvent(JSON.parse(e.data))}catch(x){}});
  _es.onerror=function(){_es.close();_es=null;setTimeout(connectSSE,2000)};
}

/* Speed string from MB/s. Drops to KB/s, then B/s for sub-KB rates, so a slow
   patch reads e.g. "512 B/s" ("it's doing something") instead of "0 KB/s", and
   "0 B/s" when work is genuinely frozen (grinding one sector's ECC) — never a
   blank gap. */
function fmtSpeed(mbs){
  mbs=+mbs||0;
  if(mbs>=1) return mbs.toFixed(1)+' MB/s';
  if(mbs*1024>=1) return (mbs*1024).toFixed(0)+' KB/s';
  return Math.round(mbs*1048576)+' B/s';
}
/* Live rip-elapsed counter (seconds resolution). */
function fmtElapsedSecs(s){if(!s||s<0)return'';s=+s;const h=Math.floor(s/3600),m=Math.floor((s%3600)/60),sec=s%60;return h>0?h+'h '+String(m).padStart(2,'0')+'m '+String(sec).padStart(2,'0')+'s':m+'m '+String(sec).padStart(2,'0')+'s'}
/* v0.25.7: tick the rip-elapsed counter every 1s. Reads each
   rip-elapsed-* span's data-started attribute (set by renderCurrent
   from the latest state push) so the value stays accurate even
   after the state push briefly rewrites the DOM. */
setInterval(()=>{
  const now=Math.floor(Date.now()/1000);
  document.querySelectorAll('[id^="rip-elapsed-"]').forEach(el=>{
    const started=+el.dataset.started||0;
    if(started>0){el.textContent=fmtElapsedSecs(now-started)}
    else{el.textContent=''}
  });
},1000);

/* ---- Candidate caches (avoid inlining titles/dirs — apostrophes break attrs;
       we key off integer indices instead). ---- */
let _REV=[];        /* held-rip items, by index */
let _RC={};         /* review TMDB candidates, by item index */
let _TC={};         /* ripper-card TMDB candidates, by device */

/* ---- Needs review (System page): rips held for a confident title ---- */
function reviewResolve(idx,action,extra){
  const it=_REV[idx]; if(!it)return;
  const body=Object.assign({dir:it.dir,action:action},extra||{});
  fetch('/api/review/resolve',{method:'POST',headers:{'Content-Type':'application/json'},body:JSON.stringify(body)})
    .then(r=>r.json().then(j=>({ok:r.ok,j:j})))
    .then(({ok,j})=>{if(!ok||(j&&j.ok===false)){alert('Resolve failed: '+((j&&j.error)||'server error'));}loadReview();})
    .catch(()=>{alert('Resolve failed: could not reach the server');loadReview();});
}
function reviewSearch(idx){
  const q=(document.getElementById('rvq-'+idx)||{}).value; if(!q||!q.trim())return;
  const box=document.getElementById('rvc-'+idx); if(box)box.textContent='searching…';
  fetch('/api/tmdb/search?q='+encodeURIComponent(q.trim())).then(r=>r.json()).then(cs=>{
    if(!box)return; _RC[idx]=cs;
    if(!cs.length){box.textContent='no matches';return}
    box.innerHTML=cs.map((c,j)=>'<button class="btn" style="margin:2px" onclick="reviewPick('+idx+','+j+')">'+esc(c.title)+(c.year?' ('+c.year+')':'')+'</button>').join('');
  }).catch(()=>{if(box)box.textContent='search failed'});
}
function reviewPick(idx,j){const c=(_RC[idx]||[])[j]; if(c)reviewResolve(idx,'retitle',{title:c.title,year:c.year||0});}
/* Split typed manual-rename text into {title, year}: a trailing "(YYYY)" is
   pulled out as the year, else `fallbackYear`. Shared by reviewManual and
   titleManual so the two entry points can't drift. Returns null on empty. */
function parseManualName(raw,fallbackYear){
  raw=(raw||'').trim(); if(!raw)return null;
  const m=raw.match(/^(.*?)\s*\((\d{4})\)\s*$/);
  const title=m?m[1].trim():raw;
  if(!title)return null;
  return {title:title,year:m?parseInt(m[2],10):(fallbackYear||0)};
}
/* Manual entry: file under exactly the typed text (no TMDB pick needed). Lets
   the operator disambiguate a variant TMDB doesn't list as its own entry —
   e.g. "Redshift 2 (Extended Cut)" alongside an already-ripped "Redshift 2". */
function reviewManual(idx){
  const it=_REV[idx]; if(!it)return;
  const p=parseManualName((document.getElementById('rvq-'+idx)||{}).value,it.year);
  if(!p){alert('Type a name first');return}
  reviewResolve(idx,'retitle',{title:p.title,year:p.year});
}
function loadReview(){
  fetch('/api/review').then(r=>r.json()).then(items=>{
    const el=document.getElementById('review'); if(!el)return;
    _REV=items||[];
    if(!_REV.length){el.innerHTML='';return}
    let h='<div class="card" style="border-left:3px solid var(--accent);margin-bottom:16px">';
    h+='<div style="font-weight:600;margin-bottom:8px">⏸ Needs review — '+_REV.length+' rip(s) held for a confident title</div>';
    _REV.forEach((it,idx)=>{
      const t=esc(it.title||it.dir)+(it.year?' ('+it.year+')':'');
      h+='<div style="padding:8px 0;border-top:1px solid var(--border)">';
      h+='<div><strong>'+t+'</strong> <span style="color:var(--text3);font-size:.8rem">'+esc(it.reason||'')+'</span></div>';
      h+='<div style="color:var(--text3);font-size:.75rem">'+esc(it.file||'')+'</div>';
      h+='<div style="margin-top:6px;display:flex;gap:6px;flex-wrap:wrap;align-items:center">';
      h+='<input id="rvq-'+idx+'" placeholder="type an exact name, or a search term…" value="'+esc(it.title||'')+'" style="padding:4px 8px;border:1px solid var(--border);border-radius:6px;width:24rem;max-width:100%">';
      h+='<button class="btn" onclick="reviewManual('+idx+')">Manual Rename</button>';
      h+='<button class="btn" onclick="reviewSearch('+idx+')">Search TMDB</button>';
      h+='<button class="btn" onclick="reviewResolve('+idx+',\'proceed\')">Proceed as-is</button>';
      h+='<button class="btn" onclick="if(confirm(\'Discard this rip?\'))reviewResolve('+idx+',\'cancel\')">Cancel</button>';
      h+='</div><div id="rvc-'+idx+'" style="margin-top:6px"></div></div>';
    });
    h+='</div>';
    el.innerHTML=h;
  }).catch(()=>{});
}
loadReview();
setInterval(loadReview,5000);

/* ---- Ripper-card title editor: correct the match BEFORE ripping ---- */
function titleEdit(dev){
  const el=document.getElementById('tedit-'+dev); if(!el)return;
  if(el.style.display!=='none'){el.style.display='none';return}
  el.style.display='block';
  el.innerHTML='<div style="display:flex;gap:6px;flex-wrap:wrap;align-items:center"><input id="tq-'+dev+'" placeholder="type an exact name, or a search term…" style="padding:4px 8px;border:1px solid var(--border);border-radius:6px;width:24rem;max-width:100%"><button class="btn" onclick="titleSearch(\''+dev+'\')">Search TMDB</button><button class="btn" onclick="titleManual(\''+dev+'\')">Manual Rename</button></div><div id="tr-'+dev+'" style="margin-top:6px"></div>';
  const i=document.getElementById('tq-'+dev); if(i)i.focus();
}
function titleSearch(dev){
  const i=document.getElementById('tq-'+dev); const q=i?i.value.trim():''; if(!q)return;
  const box=document.getElementById('tr-'+dev); if(box)box.textContent='searching…';
  fetch('/api/tmdb/search?q='+encodeURIComponent(q)).then(r=>r.json()).then(cs=>{
    if(!box)return; _TC[dev]=cs;
    if(!cs.length){box.textContent='no matches';return}
    box.innerHTML=cs.map((c,j)=>'<button class="btn" style="margin:2px" onclick="titlePick(\''+dev+'\','+j+')">'+esc(c.title)+(c.year?' ('+c.year+')':'')+'</button>').join('');
  }).catch(()=>{if(box)box.textContent='search failed'});
}
function titlePick(dev,j){
  const c=(_TC[dev]||[])[j]; if(!c)return;
  fetch('/api/title/'+dev,{method:'POST',headers:{'Content-Type':'application/json'},body:JSON.stringify(c)})
    .then(r=>r.json()).then(()=>{const el=document.getElementById('tedit-'+dev);if(el)el.style.display='none';}).catch(()=>{});
}
/* Manual rename: file the active disc under exactly the typed text, no TMDB
   pick — for a variant TMDB doesn't list separately (e.g. "Redshift 2 (Extended
   Cut)"). A trailing "(YYYY)" becomes the year; tmdb_id 0 marks it a
   free-form override (handle_title_override already accepts this). */
function titleManual(dev){
  const i=document.getElementById('tq-'+dev);
  const p=parseManualName(i?i.value:'',0);
  if(!p){alert('Type a name first');return}
  fetch('/api/title/'+dev,{method:'POST',headers:{'Content-Type':'application/json'},
    body:JSON.stringify({title:p.title,year:p.year,tmdb_id:0})})
    .then(r=>r.json().then(j=>({ok:r.ok,j:j})))
    .then(({ok,j})=>{if(!ok||(j&&j.ok===false)){alert('Rename failed: '+((j&&j.error)||'server error'));return}
      const el=document.getElementById('tedit-'+dev);if(el)el.style.display='none';})
    .catch(()=>alert('Rename failed: could not reach the server'));
}

function updateKeydb(stId){
  /* stId lets the same handler back both the System-page Data Files
     button (default 'keydb-status') and the Settings-page Local button
     ('keydb-status-settings'). Tolerates a missing status element. */
  const st=document.getElementById(stId||'keydb-status');
  const set=(t,c)=>{if(st){st.textContent=t;st.style.color=c;}};
  set('Updating…','var(--text3)');
  fetch('/api/update-keydb',{method:'POST'}).then(r=>r.json()).then(data=>{
    if(data.ok){set('Updated: '+data.entries+' entries','var(--green)');loadSystem();}
    else{set(data.error||'Update failed','var(--red)');}
  }).catch(e=>{set('Network error','var(--red)');});
}
/* ---- Mux queue with live progress (mirrors renderMoves shape) ---- */
function renderMuxes(){
  const el=document.getElementById('muxes');
  if(!el)return;
  const data=window._stateData||{};
  /* _mux on the wire is a RipState (the worker uses the synthetic
     `_mux` device key in update_state), not the MuxState struct —
     so we read disc_name / progress_pct / speed_mbs / eta. The
     synthetic device's status is "ripping" while the mux is in
     flight; treat absent or non-active as "no active mux". */
  const mx=data._mux;
  const muxActive=mx&&mx.status==='ripping'&&mx.disc_name;
  let html='';
  let hasContent=false;
  if(muxActive){
    hasContent=true;
    const pct=mx.progress_pct||0;
    const spdStr=mx.speed_mbs>=1?mx.speed_mbs.toFixed(1)+' MB/s':mx.speed_mbs>0?(mx.speed_mbs*1024).toFixed(0)+' KB/s':'';
    const etaStr=mx.eta?mx.eta+' remaining':'';
    const label=[pct+'%',spdStr,etaStr].filter(x=>x).join(' · ');
    html+='<div style="padding:6px 0"><div style="display:flex;align-items:center;gap:8px;margin-bottom:4px"><span style="display:inline-block;width:8px;height:8px;border-radius:50%;background:var(--green);animation:p 1.5s infinite;flex-shrink:0"></span><span style="font-size:.85rem;font-weight:500">'+esc(mx.disc_name)+'</span></div>';
    html+='<div style="display:flex;align-items:center;gap:8px">';
    if(pct>0)html+='<div style="flex:1;background:var(--chip);border-radius:3px;height:3px;overflow:hidden"><div style="background:var(--green);height:100%;width:'+pct+'%;transition:width 1s"></div></div>';
    html+='<span style="font-size:.75rem;color:var(--text2)">'+label+'</span></div></div>';
  }
  /* Mux queue rides on the live state payload (_mux_queue), refreshed
     every SSE tick — so a job that moves on (mux finishes → Move queue)
     disappears here on the next tick instead of lingering until a hard
     refresh. `pending_queue` already excludes the dir currently muxing
     (it carries `.muxing`) and any dir that has entered the Move queue
     (`.done`/`.review`), so no frontend de-dup band-aid is needed: a job
     is in exactly one queue. Fall back to the older _muxQueue (from the
     /api/system fetch) only if the live field is absent. */
  const muxQ=(data._mux_queue!=null)?data._mux_queue:window._muxQueue;
  if(muxQ){
    muxQ.forEach(m=>{
      hasContent=true;
      html+='<div style="padding:4px 0;font-size:.8rem"><span style="display:inline-block;width:8px;height:8px;border-radius:50%;background:var(--yellow);margin-right:8px;vertical-align:middle"></span>'+esc(m)+'</div>';
    });
  }
  if(!hasContent)html='<div style="color:var(--text3);font-size:.8rem">No pending muxes</div>';
  if(window._muxErrors&&window._muxErrors.length){
    html+='<div style="margin-top:8px;padding-top:8px;border-top:1px solid var(--chip)">';
    /* Header: re-check (refresh) + clear-all, matching the Move queue. A
       cleared error the worker still considers blocked reappears on its next
       tick UNLESS dismissed (loss-aborts stay cleared). */
    html+='<div style="display:flex;align-items:center;gap:10px;margin-bottom:4px">'
      +'<span style="font-size:.75rem;color:var(--text3);text-transform:uppercase;letter-spacing:.4px">Needs action</span>'
      +'<span style="flex:1"></span>'
      +'<a href="#" onclick="event.preventDefault();loadSystem()" style="font-size:.75rem;color:var(--text2);text-decoration:none">↻ Refresh</a>'
      +'<a href="#" onclick="event.preventDefault();clearAllMuxErr()" style="font-size:.75rem;color:var(--text2);text-decoration:none">Clear all</a>'
      +'</div>';
    window._muxErrors.forEach(e=>{
      var p=e.path||'';
      html+='<div style="padding:6px 0;font-size:.8rem">'
        +'<div style="display:flex;align-items:center;gap:8px;margin-bottom:2px">'
        +'<span style="display:inline-block;width:8px;height:8px;border-radius:50%;background:var(--red);flex-shrink:0"></span>'
        +'<span style="font-weight:500;color:var(--red);flex:1;min-width:0;word-break:break-all">'+esc(p)+'</span>'
        +'<span onclick="clearMuxErr('+JSON.stringify(p)+')" title="Clear this error" '
          +'style="flex-shrink:0;cursor:pointer;color:var(--text3);font-size:1rem;line-height:1;padding:0 2px">&times;</span>'
        +'</div>'
        +'<div style="margin-left:16px;color:var(--text2)">'+esc(e.reason||'')+'</div>'
        +(e.hint?'<div style="margin-left:16px;color:var(--text3);font-size:.75rem;margin-top:2px">'+esc(e.hint)+'</div>':'')
        +'</div>';
    });
    html+='</div>';
  }
  upd('muxes',html);
}

/* Clear a single stuck mux error (the ✕), then re-pull. */
function clearMuxErr(path){
  fetch('/api/mux-errors/clear?path='+encodeURIComponent(path),{method:'POST'})
    .then(()=>loadSystem()).catch(()=>loadSystem());
}
function clearAllMuxErr(){
  fetch('/api/mux-errors/clear-all',{method:'POST'})
    .then(()=>loadSystem()).catch(()=>loadSystem());
}
/* ---- Move queue with live progress ---- */
function renderMoves(){
  const el=document.getElementById('moves');
  if(!el)return;
  const data=window._stateData||{};
  /* `_move` is an ARRAY of per-artifact bars (the movie file and, with
     keep_iso, its companion ISO \u2014 one bar each). Tolerate the legacy single
     object shape for safety across a rolling deploy. */
  const moves=Array.isArray(data._move)?data._move:(data._move&&data._move.name?[data._move]:[]);
  let html='';
  let hasContent=false;
  /* Active move: one progress bar per artifact, labelled "Title (iso)" /
     "Title (mkv)" so the two legs of a keep_iso move read distinctly. */
  moves.forEach(mv=>{
    if(!mv||!mv.name)return;
    hasContent=true;
    const pct=mv.progress_pct||0;
    const spdStr=mv.speed_mbs>=1?mv.speed_mbs.toFixed(1)+' MB/s':mv.speed_mbs>0?(mv.speed_mbs*1024).toFixed(0)+' KB/s':'';
    const etaStr=mv.eta?mv.eta+' remaining':'';
    const label=[pct+'%',spdStr,etaStr].filter(x=>x).join(' \u00b7 ');
    const title=esc(mv.name)+(mv.artifact?' ('+esc(mv.artifact)+')':'');
    html+='<div style="padding:6px 0"><div style="display:flex;align-items:center;gap:8px;margin-bottom:4px"><span style="display:inline-block;width:8px;height:8px;border-radius:50%;background:var(--green);animation:p 1.5s infinite;flex-shrink:0"></span><span style="font-size:.85rem;font-weight:500">'+title+'</span></div>';
    html+='<div style="display:flex;align-items:center;gap:8px">';
    if(pct>0)html+='<div style="flex:1;background:var(--chip);border-radius:3px;height:3px;overflow:hidden"><div style="background:var(--green);height:100%;width:'+pct+'%;transition:width 1s"></div></div>';
    html+='<span style="font-size:.75rem;color:var(--text2)">'+label+'</span></div></div>';
  });
  /* Pending queue items — from the live state payload (_move_queue),
     refreshed every SSE tick (falls back to the /api/system _moveQueue only
     if absent). The server already excludes the actively-moving dir from this
     list (it's shown as the bars above), so no client-side de-dup is needed. */
  const moveQ=(data._move_queue!=null)?data._move_queue:window._moveQueue;
  if(moveQ){
    moveQ.forEach(m=>{
      hasContent=true;
      html+='<div style="padding:4px 0;font-size:.8rem"><span style="display:inline-block;width:8px;height:8px;border-radius:50%;background:var(--yellow);margin-right:8px;vertical-align:middle"></span>'+esc(m)+'</div>';
    });
  }
  if(!hasContent)html='<div style="color:var(--text3);font-size:.8rem">No pending moves</div>';
  /* Stuck-move errors that need user action (orphaned staging dirs etc.) */
  if(window._moveErrors&&window._moveErrors.length){
    html+='<div style="margin-top:8px;padding-top:8px;border-top:1px solid var(--chip)">';
    /* Header: re-check (refresh) + clear-all. A cleared error the mover still
       considers blocked reappears on its next tick, so refresh confirms which
       are actually solved. */
    html+='<div style="display:flex;align-items:center;gap:10px;margin-bottom:4px">'
      +'<span style="font-size:.75rem;color:var(--text3);text-transform:uppercase;letter-spacing:.4px">Needs action</span>'
      +'<span style="flex:1"></span>'
      +'<a href="#" onclick="event.preventDefault();loadSystem()" style="font-size:.75rem;color:var(--text2);text-decoration:none">↻ Refresh</a>'
      +'<a href="#" onclick="event.preventDefault();clearAllMoveErr()" style="font-size:.75rem;color:var(--text2);text-decoration:none">Clear all</a>'
      +'</div>';
    window._moveErrors.forEach(e=>{
      var p=e.path||'';
      html+='<div style="padding:6px 0;font-size:.8rem">'
        +'<div style="display:flex;align-items:center;gap:8px;margin-bottom:2px">'
        +'<span style="display:inline-block;width:8px;height:8px;border-radius:50%;background:var(--red);flex-shrink:0"></span>'
        +'<span style="font-weight:500;color:var(--red);flex:1;min-width:0;word-break:break-all">'+esc(p)+'</span>'
        +'<span onclick="clearMoveErr('+JSON.stringify(p)+')" title="Clear this error" '
          +'style="flex-shrink:0;cursor:pointer;color:var(--text3);font-size:1rem;line-height:1;padding:0 2px">&times;</span>'
        +'</div>'
        +'<div style="margin-left:16px;color:var(--text2)">'+esc(e.reason||'')+'</div>'
        +(e.hint?'<div style="margin-left:16px;color:var(--text3);font-size:.75rem;margin-top:2px">'+esc(e.hint)+'</div>':'')
        +'</div>';
    });
    html+='</div>';
  }
  upd('moves',html);
}

/* Clear a single stuck move error (the ✕), then re-pull so a still-blocked
   one reappears and a solved one stays gone. */
function clearMoveErr(path){
  fetch('/api/move-errors/clear?path='+encodeURIComponent(path),{method:'POST'})
    .then(()=>loadSystem()).catch(()=>loadSystem());
}
function clearAllMoveErr(){
  fetch('/api/move-errors/clear-all',{method:'POST'})
    .then(()=>loadSystem()).catch(()=>loadSystem());
}

/* ---- System page ---- */
function loadSystem(){
  fetch('/api/system').then(r=>r.json()).then(data=>{
    /* Move queue - store for renderMoves, then render */
    window._moveQueue=data.move_queue||[];
    window._moveErrors=data.move_errors||[];
    /* Mux queue (v0.25.3) — same shape, separate panel above */
    window._muxQueue=data.mux_queue||[];
    window._muxErrors=data.mux_errors||[];
    renderMuxes();
    renderMoves();
    /* Debug-logging toggle reflects current runtime state */
    const dbg=document.getElementById('debugToggle');
    if(dbg)dbg.checked=!!data.debug_enabled;
    /* Global flag so the device-page Debug Log box shows/polls only when on */
    window._debugOn=!!data.debug_enabled;
    /* System log */
    const logEl=document.getElementById('syslog');
    if(data.syslog){
      logEl.textContent=data.syslog.split('\n').map(utcToLocal).join('\n');
      logEl.scrollTop=0;
    }else{
      logEl.textContent='No system log available';
    }
  }).catch(()=>{});
}

/* Flip runtime debug logging via POST /api/debug; sync the checkbox to the
   authoritative state the server returns. */
function toggleDebug(on){
  fetch('/api/debug',{method:'POST',headers:{'Content-Type':'application/json'},body:JSON.stringify({enabled:on})})
    .then(r=>r.json()).then(d=>{const t=document.getElementById('debugToggle');if(t)t.checked=!!d.enabled;})
    .catch(()=>{});
}

/* ---- Settings page ---- */
function loadSettings(){
  fetch('/api/settings').then(r=>r.json()).then(renderSettings).catch(()=>{});
}

function renderSettings(s){
  /* v0.13.19: derive a virtual `rip_mode` from `max_retries` so the radio
     selector renders with the right value on load. The backend stays on
     `max_retries` (and `keep_iso`) — `saveSettings` translates rip_mode back
     before POST. */
  if(typeof s.rip_mode!=='string'){
    s.rip_mode=(s.max_retries>0)?'multi':'single';
  }
  const groups=[
    {title:'Disc Lifecycle',fields:[
      {key:'on_insert',label:'On Disc Insert',type:'radio',options:[{value:'nothing',label:'Do Nothing'},{value:'scan',label:'Scan'},{value:'rip',label:'Rip'},{value:'resume',label:'Resume'}],hint:'Rip starts fresh each time. Resume continues a resumable rip, or starts fresh if none is available. Both leave finished, review-held and muxing discs alone; Rip also leaves loss-aborted discs for Accept/Resume.'},
      {key:'auto_eject',label:'Auto Eject',type:'bool',hint:'Eject disc after rip completes'},
    ]},
    {title:'Ripping',fields:[
      // Title filters only apply when the rip ends in a mux step (MKV/M2TS/
      // Network); ISO is a whole-disc image so they hide for that format.
      {key:'output_format',label:'Output Format',type:'radio',options:[{value:'mkv',label:'MKV'},{value:'m2ts',label:'M2TS'},{value:'iso',label:'ISO (disc image)'},{value:'network',label:'Network'}],hint:'Format for ripped files. ISO copies the whole disc; the other formats mux selected titles.'},
      {key:'network_target',label:'Network Target',type:'text',hint:'host:port for network output (e.g. nas.example.com:9000)',indent:true,placeholder:'nas.example.com:9000',showIf:{key:'output_format',value:'network'}},
      {key:'main_feature',label:'Main Feature Only',type:'bool',hint:'',indent:true,hideIf:{key:'output_format',value:'iso'}},
      {key:'min_length_secs',label:'Minimum Title Length (seconds)',type:'number',hint:'Shorter titles are skipped (600 = 10 min)',indent:true,hideIf:{key:'output_format',value:'iso'}},
      {key:'abort_on_lost_secs',label:'Max Acceptable Main Movie Loss',type:'number',hint:'Governs the RIP phase only: seconds of UNREADABLE main-movie data tolerated after all retry passes. If unreadable-sector loss in the MAIN FEATURE still exceeds this once retries are exhausted, the rip aborts (leaving a resumable staging dir). 0 = require a perfect rip — abort on ANY unreadable byte, not just ≥1s. Scoped to the main movie only (a scratched menu/extra outside the title never aborts). Applies to title-selected output (MKV / M2TS / Network stream) — IGNORED for an ISO rip, which is kept whole as-is (for a byte-perfect full-disc image use the freemkv CLI). The mux itself never aborts: any undecryptable/demux-time loss is concealed and tallied, never quarantined. Multi-pass only.',indent:true,showIf:{key:'rip_mode',value:'multi'},hideIf:{key:'output_format',value:'iso'}},
    ]},
    {title:'Recovery',fields:[
      {key:'rip_mode',label:'Rip Mode',type:'radio',options:[{value:'single',label:'Single Pass'},{value:'multi',label:'Multi Pass'}],hint:'Single Pass: stream disc → MKV directly. Fastest, best for healthy discs. Multi Pass: rip an ISO, retry bad sectors with progressively smaller blocks, then mux to MKV. Use for discs with read errors.'},
      /* Single-pass error policy: only meaningful when there's no retry safety net. */
      {key:'on_read_error',label:'On Read Error',type:'radio',options:[{value:'stop',label:'Stop'},{value:'skip',label:'Skip (zero-fill)'}],hint:'Drive read error policy for single-pass rips. Stop aborts on the first bad sector. Skip zero-fills it and keeps streaming — useful when the disc is mostly fine and you accept minor loss for speed.',indent:true,showIf:{key:'rip_mode',value:'single'}},
      /* Multi-pass knobs: retries + accept-loss threshold. on_read_error doesn't apply
         in multi-pass — sweep always skips by design, retries always retry, and the
         post-retry abort decision is governed by abort_on_lost_secs (time-based). */
      {key:'max_retries',label:'Retry Passes',type:'number',hint:'How many retry passes to run on bad sectors. Each pass uses smaller blocks (60→30→15→7→1 sectors) and alternates direction. Default 5 covers most recoverable damage.',indent:true,showIf:{key:'rip_mode',value:'multi'}},
      {key:'keep_iso',label:'Keep Intermediate ISO',type:'bool',hint:'Keep the intermediate disc ISO after muxing. Off by default to reclaim disk. Filed beside the muxed title unless you set an ISO Folder (under Output).',indent:true,showIf:{key:'rip_mode',value:'multi'}},
    ]},
    {title:'Output',fields:[
      {key:'staging_dir',label:'Staging Directory',type:'text',hint:'Where rips are written before being moved to the final destination. Use a fast local disk for performance; the finished MKV is moved to the output directory on completion.'},
      {key:'output_dir',label:'Output Directory',type:'text',hint:'Where all ripped files go by default'},
      {key:'movie_dir',label:'Movies',type:'text',hint:'',indent:true,placeholder:'Same as output directory'},
      {key:'tv_dir',label:'TV Series',type:'text',hint:'',indent:true,placeholder:'Same as output directory'},
      {key:'iso_dir',label:'ISO Folder',type:'text',hint:'Where kept ISOs are stored (applies when Keep Intermediate ISO is on, or Output Format is ISO). Relative (e.g. isos) sits under the Output Directory; an absolute path (e.g. /mnt/archive/isos) targets another disk. Blank = beside the muxed title.',indent:true,placeholder:'Beside the muxed title'},
    ]},
    {title:'Library',fields:[
      {key:'library_dir',label:'Library Folder',type:'text',hint:'Where the Title/Title.mkv files are. Blank = the Movies folder above.',placeholder:'Movies folder'},
      {key:'library_iso_dir',label:'Source ISO Folder',type:'text',hint:'Where the source ISOs are, matched to MKVs by title. Blank = the ISO Folder above.',placeholder:'ISO Folder'},
      {key:'library_iso_subfolders',label:'Include ISO Subfolders',type:'bool',hint:'Also list ISOs one folder down (dvd/, hddvd/, bd/). Off = top-level ISOs only.'},
    ]},
    {title:'API Keys',fields:[
      {key:'tmdb_api_key',label:'TMDB API Key',type:'text',hint:'v3 API key from themoviedb.org'},
    ]},
    {title:'Key Source',fields:[
      {key:'key_source',label:'AACS Key Source',type:'radio',options:[{value:'local',label:'Local KEYDB'},{value:'online',label:'Online Keyserver'}],hint:'Where per-disc AACS keys come from. Local uses a KEYDB.cfg on disk; Online queries a keyserver.'},
      {key:'keydb_path',label:'KEYDB.cfg Location',type:'text',hint:'Path to KEYDB.cfg on disk. Blank = default: <AUTORIP_DIR>/keydb.cfg (normally /config/keydb.cfg in Docker).',placeholder:'/config/keydb.cfg',indent:true,showIf:{key:'key_source',value:'local'}},
      {key:'keydb_resolved',label:'Resolved KEYDB path',type:'info',hint:'The exact file autorip reads keys from right now (after the box above + defaults). If it says NOT FOUND, place keydb.cfg there or set the location above.',indent:true,showIf:{key:'key_source',value:'local'}},
      {key:'keydb_url',label:'KEYDB Update URL',type:'text',hint:'HTTP URL to download KEYDB.cfg (zip, gz, or plain text).',indent:true,showIf:{key:'key_source',value:'local'}},
      {type:'action',action:"updateKeydb('keydb-status-settings')",button:'Update KEYDB',status:'keydb-status-settings',hint:'Download the KEYDB.cfg from the URL above into the configured location.',indent:true,showIf:{key:'key_source',value:'local'}},
      {key:'keyserver_url',label:'Keyserver URL',type:'text',hint:'Full keyserver endpoint URL — the decode request is POSTed here verbatim, so include the path (e.g. https://host/decode).',indent:true,showIf:{key:'key_source',value:'online'}},
      {key:'keyserver_secret',label:'Keyserver API Secret',type:'text',hint:'Bearer token for the keyserver, if it requires one.',indent:true,showIf:{key:'key_source',value:'online'}},
      {key:'capture_without_keys',label:'Capture Discs Without Keys',type:'bool',hint:'No usable keys → capture the disc to an ISO and mux later when keys become available. Off = skip the disc.'},
    ]},
    {title:'Performance',fields:[
      {key:'decrypt_threads',label:'Decrypt Threads',type:'number',hint:'How many threads AACS decryption uses. 0 = auto (all available cores, capped at 64). Drop to 4-8 if autorip is sharing the host with other heavy workloads.'},
      {key:'log_retention_days',label:'Log Retention (days)',type:'number',hint:'Per-device .log files older than this are pruned by the in-process daily cleanup. Default 30.'},
    ]},
  ];
  let html='';
  groups.forEach(g=>{
    html+='<div class="card"><h2>'+g.title+'</h2>';
    g.fields.forEach(f=>{
      const v=s[f.key]!=null?s[f.key]:'';
      const indent=f.indent?'margin-left:20px;border-left:2px solid var(--border);padding-left:12px':'';
      const ph=f.placeholder?' placeholder="'+f.placeholder+'"':'';
      const hideShow=f.showIf&&s[f.showIf.key]!==f.showIf.value;
      const hideHide=f.hideIf&&s[f.hideIf.key]===f.hideIf.value;
      const hide=(hideShow||hideHide)?'display:none;':'';
      const showAttr=(f.showIf?' data-show-key="'+f.showIf.key+'" data-show-value="'+f.showIf.value+'"':'')+(f.hideIf?' data-hide-key="'+f.hideIf.key+'" data-hide-value="'+f.hideIf.value+'"':'');
      if(f.type==='action'){
        html+='<div class="setting" style="'+indent+hide+'"'+showAttr+'><div style="display:flex;align-items:center;gap:10px"><button type="button" class="btn" onclick="'+f.action+'">'+f.button+'</button><span id="'+f.status+'" style="font-size:.8rem"></span></div>'+(f.hint?'<div class="hint">'+f.hint+'</div>':'')+'</div>';
      }else if(f.type==='radio'){
        const opts=f.options.map(o=>'<label style="font-size:13px;cursor:pointer;display:inline-flex;align-items:center;gap:6px;margin-right:16px"><input type="radio" name="'+f.key+'" data-key="'+f.key+'" value="'+o.value+'" style="width:14px;height:14px;margin:0;accent-color:var(--accent)" onchange="toggleConditional()" '+(v===o.value?'checked':'')+'>'+o.label+'</label>').join('');
        html+='<div class="setting" style="'+indent+hide+'"'+showAttr+'><label>'+f.label+'</label><div style="margin-top:4px">'+opts+'</div>'+(f.hint?'<div class="hint">'+f.hint+'</div>':'')+'</div>';
      }else if(f.type==='bool'){
        html+='<div class="setting" style="'+indent+hide+'"'+showAttr+'><label class="toggle"><input type="checkbox" data-key="'+f.key+'" '+(v?'checked':'')+'> '+f.label+'</label>'+(f.hint?'<div class="hint">'+f.hint+'</div>':'')+'</div>';
      }else if(f.type==='info'){
        html+='<div class="setting" style="'+indent+hide+'"'+showAttr+'><label>'+f.label+'</label><div style="margin-top:4px;font-family:monospace;font-size:.8rem;color:var(--text2);word-break:break-all">'+esc(String(v))+'</div>'+(f.hint?'<div class="hint">'+f.hint+'</div>':'')+'</div>';
      }else{
        html+='<div class="setting" style="'+indent+hide+'"'+showAttr+'><label>'+f.label+'</label><input type="'+f.type+'" data-key="'+f.key+'" value="'+esc(String(v))+'"'+ph+'>'+(f.hint?'<div class="hint">'+f.hint+'</div>':'')+'</div>';
      }
    });
    html+='</div>';
    /* Insert webhooks card after Output */
    if(g.title==='Output'){
      /* Each webhook is now {url, post_rip, post_mux, post_move}; tolerate a
         legacy bare string (older payload) by coercing it to an object that
         fires on every stage. A pre-1.6.8 object with no post_mux defaults it
         to true (post_mux!==false below), matching the config loader. */
      const hooks=(s.webhook_urls||[])
        .map(h=>typeof h==='string'?{url:h,post_rip:true,post_mux:true,post_move:true}:h)
        .filter(h=>h&&h.url);
      html+='<div class="card"><h2>Webhooks</h2>';
      html+='<div id="webhook-list">';
      hooks.forEach((h,i)=>{ html+=webhookRow(i,h.url,h.post_rip!==false,h.post_mux!==false,h.post_move!==false); });
      html+='</div>';
      html+='<button class="btn" onclick="addWebhook()" style="font-size:.75rem;margin-top:4px">+ Add Webhook</button>';
      html+='<div style="font-size:12px;color:var(--text3);margin-top:8px;line-height:1.4">POST JSON to each endpoint. Choose per hook which stage fires it — Rip (disc read done, drive free), Mux (.mkv produced), Move (in library) — in any combination. Works with Discord, Jellyfin, n8n, or any HTTP endpoint.</div>';
      html+='</div>';
    }
  });
  document.getElementById('settings-form').innerHTML=html;
  toggleConditional();
}
function toggleConditional(){
  // A field may carry BOTH a showIf and a hideIf; it's hidden if showIf
  // isn't met OR hideIf is met (e.g. abort_on_lost_secs: multi-pass only
  // AND not ISO output).
  document.querySelectorAll('[data-show-key],[data-hide-key]').forEach(el=>{
    let visible=true;
    if(el.dataset.showKey){
      const r=document.querySelector('input[data-key="'+el.dataset.showKey+'"]:checked');
      if(!(r&&r.value===el.dataset.showValue)) visible=false;
    }
    if(el.dataset.hideKey){
      const r=document.querySelector('input[data-key="'+el.dataset.hideKey+'"]:checked');
      if(r&&r.value===el.dataset.hideValue) visible=false;
    }
    el.style.display=visible?'':'none';
  });
}

/* One webhook row: URL input + a "Rip", "Mux" and "Move" checkbox (which
   pipeline stage fires this hook) + a remove button. Rip = disc read done /
   drive free; Mux = .mkv produced; Move = landed in the library.
   `postRip`/`postMux`/`postMove` seed the checkboxes; new hooks default all
   three to true. The checkbox data-attributes are read back per-row in
   saveSettings(). */
function webhookRow(i,url,postRip,postMux,postMove){
  const cb=(attr,on,label)=>'<label style="display:inline-flex;align-items:center;gap:4px;font-size:12px;color:var(--text2);cursor:pointer;white-space:nowrap"><input type="checkbox" '+attr+' '+(on?'checked':'')+' style="width:14px;height:14px;margin:0;accent-color:var(--accent)">'+label+'</label>';
  return '<div class="webhook-row" style="display:flex;gap:8px;margin-bottom:6px;align-items:center;flex-wrap:wrap">'
    +'<input type="text" data-webhook="'+i+'" value="'+esc(url||'')+'" placeholder="https://discord.com/api/webhooks/..." style="flex:1;min-width:180px;padding:8px 10px;border:1px solid var(--border);border-radius:6px;background:var(--log-bg);color:var(--text);font-size:13px;font-family:inherit">'
    +cb('data-webhook-rip','undefined'==typeof postRip?true:postRip,'Rip')
    +cb('data-webhook-mux','undefined'==typeof postMux?true:postMux,'Mux')
    +cb('data-webhook-move','undefined'==typeof postMove?true:postMove,'Move')
    +'<button class="btn" onclick="this.parentElement.remove()" style="padding:5px 8px;font-size:.75rem">X</button>'
    +'</div>';
}

function addWebhook(){
  const list=document.getElementById('webhook-list');
  const i=list.children.length;
  const tmp=document.createElement('div');
  tmp.innerHTML=webhookRow(i,'',true,true,true);
  const div=tmp.firstChild;
  list.appendChild(div);
  div.querySelector('input[type="text"]').focus();
}

function saveSettings(){
  const inputs=document.querySelectorAll('#settings-form [data-key]');
  const s={};
  inputs.forEach(el=>{
    if(el.type==='radio'){if(el.checked)s[el.dataset.key]=el.value}
    else if(el.type==='checkbox')s[el.dataset.key]=el.checked;
    else if(el.type==='number')s[el.dataset.key]=parseInt(el.value)||0;
    else s[el.dataset.key]=el.value;
  });
  /* Collect webhooks as {url, post_rip, post_mux, post_move}. Read each flag
     from the row's own checkboxes so a URL only fires on the stages the
     operator chose. */
  const hooks=[];
  document.querySelectorAll('#webhook-list .webhook-row').forEach(row=>{
    const urlEl=row.querySelector('input[data-webhook]');
    const v=(urlEl&&urlEl.value||'').trim();
    if(!v)return;
    const rip=row.querySelector('input[data-webhook-rip]');
    const mux=row.querySelector('input[data-webhook-mux]');
    const mov=row.querySelector('input[data-webhook-move]');
    hooks.push({url:v,post_rip:rip?rip.checked:true,post_mux:mux?mux.checked:true,post_move:mov?mov.checked:true});
  });
  s.webhook_urls=hooks;
 /* v0.13.19: translate the virtual `rip_mode` selector back to the backend
      fields. Single → max_retries=0. Keep keep_iso unchanged so the stored
      preference survives a mode switch (the server no longer clobbers it
      from rip_mode either). Multi → keep whatever max_retries the user set;
      default to 5 if they flipped to multi without ever touching the count.
      The `rip_mode` key itself is never persisted — the backend already
      infers it from max_retries on the next render. */
   if(s.rip_mode==='single'){s.max_retries=0}
   else if(s.rip_mode==='multi'&&(!s.max_retries||s.max_retries<1)){s.max_retries=5}
   delete s.rip_mode;
  /* Loud, hard-to-miss feedback on save. The previous version flashed
     "Saved" in a small green span next to the button for 2 s and did
     nothing at all on error — easy to miss and silent on failure.
     Now: the button itself transitions through Saving… → ✓ Saved (green
     fill) → original label, and the adjacent status span carries any
     error message in red. */
  const btn=document.getElementById('savebtn');
  const status=document.getElementById('save-status');
  const origLabel=btn.textContent;
  btn.disabled=true;
  btn.textContent='Saving…';
  status.textContent='';
  status.style.color='var(--green)';
  fetch('/api/settings',{method:'POST',headers:{'Content-Type':'application/json'},body:JSON.stringify(s)})
    .then(async r=>{
      if(!r.ok){
        let msg='HTTP '+r.status;
        try{const j=await r.json();if(j&&j.error)msg=j.error;}catch(_){}
        throw new Error(msg);
      }
      btn.textContent='✓ Saved';
      btn.style.background='var(--green)';
      btn.style.color='#fff';
      btn.style.borderColor='var(--green)';
      status.textContent='Saved';
      setTimeout(()=>{
        btn.disabled=false;
        btn.textContent=origLabel;
        btn.style.background='';
        btn.style.color='';
        btn.style.borderColor='';
        status.textContent='';
      },2000);
    })
    .catch(e=>{
      btn.disabled=false;
      btn.textContent=origLabel;
      status.style.color='var(--red)';
      status.textContent='Save failed: '+e.message;
    });
}

/* ---- Init ---- */
fetch('/api/state').then(r=>r.json()).then(data=>{handleState(data);connectSSE()}).catch(()=>setTimeout(connectSSE,1000));
</script>
</body>
</html>"##;

pub fn run(cfg: &Arc<RwLock<Config>>) {
    let port = cfg.read().unwrap_or_else(|e| e.into_inner()).port;
    let addr = format!("0.0.0.0:{}", port);
    let server = match Server::http(&addr) {
        Ok(s) => Arc::new(s),
        Err(e) => {
            // Bind failure is unrecoverable — without a UI we have a dead
            // daemon. Signal SHUTDOWN so main exits non-zero and the
            // container restart policy recovers us.
            crate::server::log::syslog(&format!(
                "FATAL: web server bind failed on {}: {} — signalling shutdown",
                addr, e
            ));
            tracing::error!(
                address = %addr,
                error = %e,
                "web bind failed; signalling shutdown so the container restart policy recovers us"
            );
            crate::server::SHUTDOWN.store(true, std::sync::atomic::Ordering::SeqCst);
            return;
        }
    };
    crate::server::log::syslog(&format!("Web server listening on {}", addr));
    tracing::info!(address = %addr, "web server listening");

    for request in server.incoming_requests() {
        if crate::server::SHUTDOWN.load(std::sync::atomic::Ordering::Relaxed) {
            break;
        }
        // Bound concurrent handlers so a flood can't fork the container.
        // Body-carrying requests get a lower cap: tiny_http 0.12 has no
        // read timeout, so a stalled sender could starve the healthcheck.
        let cap = if carries_body(&request) {
            MAX_INFLIGHT_BODY_HANDLERS
        } else {
            MAX_INFLIGHT_HANDLERS
        };
        let guard = match ConnGuard::try_acquire(&INFLIGHT_HANDLERS, cap) {
            Some(g) => g,
            None => {
                tracing::warn!(max = cap, "request rejected: in-flight handler cap reached");
                json_response(request, 503, r#"{"ok":false,"error":"server busy"}"#);
                continue;
            }
        };
        let cfg = Arc::clone(cfg);
        if let Err(e) = std::thread::Builder::new()
            .name("autorip-http".into())
            .spawn(move || {
                // Hold the admission token for the handler's lifetime;
                // dropped here on return/unwind, freeing the slot.
                let _guard = guard;
                handle_request(request, &cfg);
            })
        {
            tracing::error!(error = %e, "failed to spawn request handler thread");
            // guard drops here, freeing the reserved slot.
        }
    }
    tracing::info!("web server stopping");
}

/// Extract a header value by case-insensitive field name.
fn header_value<'a>(request: &'a tiny_http::Request, name: &str) -> Option<&'a str> {
    // `HeaderField::equiv` requires a `&'static str`; compare the field
    // name ourselves so we can take a borrowed `name`. HTTP header field
    // names are case-insensitive.
    request
        .headers()
        .iter()
        .find(|h| h.field.as_str().as_str().eq_ignore_ascii_case(name))
        .map(|h| h.value.as_str())
}

/// Pull the host\[:port\] authority out of a URL or a bare Host header value.
fn authority_of(s: &str) -> Option<String> {
    // Strip scheme (origin headers look like `http://host:port`); Host
    // headers are already bare. Then strip any path/query tail.
    let after_scheme = s.split("://").last().unwrap_or(s);
    let host = after_scheme
        .split(['/', '?', '#'])
        .next()
        .unwrap_or(after_scheme)
        .trim();
    if host.is_empty() {
        None
    } else {
        Some(host.to_ascii_lowercase())
    }
}

/// Default TCP port implied by a URL scheme (used to normalize an authority
/// that omits its port). Only the two web schemes matter here.
fn default_port_for_scheme(s: &str) -> u16 {
    if s.starts_with("https://") {
        443
    } else {
        // http:// and bare Host values (no scheme) both default to 80,
        // which is the right comparison baseline for a same-origin POST.
        80
    }
}

// Normalize an authority (`host` or `host:port`) to canonical `host:port`,
// filling in `default_port` when omitted, so `http://host` (Origin) compares
// equal to `host:80` (Host header) instead of falsely reading cross-origin.
fn normalize_authority(authority: &str, default_port: u16) -> Option<String> {
    let a = authority_of(authority)?;
    // Bracketed IPv6 literal: [::1] or [::1]:8080.
    if let Some(rest) = a.strip_prefix('[') {
        let (host, after) = rest.split_once(']')?;
        let port = match after.strip_prefix(':') {
            Some(p) => p.parse::<u16>().ok()?,
            None if after.is_empty() => default_port,
            None => return None,
        };
        return Some(format!("[{host}]:{port}"));
    }
    match a.rsplit_once(':') {
        // Trailing ':NNN' is a port only if numeric; otherwise treat the
        // whole thing as a host (defensive — keeps a stray colon from
        // silently dropping the port).
        Some((host, p)) => match p.parse::<u16>() {
            Ok(port) => Some(format!("{host}:{port}")),
            Err(_) => Some(format!("{a}:{default_port}")),
        },
        None => Some(format!("{a}:{default_port}")),
    }
}

// Lightweight CSRF defense for state-changing POSTs: reject only when an Origin/Referer header
// is present and disagrees with Host (403); an absent header is allowed so curl/monitoring keep
// working.
fn is_cross_origin_post(request: &tiny_http::Request) -> bool {
    let origin = header_value(request, "Origin").or_else(|| header_value(request, "Referer"));
    let host = header_value(request, "Host");
    is_cross_origin(origin, host)
}

// Pure cross-origin decision over raw Origin/Referer + Host header values; `true` means reject.
// Absent/unparseable input can't prove cross-origin, so it's allowed.
fn is_cross_origin(origin: Option<&str>, host: Option<&str>) -> bool {
    let origin = match origin {
        None => return false,
        Some(o) if o.trim().is_empty() => return false,
        Some(o) => o,
    };
    // Origin carries the scheme, which fixes the default port so the
    // schemeless Host header normalizes to match it — otherwise
    // `http://host` wouldn't match `host:80`, falsely 403'ing same-origin.
    let default_port = default_port_for_scheme(origin.trim());
    let origin_norm = match normalize_authority(origin, default_port) {
        Some(h) => h,
        None => return false,
    };
    let host_norm = match host.and_then(|h| normalize_authority(h, default_port)) {
        Some(h) => h,
        None => return false,
    };
    origin_norm != host_norm
}

fn handle_request(request: tiny_http::Request, cfg: &Arc<RwLock<Config>>) {
    let url = request.url().to_string();
    let is_get = *request.method() == Method::Get;
    let is_post = *request.method() == Method::Post;

    // Defense-in-depth CSRF check: reject a state-changing POST whose
    // Origin/Referer host disagrees with our Host header. Absent header is
    // allowed so curl/monitoring scripts keep working (see helper doc).
    if is_post && is_cross_origin_post(&request) {
        return json_response(
            request,
            403,
            r#"{"ok":false,"error":"cross-origin request rejected"}"#,
        );
    }

    if is_get && (url == "/" || url == "/index.html") {
        serve_html(request);
    } else if is_get && url == "/favicon.svg" {
        serve_favicon(request);
    } else if is_get && url == "/api/state" {
        let staging_dir = cfg
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .staging_dir
            .clone();
        json_response(request, 200, &get_state_json(&staging_dir));
    } else if is_get && url == "/api/version" {
        json_response(
            request,
            200,
            &format!("{{\"version\":\"{}\"}}", crate::server::VERSION_LABEL),
        );
    } else if is_get && url == "/api/settings" {
        // Snapshot and drop the guard: rendering stats the keydb path, and
        // filesystem I/O (NFS) must not stall config writers.
        let c = match cfg.read() {
            Ok(c) => c.clone(),
            Err(_) => {
                return json_response(
                    request,
                    500,
                    r#"{"ok":false,"error":"config lock poisoned"}"#,
                );
            }
        };
        let json = settings_json_redacted(&c);
        json_response(request, 200, &json);
    } else if is_post && url == "/api/settings" {
        handle_settings_post(request, cfg);
    } else if is_get && url == "/api/system" {
        handle_system_info(request, cfg);
    } else if is_post && url == "/api/move-errors/clear-all" {
        crate::server::mover::clear_all_move_errors();
        json_response(request, 200, r#"{"ok":true}"#);
    } else if is_post && url.starts_with("/api/move-errors/clear?") {
        // Clear ONE move error by path. The path carries slashes/spaces, so it
        // arrives percent-encoded in the `path=` query param.
        let query = url.split_once('?').map(|x| x.1).unwrap_or("");
        let target = query
            .split('&')
            .find_map(|kv| kv.strip_prefix("path="))
            .map(percent_decode)
            .unwrap_or_default();
        if target.is_empty() {
            return json_response(request, 400, r#"{"ok":false,"error":"missing path"}"#);
        }
        crate::server::mover::clear_move_error(&target);
        json_response(request, 200, r#"{"ok":true}"#);
    } else if is_post && url == "/api/mux-errors/clear-all" {
        crate::server::muxer::clear_all_mux_errors();
        json_response(request, 200, r#"{"ok":true}"#);
    } else if is_post && url.starts_with("/api/mux-errors/clear?") {
        // Clear ONE mux error by path (percent-encoded `path=` query param).
        let query = url.split_once('?').map(|x| x.1).unwrap_or("");
        let target = query
            .split('&')
            .find_map(|kv| kv.strip_prefix("path="))
            .map(percent_decode)
            .unwrap_or_default();
        if target.is_empty() {
            return json_response(request, 400, r#"{"ok":false,"error":"missing path"}"#);
        }
        crate::server::muxer::clear_mux_error(&target);
        json_response(request, 200, r#"{"ok":true}"#);
    } else if is_get && url.starts_with("/api/logs/") {
        let device = url.trim_start_matches("/api/logs/");
        let device = percent_decode(device);
        if !is_valid_device_name(&device) {
            return json_response(request, 400, r#"{"error":"invalid device name"}"#);
        }
        handle_device_log(request, cfg, &device);
    } else if is_post && url == "/api/debug" {
        handle_debug_toggle(request);
    } else if is_get && (url == "/api/debug" || url.starts_with("/api/debug?")) {
        handle_debug_log(request, &url);
    } else if is_get && url == "/events" {
        handle_sse(request, cfg);
    } else if is_post && url.starts_with("/api/scan/") {
        let device = url.trim_start_matches("/api/scan/");
        let device = percent_decode(device);
        if !is_valid_device_name(&device) {
            return json_response(request, 400, r#"{"error":"invalid device name"}"#);
        }
        handle_scan(request, cfg, &device);
    } else if is_post && url.starts_with("/api/rip/") {
        let path = url.trim_start_matches("/api/rip/");
        // Split off the query string. URL form: /api/rip/<device>[?resume=yes|no]
        let (device_raw, query) = match path.split_once('?') {
            Some((d, q)) => (d, q),
            None => (path, ""),
        };
        let device = percent_decode(device_raw);
        if !is_valid_device_name(&device) {
            return json_response(request, 400, r#"{"error":"invalid device name"}"#);
        }
        handle_rip(request, cfg, &device, query);
    } else if is_post && url.starts_with("/api/accept-loss/") {
        let device = percent_decode(url.trim_start_matches("/api/accept-loss/"));
        if !is_valid_device_name(&device) {
            return json_response(request, 400, r#"{"error":"invalid device name"}"#);
        }
        handle_accept_loss(request, cfg, &device);
    } else if is_post && url == "/api/update-keydb" {
        handle_update_keydb(request, cfg);
    } else if is_post && url.starts_with("/api/eject/") {
        let device = url.trim_start_matches("/api/eject/");
        let device = percent_decode(device);
        if !is_valid_device_name(&device) {
            return json_response(request, 400, r#"{"error":"invalid device name"}"#);
        }
        handle_eject(request, &device);
    } else if is_post && url.starts_with("/api/stop/") {
        let device = url.trim_start_matches("/api/stop/");
        let device = percent_decode(device);
        if !is_valid_device_name(&device) {
            return json_response(request, 400, r#"{"error":"invalid device name"}"#);
        }
        handle_stop(request, cfg, &device);
    } else if is_get && url == "/api/review" {
        let staging = cfg
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .staging_dir
            .clone();
        let items = crate::server::review::list_held(&staging);
        json_response(
            request,
            200,
            &serde_json::to_string(&items).unwrap_or_else(|_| "[]".to_string()),
        );
    } else if is_post && url == "/api/review/resolve" {
        handle_review_resolve(request, cfg);
    } else if is_get && url.starts_with("/api/tmdb/search") {
        handle_tmdb_search(request, cfg, &url);
    } else if is_post && url.starts_with("/api/title/") {
        let device = percent_decode(url.trim_start_matches("/api/title/"));
        if !is_valid_device_name(&device) {
            return json_response(request, 400, r#"{"error":"invalid device name"}"#);
        }
        handle_title_override(request, &device);
    } else if url.starts_with("/api/library") {
        if let Some(request) = crate::server::library::api::handle(request, cfg) {
            json_response(request, 404, r#"{"error":"not found"}"#);
        }
    } else {
        json_response(request, 404, r#"{"error":"not found"}"#);
    }
}

// Defensive check for an operator poster URL later interpolated into an
// `<img src>` attribute: require http(s) and reject control chars/quotes so
// it can't break out of the attribute even if front-end escaping regresses.
fn is_valid_poster_url(url: &str) -> bool {
    if !(url.starts_with("http://") || url.starts_with("https://")) {
        return false;
    }
    !url.chars()
        .any(|c| c.is_control() || c == '"' || c == '\'' || c == '<' || c == '>')
}

// 404 body for per-device routes naming a device with no STATE entry: never enumerated, or a
// hot-plugged drive still in the poll loop's 60s first-seen settle window.
const UNKNOWN_DEVICE_BODY: &str = r#"{"ok":false,"error":"unknown or not yet initialized device"}"#;

// POST /api/title/<device>: operator's TMDB pick for the active disc.
// Body: {"title","year","poster_url","overview"}. Stored as a one-shot
// override `rip_disc` consumes; also reflected on the live card immediately.
fn handle_title_override(request: tiny_http::Request, device: &str) {
    // An override for an untracked drive has nothing to attach to and
    // would persist orphaned; reject before reading the body, matching
    // how other per-device routes validate (404 unknown).
    if !ripper::device_known(device) {
        return json_response(request, 404, UNKNOWN_DEVICE_BODY);
    }
    let (request, body) = match read_json_body(request) {
        Ok(rb) => rb,
        Err(()) => return,
    };
    let v: serde_json::Value = match serde_json::from_str(&body) {
        Ok(v) => v,
        Err(_) => return json_response(request, 400, r#"{"ok":false,"error":"invalid json"}"#),
    };
    // Clamp operator-supplied free text on char boundaries before it's
    // persisted and re-broadcast to every dashboard client (mirrors the
    // 200-char `q` cap). Caps: title ~300, overview ~2000, poster_url ~1000.
    let title = clamp_chars(v["title"].as_str().unwrap_or("").trim(), 300);
    if title.is_empty() {
        return json_response(request, 400, r#"{"ok":false,"error":"title required"}"#);
    }
    let year = v["year"]
        .as_u64()
        .and_then(|y| u16::try_from(y).ok())
        .unwrap_or(0);
    let poster_raw = v["poster_url"].as_str().unwrap_or("");
    if !poster_raw.is_empty() && !is_valid_poster_url(poster_raw) {
        return json_response(request, 400, r#"{"ok":false,"error":"invalid poster_url"}"#);
    }
    let poster = clamp_chars(poster_raw, 1000);
    let overview = clamp_chars(v["overview"].as_str().unwrap_or(""), 2000);
    // Preserve the disc's DETECTED media_type when the caller omits one
    // (Manual Rename does): defaulting to "movie" would flip a TV disc and
    // collapse all its episodes into one file. Fall back only when unknown.
    let media_type = match v["media_type"].as_str().filter(|s| !s.is_empty()) {
        Some(mt) => normalize_media_type(mt),
        None => {
            // Recover a poisoned lock (`into_inner`) like every other STATE
            // consumer here — abandoning would fall through to the "movie"
            // default this arm exists to avoid.
            let current = ripper::STATE
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .get(device)
                .map(|rs| rs.tmdb_media_type.clone())
                .unwrap_or_default();
            normalize_media_type(if current.is_empty() {
                "movie"
            } else {
                &current
            })
        }
    };
    // The picker posts back the chosen result's TMDB id (0 if the operator
    // typed a free-form title with no pick); carry it so the override matches
    // what a `lookup` match would have provided.
    let tmdb_id = v["tmdb_id"].as_u64().unwrap_or(0);
    ripper::set_title_override(
        device,
        crate::server::tmdb::TmdbResult {
            title: title.clone(),
            year,
            poster_url: poster.clone(),
            overview: overview.clone(),
            media_type,
            tmdb_id,
        },
    );
    // Reflect on the live card right away.
    ripper::update_state_with(device, |s| {
        s.tmdb_title = title.clone();
        s.tmdb_year = year;
        if !poster.is_empty() {
            s.tmdb_poster = poster.clone();
        }
        if !overview.is_empty() {
            s.tmdb_overview = overview.clone();
        }
    });
    json_response(request, 200, r#"{"ok":true}"#);
}

/// `POST /api/review/resolve` — resolve a held rip. Body:
/// `{"dir":"<staging subdir>","action":"proceed|retitle|cancel","title":"…","year":2024}`.
fn handle_review_resolve(request: tiny_http::Request, cfg: &Arc<RwLock<Config>>) {
    let (request, body) = match read_json_body(request) {
        Ok(rb) => rb,
        Err(()) => return,
    };
    let v: serde_json::Value = match serde_json::from_str(&body) {
        Ok(v) => v,
        Err(_) => return json_response(request, 400, r#"{"ok":false,"error":"invalid json"}"#),
    };
    // Cap operator-supplied strings before they reach a filesystem marker,
    // mirroring handle_title_override (clamp_chars by char count, not bytes).
    let dir = clamp_chars(v["dir"].as_str().unwrap_or("").trim(), 300);
    let staging = cfg
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .staging_dir
        .clone();
    let action = match v["action"].as_str().unwrap_or("") {
        "proceed" => crate::server::review::Resolve::Proceed,
        "cancel" => crate::server::review::Resolve::Cancel,
        "retitle" => {
            let title = clamp_chars(v["title"].as_str().unwrap_or("").trim(), 300);
            if title.is_empty() {
                return json_response(request, 400, r#"{"ok":false,"error":"title required"}"#);
            }
            let year = v["year"]
                .as_u64()
                .and_then(|y| u16::try_from(y).ok())
                .unwrap_or(0);
            crate::server::review::Resolve::Retitle { title, year }
        }
        _ => return json_response(request, 400, r#"{"ok":false,"error":"bad action"}"#),
    };
    match crate::server::review::resolve(&staging, &dir, action) {
        Ok(()) => json_response(request, 200, r#"{"ok":true}"#),
        Err(e) => {
            // Build the error payload with serde so backslashes, newlines,
            // and control chars in a filesystem error string are escaped
            // properly — manual quote-replacement produced malformed JSON.
            let body = serde_json::json!({ "ok": false, "error": e }).to_string();
            json_response(request, 400, &body)
        }
    }
}

/// `GET /api/tmdb/search?q=<query>` — candidate matches for the review picker.
fn handle_tmdb_search(request: tiny_http::Request, cfg: &Arc<RwLock<Config>>, url: &str) {
    // Parse via parse_query so `q` is found regardless of parameter order
    // (split_once("?q=") only matched q as the first query parameter, so
    // e.g. /api/tmdb/search?version=2&q=movie yielded an empty query).
    let q = parse_query(url).get("q").cloned().unwrap_or_default();
    let q = q.trim();
    // Reject empty queries and cap length so we never forward an abusive
    // request to TMDB.
    if q.is_empty() || q.len() > 200 {
        return json_response(request, 400, r#"{"error":"invalid query"}"#);
    }
    // Global cooldown: an unauthenticated LAN client could otherwise flood
    // TMDB through this proxy. Gate on the time since the last forwarded
    // search; reply 429 if a request arrived too recently.
    {
        use std::sync::Mutex;
        use std::time::{Duration, Instant};
        static LAST_TMDB_SEARCH: Mutex<Option<Instant>> = Mutex::new(None);
        const TMDB_MIN_INTERVAL: Duration = Duration::from_millis(500);
        let mut last = LAST_TMDB_SEARCH.lock().unwrap_or_else(|e| e.into_inner());
        let now = Instant::now();
        if let Some(prev) = *last
            && now.duration_since(prev) < TMDB_MIN_INTERVAL
        {
            return json_response(request, 429, r#"{"error":"rate limited"}"#);
        }
        *last = Some(now);
    }
    let key = cfg
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .tmdb_api_key
        .clone();
    let results = crate::server::tmdb::search(q, &key, 8);
    json_response(
        request,
        200,
        &serde_json::to_string(&results).unwrap_or_else(|_| "[]".to_string()),
    );
}

// ---------- Helpers ----------

fn serve_html(request: tiny_http::Request) {
    let header =
        Header::from_bytes(&b"Content-Type"[..], &b"text/html; charset=utf-8"[..]).unwrap();
    let html = DASHBOARD_HTML.replace("{VERSION}", crate::server::VERSION_LABEL);
    // The dashboard IS the app shell (single self-contained HTML+CSS+JS
    // page); serve it non-cacheable so browsers don't keep running the
    // OLD UI after a deploy. no-store forces a fresh fetch on every load.
    let response = Response::from_string(html).with_header(header).with_header(
        Header::from_bytes(
            &b"Cache-Control"[..],
            &b"no-store, no-cache, must-revalidate"[..],
        )
        .unwrap(),
    );
    let _ = request.respond(response);
}

fn serve_favicon(request: tiny_http::Request) {
    let header = Header::from_bytes(&b"Content-Type"[..], &b"image/svg+xml"[..]).unwrap();
    // Unlike the app shell, the icon is immutable brand art — let the browser
    // cache it so the tab icon doesn't refetch on every poll/load.
    let response = Response::from_string(FAVICON_SVG)
        .with_header(header)
        .with_header(
            Header::from_bytes(&b"Cache-Control"[..], &b"public, max-age=86400"[..]).unwrap(),
        );
    let _ = request.respond(response);
}

pub(crate) fn json_response(request: tiny_http::Request, status: u16, body: &str) {
    let header = Header::from_bytes(&b"Content-Type"[..], &b"application/json"[..]).unwrap();
    // This is a local control app, not a website: NOTHING is cacheable. The API
    // responses (state/version/etc.) are polled live, so a cached body would show
    // stale rip state. Match the HTML shell's no-store.
    let response = Response::from_string(body)
        .with_status_code(StatusCode(status))
        .with_header(header)
        .with_header(
            Header::from_bytes(
                &b"Cache-Control"[..],
                &b"no-store, no-cache, must-revalidate"[..],
            )
            .unwrap(),
        );
    let _ = request.respond(response);
}

// Sentinel returned in place of a stored secret on GET /api/settings; a
// POST field carrying exactly this value is treated as "unchanged" so the
// UI can round-trip the redacted form without clobbering the real secret.
const SECRET_SENTINEL: &str = "********";

// Mask a webhook URL for display: keep the origin (scheme://host[:port])
// but replace the path/query — where the secret token lives — with the
// sentinel; a value containing the sentinel round-trips as "unchanged".
fn mask_webhook_url(url: &str) -> String {
    // Origin = everything up to the first '/', '?', or '#' after `scheme://`.
    // Treating '?' and '#' as terminators prevents a token carried in a query
    // string (`https://host?token=SECRET`) from slipping through unredacted.
    if let Some(scheme_end) = url.find("://") {
        let after = scheme_end + 3;
        let origin_end = url[after..]
            .find(['/', '?', '#'])
            .map(|i| after + i)
            .unwrap_or(url.len());
        // If the authority carries HTTP basic-auth userinfo (`user:pass@host`),
        // the masked value would otherwise LEAK the credentials to the client.
        // Drop up to and including the last '@' so only `host[:port]` survives.
        let authority = &url[after..origin_end];
        let host_start = match authority.rfind('@') {
            Some(at) => after + at + 1,
            None => after,
        };
        return format!(
            "{}{}/{}",
            &url[..after],
            &url[host_start..origin_end],
            SECRET_SENTINEL
        );
    }
    // No scheme — nothing identifiable to preserve; fully mask.
    SECRET_SENTINEL.to_string()
}

// Mask a webhook URL, appending a stable `#<idx>` (its index in
// `webhook_urls`) so POST resolves by identity, not origin — two hooks
// sharing an origin would otherwise mask identically and collide.
fn mask_webhook_url_indexed(url: &str, idx: usize) -> String {
    format!("{}#{idx}", mask_webhook_url(url))
}

// True if `s` is a redacted placeholder from `mask_webhook_url[_indexed]`:
// ends with the sentinel, or `********#<digits>`. Strict on purpose — a URL
// that merely embeds the sentinel mid-path still gets validated.
fn is_masked_webhook(s: &str) -> bool {
    if s.ends_with(SECRET_SENTINEL) {
        return true;
    }
    if let Some((head, idx)) = s.rsplit_once('#') {
        return head.ends_with(SECRET_SENTINEL)
            && !idx.is_empty()
            && idx.bytes().all(|b| b.is_ascii_digit());
    }
    false
}

// One webhook as it arrives on POST /api/settings: a (possibly masked) URL
// plus its per-event flags. `resolve_webhook_entries` turns these into
// stored `WebhookEntry`s, unmasking the URL while keeping the flags.
struct IncomingWebhook {
    /// May be a real URL (newly entered) or a masked placeholder to resolve.
    url: String,
    post_rip: bool,
    post_mux: bool,
    post_move: bool,
}

// Resolve an incoming webhook_urls array against stored entries, unmasking each placeholder by
// stable #idx (falling back to origin) — never by array position, since rows can be reordered.
fn resolve_webhook_entries(
    incoming: &[IncomingWebhook],
    existing: &[WebhookEntry],
) -> Result<Vec<WebhookEntry>, String> {
    let mut resolved: Vec<WebhookEntry> = Vec::with_capacity(incoming.len());
    for hook in incoming {
        let s = hook.url.as_str();
        // Resolve the URL only; the flags are always taken from `incoming`.
        let url = if is_masked_webhook(s) {
            // Preferred: resolve by the stable `#<idx>` identifier so two
            // same-origin webhooks round-trip unambiguously. The index must
            // be in range AND still mask to this placeholder, else reject.
            if let Some((origin_mask, idx_str)) = s.rsplit_once('#')
                && let Ok(idx) = idx_str.parse::<usize>()
            {
                match existing.get(idx) {
                    Some(stored) if mask_webhook_url(&stored.url) == origin_mask => {
                        stored.url.clone()
                    }
                    // Index stale (row deleted/reordered) — reject rather
                    // than guess.
                    _ => return Err(s.to_string()),
                }
            } else {
                // Fallback: no embedded index (older client). Match by origin;
                // only unambiguous when exactly one stored URL shares the origin.
                let matches: Vec<&WebhookEntry> = existing
                    .iter()
                    .filter(|stored| mask_webhook_url(&stored.url) == s)
                    .collect();
                match matches.as_slice() {
                    [one] => one.url.clone(),
                    _ => return Err(s.to_string()),
                }
            }
        } else {
            s.to_string()
        };
        if url.trim().is_empty() {
            continue;
        }
        resolved.push(WebhookEntry {
            url,
            post_rip: hook.post_rip,
            post_mux: hook.post_mux,
            post_move: hook.post_move,
        });
    }
    Ok(resolved)
}

// Serialize Config for GET /api/settings with credential fields redacted:
// no route is authenticated and the server binds 0.0.0.0, so cleartext
// keyserver_secret/tmdb_api_key would hand any LAN client the operator's key.
fn settings_json_redacted(c: &Config) -> String {
    let mut v = serde_json::to_value(c).unwrap_or_else(|_| serde_json::json!({}));
    for field in ["keyserver_secret", "tmdb_api_key"] {
        if let Some(s) = v.get(field).and_then(|x| x.as_str())
            && !s.is_empty()
        {
            v[field] = serde_json::json!(SECRET_SENTINEL);
        }
    }
    // keyserver_url/keydb_url may carry auth tokens in the path/query; mask
    // with the origin-preserving helper so the operator sees the host but
    // not the secret. A masked value round-trips on POST.
    for field in ["keyserver_url", "keydb_url"] {
        if let Some(s) = v.get(field).and_then(|x| x.as_str())
            && !s.is_empty()
        {
            v[field] = serde_json::json!(mask_webhook_url(s));
        }
    }
    // keydb_path is an absolute container path; leaking it would expose the
    // internal filesystem layout, so return only the filename component.
    if let Some(s) = v.get("keydb_path").and_then(|x| x.as_str())
        && !s.is_empty()
    {
        let name = std::path::Path::new(s)
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        v["keydb_path"] = serde_json::json!(name);
    }
    // Each entry serializes as {url, post_rip, post_mux, post_move}. Mask the token in
    // `url` (keeping the origin + stable `#idx` for round-trip resolution) and
    // pass the boolean flags through untouched.
    if let Some(arr) = v.get_mut("webhook_urls").and_then(|x| x.as_array_mut()) {
        for (idx, entry) in arr.iter_mut().enumerate() {
            if let Some(u) = entry.get("url").and_then(|x| x.as_str())
                && !u.is_empty()
            {
                let masked = mask_webhook_url_indexed(u, idx);
                entry["url"] = serde_json::json!(masked);
            }
        }
    }
    // Operator-facing, NOT persisted (serde drops it on POST): the ACTUAL path
    // the keydb reads resolve to + whether a file is present there, so the
    // Settings UI shows exactly where autorip looks for keys (issue #46).
    let rp = crate::server::keysource::keydb_path(c);
    v["keydb_resolved"] = serde_json::json!(format!(
        "{}  —  {}",
        rp.display(),
        if rp.exists() {
            "file present"
        } else {
            "NOT FOUND (autorip will report NO KEY here)"
        }
    ));
    v.to_string()
}

// Cap on a request body read fully into memory. POST bodies are small JSON
// (settings/title/review/debug); without this cap an unauthenticated LAN
// client could stream a multi-GB body and OOM the container (DoS).
const MAX_REQUEST_BODY: u64 = 1024 * 1024;

/// Outcome of [`read_body_capped`].
enum BodyRead {
    /// Body read successfully, within the cap.
    Ok(String),
    /// The reader errored before EOF (truncated/disconnected client).
    Err,
    /// The body exceeded `MAX_REQUEST_BODY` before EOF.
    TooLarge,
}

// Read a body into a String, capped at MAX_REQUEST_BODY + 1 bytes: the
// extra byte lets an exactly-at-limit body pass while detecting oversize.
// Content-Length is never trusted; `take` bounds actual bytes read.
fn read_body_capped(request: &mut tiny_http::Request) -> BodyRead {
    let mut body = String::new();
    match request
        .as_reader()
        .take(MAX_REQUEST_BODY + 1)
        .read_to_string(&mut body)
    {
        Ok(_) => {
            if body.len() as u64 > MAX_REQUEST_BODY {
                BodyRead::TooLarge
            } else {
                BodyRead::Ok(body)
            }
        }
        Err(_) => BodyRead::Err,
    }
}

/// Read a JSON POST body with the shared size cap, replying with the
/// appropriate error status (400 bad body / 413 too large) on failure.
/// Returns `None` once a response has already been sent.
pub(crate) fn read_json_body(
    mut request: tiny_http::Request,
) -> Result<(tiny_http::Request, String), ()> {
    match read_body_capped(&mut request) {
        BodyRead::Ok(body) => Ok((request, body)),
        BodyRead::Err => {
            json_response(request, 400, r#"{"ok":false,"error":"bad body"}"#);
            Err(())
        }
        BodyRead::TooLarge => {
            json_response(
                request,
                413,
                r#"{"ok":false,"error":"request body too large"}"#,
            );
            Err(())
        }
    }
}

// Validate device name is alphanumeric-only (sgN/diskN/CdRomN): rejects
// slashes/traversal so a malformed URL like /api/rip/sg4/stop can't reach
// the rip handler with device="sg4/stop" (previously spawned a doomed thread).
fn is_valid_device_name(s: &str) -> bool {
    // Cross-OS device key (Linux `sgN`, macOS `diskN`, Windows `CdRomN`).
    // ASCII-alphanumeric only is the path-safety boundary rejecting
    // separators/traversal (`sg4/stop`); not a "this drive exists" check.
    (3..=64).contains(&s.len()) && s.bytes().all(|b| b.is_ascii_alphanumeric())
}

// media_type on a title override is the one field handle_title_override left
// unclamped/unallow-listed; the router only acts on "tv" vs everything else
// (and TMDB only ever yields these two), so allow-listing is the honest bound.
fn normalize_media_type(raw: &str) -> String {
    match raw.trim().to_ascii_lowercase().as_str() {
        "tv" => "tv".to_string(),
        _ => "movie".to_string(),
    }
}

// Clamp `s` to `max` Unicode scalar values without splitting a multi-byte
// char; bounds operator-supplied free text (title/overview/poster_url)
// before it's persisted and re-broadcast.
fn clamp_chars(s: &str, max: usize) -> String {
    match s.char_indices().nth(max) {
        Some((byte_idx, _)) => s[..byte_idx].to_string(),
        None => s.to_string(),
    }
}

// ── SSRF guard: operator-supplied URLs are blocked at store/fetch time and
// pinned to the validated IP (DNS-rebinding TOCTOU). Conservative: anything
// not clearly a routable public address is blocked (loopback/RFC1918/etc).
fn is_blocked_ip(ip: &IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            v4.is_loopback()
                || v4.is_private()
                || v4.is_link_local() // 169.254.0.0/16, incl. metadata 169.254.169.254
                || v4.is_broadcast()
                || v4.is_documentation()
                || v4.is_unspecified()
                || v4.is_multicast()
                // Carrier-grade NAT 100.64.0.0/10 (not flagged by std helpers).
                || (v4.octets()[0] == 100 && (v4.octets()[1] & 0xc0) == 0x40)
                // 0.0.0.0/8 "this network".
                || v4.octets()[0] == 0
                // Benchmarking 198.18.0.0/15 (RFC 2544) — 198.18.x and 198.19.x
                // (the /15 second octet is 18 with the low bit free, i.e. 18|19).
                || (v4.octets()[0] == 198 && (v4.octets()[1] & 0xfe) == 18)
                // IETF protocol assignments 192.0.0.0/24 (RFC 6890), which
                // includes 192.0.0.170/171 (NAT64/DNS64 discovery). Distinct
                // from 192.0.2.0/24 TEST-NET-1, already caught by is_documentation.
                || (v4.octets()[0] == 192 && v4.octets()[1] == 0 && v4.octets()[2] == 0)
                // Class-E reserved 240.0.0.0/4 (not flagged by std helpers).
                || v4.octets()[0] >= 240
        }
        IpAddr::V6(v6) => {
            let seg = v6.segments();
            // 6to4 (2002::/16) embeds an IPv4 in segments[1..3]; Teredo
            // (2001:0000::/32) embeds it in the last two segments, each XOR 0xffff.
            // Re-check both as their embedded IPv4 or an internal target tunnels in.
            let sixtofour = (seg[0] == 0x2002)
                .then(|| std::net::Ipv4Addr::from(((seg[1] as u32) << 16) | (seg[2] as u32)));
            let teredo = (seg[0] == 0x2001 && seg[1] == 0x0000).then(|| {
                std::net::Ipv4Addr::from(
                    (((seg[6] ^ 0xffff) as u32) << 16) | ((seg[7] ^ 0xffff) as u32),
                )
            });
            // NAT64 well-known prefix 64:ff9b::/96 (RFC 6052) embeds the IPv4
            // target in the last 32 bits; re-check it or an internal address
            // slips through a NAT64 translator.
            let nat64 = (seg[0] == 0x0064 && seg[1] == 0xff9b)
                .then(|| std::net::Ipv4Addr::from(((seg[6] as u32) << 16) | (seg[7] as u32)));
            v6.is_loopback()
                || v6.is_unspecified()
                || v6.is_multicast()
                // Unique-local fc00::/7.
                || (seg[0] & 0xfe00) == 0xfc00
                // Link-local fe80::/10.
                || (seg[0] & 0xffc0) == 0xfe80
                // IPv4-mapped (::ffff:a.b.c.d) and IPv4-compatible (::a.b.c.d)
                // — to_ipv4() catches both forms; re-check the unwrapped address.
                || v6.to_ipv4().map(|m| is_blocked_ip(&IpAddr::V4(m))) == Some(true)
                || sixtofour.is_some_and(|v4| is_blocked_ip(&IpAddr::V4(v4)))
                || teredo.is_some_and(|v4| is_blocked_ip(&IpAddr::V4(v4)))
                || nat64.is_some_and(|v4| is_blocked_ip(&IpAddr::V4(v4)))
        }
    }
}

// The three "could not find out" failure strings, as opposed to "this URL is not allowed"; kept
// as constants so is_transient_resolve_error can classify without duplicated literals.
pub(crate) const RESOLVE_TIMEOUT_MSG: &str = "DNS resolution timed out";
pub(crate) const RESOLVE_FAILED_PREFIX: &str = "could not resolve host: ";
pub(crate) const RESOLVE_NO_ADDRS_MSG: &str = "host did not resolve to any address";

// True when the error means the host could not be looked up right now (DNS blip), not a
// permanent verdict on the URL — a resolver blip is not evidence the remote service is down.
pub(crate) fn is_transient_resolve_error(msg: &str) -> bool {
    msg == RESOLVE_TIMEOUT_MSG
        || msg == RESOLVE_NO_ADDRS_MSG
        || msg.starts_with(RESOLVE_FAILED_PREFIX)
}

// Resolve host:port with a bounded deadline: ToSocketAddrs blocks and can hang for the OS
// resolver timeout, freezing the calling handler thread. Runs on a spawned thread, joined with
// a short deadline.
pub(crate) fn resolve_with_timeout(host: &str, port: u16) -> Result<Vec<SocketAddr>, String> {
    use std::sync::mpsc;
    use std::time::Duration;
    const DNS_TIMEOUT: Duration = Duration::from_secs(4);
    // A timed-out resolver thread can't be cancelled — it lingers until
    // `to_socket_addrs` returns. Cap detached resolvers in flight so repeated
    // timeouts can't leak them unboundedly; at the cap, fail fast instead.
    const MAX_INFLIGHT: usize = 8;
    static INFLIGHT: AtomicUsize = AtomicUsize::new(0);

    // RAII admission token: moved into the resolver closure so the slot is
    // held until the (possibly detached) worker returns; if `thread::spawn`
    // itself unwinds, the guard drops here instead, so it never leaks.
    let guard = match ConnGuard::try_acquire(&INFLIGHT, MAX_INFLIGHT) {
        Some(g) => g,
        None => return Err(RESOLVE_TIMEOUT_MSG.to_string()),
    };

    let host = host.to_string();
    // Bounded channel of capacity 1: the resolver's single send never blocks,
    // so the thread always exits cleanly even if the receiver has already
    // timed out and gone away.
    let (tx, rx) = mpsc::sync_channel::<Result<Vec<SocketAddr>, std::io::Error>>(1);
    std::thread::spawn(move || {
        let _g = guard;
        let res = (host.as_str(), port)
            .to_socket_addrs()
            .map(|it| it.collect::<Vec<SocketAddr>>());
        // Receiver may be gone after the timeout — ignore the send error.
        let _ = tx.send(res);
    });
    match rx.recv_timeout(DNS_TIMEOUT) {
        Ok(Ok(addrs)) => Ok(addrs),
        Ok(Err(e)) => Err(format!("{RESOLVE_FAILED_PREFIX}{e}")),
        Err(_) => Err(RESOLVE_TIMEOUT_MSG.to_string()),
    }
}

// Validate an operator-supplied fetch/POST URL against the SSRF guard: requires http(s),
// resolves the host once, and rejects blocked addresses. Returns resolved sockets so the caller
// can pin the connection.
pub(crate) fn validate_fetch_url(url: &str) -> Result<Vec<SocketAddr>, String> {
    let url = url.trim();
    if url.is_empty() {
        return Err("URL is empty".to_string());
    }
    // Minimal scheme + authority parse — no URL crate dep, mirroring the
    // hand-rolled parsers already in this module.
    let rest = if let Some(r) = url.strip_prefix("https://") {
        (r, 443u16)
    } else if let Some(r) = url.strip_prefix("http://") {
        (r, 80u16)
    } else {
        return Err("URL must start with http:// or https://".to_string());
    };
    let (authority, default_port) = rest;
    // Strip path/query/fragment — keep only the authority (host[:port]).
    let authority = authority.split(['/', '?', '#']).next().unwrap_or(authority);
    // Strip userinfo if present (user:pass@host).
    let authority = authority.rsplit('@').next().unwrap_or(authority);
    if authority.is_empty() {
        return Err("URL has no host".to_string());
    }
    // Split host:port, handling bracketed IPv6 literals [::1]:8080.
    let (host, port): (String, u16) = if let Some(stripped) = authority.strip_prefix('[') {
        match stripped.split_once(']') {
            Some((h, after)) => {
                let p = after
                    .strip_prefix(':')
                    .map(|s| s.parse::<u16>().map_err(|_| "invalid port".to_string()))
                    .transpose()?
                    .unwrap_or(default_port);
                (h.to_string(), p)
            }
            None => return Err("malformed IPv6 host".to_string()),
        }
    } else if let Some((h, p)) = authority.rsplit_once(':') {
        // Only treat the trailing ':' as a port separator if the right
        // side is numeric (avoids mis-splitting a bare IPv6 literal,
        // though those should be bracketed).
        match p.parse::<u16>() {
            Ok(p) => (h.to_string(), p),
            Err(_) => (authority.to_string(), default_port),
        }
    } else {
        (authority.to_string(), default_port)
    };
    if host.is_empty() {
        return Err("URL has no host".to_string());
    }

    // Resolve once, with a bounded deadline (see resolve_with_timeout).
    let addrs: Vec<SocketAddr> = resolve_with_timeout(&host, port)?;
    if addrs.is_empty() {
        return Err(RESOLVE_NO_ADDRS_MSG.to_string());
    }
    for a in &addrs {
        if is_blocked_ip(&a.ip()) {
            return Err(format!(
                "refusing to connect to non-public address {} (SSRF guard)",
                a.ip()
            ));
        }
    }
    Ok(addrs)
}

// Validate an operator network output target against the SSRF guard.
// Unlike validate_fetch_url the target is a bare host:port (no scheme) —
// libfreemkv streams decrypted disc content to it — same blocked-address rule.
pub(crate) fn validate_network_target(target: &str) -> Result<(), String> {
    let target = target.trim();
    if target.is_empty() {
        return Err("network target is empty".to_string());
    }
    // Split host:port, handling bracketed IPv6 literals [::1]:9000.
    let (host, port): (String, u16) = if let Some(stripped) = target.strip_prefix('[') {
        match stripped.split_once(']') {
            Some((h, after)) => {
                let p = after
                    .strip_prefix(':')
                    .ok_or_else(|| "network target needs a port (host:port)".to_string())?
                    .parse::<u16>()
                    .map_err(|_| "invalid port".to_string())?;
                (h.to_string(), p)
            }
            None => return Err("malformed IPv6 host".to_string()),
        }
    } else {
        let (h, p) = target
            .rsplit_once(':')
            .ok_or_else(|| "network target needs a port (host:port)".to_string())?;
        let p = p.parse::<u16>().map_err(|_| "invalid port".to_string())?;
        (h.to_string(), p)
    };
    if host.is_empty() {
        return Err("network target has no host".to_string());
    }

    // Bounded DNS — same shared helper validate_fetch_url uses, so an
    // unauthenticated settings POST can't freeze the handler on a slow resolver.
    let addrs: Vec<SocketAddr> = resolve_with_timeout(&host, port)?;
    if addrs.is_empty() {
        return Err(RESOLVE_NO_ADDRS_MSG.to_string());
    }
    for a in &addrs {
        if is_blocked_ip(&a.ip()) {
            return Err(format!(
                "refusing to stream to non-public address {} (SSRF guard)",
                a.ip()
            ));
        }
    }
    Ok(())
}

// Cap on pinned addresses: ureq's fixed 16-slot ResolvedSocketAddrs panics (out-of-bounds) on a
// 17th push, so cap to the first 16 validate_fetch_url already vetted.
const MAX_PINNED_ADDRS: usize = 16;

// The addresses a resolve may actually hand back, capped at MAX_PINNED_ADDRS. Separated from
// the Resolver impl so the cap is testable — ureq's resolver types aren't nameable outside the
// crate.
fn pinned_addrs(addrs: &[SocketAddr]) -> Vec<SocketAddr> {
    addrs.iter().copied().take(MAX_PINNED_ADDRS).collect()
}

// Pinned-address resolver behind guarded_agent_with_timeouts. Must be wired via
// Agent::with_parts — Agent::new_with_config compiles but silently uses the default
// (re-resolving) resolver.
#[derive(Debug)]
struct PinnedResolver(Vec<SocketAddr>);

impl ureq::unversioned::resolver::Resolver for PinnedResolver {
    fn resolve(
        &self,
        _uri: &ureq::http::Uri,
        _config: &ureq::config::Config,
        _timeout: ureq::unversioned::transport::NextTimeout,
    ) -> Result<ureq::unversioned::resolver::ResolvedSocketAddrs, ureq::Error> {
        let addrs = pinned_addrs(&self.0);
        if addrs.is_empty() {
            return Err(ureq::Error::HostNotFound);
        }
        let mut out = self.empty();
        for addr in addrs {
            out.push(addr);
        }
        Ok(out)
    }
}

// A short, URL-FREE description of a ureq failure: these summaries reach
// syslog/autorip.jsonl/unauthenticated endpoints, so each variant maps to a fixed label instead
// of ever formatting the error.
pub(crate) fn ureq_error_kind(e: &ureq::Error) -> String {
    match e {
        ureq::Error::StatusCode(code) => format!("HTTP {code}"),
        ureq::Error::Io(io) => match io.raw_os_error() {
            // An OS-generated error (has an errno): its Display is the
            // syscall's own message, derived purely from the errno and never
            // the URL — surface it instead of the useless generic label.
            Some(_) => format!("io: {io}"),
            // No errno — a ureq/std-synthesized io error whose payload we do
            // NOT trust to be URL-free. Fall back to the ErrorKind's fixed
            // description, which is a constant string and never the URL.
            None => format!("io: {}", io.kind()),
        },
        ureq::Error::Timeout(_) => "timeout".to_string(),
        ureq::Error::HostNotFound => "host not found".to_string(),
        ureq::Error::ConnectionFailed => "connection failed".to_string(),
        ureq::Error::TooManyRedirects => "too many redirects".to_string(),
        ureq::Error::Tls(_) => "tls error".to_string(),
        ureq::Error::BodyExceedsLimit(_) => "body exceeds limit".to_string(),
        _ => "transport error".to_string(),
    }
}

// Rolling stall detector (re-armed on every read that returns bytes): no progress for this long
// means the peer is dead. Replaces the ureq 2 timeout_read knob the 2→3 migration dropped.
pub(crate) const STALL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(20);

// Chained after DefaultConnector to re-arm a ROLLING per-read idle bound on every body read,
// restoring the stall detection ureq 3.4.1 removed (#1194).
#[derive(Debug)]
struct IdleReCapConnector {
    idle: std::time::Duration,
}

impl<In: ureq::unversioned::transport::Transport> ureq::unversioned::transport::Connector<In>
    for IdleReCapConnector
{
    type Out = IdleReCapTransport<In>;

    fn connect(
        &self,
        _details: &ureq::unversioned::transport::ConnectionDetails,
        chained: Option<In>,
    ) -> Result<Option<Self::Out>, ureq::Error> {
        Ok(chained.map(|inner| IdleReCapTransport {
            inner,
            idle: self.idle,
        }))
    }
}

#[derive(Debug)]
struct IdleReCapTransport<In> {
    inner: In,
    idle: std::time::Duration,
}

impl<In> IdleReCapTransport<In> {
    // Cap body reads at the idle bound. Global/PerCall mask the phase (ureq reports the earliest
    // deadline), so they are capped too: any caller setting timeout_global/timeout_per_call on a
    // guarded agent also gets its header wait clamped to idle. min keeps the tighter bound.
    fn cap(
        &self,
        timeout: ureq::unversioned::transport::NextTimeout,
    ) -> ureq::unversioned::transport::NextTimeout {
        use ureq::unversioned::transport::time::Duration as UreqDuration;
        if !matches!(
            timeout.reason,
            ureq::Timeout::RecvBody | ureq::Timeout::Global | ureq::Timeout::PerCall
        ) {
            return timeout;
        }
        let idle = UreqDuration::from_millis(self.idle.as_millis() as u64);
        let after = if timeout.after < idle {
            timeout.after
        } else {
            idle
        };
        ureq::unversioned::transport::NextTimeout {
            after,
            reason: timeout.reason,
        }
    }
}

impl<In: ureq::unversioned::transport::Transport> ureq::unversioned::transport::Transport
    for IdleReCapTransport<In>
{
    fn buffers(&mut self) -> &mut dyn ureq::unversioned::transport::Buffers {
        self.inner.buffers()
    }

    fn transmit_output(
        &mut self,
        amount: usize,
        timeout: ureq::unversioned::transport::NextTimeout,
    ) -> Result<(), ureq::Error> {
        self.inner.transmit_output(amount, timeout)
    }

    fn await_input(
        &mut self,
        timeout: ureq::unversioned::transport::NextTimeout,
    ) -> Result<bool, ureq::Error> {
        let capped = self.cap(timeout);
        self.inner.await_input(capped)
    }

    fn is_open(&mut self) -> bool {
        self.inner.is_open()
    }

    fn is_tls(&self) -> bool {
        self.inner.is_tls()
    }
}

// Build the ONE DNS-pinned, redirect-blocking ureq agent, with caller-chosen
// connect/response/idle timeouts (ureq sets no defaults, so an unresponsive peer would
// otherwise block the thread forever).
pub(crate) fn guarded_agent_with_timeouts(
    pinned: Vec<SocketAddr>,
    connect: std::time::Duration,
    response: std::time::Duration,
    idle: std::time::Duration,
) -> ureq::Agent {
    // response bounds header arrival and (as timeout_recv_body) the TOTAL body transfer; idle
    // is the rolling stall bound, layered on by IdleReCapConnector since ureq 3 dropped it.
    let config = ureq::config::Config::builder()
        .max_redirects(0)
        .timeout_connect(Some(connect))
        .timeout_recv_response(Some(response))
        .timeout_recv_body(Some(response))
        .build();
    // `with_parts`, never `new_with_config` — see [`PinnedResolver`].
    // DefaultConnector opens the (TLS) socket; IdleReCapConnector wraps its
    // transport to re-arm the rolling idle bound on every body read.
    use ureq::unversioned::transport::Connector as _;
    ureq::Agent::with_parts(
        config,
        ureq::unversioned::transport::DefaultConnector::new().chain(IdleReCapConnector { idle }),
        PinnedResolver(pinned),
    )
}

// Agent for webhook delivery: a plain outbound POST with the standard resolver, deliberately
// NOT SSRF-guarded — a webhook targeting a LAN service (Home Assistant, a NAS) is the intended
// use.
pub(crate) fn webhook_agent() -> ureq::Agent {
    let config = ureq::config::Config::builder()
        .max_redirects(0)
        .timeout_connect(Some(std::time::Duration::from_secs(5)))
        .timeout_recv_response(Some(std::time::Duration::from_secs(30)))
        .timeout_recv_body(Some(STALL_TIMEOUT))
        .build();
    ureq::Agent::new_with_config(config)
}

// SSRF-guarded HTTP GET: the single entry point for fetching an operator- supplied URL, instead
// of ureq::get directly (which bypasses the guard). `pub` for the lib facade re-export; only
// bin/tests call it.
pub fn guarded_get(url: &str) -> Result<ureq::http::Response<ureq::Body>, String> {
    guarded_get_within(url, KEYDB_TRANSFER_BUDGET)
}

// End-to-end ceiling on the unauthenticated /api KEYDB update; tighter than
// KEYDB_TRANSFER_BUDGET because this path holds an in-flight handler slot and the update flag
// that 429s everyone else.
pub(crate) const KEYDB_FETCH_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);

// The /api/update-keydb fetch: guarded agent plus a request-level end-to-end ceiling, which
// keeps this LAN-facing path at KEYDB_FETCH_TIMEOUT rather than KEYDB_TRANSFER_BUDGET.
fn keydb_update_call(
    pinned: Vec<SocketAddr>,
    url: &str,
    ceiling: std::time::Duration,
    idle: std::time::Duration,
) -> Result<ureq::http::Response<ureq::Body>, ureq::Error> {
    guarded_agent_with_timeouts(pinned, std::time::Duration::from_secs(5), ceiling, idle)
        .get(url)
        .config()
        .timeout_global(Some(ceiling))
        .build()
        .call()
}

// How long a KEYDB body may take IN TOTAL once headers are in — sized to a real single-digit-MB
// keydb export, not to KEYDB_MAX_BYTES's defensive 100 MiB DoS cap.
pub(crate) const KEYDB_TRANSFER_BUDGET: std::time::Duration = std::time::Duration::from_secs(120);

/// [`guarded_get`] with an explicit total-transfer budget.
pub(crate) fn guarded_get_within(
    url: &str,
    budget: std::time::Duration,
) -> Result<ureq::http::Response<ureq::Body>, String> {
    let pinned = validate_fetch_url(url)?;
    guarded_agent_with_timeouts(
        pinned,
        std::time::Duration::from_secs(5),
        budget,
        STALL_TIMEOUT,
    )
    .get(url)
    .call()
    // Do NOT embed `e` directly: ureq 2's Display carried the request URL,
    // leaking a token-bearing keydb_url to the system log. ureq 3 is
    // URL-free here, but `BadUri` still prints it — keep masking.
    .map_err(|e| format!("fetch failed: {}", ureq_error_kind(&e)))
}

// ── Connection caps: run() spawns one OS thread per connection and /events holds its thread
// until disconnect, so without a cap a LAN client can pin N threads and exhaust the container.
const MAX_INFLIGHT_HANDLERS: usize = 64;

// Lower than MAX_INFLIGHT_HANDLERS on purpose: the gap is reserved for bodyless requests so
// stalled POSTs can never starve the healthcheck (GET /api/state).
const MAX_INFLIGHT_BODY_HANDLERS: usize = 48;

// The gap is what the healthcheck survives on, checked at compile time since equalising the
// caps cannot even build.
const _: () = assert!(MAX_INFLIGHT_BODY_HANDLERS < MAX_INFLIGHT_HANDLERS);

// Whether a request will make its handler read a body off the socket; read from headers
// tiny_http already parsed, so this is free.
fn carries_body(request: &tiny_http::Request) -> bool {
    match request.body_length() {
        Some(0) => false,
        Some(_) => true,
        // No Content-Length. For a method that never has a body this is just
        // an ordinary GET; for anything else, assume the reader will wait.
        None => !matches!(
            request.method(),
            tiny_http::Method::Get | tiny_http::Method::Head | tiny_http::Method::Options
        ),
    }
}

// Max concurrent SSE (/events) streams; each pins a thread for its whole
// lifetime, so this is the tighter bound.
const MAX_SSE_CLIENTS: usize = 8;

static INFLIGHT_HANDLERS: AtomicUsize = AtomicUsize::new(0);
static SSE_CLIENTS: AtomicUsize = AtomicUsize::new(0);

// RAII admission token for a counted connection slot: decrements its counter
// on drop so the slot frees on any exit path (return, panic-unwind).
// try_acquire returns None when the cap is already reached.
struct ConnGuard(&'static AtomicUsize);

impl ConnGuard {
    fn try_acquire(counter: &'static AtomicUsize, max: usize) -> Option<ConnGuard> {
        // fetch_update gives us a CAS loop that only increments while
        // under the cap, so the count can never exceed `max`.
        let ok = counter
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| {
                if n < max { Some(n + 1) } else { None }
            })
            .is_ok();
        if ok { Some(ConnGuard(counter)) } else { None }
    }
}

impl Drop for ConnGuard {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

#[cfg(test)]
#[allow(clippy::items_after_test_module)]
mod web_tests {
    use super::*;

    // handle_accept_loss refuses (409) while the mux worker owns the
    // dir (.muxing), else a lock-free state.json write could clobber the
    // worker's terminal quarantine and silently drop the operator's Accept.
    #[test]
    fn accept_loss_entry_verdict_gates_on_muxing() {
        assert_eq!(
            accept_loss_entry_verdict(false, false),
            AcceptLossEntry::NoStagingDir,
            "no staging dir on disk → 404"
        );
        assert_eq!(
            accept_loss_entry_verdict(true, true),
            AcceptLossEntry::MuxInProgress,
            "a dir the worker is actively muxing must be refused (409), not reopened"
        );
        assert_eq!(
            accept_loss_entry_verdict(true, false),
            AcceptLossEntry::Proceed,
            "a present, unowned dir proceeds to arm the override"
        );
    }

    // H6: only a real NotFound means "gone"; EACCES/ESTALE must not read as a
    // missing staging dir (the muxer's definitely_absent rule).
    #[cfg(unix)]
    #[test]
    fn accept_loss_entry_unreadable_dir_is_not_gone() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::TempDir::new().unwrap();
        assert_eq!(
            accept_loss_entry_for(&tmp.path().join("missing")),
            AcceptLossEntry::NoStagingDir
        );
        let dangling = tmp.path().join("dangling");
        std::os::unix::fs::symlink(tmp.path().join("nowhere"), &dangling).unwrap();
        assert_eq!(
            accept_loss_entry_for(&dangling),
            AcceptLossEntry::NoStagingDir,
            "a dangling symlink is gone (404), not a present dir"
        );
        let parent = tmp.path().join("locked");
        let dir = parent.join("Disc");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::set_permissions(&parent, std::fs::Permissions::from_mode(0o000)).unwrap();
        let bypass = std::fs::metadata(&dir).is_ok();
        let verdict = accept_loss_entry_for(&dir);
        std::fs::set_permissions(&parent, std::fs::Permissions::from_mode(0o755)).unwrap();
        if bypass {
            eprintln!("skipped: running with permission bypass (root)");
            return;
        }
        assert_eq!(
            verdict,
            AcceptLossEntry::StagingUnreadable,
            "EACCES on the staging dir must not be treated as 'no staging dir'"
        );
        // An unreadable state.json must not read as "not muxing" either.
        let state = dir.join(crate::server::ripper::staging::STATE_FILE);
        std::fs::write(&state, b"{}").unwrap();
        std::fs::set_permissions(&state, std::fs::Permissions::from_mode(0o000)).unwrap();
        let verdict = accept_loss_entry_for(&dir);
        std::fs::set_permissions(&state, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert_eq!(
            verdict,
            AcceptLossEntry::StagingUnreadable,
            "an unreadable state.json must fail closed, not proceed as 'not muxing'"
        );
    }

    // Regression (bug #3): the Mux and Move queues must be mutually
    // exclusive — a disc can never appear in both. Walk a staging dir
    // through the post-mux marker sequence and assert that at each step.
    #[test]
    fn build_queue_views_mutually_exclusive() {
        use std::fs;
        let tmp = tempfile::TempDir::new().unwrap();
        let staging = tmp.path().to_string_lossy().to_string();
        let disc = tmp.path().join("Border_Town");
        fs::create_dir_all(&disc).unwrap();

        let both_contain = |mux: &[String], mv: &[String]| -> bool {
            mux.iter().any(|m| {
                let name = m.replace(" (queued)", "").replace(" (malformed)", "");
                mv.iter().any(|v| v.replace(" (moving)", "") == name)
            })
        };

        // Step 1: fresh hand-off — `.ripped` only. In the Mux queue, not Move.
        crate::server::muxer::write_marker(
            &disc,
            &crate::server::muxer::RippedMarker {
                schema_version: crate::server::muxer::RIPPED_MARKER_SCHEMA,
                iso_path: "/x/Border_Town/Border_Town.iso".into(),
                mapfile_path: "/x/Border_Town/Border_Town.iso.mapfile".into(),
                display_name: "Border Town".into(),
                disc_format: "uhd".into(),
                mkv_filename: "Border_Town.mkv".into(),
                tmdb_title: "Border Town".into(),
                tmdb_year: 2024,
                tmdb_poster: String::new(),
                tmdb_overview: String::new(),
                tmdb_media_type: "movie".into(),
                max_retries: 5,
                abort_on_lost_secs: 0,
                rip_elapsed_secs: 0.0,
                rip_errors: 0,
                rip_lost_video_secs: 0.0,
                rip_last_sector: 0,
                origin_device: "sg0".into(),
                sweep_errors: 0,
                sweep_total_lost_ms: 0.0,
                sweep_main_lost_ms: 0.0,
                sweep_num_bad_ranges: 0,
                sweep_largest_gap_ms: 0.0,
                title_confident: true,
            },
        )
        .unwrap();
        let (mux, mv, _, _) = build_queue_views(&staging);
        assert_eq!(mux.len(), 1, "fresh .ripped must be in the Mux queue");
        assert!(mv.is_empty(), "not yet in the Move queue");
        assert!(!both_contain(&mux, &mv));

        // Step 2: mux in flight — `.muxing` added. Out of the Mux queue
        // (shown as the live `_mux` device), still not in Move.
        crate::server::ripper::staging::write_muxing_marker(&disc);
        let (mux, mv, _, _) = build_queue_views(&staging);
        assert!(
            mux.is_empty(),
            "an actively-muxing dir leaves the queued list"
        );
        assert!(mv.is_empty());
        assert!(!both_contain(&mux, &mv));
        crate::server::ripper::staging::clear_muxing_marker(&disc);

        // Step 3: mux done — hand-off to `state: Done` (mover hand-off),
        // `.completed` not yet written. THIS is the double-listing bug
        // window: it must be in the Move queue ONLY.
        crate::server::ripper::staging::mark_handoff(&disc, true, |_s| {}).unwrap();
        let (mux, mv, _, _) = build_queue_views(&staging);
        assert!(
            mux.is_empty(),
            "a dir in the Move queue (.done) must not also be (queued) in the Mux queue, got {mux:?}"
        );
        assert_eq!(mv.len(), 1, "must be in the Move queue");
        assert!(
            !both_contain(&mux, &mv),
            "BUG #3: a disc must never appear in both the mux and move queues"
        );

        // Step 4: terminal `.completed` lands — still Move-only, never both.
        crate::server::ripper::staging::write_completed_marker(&disc);
        let (mux, mv, _, _) = build_queue_views(&staging);
        assert!(mux.is_empty());
        assert_eq!(mv.len(), 1);
        assert!(!both_contain(&mux, &mv));
    }

    // Regression: a moving dir keeps its .done marker, so build_queue_views
    // must exclude ACTIVE_MOVE_DIR by exact basename or it double-renders
    // (old title-based de-dup broke on punctuation like `:`).
    #[test]
    fn build_queue_views_excludes_the_actively_moving_dir() {
        use std::fs;
        // Serialize against every test that touches the global move statics.
        let _g = crate::server::mover::TEST_STATE_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());

        let tmp = tempfile::TempDir::new().unwrap();
        let staging = tmp.path().to_string_lossy().to_string();
        // Two pending moves. On disk the title's colon has been sanitized away
        // (`X-Men: Apocalypse` → `X-Men_Apocalypse`), which is exactly what
        // used to defeat the client-side title match.
        let active = tmp.path().join("X-Men_Apocalypse");
        let other = tmp.path().join("Interstellar");
        fs::create_dir_all(&active).unwrap();
        fs::create_dir_all(&other).unwrap();
        fs::write(active.join(".done"), b"{}").unwrap();
        fs::write(other.join(".done"), b"{}").unwrap();

        // Nothing moving yet: both dirs are queued.
        *crate::server::mover::ACTIVE_MOVE_DIR
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = None;
        let (_, mv, _, _) = build_queue_views(&staging);
        assert_eq!(mv.len(), 2, "with nothing moving, both .done dirs queue");

        // Mark X-Men as the actively-moving dir (by its on-disk basename).
        *crate::server::mover::ACTIVE_MOVE_DIR
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = Some("X-Men_Apocalypse".to_string());
        let (_, mv, _, full) = build_queue_views(&staging);
        assert_eq!(
            mv,
            vec!["Interstellar (moving)".to_string()],
            "the actively-moving dir must be excluded from the queue (shown as bars instead)"
        );
        assert_eq!(
            full, 1,
            "the uncapped count must also exclude the active dir"
        );

        // Clear so no other test observes a stale active dir.
        *crate::server::mover::ACTIVE_MOVE_DIR
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = None;
    }

    // COMPREHENSIVE rip→mux→move→done state-machine coverage: the three
    // views (tile status, Mux queue, Move queue) must stay consistent
    // across every marker transition with multiple discs in staging.

    /// Build a schema-valid `.ripped` marker for `display_name` whose
    /// `origin_device` is `origin`. Keeps the lifecycle tests terse.
    fn ripped_marker_for(display_name: &str, origin: &str) -> crate::server::muxer::RippedMarker {
        let safe = display_name.replace(' ', "_");
        crate::server::muxer::RippedMarker {
            schema_version: crate::server::muxer::RIPPED_MARKER_SCHEMA,
            iso_path: format!("/x/{safe}/{safe}.iso"),
            mapfile_path: format!("/x/{safe}/{safe}.iso.mapfile"),
            display_name: display_name.into(),
            disc_format: "uhd".into(),
            mkv_filename: format!("{safe}.mkv"),
            tmdb_title: display_name.into(),
            tmdb_year: 2024,
            tmdb_poster: String::new(),
            tmdb_overview: String::new(),
            tmdb_media_type: "movie".into(),
            max_retries: 5,
            abort_on_lost_secs: 0,
            rip_elapsed_secs: 0.0,
            rip_errors: 0,
            rip_lost_video_secs: 0.0,
            rip_last_sector: 0,
            origin_device: origin.into(),
            sweep_errors: 0,
            sweep_total_lost_ms: 0.0,
            sweep_main_lost_ms: 0.0,
            sweep_num_bad_ranges: 0,
            sweep_largest_gap_ms: 0.0,
            title_confident: true,
        }
    }

    /// Does `name` appear in BOTH queues at once? (Strips the trailing
    /// status suffixes so `"X (queued)"` and `"X (moving)"` compare equal.)
    fn in_both_queues(mux: &[String], mv: &[String]) -> bool {
        let strip = |s: &str| -> String {
            s.replace(" (queued)", "")
                .replace(" (malformed)", "")
                .replace(" (moving)", "")
        };
        mux.iter().any(|m| mv.iter().any(|v| strip(m) == strip(v)))
    }

    // FULL marker lifecycle with device-status folded in: at each step assert
    // which queue(s) the disc is in and the tile status; the disc is never
    // in two queues. Covers .ripped -> .muxing -> .done -> .completed.
    #[test]
    fn full_lifecycle_queue_and_status_consistent() {
        use std::fs;
        let tmp = tempfile::TempDir::new().unwrap();
        let staging = tmp.path().to_string_lossy().to_string();
        let disc = tmp.path().join("Mercy");
        fs::create_dir_all(&disc).unwrap();
        let device = "sg_lifecycle_dev";

        // --- Stage 0: sweep in progress. `.sweeping` marker, tile=ripping.
        crate::server::ripper::staging::write_sweeping_marker(&disc);
        crate::server::ripper::update_state(
            device,
            crate::server::ripper::RipState {
                device: device.to_string(),
                status: "ripping".to_string(),
                disc_name: "Mercy".to_string(),
                ..Default::default()
            },
        );
        let (mux, mv, _, _) = build_queue_views(&staging);
        assert!(
            mux.is_empty() && mv.is_empty(),
            "during sweep: in neither queue"
        );
        assert_eq!(device_status(device), Some("ripping".into()));

        // --- Stage 1: `.ripped` hand-off. The read is DONE: tile=done(100%),
        // disc enters the Mux queue ONLY. (`write_marker` also clears
        // `.sweeping`.)
        crate::server::muxer::write_marker(&disc, &ripped_marker_for("Mercy", device)).unwrap();
        crate::server::ripper::update_state(
            device,
            crate::server::ripper::RipState {
                device: device.to_string(),
                status: "done".to_string(),
                progress_pct: 100,
                disc_name: "Mercy".to_string(),
                output_file: "Mercy.mkv".to_string(),
                ..Default::default()
            },
        );
        let (mux, mv, _, _) = build_queue_views(&staging);
        assert_eq!(mux.len(), 1, ".ripped → Mux queue");
        assert!(mv.is_empty(), "not in Move queue yet");
        assert!(!in_both_queues(&mux, &mv));
        assert_eq!(
            device_status(device),
            Some("done".into()),
            "tile is 'done' the instant the read finishes, even though the mux is pending"
        );

        // --- Stage 2: mux in flight. `.muxing` lock; disc leaves the static
        // Mux queue (it's the live `_mux` device now); tile stays done.
        crate::server::ripper::staging::write_muxing_marker(&disc);
        let (mux, mv, _, _) = build_queue_views(&staging);
        assert!(
            mux.is_empty(),
            "actively-muxing dir leaves the (queued) list"
        );
        assert!(mv.is_empty());
        assert!(!in_both_queues(&mux, &mv));
        assert_eq!(device_status(device), Some("done".into()));
        crate::server::ripper::staging::clear_muxing_marker(&disc);

        // --- Stage 3: mux success. Hand-off to `state: Done` written BEFORE
        // `.completed`. Disc moves to the Move queue ONLY — the
        // double-listing bug window.
        crate::server::ripper::staging::mark_handoff(&disc, true, |_s| {}).unwrap();
        let (mux, mv, _, _) = build_queue_views(&staging);
        assert!(
            mux.is_empty(),
            "a .done dir must NOT still be (queued) in the Mux queue"
        );
        assert_eq!(mv.len(), 1, ".done → Move queue");
        assert!(!in_both_queues(&mux, &mv), "BUG #3: never in both queues");
        assert_eq!(device_status(device), Some("done".into()));

        // --- Stage 4: `.completed` lands (terminal). Still Move-only.
        crate::server::ripper::staging::write_completed_marker(&disc);
        let (mux, mv, _, _) = build_queue_views(&staging);
        assert!(mux.is_empty());
        assert_eq!(
            mv.len(),
            1,
            "still in the Move queue until the mover relocates it"
        );
        assert!(!in_both_queues(&mux, &mv));

        crate::server::ripper::STATE.lock().unwrap().remove(device);
    }

    // LOW-CONFIDENCE lifecycle: the mux writes .review (not .done) for an
    // operator hold. The disc must leave the Mux queue and never appear in
    // both the Mux "(queued)" and Move "(moving)" lists at once.
    #[test]
    fn review_hold_leaves_mux_queue_no_double_listing() {
        use std::fs;
        let tmp = tempfile::TempDir::new().unwrap();
        let staging = tmp.path().to_string_lossy().to_string();
        let disc = tmp.path().join("Held_Title");
        fs::create_dir_all(&disc).unwrap();

        crate::server::muxer::write_marker(&disc, &ripped_marker_for("Held Title", "sg0")).unwrap();
        let (mux, _, _, _) = build_queue_views(&staging);
        assert_eq!(mux.len(), 1, "fresh .ripped is queued for mux");

        // Low-confidence mux success: hand-off to `state: Review` instead of
        // `state: Done`, then `.completed`.
        crate::server::ripper::staging::mark_handoff(&disc, false, |_s| {}).unwrap();
        let (mux, mv, _, _) = build_queue_views(&staging);
        assert!(mux.is_empty(), "a .review dir must leave the Mux queue");
        assert!(
            !in_both_queues(&mux, &mv),
            "never in both queues on the review path"
        );

        crate::server::ripper::staging::write_completed_marker(&disc);
        let (mux, mv, _, _) = build_queue_views(&staging);
        assert!(mux.is_empty());
        assert!(!in_both_queues(&mux, &mv));
    }

    /// FAILURE path: a terminal mux failure writes `.failed` (no `.done`/
    /// `.completed`). The disc must leave BOTH queues, and the device tile
    /// reflects "error".
    #[test]
    fn abort_failed_leaves_both_queues_and_marks_error() {
        use std::fs;
        let tmp = tempfile::TempDir::new().unwrap();
        let staging = tmp.path().to_string_lossy().to_string();
        let disc = tmp.path().join("Lossy_Disc");
        fs::create_dir_all(&disc).unwrap();
        let device = "sg_abort_dev";

        crate::server::muxer::write_marker(&disc, &ripped_marker_for("Lossy Disc", device))
            .unwrap();
        let (mux, _, _, _) = build_queue_views(&staging);
        assert_eq!(mux.len(), 1);

        // A terminal mux failure quarantines: `.failed`, tile=error.
        crate::server::ripper::staging::write_failed_marker(
            &disc,
            "mux finalize failed (unseekable output)",
        );
        crate::server::ripper::update_state(
            device,
            crate::server::ripper::RipState {
                device: device.to_string(),
                status: "error".to_string(),
                disc_name: "Lossy Disc".to_string(),
                last_error: "mux finalize failed (unseekable output)".to_string(),
                ..Default::default()
            },
        );
        let (mux, mv, _, _) = build_queue_views(&staging);
        assert!(mux.is_empty(), ".failed dir must leave the Mux queue");
        assert!(
            mv.is_empty(),
            ".failed dir is NOT in the Move queue (no .done)"
        );
        assert_eq!(device_status(device), Some("error".into()));

        crate::server::ripper::STATE.lock().unwrap().remove(device);
    }

    /// CONCURRENT devices: two drives, each with its own staged job at a
    /// DIFFERENT lifecycle stage, must not cross-contaminate queue
    /// membership or device status. Disc A is mid-mux-queue (`.ripped`);
    /// disc B has finished (`.done` → Move queue).
    #[test]
    fn concurrent_devices_no_cross_contamination() {
        use std::fs;
        let tmp = tempfile::TempDir::new().unwrap();
        let staging = tmp.path().to_string_lossy().to_string();
        let dev_a = "sg_concurrent_a";
        let dev_b = "sg_concurrent_b";

        // Disc A: freshly handed off → Mux queue, tile A = done.
        let disc_a = tmp.path().join("Alpha");
        fs::create_dir_all(&disc_a).unwrap();
        crate::server::muxer::write_marker(&disc_a, &ripped_marker_for("Alpha", dev_a)).unwrap();
        crate::server::ripper::update_state(
            dev_a,
            crate::server::ripper::RipState {
                device: dev_a.to_string(),
                status: "done".to_string(),
                progress_pct: 100,
                disc_name: "Alpha".to_string(),
                ..Default::default()
            },
        );

        // Disc B: mux finished → Move queue, tile B = done.
        let disc_b = tmp.path().join("Beta");
        fs::create_dir_all(&disc_b).unwrap();
        crate::server::muxer::write_marker(&disc_b, &ripped_marker_for("Beta", dev_b)).unwrap();
        crate::server::ripper::staging::mark_handoff(&disc_b, true, |_s| {}).unwrap();
        crate::server::ripper::staging::write_completed_marker(&disc_b);
        crate::server::ripper::update_state(
            dev_b,
            crate::server::ripper::RipState {
                device: dev_b.to_string(),
                status: "done".to_string(),
                progress_pct: 100,
                disc_name: "Beta".to_string(),
                ..Default::default()
            },
        );

        let (mux, mv, _, _) = build_queue_views(&staging);
        // Alpha is in the Mux queue ONLY; Beta in the Move queue ONLY.
        assert!(
            mux.iter().any(|m| m.contains("Alpha")),
            "Alpha must be in the Mux queue"
        );
        assert!(
            !mux.iter().any(|m| m.contains("Beta")),
            "Beta must NOT be in the Mux queue"
        );
        assert!(
            mv.iter().any(|m| m.contains("Beta")),
            "Beta must be in the Move queue"
        );
        assert!(
            !mv.iter().any(|m| m.contains("Alpha")),
            "Alpha must NOT be in the Move queue"
        );
        assert!(
            !in_both_queues(&mux, &mv),
            "neither disc may be in both queues"
        );
        // Each device tile is independent.
        assert_eq!(device_status(dev_a), Some("done".into()));
        assert_eq!(device_status(dev_b), Some("done".into()));

        crate::server::ripper::STATE.lock().unwrap().remove(dev_a);
        crate::server::ripper::STATE.lock().unwrap().remove(dev_b);
    }

    // get_state_json END-TO-END: the serialized live payload (SSE/dashboard
    // source) must never list a disc in both _mux_queue and _move_queue,
    // across multiple discs — all three views derive from one snapshot.
    #[test]
    fn get_state_json_never_double_lists_across_discs() {
        use std::fs;
        let tmp = tempfile::TempDir::new().unwrap();
        let staging = tmp.path().to_string_lossy().to_string();

        // Three discs spanning the lifecycle: Queued/AlsoQueued (.ripped
        // only → Mux queue) and Moving (.done + .completed → Move queue).
        for (name, finished) in [("Queued", false), ("Moving", true), ("AlsoQueued", false)] {
            let d = tmp.path().join(name);
            fs::create_dir_all(&d).unwrap();
            crate::server::muxer::write_marker(&d, &ripped_marker_for(name, "sg0")).unwrap();
            if finished {
                crate::server::ripper::staging::mark_handoff(&d, true, |_s| {}).unwrap();
                crate::server::ripper::staging::write_completed_marker(&d);
            }
        }

        let json = get_state_json(&staging);
        let v: serde_json::Value = serde_json::from_str(&json).expect("state json must parse");
        let to_names = |key: &str| -> Vec<String> {
            v.get(key)
                .and_then(|q| q.as_array())
                .map(|a| {
                    a.iter()
                        .filter_map(|x| x.as_str())
                        .map(|s| {
                            s.replace(" (queued)", "")
                                .replace(" (malformed)", "")
                                .replace(" (moving)", "")
                        })
                        .collect()
                })
                .unwrap_or_default()
        };
        let mux_names = to_names("_mux_queue");
        let move_names = to_names("_move_queue");

        assert!(mux_names.contains(&"Queued".to_string()));
        assert!(mux_names.contains(&"AlsoQueued".to_string()));
        assert!(move_names.contains(&"Moving".to_string()));
        // The cross-queue invariant: no disc in both lists.
        for name in &mux_names {
            assert!(
                !move_names.contains(name),
                "BUG #3 (get_state_json): '{name}' is in BOTH _mux_queue and _move_queue"
            );
        }
    }

    #[test]
    fn queue_view_cache_reuses_within_ttl_and_refreshes_after() {
        // build_queue_views_cached lets concurrent per-client SSE calls share
        // ONE staging-dir scan. Pin: a same-dir call within the TTL reuses the
        // stale scan; a different dir, or the TTL elapsing, forces a re-scan.
        use std::fs;
        let tmp_a = tempfile::TempDir::new().unwrap();
        let staging_a = tmp_a.path().to_string_lossy().to_string();
        let disc1 = tmp_a.path().join("First");
        fs::create_dir_all(&disc1).unwrap();
        crate::server::muxer::write_marker(&disc1, &ripped_marker_for("First", "sg0")).unwrap();

        let (mux1, _, _, _) = build_queue_views_cached(&staging_a);
        assert!(
            mux1.iter().any(|s| s.contains("First")),
            "initial scan must see the pre-existing disc"
        );

        // A different staging dir, scanned right after, must reflect ITS
        // OWN contents (empty), not staging_a's cached entry.
        let tmp_b = tempfile::TempDir::new().unwrap();
        let staging_b = tmp_b.path().to_string_lossy().to_string();
        let (mux_b, _, _, _) = build_queue_views_cached(&staging_b);
        assert!(
            mux_b.is_empty(),
            "a different staging dir must not be served staging_a's cached queue"
        );

        // Add a second disc to staging_a's directory, then immediately
        // re-query staging_a within the TTL window: the cache must still
        // return the STALE (pre-addition) view.
        let disc2 = tmp_a.path().join("Second");
        fs::create_dir_all(&disc2).unwrap();
        crate::server::muxer::write_marker(&disc2, &ripped_marker_for("Second", "sg1")).unwrap();
        let (mux2, _, _, _) = build_queue_views_cached(&staging_a);
        assert!(
            !mux2.iter().any(|s| s.contains("Second")),
            "a call within the TTL must reuse the cached (stale) scan, not re-walk the dir"
        );

        // After the TTL elapses, the next call must re-scan and see the new disc.
        std::thread::sleep(QUEUE_VIEW_CACHE_TTL + std::time::Duration::from_millis(150));
        let (mux3, _, _, _) = build_queue_views_cached(&staging_a);
        assert!(
            mux3.iter().any(|s| s.contains("Second")),
            "after the TTL expires, the next call must re-scan and see the new disc"
        );
    }

    // /api/state (and thus the Dockerfile HEALTHCHECK) must stay responsive
    // while a staging-dir refresh is in flight — a slow read_dir must never
    // park every other caller. 250ms bound trips a blocked reader, not a hang.
    #[test]
    fn queue_view_cache_reader_not_blocked_by_in_flight_scan() {
        use std::time::{Duration, Instant};
        const SCAN_MS: u64 = 2000;
        // The reader serves the cached (stale) view in ~1ms, so any measurable
        // delay means it BLOCKED behind the in-flight scan. Bound at HALF the
        // scan: above CI jitter for a hit, yet a real block still trips it.
        const READER_BOUND_MS: u128 = (SCAN_MS / 2) as u128;

        let tmp = tempfile::TempDir::new().unwrap();
        // Unique fixture path (tempdir) so the process-global probe/cache
        // keyed by staging_dir cannot collide with another test.
        let staging = tmp.path().to_string_lossy().to_string();
        let disc = tmp.path().join("Primed");
        std::fs::create_dir_all(&disc).unwrap();
        crate::server::muxer::write_marker(&disc, &ripped_marker_for("Primed", "sg0")).unwrap();

        // Prime the cache with a fast scan (the steady state on the rig:
        // /api/state has been polled once a second for the container's life).
        let (primed, _, _, _) = build_queue_views_cached(&staging);
        assert!(
            primed.iter().any(|s| s.contains("Primed")),
            "priming scan must see the pre-existing disc"
        );

        // Now make this dir's scan pathologically slow, and let the cached
        // entry age past the TTL so the next caller triggers a refresh.
        queue_scan_probe::arm(&staging, SCAN_MS);
        std::thread::sleep(QUEUE_VIEW_CACHE_TTL + Duration::from_millis(50));

        let s2 = staging.clone();
        let refresher = std::thread::spawn(move || build_queue_views_cached(&s2));

        // Bounded wait until the slow scan is genuinely in flight.
        let spin = Instant::now();
        while queue_scan_probe::scans(&staging) < 1 && spin.elapsed() < Duration::from_millis(500) {
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(
            queue_scan_probe::scans(&staging),
            1,
            "the slow refresh scan never started; test setup is wrong"
        );

        // The healthcheck-equivalent read, concurrent with that scan.
        let t0 = Instant::now();
        let (mux, _, _, _) = build_queue_views_cached(&staging);
        let elapsed = t0.elapsed();
        let _ = refresher.join();

        assert!(
            mux.iter().any(|s| s.contains("Primed")),
            "a reader served during a refresh must still get a usable (stale) queue view"
        );
        assert!(
            elapsed.as_millis() < READER_BOUND_MS,
            "/api/state reader blocked {elapsed:?} behind an in-flight staging scan \
             (bound {READER_BOUND_MS}ms, scan {SCAN_MS}ms) — a stalled scan can stall \
             the Docker healthcheck"
        );
    }

    // Root-cause guard: the Phase-3 prune must NOT evict a stale-but-recent
    // key. Evicting at the sub-second serve TTL destroyed the stale snapshot
    // stale-while-revalidate needs, forcing the next reader to block the scan.
    #[test]
    fn queue_view_prune_keeps_stale_but_recent_key_serveable() {
        use std::time::Duration;

        let tmp_a = tempfile::TempDir::new().unwrap();
        let a = tmp_a.path().to_string_lossy().to_string();
        let disc = tmp_a.path().join("Kept");
        std::fs::create_dir_all(&disc).unwrap();
        crate::server::muxer::write_marker(&disc, &ripped_marker_for("Kept", "sg0")).unwrap();
        let (primed, _, _, _) = build_queue_views_cached(&a);
        assert!(
            primed.iter().any(|s| s.contains("Kept")),
            "priming scan must see the disc"
        );

        // Age A's snapshot past the serve TTL — the threshold the OLD prune
        // (wrongly) used to evict, but well within the RETAIN horizon.
        std::thread::sleep(QUEUE_VIEW_CACHE_TTL + Duration::from_millis(50));

        // A scan of an unrelated dir B runs the Phase-3 prune across the whole
        // map. Under the old TTL-based retain this evicted A; it must not now.
        let tmp_b = tempfile::TempDir::new().unwrap();
        let b = tmp_b.path().to_string_lossy().to_string();
        let _ = build_queue_views_cached(&b);

        let map = QUEUE_VIEW_CACHE.lock().unwrap_or_else(|e| e.into_inner());
        let kept = map
            .get(&a)
            .and_then(|e| e.snapshot.as_ref())
            .map(|s| s.views().0);
        assert!(
            kept.is_some_and(|mux| mux.iter().any(|s| s.contains("Kept"))),
            "the prune evicted a stale-but-recent key's snapshot; \
             stale-while-revalidate is broken and a concurrent reader of that \
             key will block a full staging scan (the healthcheck stall)"
        );
    }

    /// The counterpart guard: fixing the blocking above must NOT turn the
    /// cache into a thundering herd. N concurrent cold callers must produce
    /// ONE scan of the staging dir, not N.
    #[test]
    fn queue_view_cache_single_flights_concurrent_callers() {
        const SCAN_MS: u64 = 300;
        const CALLERS: usize = 8;

        let tmp = tempfile::TempDir::new().unwrap();
        let staging = tmp.path().to_string_lossy().to_string();
        let disc = tmp.path().join("Solo");
        std::fs::create_dir_all(&disc).unwrap();
        crate::server::muxer::write_marker(&disc, &ripped_marker_for("Solo", "sg0")).unwrap();

        // Armed BEFORE the first call: this dir has never been scanned, so
        // every caller below is a cold miss racing every other one.
        queue_scan_probe::arm(&staging, SCAN_MS);

        let handles: Vec<_> = (0..CALLERS)
            .map(|_| {
                let s = staging.clone();
                std::thread::spawn(move || build_queue_views_cached(&s))
            })
            .collect();
        for h in handles {
            let (mux, _, _, _) = h.join().expect("caller thread panicked");
            assert!(
                mux.iter().any(|s| s.contains("Solo")),
                "every concurrent caller must get the real queue view"
            );
        }
        assert_eq!(
            queue_scan_probe::scans(&staging),
            1,
            "{CALLERS} concurrent callers caused {} staging scans; single-flight is broken \
             (thundering herd on the staging dir)",
            queue_scan_probe::scans(&staging)
        );

        // And a further caller inside the TTL window still re-uses it.
        let _ = build_queue_views_cached(&staging);
        assert_eq!(
            queue_scan_probe::scans(&staging),
            1,
            "a caller within the TTL must be served from cache"
        );
    }

    // Run build_queue_views_cached(dir) on a scratch thread, returning None on
    // timeout so a regression surfaces as a FAILED assertion, not a CI hang.
    // The thread is deliberately never joined — a wedged scan is what we simulate.
    fn cached_within(
        dir: &str,
        bound: std::time::Duration,
    ) -> Option<(Vec<String>, Vec<String>, usize, usize)> {
        let (tx, rx) = std::sync::mpsc::channel();
        let d = dir.to_string();
        std::thread::spawn(move || {
            let _ = tx.send(build_queue_views_cached(&d));
        });
        rx.recv_timeout(bound).ok()
    }

    /// Spin until this dir has taken `n` scans, or `bound` elapses.
    fn await_scans(dir: &str, n: usize, bound: std::time::Duration) -> bool {
        let t0 = std::time::Instant::now();
        while queue_scan_probe::scans(dir) < n {
            if t0.elapsed() >= bound {
                return false;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        true
    }

    // A refresher that HANGS (wedged read_dir) must not latch the
    // single-flight marker forever: the old plain-bool marker stayed set for
    // the process lifetime, freezing queue views with no surfaced failure.
    #[test]
    fn queue_view_cache_recovers_from_a_refresher_that_never_returns() {
        use std::time::Duration;
        const WEDGE_MS: u64 = 60_000;
        const DEADLINE_MS: u64 = 300;
        const TAKEOVER_BOUND: Duration = Duration::from_secs(4);

        let tmp = tempfile::TempDir::new().unwrap();
        // Unique to this test: the cache, the probe and STATE are all
        // process-global, so a shared fixture name would be a real race.
        let staging = tmp.path().to_string_lossy().to_string();
        let first = tmp.path().join("WedgeFirst");
        std::fs::create_dir_all(&first).unwrap();
        crate::server::muxer::write_marker(&first, &ripped_marker_for("WedgeFirst", "sg0"))
            .unwrap();

        // Steady state: the cache is warm, as it is on the rig after the
        // first second of /api/state polling.
        let (primed, _, _, _) =
            cached_within(&staging, Duration::from_secs(5)).expect("priming scan must return");
        assert!(
            primed.iter().any(|s| s.contains("WedgeFirst")),
            "priming scan must see the pre-existing disc"
        );

        queue_scan_probe::set_refresh_deadline(&staging, DEADLINE_MS);
        // The mount wedges. Age the entry past the TTL so the next caller
        // owns the refresh, then let that caller disappear into `read_dir`.
        queue_scan_probe::arm(&staging, WEDGE_MS);
        std::thread::sleep(QUEUE_VIEW_CACHE_TTL + Duration::from_millis(50));
        let wedged = staging.clone();
        std::thread::spawn(move || build_queue_views_cached(&wedged));
        assert!(
            await_scans(&staging, 1, Duration::from_secs(2)),
            "the wedged refresh never started; test setup is wrong"
        );

        // Reality moves on underneath the wedged refresher.
        let second = tmp.path().join("WedgeSecond");
        std::fs::create_dir_all(&second).unwrap();
        crate::server::muxer::write_marker(&second, &ripped_marker_for("WedgeSecond", "sg1"))
            .unwrap();
        // ...and the mount comes back for anyone who tries again. The
        // original refresher is still parked in its 60 s `read_dir`.
        queue_scan_probe::arm(&staging, 0);

        // Poll as /api/state does. Once the marker's deadline passes, some
        // caller must take the refresh over and publish the new disc.
        let deadline = std::time::Instant::now() + TAKEOVER_BOUND;
        let mut saw_second = false;
        while std::time::Instant::now() < deadline {
            let (mux, _, _, _) = cached_within(&staging, TAKEOVER_BOUND)
                .expect("a caller blocked past the takeover bound behind a wedged refresh");
            if mux.iter().any(|s| s.contains("WedgeSecond")) {
                saw_second = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        assert!(
            saw_second,
            "the single-flight marker latched on a refresher that never returned: \
             the queue views are frozen at the moment the mount wedged and will \
             stay frozen for the process lifetime"
        );
    }

    // Cold-path counterpart: with nothing to serve and the first scan wedged,
    // callers must neither park forever nor pile up (old valve burned an
    // HTTP worker per waiter every 5s until /api/state 503'd and restarted).
    #[test]
    fn queue_view_cache_cold_callers_neither_park_nor_pile_up_on_a_wedged_scan() {
        use std::time::Duration;
        const WEDGE_MS: u64 = 60_000;
        const COLD_WAIT_MS: u64 = 200;
        const CALLER_BOUND: Duration = Duration::from_secs(3);
        const CALLERS: usize = 6;

        let tmp = tempfile::TempDir::new().unwrap();
        let staging = tmp.path().to_string_lossy().to_string();
        let disc = tmp.path().join("ColdWedge");
        std::fs::create_dir_all(&disc).unwrap();
        crate::server::muxer::write_marker(&disc, &ripped_marker_for("ColdWedge", "sg0")).unwrap();

        // Armed before the first ever call: this key is genuinely cold, so
        // there is no snapshot to fall back on.
        queue_scan_probe::arm(&staging, WEDGE_MS);
        queue_scan_probe::set_cold_wait(&staging, COLD_WAIT_MS);
        let wedged = staging.clone();
        std::thread::spawn(move || build_queue_views_cached(&wedged));
        assert!(
            await_scans(&staging, 1, Duration::from_secs(2)),
            "the wedged cold scan never started; test setup is wrong"
        );

        // Every subsequent caller must come back — degraded is fine, wedged
        // is not. This is the /api/state + --healthcheck path.
        for i in 0..CALLERS {
            assert!(
                cached_within(&staging, CALLER_BOUND).is_some(),
                "cold caller {i} was still parked after {CALLER_BOUND:?} behind a wedged \
                 first scan — /api/state (and the Docker HEALTHCHECK) stalls with it"
            );
        }

        // ...and none of them may launch a scan of its own: that is the
        // accumulation that eats an HTTP worker per giving-up caller.
        assert_eq!(
            queue_scan_probe::scans(&staging),
            1,
            "{CALLERS} cold callers launched {} scans of a wedged staging dir; the cold \
             valve abandoned single-flight, so each one burns an HTTP worker thread and \
             its admission token until /api/state 503s",
            queue_scan_probe::scans(&staging),
        );
    }

    /// Helper: current status string of a device in the global STATE map.
    fn device_status(device: &str) -> Option<String> {
        ripper::STATE
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(device)
            .map(|s| s.status.clone())
    }

    #[test]
    fn keydb_body_under_cap_is_accepted() {
        let body = vec![b'x'; 100];
        let out = read_capped_keydb_body(&body[..], 10 * 1024 * 1024).unwrap();
        assert_eq!(out, body);
    }

    #[test]
    fn keydb_body_exactly_at_cap_is_accepted() {
        // The cap is inclusive: a body of exactly max_bytes must pass (no
        // false-positive on a legitimately cap-sized keydb).
        let cap: u64 = 4096;
        let body = vec![b'x'; cap as usize];
        let out = read_capped_keydb_body(&body[..], cap).unwrap();
        assert_eq!(out.len() as u64, cap);
    }

    #[test]
    fn keydb_body_over_cap_is_rejected() {
        // Regression (finding 2): a body one byte past the cap must be
        // detected as TooLarge, not silently truncated to the cap.
        let cap: u64 = 4096;
        let body = vec![b'x'; cap as usize + 1];
        let err = read_capped_keydb_body(&body[..], cap).unwrap_err();
        assert_eq!(err, KeydbReadError::TooLarge);
    }

    #[test]
    fn device_name_accepts_cross_os_keys() {
        // Linux sg, macOS disk, Windows CdRom — the basenames list_drives yields.
        assert!(is_valid_device_name("sg0"));
        assert!(is_valid_device_name("sg4"));
        assert!(is_valid_device_name("sg15"));
        assert!(is_valid_device_name("disk6")); // macOS
        assert!(is_valid_device_name("CdRom0")); // Windows
    }

    #[test]
    fn device_name_rejects_path_traversal_and_typos() {
        // The exact bug that created the phantom "sg4/stop" tab. This is a
        // path-safety boundary, not a drive-existence check — an unknown
        // well-formed name is accepted as *format* and fails downstream.
        assert!(!is_valid_device_name("sg4/stop"));
        assert!(!is_valid_device_name("sg4/verify"));
        assert!(!is_valid_device_name("../etc/passwd"));
        assert!(!is_valid_device_name("sg4 ")); // trailing space
        assert!(!is_valid_device_name("sg")); // too short (< 3)
        assert!(!is_valid_device_name(""));
        assert!(!is_valid_device_name("a/b"));
        assert!(!is_valid_device_name("..")); // dots are separators
    }

    #[test]
    fn poster_url_validation() {
        assert!(is_valid_poster_url(
            "https://image.tmdb.org/t/p/w500/abc.jpg"
        ));
        assert!(is_valid_poster_url("http://example.com/poster.png"));
        // Wrong scheme.
        assert!(!is_valid_poster_url("javascript:alert(1)"));
        assert!(!is_valid_poster_url("ftp://example.com/x.jpg"));
        assert!(!is_valid_poster_url("//example.com/x.jpg"));
        // Attribute-breakout / control chars.
        assert!(!is_valid_poster_url("https://example.com/\"><script>"));
        assert!(!is_valid_poster_url("https://example.com/x'onerror=1"));
        assert!(!is_valid_poster_url("https://example.com/a\nb"));
    }

    // The dashboard's esc() must HTML-escape all five sensitive characters
    // (& < > " '); a textContent/innerHTML round-trip (prior impl) leaves
    // " and ' unescaped. Also assert the shipped JS carries the quote escapes.
    #[test]
    fn dashboard_esc_escapes_all_five() {
        fn esc(s: &str) -> String {
            s.replace('&', "&amp;")
                .replace('<', "&lt;")
                .replace('>', "&gt;")
                .replace('"', "&quot;")
                .replace('\'', "&#39;")
        }
        assert_eq!(esc("\"x<>&'"), "&quot;x&lt;&gt;&amp;&#39;");
        // The shipped JS must escape quotes and apostrophes, not just <>&.
        assert!(DASHBOARD_HTML.contains(r#"replace(/"/g,'&quot;')"#));
        assert!(DASHBOARD_HTML.contains(r"replace(/'/g,'&#39;')"));
    }

    // Error text reaches the dashboard via innerHTML — bare URLs need
    // escLinks() (built on esc()) to become anchors while staying safe.
    #[test]
    fn dashboard_error_text_linkifies_urls() {
        assert!(DASHBOARD_HTML.contains("function escLinks(s){"));
        // The step detail line ("Error — <message>").
        assert!(DASHBOARD_HTML.contains(r"'+escLinks(detail)}"));
        // The red error banner.
        assert!(DASHBOARD_HTML.contains("escLinks(s.last_error)"));
        // Only https is linkified — no javascript:/data: anchors.
        assert!(DASHBOARD_HTML.contains(r#"/https:\/\/[^\s<>"']+/g"#));
    }

    // H7: executes the shipped esc/escLinks JS under node (skipped when node
    // is absent). A URL next to a quote must not swallow the escaped entity.
    #[test]
    fn dashboard_esc_links_executes_correctly_under_node() {
        if std::process::Command::new("node")
            .arg("--version")
            .output()
            .is_err()
        {
            // Skip for local dev only; CI must actually execute the JS.
            assert!(
                std::env::var_os("CI").is_none(),
                "node not found on PATH but CI is set: the escLinks test must run on CI"
            );
            eprintln!("skipped: node not found on PATH");
            return;
        }
        let esc_at = DASHBOARD_HTML.find("function esc(s){").unwrap();
        let esc_end = esc_at + DASHBOARD_HTML[esc_at..].find('\n').unwrap();
        let links_at = DASHBOARD_HTML.find("function escLinks(s){").unwrap();
        let links_end = links_at + DASHBOARD_HTML[links_at..].find("\nfunction ").unwrap();
        let a = |u: &str| {
            format!(
                r#"<a href="{u}" target="_blank" rel="noopener noreferrer" style="color:inherit">{u}</a>"#
            )
        };
        let cases: Vec<(&str, String)> = vec![
            (
                "see 'https://example.com/x' now",
                format!("see &#39;{}&#39; now", a("https://example.com/x")),
            ),
            (
                r#"at "https://example.com/y"."#,
                format!("at &quot;{}&quot;.", a("https://example.com/y")),
            ),
            (
                "https://example.com/q?a=1&b=2.",
                format!("{}.", a("https://example.com/q?a=1&amp;b=2")),
            ),
            (
                "<b>https://example.com/z</b>",
                format!("&lt;b&gt;{}&lt;/b&gt;", a("https://example.com/z")),
            ),
            (
                "no link & 'quotes'",
                "no link &amp; &#39;quotes&#39;".into(),
            ),
        ];
        let inputs: Vec<&str> = cases.iter().map(|c| c.0).collect();
        let script = format!(
            "{}\n{}\nconsole.log(JSON.stringify({}.map(escLinks)));",
            &DASHBOARD_HTML[esc_at..esc_end],
            &DASHBOARD_HTML[links_at..links_end],
            serde_json::to_string(&inputs).unwrap()
        );
        let out = std::process::Command::new("node")
            .arg("-e")
            .arg(&script)
            .output()
            .expect("run node");
        assert!(
            out.status.success(),
            "node failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        let got: Vec<String> = serde_json::from_slice(&out.stdout).expect("node JSON output");
        for ((input, want), got) in cases.iter().zip(&got) {
            assert_eq!(got, want, "escLinks({input:?})");
        }
    }

    #[test]
    fn settings_get_redacts_secrets() {
        let c = Config {
            tmdb_api_key: "real-tmdb-key".into(),
            keyserver_secret: "real-bearer-token".into(),
            ..Config::default()
        };
        let json: serde_json::Value = serde_json::from_str(&settings_json_redacted(&c)).unwrap();
        assert_eq!(json["tmdb_api_key"], SECRET_SENTINEL);
        assert_eq!(json["keyserver_secret"], SECRET_SENTINEL);
        // An empty secret stays empty (no sentinel) so the UI shows a blank field.
        let json2: serde_json::Value =
            serde_json::from_str(&settings_json_redacted(&Config::default())).unwrap();
        assert_eq!(json2["tmdb_api_key"], "");
    }

    #[test]
    fn settings_get_masks_keyserver_url_token_in_path() {
        // keyserver_url may carry an auth token in the path
        // (e.g. https://keys.example.com/mytoken/decode). GET must mask the
        // path but keep the origin so the operator can identify the server.
        let c = Config {
            keyserver_url: "https://keys.example.com/mysecrettoken/decode".into(),
            keydb_url: "https://keydb.example.com/authtoken/keydb.zip".into(),
            ..Config::default()
        };
        let json: serde_json::Value = serde_json::from_str(&settings_json_redacted(&c)).unwrap();
        // Origin preserved, token-bearing path replaced with sentinel.
        assert_eq!(json["keyserver_url"], "https://keys.example.com/********");
        assert_eq!(json["keydb_url"], "https://keydb.example.com/********");
        // Tokens must not appear in the redacted output.
        assert!(
            !json["keyserver_url"]
                .as_str()
                .unwrap()
                .contains("mysecrettoken")
        );
        assert!(!json["keydb_url"].as_str().unwrap().contains("authtoken"));
        // Empty URLs stay empty (no sentinel so the UI shows a blank field).
        let json2: serde_json::Value =
            serde_json::from_str(&settings_json_redacted(&Config::default())).unwrap();
        assert_eq!(json2["keyserver_url"], "");
        assert_eq!(json2["keydb_url"], "");
    }

    // handle_settings_post must DROP the config write guard before saving:
    // config::save's fs::write+rename can hang on NFS, blocking every
    // concurrent cfg.read() (the 0.20.8 lock stall) — pinned against source.
    #[test]
    fn settings_post_saves_outside_the_config_write_guard() {
        let src = crate::server::util::source_lf(include_str!("web.rs"));
        // Leading newline so this does not match the literal on this line.
        let start = src
            .find("\nfn handle_settings_post(")
            .expect("web.rs must define handle_settings_post");
        let body = &src[start..];

        // The mutation window: a `snapshot` bound from a block that takes
        // `cfg.write()`, closing at the block's `};`.
        let snap = body
            .find("let snapshot: Config = {")
            .expect("handle_settings_post must snapshot the config out of the guard");
        assert!(
            body[snap..].starts_with("let snapshot: Config = {")
                && body[snap..]
                    .find("cfg.write()")
                    .is_some_and(|w| w < body[snap..].find("\n    };").unwrap_or(usize::MAX)),
            "the write guard must be taken INSIDE the snapshot block"
        );
        let guard_end = snap
            + body[snap..]
                .find("\n    };")
                .expect("the snapshot block must close at function-body indentation");

        let save = guard_end
            + body[guard_end..]
                .find("config::save_coalesced(snapshot, save_gen)")
                .expect("handle_settings_post must queue the snapshot with its generation");

        // Nothing may re-take the write guard between the snapshot block and
        // the save — that is the whole ordering.
        assert!(
            !body[guard_end..save].contains("cfg.write()"),
            "the config write guard must not be held across config::save; \
             re-taking it before the save reintroduces the 0.20.8 lock stall"
        );
        // H9: the generation is allocated INSIDE the guard, after the write
        // lock, so save order matches in-memory mutation order.
        let write_at = snap + body[snap..].find("cfg.write()").unwrap_or(usize::MAX);
        let gen_at = body[snap..guard_end]
            .find("save_gen = config::next_save_generation();")
            .map(|i| snap + i)
            .expect("save_gen must be allocated inside the snapshot (write-guard) block");
        assert!(
            gen_at > write_at,
            "save_gen must be taken after cfg.write()"
        );
        let fn_end = body[1..].find("\nfn ").map_or(body.len(), |i| i + 1);
        assert_eq!(
            body[..fn_end].matches("next_save_generation()").count(),
            1,
            "exactly one generation allocation in handle_settings_post"
        );
        // Awaited with a deadline, never inline-blocking on the save.
        assert!(
            body[save..].contains("rx.recv_timeout("),
            "the handler must await the queued save with a deadline"
        );
    }

    // NOTE: the keyserver_url sentinel round-trip is now tested
    // executing-style by `http::settings_post_masked_keyserver_url_preserves_stored`,
    // driving the real handler via a live server, not an inline reimplementation.

    /// A stored webhook entry that fires on every stage — the common case in
    /// these tests, which predate per-stage flags and care only about URL
    /// masking/resolution.
    fn we(url: &str) -> WebhookEntry {
        WebhookEntry {
            url: url.to_string(),
            post_rip: true,
            post_mux: true,
            post_move: true,
        }
    }

    /// An incoming (POST-side) webhook with all flags set — mirrors what the
    /// UI sends for a fire-on-every-stage hook.
    fn inc(url: &str) -> IncomingWebhook {
        IncomingWebhook {
            url: url.to_string(),
            post_rip: true,
            post_mux: true,
            post_move: true,
        }
    }

    /// Just the resolved URLs, for asserting URL resolution independently of
    /// the flags (which these tests carry through unchanged).
    fn urls(entries: &[WebhookEntry]) -> Vec<String> {
        entries.iter().map(|e| e.url.clone()).collect()
    }

    #[test]
    fn settings_get_masks_webhook_token_keeps_origin() {
        // Webhook URLs embed bearer tokens (Discord/Slack/Jellyfin) in the
        // path, so a GET must mask the token — but keep the origin visible so
        // the operator can tell which hook is which.
        let c = Config {
            webhook_urls: vec![
                we("https://discord.com/api/webhooks/123/secrettoken"),
                we(""),
                we("https://hooks.slack.com/services/AAA/BBB/cccsecret"),
            ],
            ..Config::default()
        };
        let json: serde_json::Value = serde_json::from_str(&settings_json_redacted(&c)).unwrap();
        let arr = json["webhook_urls"].as_array().unwrap();
        // Each entry serializes as an object; only the `url` is masked and it
        // carries a stable per-entry index (#<pos>) so two same-origin hooks
        // round-trip unambiguously (#8). The flags pass through untouched.
        assert_eq!(arr[0]["url"], "https://discord.com/********#0");
        assert_eq!(arr[0]["post_rip"], true);
        assert_eq!(arr[0]["post_mux"], true);
        assert_eq!(arr[0]["post_move"], true);
        // Empty entry stays empty (no sentinel) so the UI shows a blank row.
        assert_eq!(arr[1]["url"], "");
        assert_eq!(arr[2]["url"], "https://hooks.slack.com/********#2");
        // The masked form must NOT leak the token.
        assert!(!arr[0]["url"].as_str().unwrap().contains("secrettoken"));
        assert!(!arr[2]["url"].as_str().unwrap().contains("cccsecret"));
    }

    #[test]
    fn settings_get_redacts_keydb_path_to_filename() {
        // keydb_path is an absolute container path (mount layout, username);
        // GET must strip it to the bare filename so a LAN client can't
        // learn the container's filesystem layout.
        let c = Config {
            keydb_path: Some("/data/keys/subdir/KEYDB.cfg".into()),
            ..Config::default()
        };
        let json: serde_json::Value = serde_json::from_str(&settings_json_redacted(&c)).unwrap();
        assert_eq!(json["keydb_path"], "KEYDB.cfg");
        assert!(
            !json["keydb_path"].as_str().unwrap().contains('/'),
            "redacted keydb_path must not leak any directory component"
        );
        // No keydb_path set (None) — field passes through untouched (null),
        // no panic, nothing to redact.
        let json_none: serde_json::Value =
            serde_json::from_str(&settings_json_redacted(&Config::default())).unwrap();
        assert!(json_none["keydb_path"].is_null());
        // An explicitly empty string is left alone (not redacted into "" ->
        // something else), matching the other secret fields' "empty stays
        // empty" convention.
        let c_empty = Config {
            keydb_path: Some(String::new()),
            ..Config::default()
        };
        let json_empty: serde_json::Value =
            serde_json::from_str(&settings_json_redacted(&c_empty)).unwrap();
        assert_eq!(json_empty["keydb_path"], "");
    }

    // Operator-facing diagnostic (issue #46): GET /api/settings surfaces the
    // ACTUAL resolved keydb path + present/absent status so the UI shows exactly
    // where autorip reads keys — the missing signal that made #46 undiagnosable.
    #[test]
    fn settings_get_surfaces_resolved_keydb_path_and_status() {
        // Present: canonical file exists at the resolved path.
        let tmp = tempfile::tempdir().unwrap();
        let cfg = Config {
            autorip_dir: tmp.path().to_string_lossy().into_owned(),
            keydb_path: None,
            ..Config::default()
        };
        let expected = tmp.path().join("keydb.cfg");
        std::fs::write(&expected, "x").unwrap();
        let json: serde_json::Value = serde_json::from_str(&settings_json_redacted(&cfg)).unwrap();
        let resolved = json["keydb_resolved"].as_str().unwrap();
        assert!(
            resolved.contains(&expected.display().to_string()),
            "resolved path must be shown for operator diagnosis: {resolved}"
        );
        assert!(resolved.contains("file present"));

        // Absent: an explicit path with no file → NOT FOUND status.
        let cfg2 = Config {
            keydb_path: Some("/no/such/dir/keydb.cfg".into()),
            ..Config::default()
        };
        let json2: serde_json::Value =
            serde_json::from_str(&settings_json_redacted(&cfg2)).unwrap();
        assert!(
            json2["keydb_resolved"]
                .as_str()
                .unwrap()
                .contains("NOT FOUND")
        );
    }

    #[test]
    fn mask_webhook_url_variants() {
        assert_eq!(
            mask_webhook_url("https://discord.com/api/webhooks/1/tok"),
            "https://discord.com/********"
        );
        // Host with port.
        assert_eq!(
            mask_webhook_url("http://jellyfin.example:8096/webhook/abc"),
            "http://jellyfin.example:8096/********"
        );
        // Bare origin, no path → still origin/sentinel.
        assert_eq!(
            mask_webhook_url("https://example.com"),
            "https://example.com/********"
        );
        // No scheme → fully masked (nothing identifiable to keep).
        assert_eq!(mask_webhook_url("not-a-url"), SECRET_SENTINEL);
    }

    #[test]
    fn mask_webhook_url_strips_query_string_token() {
        // Token in query string with no path slash — must not appear in output.
        assert_eq!(
            mask_webhook_url("https://hooks.example.com?token=SUPERSECRET"),
            "https://hooks.example.com/********"
        );
        // Fragment-only (no path) — similarly stripped.
        assert_eq!(
            mask_webhook_url("https://hooks.example.com#frag"),
            "https://hooks.example.com/********"
        );
    }

    #[test]
    fn mask_webhook_url_strips_basic_auth_userinfo() {
        // user:pass@host must NOT leak into the masked value returned to the
        // client. Only scheme://host[:port] survives.
        assert_eq!(
            mask_webhook_url("https://user:pass@host/x"),
            "https://host/********"
        );
        // Userinfo + explicit port.
        assert_eq!(
            mask_webhook_url("https://user:pass@host:8443/webhook/tok"),
            "https://host:8443/********"
        );
        // user-only (no colon) userinfo also stripped.
        assert_eq!(
            mask_webhook_url("http://alice@example.com/hook"),
            "http://example.com/********"
        );
        // An '@' only inside the path (no userinfo in authority) is untouched.
        assert_eq!(
            mask_webhook_url("https://example.com/a@b/c"),
            "https://example.com/********"
        );
        // A bare-origin URL with userinfo (no path) is still stripped.
        assert_eq!(
            mask_webhook_url("https://user:pass@example.com"),
            "https://example.com/********"
        );
    }

    #[test]
    fn is_masked_webhook_recognizes_only_real_placeholders() {
        // Bare sentinel (mask_webhook_url's own output, e.g. no identifiable
        // scheme).
        assert!(is_masked_webhook(SECRET_SENTINEL));
        // Indexed placeholder form produced by mask_webhook_url_indexed.
        assert!(is_masked_webhook(&format!(
            "https://discord.com/{SECRET_SENTINEL}#1"
        )));
        // Index 0 is still a valid, non-empty all-digit index.
        assert!(is_masked_webhook(&format!(
            "https://discord.com/{SECRET_SENTINEL}#0"
        )));
        // Empty index after '#' must NOT be treated as masked.
        assert!(!is_masked_webhook(&format!(
            "https://discord.com/{SECRET_SENTINEL}#"
        )));
        // Non-digit index must NOT be treated as masked.
        assert!(!is_masked_webhook(&format!(
            "https://discord.com/{SECRET_SENTINEL}#abc"
        )));
        // A hostile URL merely ending in "#<digits>" must NOT be misclassified
        // as a masked placeholder — the SSRF bypass an &&->|| mutation in
        // is_masked_webhook would open up (a metadata URL + `#1` fragment).
        assert!(!is_masked_webhook("http://169.254.169.254/x#1"));
        // No '#' at all, and no sentinel — not masked.
        assert!(!is_masked_webhook("http://example.com/hook/realtoken"));
    }

    #[test]
    fn webhook_sentinel_filter_uses_ends_with() {
        // A URL that CONTAINS but does not END WITH the sentinel must NOT be
        // skipped by the SSRF-validation filter — it could be an attacker URL
        // crafted to embed the sentinel in a path segment.
        let sentinel = SECRET_SENTINEL;
        let tricky = format!("https://evil.com/{}@attacker.com/path", sentinel);
        // ends_with check: this does not end with the sentinel, so it is NOT
        // filtered (it would be validated / rejected by validate_fetch_url).
        assert!(!tricky.ends_with(sentinel));
        // The masked form DOES end with the sentinel and IS filtered.
        let masked = format!("https://discord.com/{}", sentinel);
        assert!(masked.ends_with(sentinel));
    }

    // A genuine URL that merely EMBEDS the sentinel is not masked, so the
    // resolver must take it verbatim. Using `contains` instead rejected such
    // a URL 400 as "ambiguous masked entry", killing settings save entirely.
    #[test]
    fn a_url_that_only_embeds_the_sentinel_is_saved_verbatim() {
        // Not masked by the strict predicate — the filter validates it.
        let embedded = format!("https://example.com/hook/{SECRET_SENTINEL}/tail");
        assert!(
            !is_masked_webhook(&embedded),
            "fixture must be a NON-masked URL for this test to mean anything"
        );

        let existing = vec![we("https://discord.com/api/webhooks/1/aaa")];
        let resolved = resolve_webhook_entries(&[inc(&embedded)], &existing)
            .expect("a genuine URL must not be rejected as an ambiguous placeholder");
        assert_eq!(
            urls(&resolved),
            vec![embedded],
            "a non-masked entry is taken verbatim"
        );
    }

    #[test]
    fn webhook_post_sentinel_preserves_stored_url() {
        // A GET→POST round-trip of the redacted form must NOT wipe the
        // token-bearing stored URL. A masked placeholder resolves back to its
        // stored secret by origin; a real entry replaces; an empty entry drops.
        let existing = vec![
            we("https://discord.com/api/webhooks/1/aaa"),
            we("https://hooks.slack.com/services/x/y/zzz"),
        ];
        let incoming = [
            inc("https://discord.com/********"), // masked → keep discord secret
            inc("https://example.com/new-hook"), // changed → replace
        ];
        let resolved = resolve_webhook_entries(&incoming, &existing).unwrap();
        assert_eq!(resolved.len(), 2);
        assert_eq!(resolved[0].url, "https://discord.com/api/webhooks/1/aaa");
        assert_eq!(resolved[1].url, "https://example.com/new-hook");
    }

    #[test]
    fn webhook_post_masked_resolves_by_origin_not_position() {
        // HIGH regression: the UI reorders masked rows to [slack, discord].
        // Resolving BY POSITION would bind slack's row to discord's secret —
        // a silent confusion bug. By origin, each resolves to its own secret.
        let existing = vec![
            we("https://discord.com/api/webhooks/1/secretA"),
            we("https://hooks.slack.com/services/x/y/secretB"),
        ];
        // Reordered: slack first, discord second (each still masked).
        let reordered = [
            inc("https://hooks.slack.com/********"),
            inc("https://discord.com/********"),
        ];
        let resolved = resolve_webhook_entries(&reordered, &existing).unwrap();
        assert_eq!(
            urls(&resolved),
            vec![
                "https://hooks.slack.com/services/x/y/secretB".to_string(),
                "https://discord.com/api/webhooks/1/secretA".to_string(),
            ],
            "each masked entry must carry its own origin's secret, not the other's"
        );

        // Deleting the discord row and keeping only the (masked) slack row must
        // still resolve slack correctly — never to discord's secret.
        let only_slack = [inc("https://hooks.slack.com/********")];
        let resolved = resolve_webhook_entries(&only_slack, &existing).unwrap();
        assert_eq!(
            urls(&resolved),
            vec!["https://hooks.slack.com/services/x/y/secretB".to_string()]
        );
    }

    #[test]
    fn webhook_post_masked_unresolvable_origin_is_rejected() {
        // A masked entry whose origin matches NO stored URL (the referenced row
        // was deleted) is ambiguous — reject rather than guess. Likewise when
        // two stored hooks share an origin (>1 match).
        let existing = vec![we("https://discord.com/api/webhooks/1/aaa")];
        // Masked slack origin has no stored counterpart → Err.
        let orphan = [inc("https://hooks.slack.com/********")];
        assert!(resolve_webhook_entries(&orphan, &existing).is_err());

        // Two stored discord hooks share an origin → a masked discord entry is
        // ambiguous (>1 match) → Err.
        let two_discord = vec![
            we("https://discord.com/api/webhooks/1/aaa"),
            we("https://discord.com/api/webhooks/2/bbb"),
        ];
        let masked = [inc("https://discord.com/********")];
        assert!(resolve_webhook_entries(&masked, &two_discord).is_err());
    }

    #[test]
    fn webhook_two_same_origin_round_trip_by_index() {
        // Regression (#8): same-origin webhooks used to mask to the SAME
        // placeholder, making a GET→POST round-trip ambiguous and the save
        // permanently rejected. A stable per-entry index fixes that.
        let existing = vec![
            we("https://discord.com/api/webhooks/1/secretA"),
            we("https://discord.com/api/webhooks/2/secretB"),
        ];
        // Exactly what GET /api/settings now emits.
        let masked0 = mask_webhook_url_indexed(&existing[0].url, 0);
        let masked1 = mask_webhook_url_indexed(&existing[1].url, 1);
        assert_ne!(masked0, masked1, "same-origin masks must differ by index");

        let incoming = [inc(&masked0), inc(&masked1)];
        let resolved = resolve_webhook_entries(&incoming, &existing).unwrap();
        assert_eq!(
            urls(&resolved),
            vec![
                "https://discord.com/api/webhooks/1/secretA".to_string(),
                "https://discord.com/api/webhooks/2/secretB".to_string(),
            ],
            "each indexed mask must resolve to its own stored secret"
        );

        // A stale index whose origin mask no longer matches must be rejected,
        // not silently bound to the wrong secret.
        let stale = [inc(&mask_webhook_url_indexed("https://discord.com/x", 5))];
        assert!(resolve_webhook_entries(&stale, &existing).is_err());
    }

    #[test]
    fn resolve_webhook_entries_carries_flags_through_masking() {
        // Per-event flags come from the INCOMING request, never the stored
        // entry — resolving a masked URL must not restore its old flags.
        // The client re-saves as move-only; that intent must win.
        let existing = vec![WebhookEntry {
            url: "https://discord.com/api/webhooks/1/secretA".into(),
            post_rip: true,
            post_mux: true,
            post_move: true,
        }];
        let masked = mask_webhook_url_indexed(&existing[0].url, 0);
        let incoming = [IncomingWebhook {
            url: masked,
            post_rip: false,
            post_mux: false,
            post_move: true,
        }];
        let resolved = resolve_webhook_entries(&incoming, &existing).unwrap();
        assert_eq!(
            resolved,
            vec![WebhookEntry {
                url: "https://discord.com/api/webhooks/1/secretA".into(),
                post_rip: false,
                post_mux: false,
                post_move: true,
            }],
            "URL resolves to the stored secret but the flags follow the new request"
        );
    }

    #[test]
    fn port_range_validation_rejects_out_of_range() {
        // handle_settings_post validates the parsed port BEFORE taking the
        // Config write guard, so a bad value (e.g. 70000, truncating to 4464
        // as u16) can't leave a partial in-memory mutation behind.
        let ok = |v: u64| (1..=65535).contains(&v);
        assert!(!ok(0), "0 is not a valid bind port");
        assert!(
            !ok(70000),
            "70000 must be rejected (would truncate to 4464)"
        );
        assert!(!ok(65536), "65536 overflows u16");
        assert!(ok(1));
        assert!(ok(8080));
        assert!(ok(65535));
    }

    // ── Cross-origin (CSRF defense-in-depth) ───────────────────────────

    #[test]
    fn cross_origin_post_rejected_when_origin_host_differs() {
        // A browser on the LAN forging a POST carries an Origin header
        // whose host won't match our Host header → reject.
        assert!(is_cross_origin(
            Some("http://evil.example.com"),
            Some("autorip.test")
        ));
        // Referer fallback host mismatch is likewise rejected (the request
        // helper falls back to Referer when Origin is absent).
        assert!(is_cross_origin(
            Some("http://evil.example.com/page"),
            Some("autorip.test")
        ));
    }

    #[test]
    fn cross_origin_post_allowed_when_origin_absent_or_same() {
        // curl / monitoring scripts send no Origin → allow.
        assert!(!is_cross_origin(None, Some("autorip.test")));
        // Empty Origin → allow.
        assert!(!is_cross_origin(Some(""), Some("autorip.test")));
        // Same host (scheme/path stripped, case-insensitive) → allow.
        assert!(!is_cross_origin(
            Some("http://autorip.test"),
            Some("autorip.test")
        ));
        assert!(!is_cross_origin(
            Some("http://Host.Test:8080/x"),
            Some("host.test:8080")
        ));
        // No Host header to compare against → can't prove cross-origin, allow.
        assert!(!is_cross_origin(Some("http://evil.example.com"), None));
    }

    #[test]
    fn cross_origin_default_port_normalization() {
        // Origin omits the default port; Host carries it explicitly. These
        // are the SAME origin and must NOT be rejected. (The pre-fix exact
        // string compare 403'd these.)
        assert!(!is_cross_origin(
            Some("http://autorip.test"),
            Some("autorip.test:80")
        ));
        assert!(!is_cross_origin(
            Some("https://autorip.test"),
            Some("autorip.test:443")
        ));
        // Inverse: Origin carries the default port, Host omits it.
        assert!(!is_cross_origin(
            Some("http://autorip.test:80"),
            Some("autorip.test")
        ));
        // IPv6 literal, default-port both sides.
        assert!(!is_cross_origin(Some("http://[::1]"), Some("[::1]:80")));
        // A genuinely different port is still cross-origin.
        assert!(is_cross_origin(
            Some("http://autorip.test:8080"),
            Some("autorip.test:9090")
        ));
        // https default (443) must not collapse onto http default (80):
        // an https Origin compared against a Host carrying :80 is a real
        // mismatch.
        assert!(is_cross_origin(
            Some("https://autorip.test"),
            Some("autorip.test:80")
        ));
    }

    /// Edge branches of the authority normaliser the higher-level cross-origin
    /// tests don't reach directly: a malformed bracketed IPv6 literal, a
    /// non-numeric port, and an Origin that normalises to nothing.
    #[test]
    fn normalize_authority_and_cross_origin_edge_branches() {
        use super::normalize_authority;
        // Malformed IPv6: a `]` followed by junk that is neither empty nor a
        // `:port` must be rejected outright (defensive `None`).
        assert_eq!(normalize_authority("[::1]junk", 80), None);
        // A trailing `:token` whose port is NOT numeric is treated as part of
        // the host, and the scheme's default port is appended instead of
        // silently dropping it.
        assert_eq!(
            normalize_authority("host:notaport", 80).as_deref(),
            Some("host:notaport:80")
        );
        // An Origin present but normalising to nothing (bare scheme, empty
        // authority) can't prove cross-origin, so the request is allowed.
        assert!(!is_cross_origin(Some("http://"), Some("autorip.test:80")));
    }

    // ── SSRF guard ─────────────────────────────────────────────────────

    #[test]
    fn blocks_loopback_private_and_metadata_ips() {
        use std::net::{Ipv4Addr, Ipv6Addr};
        // Loopback.
        assert!(is_blocked_ip(&IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1))));
        // RFC1918 private ranges.
        assert!(is_blocked_ip(&IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1))));
        assert!(is_blocked_ip(&IpAddr::V4(Ipv4Addr::new(192, 168, 1, 50))));
        assert!(is_blocked_ip(&IpAddr::V4(Ipv4Addr::new(172, 16, 0, 1))));
        // Cloud metadata anycast (link-local).
        assert!(is_blocked_ip(&IpAddr::V4(Ipv4Addr::new(
            169, 254, 169, 254
        ))));
        // Carrier-grade NAT 100.64.0.0/10 and "this network" 0.0.0.0/8.
        assert!(is_blocked_ip(&IpAddr::V4(Ipv4Addr::new(100, 64, 0, 1))));
        assert!(is_blocked_ip(&IpAddr::V4(Ipv4Addr::new(0, 0, 0, 0))));
        // IPv6 loopback, ULA, link-local.
        assert!(is_blocked_ip(&IpAddr::V6(Ipv6Addr::LOCALHOST)));
        assert!(is_blocked_ip(&IpAddr::V6(Ipv6Addr::new(
            0xfd00, 0, 0, 0, 0, 0, 0, 1
        ))));
        assert!(is_blocked_ip(&IpAddr::V6(Ipv6Addr::new(
            0xfe80, 0, 0, 0, 0, 0, 0, 1
        ))));
        // IPv4-mapped loopback ::ffff:127.0.0.1 must also be blocked.
        assert!(is_blocked_ip(&IpAddr::V6(
            Ipv4Addr::new(127, 0, 0, 1).to_ipv6_mapped()
        )));
    }

    #[test]
    fn blocks_ipv4_compat_and_class_e() {
        use std::net::{Ipv4Addr, Ipv6Addr};
        // IPv4-compatible ::127.0.0.1 (deprecated but still parseable).
        // to_ipv4_mapped() would miss this; to_ipv4() catches it.
        assert!(is_blocked_ip(&IpAddr::V6(Ipv6Addr::new(
            0, 0, 0, 0, 0, 0, 0x7f00, 0x0001
        ))));
        // Class-E 240.0.0.0/4 — reserved, not public.
        assert!(is_blocked_ip(&IpAddr::V4(Ipv4Addr::new(240, 0, 0, 1))));
        assert!(is_blocked_ip(&IpAddr::V4(Ipv4Addr::new(
            255, 255, 255, 254
        ))));
        // 239.x is multicast (already caught by is_multicast), not Class-E.
        // Boundary check: 239.255.255.255 is multicast, 240.0.0.0 is Class-E.
        assert!(is_blocked_ip(&IpAddr::V4(Ipv4Addr::new(240, 0, 0, 0))));
    }

    #[test]
    fn allows_public_ips() {
        use std::net::{Ipv4Addr, Ipv6Addr};
        assert!(!is_blocked_ip(&IpAddr::V4(Ipv4Addr::new(8, 8, 8, 8))));
        assert!(!is_blocked_ip(&IpAddr::V4(Ipv4Addr::new(1, 1, 1, 1))));
        // Public IPv6 (Cloudflare DNS).
        assert!(!is_blocked_ip(&IpAddr::V6(Ipv6Addr::new(
            0x2606, 0x4700, 0x4700, 0, 0, 0, 0, 0x1111
        ))));
    }

    #[test]
    fn blocks_multicast_ipv4_and_ipv6() {
        use std::net::{Ipv4Addr, Ipv6Addr};
        // Pure multicast, only reachable via is_multicast(). An `||`->`&&`
        // mutant immediately before it (folding into `is_unspecified() &&
        // is_multicast()`, never true) would let this through.
        assert!(is_blocked_ip(&IpAddr::V4(Ipv4Addr::new(230, 1, 2, 3))));
        // IPv6 multicast, not unspecified — same shape of gap on the v6 side.
        assert!(is_blocked_ip(&IpAddr::V6(Ipv6Addr::new(
            0xff02, 0, 0, 0, 0, 0, 0, 1
        ))));
    }

    #[test]
    fn cgn_check_does_not_over_block_unrelated_public_space() {
        use std::net::Ipv4Addr;
        // Carrier-grade NAT 100.64.0.0/10: octet[0]==100 AND octet[1] top
        // bits == 01. 100.64.0.1 is inside and blocked; 100.128.0.1 is
        // outside and allowed. A `&&`->`||` mutant would block both.
        assert!(is_blocked_ip(&IpAddr::V4(Ipv4Addr::new(100, 64, 0, 1))));
        assert!(!is_blocked_ip(&IpAddr::V4(Ipv4Addr::new(100, 128, 0, 1))));
        // A public address whose second octet has the CGN-like bit pattern
        // (01xxxxxx) but whose first octet is NOT 100 must NOT be blocked —
        // pins the `&&` (not `||`) between the two octet checks.
        assert!(!is_blocked_ip(&IpAddr::V4(Ipv4Addr::new(1, 65, 2, 3))));
    }

    // Benchmarking 198.18.0.0/15 (RFC 2544) and IETF protocol assignments
    // 192.0.0.0/24 (RFC 6890, incl. the 192.0.0.170/171 NAT64/DNS64 anycast)
    // are non-public and must be blocked outbound — parity with keysources.
    #[test]
    fn ssrf_guard_blocks_benchmarking_and_protocol_assignment_ranges() {
        use std::net::Ipv4Addr;
        // 198.18.0.0/15 spans 198.18.x AND 198.19.x — both octets blocked.
        assert!(is_blocked_ip(&IpAddr::V4(Ipv4Addr::new(198, 18, 0, 1))));
        assert!(is_blocked_ip(&IpAddr::V4(Ipv4Addr::new(198, 18, 255, 255))));
        assert!(is_blocked_ip(&IpAddr::V4(Ipv4Addr::new(198, 19, 0, 1))));
        assert!(is_blocked_ip(&IpAddr::V4(Ipv4Addr::new(198, 19, 200, 5))));
        // 198.17.x and 198.20.x are OUTSIDE the /15 — must stay allowed.
        assert!(!is_blocked_ip(&IpAddr::V4(Ipv4Addr::new(198, 17, 0, 1))));
        assert!(!is_blocked_ip(&IpAddr::V4(Ipv4Addr::new(198, 20, 0, 1))));
        // 192.0.0.0/24, including 192.0.0.170 / 192.0.0.171.
        assert!(is_blocked_ip(&IpAddr::V4(Ipv4Addr::new(192, 0, 0, 0))));
        assert!(is_blocked_ip(&IpAddr::V4(Ipv4Addr::new(192, 0, 0, 170))));
        assert!(is_blocked_ip(&IpAddr::V4(Ipv4Addr::new(192, 0, 0, 171))));
        assert!(is_blocked_ip(&IpAddr::V4(Ipv4Addr::new(192, 0, 0, 255))));
        // The adjacent 192.0.1.0 is a different block — not covered here.
        assert!(!is_blocked_ip(&IpAddr::V4(Ipv4Addr::new(192, 0, 1, 1))));
    }

    // 6to4 (2002::/16) and Teredo (2001:0000::/32) tunnel an IPv4 inside an
    // IPv6 address; the guard must decode and re-check that embedded IPv4 or an
    // internal target slips through the tunnel — parity with keysources.
    #[test]
    fn ssrf_guard_blocks_embedded_ipv4_via_6to4_and_teredo() {
        use std::net::Ipv6Addr;
        // 6to4 for 127.0.0.1: 2002:7f00:0001:: (embedded in segments[1..3]).
        assert!(is_blocked_ip(&IpAddr::V6(Ipv6Addr::new(
            0x2002, 0x7f00, 0x0001, 0, 0, 0, 0, 0
        ))));
        // 6to4 for 169.254.169.254 (cloud metadata): 2002:a9fe:a9fe::.
        assert!(is_blocked_ip(&IpAddr::V6(Ipv6Addr::new(
            0x2002, 0xa9fe, 0xa9fe, 0, 0, 0, 0, 0
        ))));
        // Teredo for 127.0.0.1: client IPv4 lives in the last two segments XOR
        // 0xffff, so 0x7f00^0xffff=0x80ff and 0x0001^0xffff=0xfffe.
        assert!(is_blocked_ip(&IpAddr::V6(Ipv6Addr::new(
            0x2001, 0x0000, 0, 0, 0, 0, 0x80ff, 0xfffe
        ))));
        // A 6to4 wrapping a PUBLIC IPv4 (8.8.8.8 → 2002:0808:0808::) is allowed.
        assert!(!is_blocked_ip(&IpAddr::V6(Ipv6Addr::new(
            0x2002, 0x0808, 0x0808, 0, 0, 0, 0, 0
        ))));
    }

    #[test]
    fn ssrf_guard_blocks_embedded_ipv4_via_nat64() {
        use std::net::Ipv6Addr;
        // NAT64 well-known prefix 64:ff9b::/96 embeds the IPv4 in the last 32
        // bits. 127.0.0.1 → 64:ff9b::7f00:0001.
        assert!(is_blocked_ip(&IpAddr::V6(Ipv6Addr::new(
            0x0064, 0xff9b, 0, 0, 0, 0, 0x7f00, 0x0001
        ))));
        // 169.254.169.254 (cloud metadata) → 64:ff9b::a9fe:a9fe.
        assert!(is_blocked_ip(&IpAddr::V6(Ipv6Addr::new(
            0x0064, 0xff9b, 0, 0, 0, 0, 0xa9fe, 0xa9fe
        ))));
        // NAT64 wrapping a PUBLIC IPv4 (8.8.8.8 → 64:ff9b::0808:0808) is allowed.
        assert!(!is_blocked_ip(&IpAddr::V6(Ipv6Addr::new(
            0x0064, 0xff9b, 0, 0, 0, 0, 0x0808, 0x0808
        ))));
    }

    #[test]
    fn validate_fetch_url_rejects_internal_and_bad_scheme() {
        // Numeric internal/metadata literals resolve without DNS and must
        // be rejected.
        assert!(validate_fetch_url("http://127.0.0.1/x").is_err());
        assert!(validate_fetch_url("http://169.254.169.254/latest/meta-data/").is_err());
        assert!(
            validate_fetch_url(&format!("http://{}.{}.{}.{}:8080/decode", 10, 0, 0, 5)).is_err()
        );
        assert!(validate_fetch_url(&format!("https://{}.{}.{}.{}/", 192, 168, 0, 1)).is_err());
        assert!(validate_fetch_url("http://[::1]:9000/").is_err());
        // Non-http schemes and junk.
        assert!(validate_fetch_url("ftp://example.com/x").is_err());
        assert!(validate_fetch_url("file:///etc/passwd").is_err());
        assert!(validate_fetch_url("not a url").is_err());
        assert!(validate_fetch_url("").is_err());
    }

    #[test]
    fn guarded_get_rejects_rfc1918_before_connecting() {
        // guarded_get must run the SSRF guard FIRST, so an RFC1918/loopback/
        // metadata literal is rejected with no socket ever opened — the
        // guard daemon.rs's KEYDB fetch paths route through, not bare ureq::get.
        assert!(guarded_get(&format!("http://{}.{}.{}.{}/keydb.zip", 10, 0, 0, 5)).is_err());
        assert!(guarded_get(&format!("http://{}.{}.{}.{}/keydb.zip", 192, 168, 1, 10)).is_err());
        assert!(guarded_get(&format!("http://{}.{}.{}.{}/keydb.zip", 172, 20, 0, 1)).is_err());
        assert!(guarded_get("http://127.0.0.1/keydb.zip").is_err());
        assert!(guarded_get("http://169.254.169.254/latest/").is_err());
        assert!(guarded_get("http://[::1]:9000/keydb.zip").is_err());
        // Wrong scheme is rejected too (no connect attempt).
        assert!(guarded_get("file:///etc/passwd").is_err());
    }

    // A KEYDB body that is SLOW but PROGRESSING must finish. ureq 3.4.1 (#1194) made
    // timeout_recv_body a TOTAL deadline that no longer re-arms; autorip restores the rolling
    // idle bound itself.
    #[test]
    fn a_slow_but_progressing_keydb_body_is_not_killed_by_the_header_deadline() {
        use std::io::{Read as _, Write as _};
        use std::net::TcpListener;

        let listener =
            TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).expect("bind stub listener");
        let pinned = listener.local_addr().expect("stub listener address");

        let server = std::thread::spawn(move || {
            let (mut sock, _peer) = listener.accept().expect("accept failed");
            let mut head = Vec::new();
            let mut byte = [0u8; 1];
            while !head.ends_with(b"\r\n\r\n") {
                match sock.read(&mut byte) {
                    Ok(0) | Err(_) => break,
                    Ok(_) => head.push(byte[0]),
                }
            }
            let _ = sock.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 60\r\n\r\n");
            let _ = sock.flush();
            // Sixty bytes, 100 ms apart: ~6 s of body, with no single gap
            // anywhere near the idle bound. The TOTAL is what matters — see
            // the timeout comment below.
            for _ in 0..60 {
                if sock.write_all(b"k").is_err() {
                    return;
                }
                let _ = sock.flush();
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
        });

        // 100ms per-gap vs a 3s idle bound (30x headroom, so a scheduling stall can't
        // spuriously trip it), and ~6s total body vs that 3s bound (2x, so a TOTAL
        // interpretation still fails).
        let idle = std::time::Duration::from_secs(3);
        let agent = guarded_agent_with_timeouts(
            vec![pinned],
            std::time::Duration::from_secs(5),
            std::time::Duration::from_secs(30),
            idle,
        );
        let resp = agent
            .get("http://keydb-mirror.test/keydb.zip")
            .call()
            .expect("headers must arrive");
        let started = std::time::Instant::now();
        let mut body = Vec::new();
        let read = resp.into_body().into_reader().read_to_end(&mut body);
        let elapsed = started.elapsed();
        let _ = server.join();

        assert!(
            read.is_ok(),
            "a steadily-progressing body was aborted: {:?}",
            read.err()
        );
        assert_eq!(body, vec![b'k'; 60], "the whole body must arrive");
        // Relative-progress proof, robust to runner speed: the transfer outlasted
        // one idle window, so a TOTAL interpretation would have killed it — it did
        // not, therefore the bound rolled (a slow runner only grows `elapsed`).
        assert!(
            elapsed > idle,
            "body finished within a single idle window ({elapsed:?} <= {idle:?}); \
             the rolling-vs-total distinction is no longer exercised"
        );
    }

    // The other half: a peer sending headers then NOTHING must be cut off by the rolling idle
    // bound, not held for the whole total budget — the protection ureq 2's timeout_read gave.
    #[test]
    fn a_stalled_body_is_cut_off_by_the_idle_bound_not_the_total_budget() {
        use std::io::{Read as _, Write as _};
        use std::net::TcpListener;

        let listener =
            TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).expect("bind stub listener");
        let pinned = listener.local_addr().expect("stub listener address");

        let server = std::thread::spawn(move || {
            let (mut sock, _peer) = listener.accept().expect("accept failed");
            let mut head = Vec::new();
            let mut byte = [0u8; 1];
            while !head.ends_with(b"\r\n\r\n") {
                match sock.read(&mut byte) {
                    Ok(0) | Err(_) => break,
                    Ok(_) => head.push(byte[0]),
                }
            }
            let _ = sock.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 1048576\r\n\r\n");
            let _ = sock.flush();
            // Promise a megabyte and send none of it — hold the socket by
            // BLOCKING ON A READ, not sleeping, so the read returns the moment
            // the client drops the connection instead of outliving the test.
            let mut sink = [0u8; 1];
            let _ = sock.read(&mut sink);
        });

        let idle = std::time::Duration::from_secs(1);
        let agent = guarded_agent_with_timeouts(
            vec![pinned],
            std::time::Duration::from_secs(5),
            // A total budget far larger than the idle bound, so only the idle
            // bound can be what ends this.
            std::time::Duration::from_secs(120),
            idle,
        );
        let started = std::time::Instant::now();
        let resp = agent
            .get("http://keydb-mirror.test/keydb.zip")
            .call()
            .expect("headers must arrive");
        let mut body = Vec::new();
        let read = resp.into_body().into_reader().read_to_end(&mut body);
        let elapsed = started.elapsed();

        assert!(read.is_err(), "a stalled body must not read as success");
        assert!(
            elapsed < std::time::Duration::from_secs(20),
            "a stalled peer was held for {elapsed:?} — the idle bound did not fire, \
             so the total budget is the only thing ending this"
        );
        // Joinable because the stub ends on client disconnect; `drop` on a
        // JoinHandle only detaches, it does not stop the thread.
        let _ = server.join();
    }

    // The real /api/update-keydb call path sets a request-level ceiling (timeout_global);
    // a peer that sends headers then stalls must still be cut off by the idle bound.
    #[test]
    fn keydb_update_stalled_body_is_cut_off_by_idle_bound_under_a_global_ceiling() {
        use std::io::{Read as _, Write as _};
        use std::net::TcpListener;

        let listener =
            TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).expect("bind stub listener");
        let pinned = listener.local_addr().expect("stub listener address");
        let server = std::thread::spawn(move || {
            let (mut sock, _peer) = listener.accept().expect("accept failed");
            let mut head = Vec::new();
            let mut byte = [0u8; 1];
            while !head.ends_with(b"\r\n\r\n") {
                match sock.read(&mut byte) {
                    Ok(0) | Err(_) => break,
                    Ok(_) => head.push(byte[0]),
                }
            }
            let _ = sock.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 1048576\r\n\r\n");
            let _ = sock.flush();
            let mut sink = [0u8; 1];
            let _ = sock.read(&mut sink);
        });

        // Same 3:1 ceiling:idle ratio as production (60s:20s), scaled down.
        let idle = std::time::Duration::from_secs(1);
        let ceiling = std::time::Duration::from_secs(8);
        let started = std::time::Instant::now();
        let resp = keydb_update_call(
            vec![pinned],
            "http://keydb-mirror.test/k.zip",
            ceiling,
            idle,
        )
        .expect("headers must arrive");
        let mut body = Vec::new();
        let read = resp.into_body().into_reader().read_to_end(&mut body);
        let elapsed = started.elapsed();
        let _ = server.join();

        assert!(read.is_err(), "a stalled body must not read as success");
        assert!(
            elapsed < std::time::Duration::from_secs(5),
            "stalled keydb update held for {elapsed:?}: the global ceiling, not the idle bound, ended it"
        );
    }

    // The rejection tests above never connect, so they'd still pass if the
    // agent silently ignored pinned addresses and re-resolved via live DNS.
    // This pins a loopback listener and requests an unresolvable `.test` host.
    #[test]
    fn guarded_agent_connects_to_the_pinned_address_not_dns() {
        use std::io::{Read as _, Write as _};
        use std::net::TcpListener;
        use std::sync::mpsc;

        let listener =
            TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).expect("bind stub listener");
        let pinned = listener.local_addr().expect("stub listener address");
        let (tx, rx) = mpsc::channel();

        let server = std::thread::spawn(move || {
            let (mut sock, _peer) = listener.accept().expect("stub listener accept failed");
            let _ = tx.send(());
            let mut head = Vec::new();
            let mut byte = [0u8; 1];
            while !head.ends_with(b"\r\n\r\n") {
                match sock.read(&mut byte) {
                    Ok(0) | Err(_) => break,
                    Ok(_) => head.push(byte[0]),
                }
            }
            let _ = sock
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nhi");
            let _ = sock.flush();
            head
        });

        let sent = guarded_agent_with_timeouts(
            vec![pinned],
            std::time::Duration::from_secs(5),
            std::time::Duration::from_secs(30),
            STALL_TIMEOUT,
        )
        .get("http://keydb-mirror.test/keydb.zip")
        .call();

        rx.recv_timeout(std::time::Duration::from_secs(10)).expect(
            "guarded_agent never connected to the pinned address — the custom \
             resolver is not being consulted, so a DNS rebind between \
             validate_fetch_url and the fetch can still redirect the request",
        );
        let resp = sent.expect("the pinned round-trip must complete");
        assert_eq!(resp.status(), 200, "the stub server's reply must come back");
        let head = server.join().expect("stub server panicked");
        let head = String::from_utf8_lossy(&head);
        assert!(
            head.contains("keydb-mirror.test"),
            "the pinned agent must still address the original host; got: {head}"
        );
    }

    // Secret-leak guard: guarded_get error strings must never embed the full
    // request URL (path/query may hold a token). Uses a public IP with
    // nothing listening to reach ureq's Transport error arm, not the SSRF guard.
    #[test]
    fn guarded_get_ureq_error_does_not_embed_url() {
        // Port 1 on a public IP: passes SSRF guard (it's public) but the
        // connection will be refused immediately (nothing listens on port 1).
        // The URL has a fake token in the path that must not appear in the error.
        let token = "supersecret_api_token_12345";
        let url = format!("http://8.8.8.8:1/keydb/{token}.zip");
        let err = guarded_get(&url).unwrap_err();
        assert!(
            !err.contains(token),
            "ureq transport error must not leak the URL token; got: {err:?}"
        );
        // The error string must be our summary, not ureq's URL-bearing Display.
        assert!(
            err.starts_with("fetch failed:"),
            "error should be our summary; got: {err:?}"
        );
    }

    // is_transient_resolve_error must say NO to every permanent verdict
    // validate_fetch_url can reach without a network round-trip — its
    // consumer turns `true` into "the key service is down" and parks a disc.
    #[test]
    fn a_rejected_url_is_never_classified_as_a_failed_lookup() {
        for url in [
            "",
            "ftp://example.com/keys",
            "http://",
            // Literal, so no DNS is involved — the guard rejects the address.
            "http://127.0.0.1:8080/keys",
            "http://169.254.169.254/latest/meta-data",
        ] {
            let err = validate_fetch_url(url)
                .expect_err("this URL must be rejected outright, not accepted");
            assert!(
                !is_transient_resolve_error(&err),
                "{url:?} is a permanent verdict on the URL, but its error \
                 {err:?} classifies as a failed lookup"
            );
        }
    }

    #[test]
    fn validate_network_target_rejects_internal_hosts() {
        // Bare host:port (no scheme). Internal/metadata literals resolve
        // without DNS and must be rejected — at rip time decrypted content
        // streams here.
        assert!(validate_network_target("169.254.169.254:80").is_err());
        assert!(validate_network_target("127.0.0.1:9000").is_err());
        assert!(validate_network_target(&format!("{}.{}.{}.{}:9000", 10, 0, 0, 5)).is_err());
        assert!(validate_network_target(&format!("{}.{}.{}.{}:9000", 192, 168, 0, 1)).is_err());
        assert!(validate_network_target("[::1]:9000").is_err());
        // RFC5737 documentation range is non-public and blocked.
        assert!(validate_network_target("198.51.100.10:9000").is_err());
        // Malformed / missing port.
        assert!(validate_network_target("nas.example.com").is_err());
        assert!(validate_network_target("169.254.169.254").is_err());
        assert!(validate_network_target("").is_err());
    }

    #[test]
    fn validate_network_target_accepts_public_literal() {
        // A public numeric host:port (no DNS needed) should validate.
        assert!(validate_network_target("8.8.8.8:9000").is_ok());
        assert!(validate_network_target("1.1.1.1:443").is_ok());
    }

    #[test]
    fn resolve_with_timeout_resolves_literal() {
        // A numeric literal resolves without touching DNS and returns within
        // the deadline. Shared by validate_network_target + validate_fetch_url.
        let addrs = resolve_with_timeout("9.9.9.9", 853).expect("literal resolves");
        assert!(addrs.iter().any(|a| a.port() == 853 && a.ip().is_ipv4()));
    }

    #[test]
    fn resolve_with_timeout_does_not_leak_inflight_slots() {
        // Regression for the unbounded-thread leak: the in-flight cap is 8.
        // A completed resolve must release its slot, so many sequential
        // resolves succeed — if slots leaked, the 9th+ call would fail fast.
        for _ in 0..40 {
            let addrs = resolve_with_timeout("9.9.9.9", 853).expect("literal resolves");
            assert!(addrs.iter().any(|a| a.port() == 853));
            // Let the detached resolver thread finish (dropping its ConnGuard)
            // before the next iteration so the slot is reliably released.
            std::thread::yield_now();
        }
    }

    #[test]
    fn validate_fetch_url_accepts_public_literal() {
        // A public numeric host (no DNS needed) should validate and yield
        // the pinned address with the default port for the scheme.
        let addrs = validate_fetch_url("https://8.8.8.8/keydb.zip").expect("public IP allowed");
        assert!(addrs.iter().any(|a| a.port() == 443));
        let addrs = validate_fetch_url("http://1.1.1.1:8080/decode").expect("public IP allowed");
        assert!(addrs.iter().any(|a| a.port() == 8080));
    }

    // ── Connection cap ─────────────────────────────────────────────────

    #[test]
    fn conn_guard_releases_slot_when_holder_unwinds() {
        // Regression (resolve_with_timeout INFLIGHT leak): the DNS throttle's
        // ConnGuard must release its slot even when the holder unwinds rather
        // than returns — exactly what happens if `thread::spawn` panics.
        static C: AtomicUsize = AtomicUsize::new(0);
        let g0 = ConnGuard::try_acquire(&C, 4).expect("first slot");
        assert_eq!(C.load(Ordering::SeqCst), 1);
        let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _g = ConnGuard::try_acquire(&C, 4).expect("second slot");
            assert_eq!(C.load(Ordering::SeqCst), 2);
            panic!("simulate thread::spawn unwinding with the guard held");
        }));
        assert!(r.is_err(), "the inner closure must have panicked");
        // The unwound guard's Drop must have released its slot; only g0 remains.
        assert_eq!(
            C.load(Ordering::SeqCst),
            1,
            "guard slot leaked across an unwind"
        );
        drop(g0);
        assert_eq!(C.load(Ordering::SeqCst), 0);
    }

    // ── A stalled POST must not starve the healthcheck (tiny_http 0.12 has
    // no socket read timeout). Fill the body cap, then confirm a bodyless
    // request still gets admitted while a body-carrying one does not.
    #[test]
    fn a_bodyless_request_is_admitted_while_the_body_cap_is_full() {
        static C: AtomicUsize = AtomicUsize::new(0);
        let held: Vec<_> = (0..MAX_INFLIGHT_BODY_HANDLERS)
            .map(|_| {
                ConnGuard::try_acquire(&C, MAX_INFLIGHT_BODY_HANDLERS).expect("under the body cap")
            })
            .collect();
        assert!(
            ConnGuard::try_acquire(&C, MAX_INFLIGHT_BODY_HANDLERS).is_none(),
            "the body cap must actually stop admitting"
        );
        let spare = ConnGuard::try_acquire(&C, MAX_INFLIGHT_HANDLERS);
        assert!(
            spare.is_some(),
            "a bodyless request must still be admitted — this is the slot the \
             healthcheck lives in"
        );
        drop(spare);
        drop(held);
        assert_eq!(C.load(Ordering::SeqCst), 0, "every slot released");
    }

    // ── Two guards nothing exercised: the pin caps at ureq's fixed 16-slot
    // array. A 17th address is an out-of-bounds panic in a resolver that
    // runs on every request; validate_fetch_url applies no count limit.
    #[test]
    fn the_pinned_address_list_cannot_overrun_ureqs_fixed_array() {
        let many: Vec<SocketAddr> = (0..MAX_PINNED_ADDRS + 1)
            .map(|i| SocketAddr::from(([203, 0, 113, 1], 1000 + i as u16)))
            .collect();
        assert_eq!(
            pinned_addrs(&many).len(),
            MAX_PINNED_ADDRS,
            "more addresses than ureq's array holds must be truncated"
        );
        // An ordinary answer is passed through untouched.
        let few: Vec<SocketAddr> = many.iter().copied().take(3).collect();
        assert_eq!(pinned_addrs(&few), few);
    }

    /// An EMPTY pin must not resolve to anything: the resolver turns that into
    /// `HostNotFound` rather than letting the agent fall back to live DNS,
    /// which would reopen the rebinding hole the pin exists to close.
    #[test]
    fn an_empty_pin_yields_no_addresses() {
        assert!(pinned_addrs(&[]).is_empty());
    }

    // ureq_error_kind is the single chokepoint keeping token-bearing URLs out
    // of log sites reaching unauthenticated /api/debug + /api/system. Covers
    // every variant, including the non_exhaustive catch-all where BadUri lands.
    #[test]
    fn no_ureq_error_kind_output_can_carry_a_url() {
        let cases = vec![
            ureq::Error::StatusCode(404),
            ureq::Error::Io(std::io::Error::from(std::io::ErrorKind::ConnectionRefused)),
            ureq::Error::HostNotFound,
            ureq::Error::ConnectionFailed,
            ureq::Error::TooManyRedirects,
            // The variant the catch-all exists for: its Display embeds the URI.
            ureq::Error::BadUri("https://keydb.example/t/SECRETTOKEN/keydb.zip".into()),
        ];
        for e in &cases {
            let kind = ureq_error_kind(e);
            assert!(
                !kind.contains("://") && !kind.contains("SECRETTOKEN"),
                "a URL reached the summary for {e:?}: {kind}"
            );
        }
        assert_eq!(ureq_error_kind(&ureq::Error::StatusCode(404)), "HTTP 404");
    }

    // An OS-generated transport error (real errno, e.g. ECONNRESET) must
    // surface its descriptive syscall message, not collapse to the useless
    // "io: uncategorized error" io.kind() alone prints for Uncategorized.
    #[test]
    fn ureq_error_kind_surfaces_os_error_detail() {
        // ECONNRESET (54 on macOS/BSD, 104 on Linux) can arrive as the
        // unhelpful Uncategorized kind; build via from_raw_os_error so
        // raw_os_error() is Some and the descriptive Display is used.
        let econnreset = if cfg!(target_os = "linux") { 104 } else { 54 };
        let e = ureq::Error::Io(std::io::Error::from_raw_os_error(econnreset));
        let summary = ureq_error_kind(&e);
        assert!(
            summary.contains(&format!("os error {econnreset}")),
            "the errno detail must be surfaced, got: {summary}"
        );
        // Still URL-free — the whole point of routing through this function.
        assert!(!summary.contains("://"));

        // An io error WITHOUT an errno (constructed from a bare ErrorKind, as
        // ureq/std synthesize) falls back to the fixed kind description.
        let synthetic =
            ureq::Error::Io(std::io::Error::from(std::io::ErrorKind::ConnectionRefused));
        assert_eq!(ureq_error_kind(&synthetic), "io: connection refused");
    }

    // The fourth ureq log site, missed by round 1: it masked the KEYDB origin
    // from the client but logged the raw (token-bearing) error anyway,
    // reaching autorip.jsonl and unauthenticated GET /api/debug.
    #[test]
    fn the_keydb_update_handler_masks_its_ureq_error() {
        let src = crate::server::util::source_lf(include_str!("web.rs"));
        // Anchored on the DEFINITION, not the name (this test mentions it
        // too). Both ends are `expect`ed — an anchor that stops matching
        // would otherwise silently widen the slice to other handlers' logs.
        let start = src
            .find("\nfn handle_update_keydb(request: tiny_http::Request")
            .expect("handle_update_keydb definition present");
        let end = start
            + src[start..]
                .find("\n    // Write to the service-canonical keydb path")
                .expect("the handler's post-fetch section still starts here");
        let body = &src[start..end];
        assert!(
            body.contains("ureq_error_kind(&e)"),
            "the keydb-update handler must summarise its ureq failure through \
             ureq_error_kind, which is URL-free"
        );
        assert!(
            !body.contains("error = %e"),
            "the keydb-update handler must not format its ureq error by Display"
        );
    }

    // Catches the mutation restoring get_state_json's Err(_) => return "{}" bail-out on a
    // poisoned STATE; source-pinned since poisoning a real Mutex would break every other
    // STATE-locking test.
    #[test]
    fn get_state_json_recovers_a_poisoned_state_lock() {
        let src = crate::server::util::source_lf(include_str!("web.rs"));
        // Anchored on the DEFINITION (leading newline) so this test's own
        // mention of the name cannot match, and both ends are `expect`ed so a
        // stale anchor fails loudly instead of silently widening the slice.
        let start = src
            .find("\nfn get_state_json(staging_dir: &str) -> String {")
            .expect("web.rs must define get_state_json");
        let end = start
            + src[start..]
                .find("\n    let move_state =")
                .expect("get_state_json still binds move_state after the STATE lock");
        // Comment lines are stripped: this function's own comment quotes the
        // defective arm verbatim (house style), so a naive substring search
        // would otherwise match the explanation of the fix, not the fix.
        let body: String = src[start..end]
            .lines()
            .filter(|l| !l.trim_start().starts_with("//"))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            body.contains("STATE.lock().unwrap_or_else(|e| e.into_inner())"),
            "get_state_json must recover a poisoned STATE guard, like every \
             other STATE consumer in the crate"
        );
        assert!(
            !body.contains("Err(_) =>"),
            "get_state_json must not have an abandon-on-poison arm: it turns \
             one panic into a permanently blank dashboard served with 200, \
             which run_healthcheck reads as healthy forever"
        );
    }

    #[test]
    fn resolve_with_timeout_uses_raii_guard_not_closure_side_fetch_sub() {
        // Source-pin for the INFLIGHT-leak fix: the decrement must NOT be a
        // bare `INFLIGHT.fetch_sub(...)` in the resolver closure (leaked on
        // panic) — it must flow through a ConnGuard whose Drop always runs.
        let src = crate::server::util::source_lf(include_str!("web.rs"));
        let start = src
            .find("pub(crate) fn resolve_with_timeout")
            .expect("resolve_with_timeout present");
        let body = &src[start..start + 1500];
        assert!(
            body.contains("ConnGuard::try_acquire(&INFLIGHT, MAX_INFLIGHT)"),
            "resolve_with_timeout must acquire its slot via ConnGuard"
        );
        assert!(
            !body.contains("INFLIGHT.fetch_sub"),
            "resolve_with_timeout must not decrement INFLIGHT by hand; the \
             ConnGuard Drop owns the release"
        );
    }

    #[test]
    fn conn_guard_enforces_cap_and_releases_on_drop() {
        static C: AtomicUsize = AtomicUsize::new(0);
        let g1 = ConnGuard::try_acquire(&C, 2);
        let g2 = ConnGuard::try_acquire(&C, 2);
        assert!(g1.is_some());
        assert!(g2.is_some());
        assert_eq!(C.load(Ordering::SeqCst), 2);
        // Third over the cap is rejected.
        assert!(ConnGuard::try_acquire(&C, 2).is_none());
        // Dropping one frees a slot so the next acquire succeeds.
        drop(g1);
        assert_eq!(C.load(Ordering::SeqCst), 1);
        let g3 = ConnGuard::try_acquire(&C, 2);
        assert!(g3.is_some());
        drop(g2);
        drop(g3);
        assert_eq!(C.load(Ordering::SeqCst), 0);
    }

    // ── percent_decode trailing %XX ────────────────────────────────────

    #[test]
    fn percent_decode_handles_trailing_encoded_byte() {
        // A value ending in a percent-encoded byte must decode (the old
        // off-by-one dropped it through as literal text).
        assert_eq!(percent_decode("a%20b"), "a b");
        assert_eq!(percent_decode("end%20"), "end ");
        // A bare trailing '%' or incomplete '%X' stays literal (no panic).
        assert_eq!(percent_decode("100%"), "100%");
        assert_eq!(percent_decode("50%2"), "50%2");
    }

    // media_type was the one title-override field neither clamped nor
    // allow-listed on an unauthenticated route whose value is persisted and
    // re-broadcast; the router only ever acts on "tv" vs everything else.
    #[test]
    fn media_type_is_reduced_to_the_routers_vocabulary() {
        assert_eq!(normalize_media_type("tv"), "tv");
        assert_eq!(normalize_media_type("movie"), "movie");
        // Case/whitespace noise still resolves to the real values.
        assert_eq!(normalize_media_type(" TV "), "tv");
        // Anything else routes as a movie today, so it is stored as one
        // instead of being persisted verbatim.
        assert_eq!(normalize_media_type("anime"), "movie");
        assert_eq!(normalize_media_type(""), "movie");
        assert_eq!(
            normalize_media_type(&"A".repeat(10_000)),
            "movie",
            "an unbounded caller-supplied string must never reach STATE, the \
             .done marker, or the dashboard broadcast"
        );
    }

    // Only ASCII hex digits are percent-escape payloads: from_str_radix
    // accepts a leading sign, so %+3 used to parse as 3 (a control byte)
    // instead of staying literal. RFC 3986 admits HEXDIG only.
    #[test]
    fn percent_decode_rejects_non_hex_escape_payloads() {
        assert_eq!(
            percent_decode("a%+3b"),
            "a%+3b",
            "'+' is not a hex digit — `%+3` must stay literal, not decode to 0x03"
        );
        assert_eq!(
            percent_decode("%-1"),
            "%-1",
            "a leading '-' must not be accepted as a sign either"
        );
        // Whitespace is the other thing from_str_radix-adjacent parsers trip
        // on; and the valid cases must keep working, upper and lower case.
        assert_eq!(percent_decode("%2f"), "/");
        assert_eq!(percent_decode("%2F"), "/");
    }

    // ── Real HTTP integration: drive handle_request via a live server —
    // tiny_http::Request has no public constructor, so these tests bind a
    // loopback server and hand a received real Request to production code.
    mod http {
        use super::*;
        use std::io::{Read, Write};
        use std::net::TcpStream;

        // One real request/response round-trip through handle_request: binds
        // an ephemeral loopback server, spawns a client that writes the raw
        // request, and dispatches through production code. Returns (status, body).
        fn roundtrip(
            cfg: &Arc<RwLock<Config>>,
            method: &str,
            path: &str,
            body: Option<&str>,
            extra_headers: &[(&str, &str)],
        ) -> (u16, String) {
            let server = Server::http("127.0.0.1:0").expect("bind loopback server");
            let addr = server.server_addr().to_ip().expect("ip addr");

            let method = method.to_string();
            let path = path.to_string();
            let body = body.map(|b| b.to_string());
            let extra: Vec<(String, String)> = extra_headers
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect();

            let client = std::thread::spawn(move || {
                let mut stream = TcpStream::connect(addr).expect("connect");
                let body = body.unwrap_or_default();
                let mut req = format!("{method} {path} HTTP/1.1\r\nHost: 127.0.0.1\r\n");
                for (k, v) in &extra {
                    req.push_str(&format!("{k}: {v}\r\n"));
                }
                req.push_str(&format!("Content-Length: {}\r\n", body.len()));
                req.push_str("Connection: close\r\n\r\n");
                req.push_str(&body);
                stream.write_all(req.as_bytes()).expect("write request");
                stream.flush().ok();
                let mut resp = Vec::new();
                stream.read_to_end(&mut resp).expect("read response");
                String::from_utf8_lossy(&resp).to_string()
            });

            // Accept exactly one request and dispatch it through production.
            let request = server.recv().expect("recv request");
            handle_request(request, cfg);

            let raw = client.join().expect("client thread");
            parse_response(&raw)
        }

        // Classify ONE real tiny_http::Request, sent verbatim (roundtrip
        // always adds a Content-Length; the arm that matters here has none),
        // via production carries_body. recv_timeout(5s) fails, not hangs.
        fn carries_body_of(head: &str) -> bool {
            let server = Server::http("127.0.0.1:0").expect("bind loopback server");
            let addr = server.server_addr().to_ip().expect("ip addr");
            let head = head.to_string();
            let client = std::thread::spawn(move || {
                let mut stream = TcpStream::connect(addr).expect("connect");
                stream.write_all(head.as_bytes()).expect("write request");
                stream.flush().ok();
                let mut resp = Vec::new();
                let _ = stream.read_to_end(&mut resp);
            });
            let request = server
                .recv_timeout(std::time::Duration::from_secs(5))
                .expect("server error")
                .expect("the request must arrive within 5s");
            let carries = carries_body(&request);
            let _ = request.respond(tiny_http::Response::empty(204));
            client.join().expect("client thread");
            carries
        }

        // The accept loop reserves a smaller cap for body-carrying requests,
        // decided ONLY by carries_body — the sibling cap test never called
        // it, so an inverted arm there stayed green. Drive with real requests.
        #[test]
        fn carries_body_classifies_real_requests() {
            // No Content-Length, bodyless method — the healthcheck's own
            // shape. This is the slot that must stay available when the body
            // cap is full.
            assert!(
                !carries_body_of("GET /api/state HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n"),
                "a GET with no Content-Length carries no body"
            );
            // Explicit zero length: a body header, but nothing to read.
            assert!(
                !carries_body_of(
                    "POST /api/settings HTTP/1.1\r\nHost: x\r\nContent-Length: 0\r\n\
                     Connection: close\r\n\r\n"
                ),
                "Content-Length: 0 is not a body"
            );
            // A real body.
            assert!(
                carries_body_of(
                    "POST /api/settings HTTP/1.1\r\nHost: x\r\nContent-Length: 5\r\n\
                     Connection: close\r\n\r\nhello"
                ),
                "a non-zero Content-Length carries a body"
            );
            // No Content-Length on a method that may carry one: assume the
            // reader will wait, and charge it the body cap.
            assert!(
                carries_body_of(
                    "POST /api/settings HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n"
                ),
                "a POST with no Content-Length must be charged the body cap"
            );
        }

        /// Extract the status code and body from a raw HTTP/1.1 response.
        fn parse_response(raw: &str) -> (u16, String) {
            let (head, body) = raw.split_once("\r\n\r\n").unwrap_or((raw, ""));
            let status_line = head.lines().next().unwrap_or_default();
            // "HTTP/1.1 200 OK"
            let code = status_line
                .split_whitespace()
                .nth(1)
                .and_then(|c| c.parse::<u16>().ok())
                .unwrap_or(0);
            (code, body.to_string())
        }

        /// A Config whose autorip_dir points at a writable tempdir so
        /// `config::save` (invoked by handle_settings_post) succeeds and we can
        /// read back the persisted settings.json.
        fn cfg_in_tempdir(dir: &std::path::Path) -> Arc<RwLock<Config>> {
            let c = Config {
                autorip_dir: dir.to_string_lossy().to_string(),
                staging_dir: dir.join("staging").to_string_lossy().to_string(),
                output_dir: dir.join("output").to_string_lossy().to_string(),
                ..Config::default()
            };
            Arc::new(RwLock::new(c))
        }

        // ── Route dispatch + method gating ──────────────────────────────

        #[test]
        fn library_routes_list_queue_and_gate() {
            let dir = tempfile::tempdir().unwrap();
            let cfg = cfg_in_tempdir(dir.path());
            let (lib, isos) = (dir.path().join("movies"), dir.path().join("isos"));
            std::fs::create_dir_all(&lib).unwrap();
            std::fs::create_dir_all(&isos).unwrap();
            std::fs::write(isos.join("New (2020).iso"), b"iso").unwrap();
            {
                let mut c = cfg.write().unwrap();
                c.library_dir = lib.to_string_lossy().into_owned();
                c.library_iso_dir = isos.to_string_lossy().into_owned();
            }
            // Two MKVs with ISOs: one muxed by an older freemkv, one by mkvmerge.
            use crate::server::library::probe::testmkv::mkv;
            for (t, app) in [
                ("Old (2001)", "freemkv 1.6.11 (g1)"),
                ("Merged (2002)", "mkvmerge v96.0 ('x') 64-bit"),
            ] {
                std::fs::create_dir_all(lib.join(t)).unwrap();
                std::fs::write(
                    lib.join(t).join(format!("{t}.mkv")),
                    mkv(app, Some(60.0), Some(58), true),
                )
                .unwrap();
                std::fs::write(isos.join(format!("{t}.iso")), b"iso").unwrap();
            }
            // Before the first scan the answer is immediate and says so.
            let (code, body) = roundtrip(&cfg, "GET", "/api/library", None, &[]);
            assert_eq!(code, 200, "{body}");
            let v: serde_json::Value = serde_json::from_str(&body).unwrap();
            assert_eq!(v["scanning"], true);
            let (code, body) = roundtrip(&cfg, "POST", "/api/library/queue/out-of-date", None, &[]);
            assert_eq!(code, 409, "no silent zero before the scan: {body}");

            let c = cfg.read().unwrap().clone();
            let library = crate::server::library::instance(&c);
            assert!(library.index_now(&crate::server::library::dirs(&c)));
            // Indexed: the answer comes from memory, even with the folders gone.
            let moved = dir.path().join("moved-away");
            std::fs::rename(&lib, &moved).unwrap();
            let t = std::time::Instant::now();
            let (code, body) = roundtrip(&cfg, "GET", "/api/library", None, &[]);
            assert!(t.elapsed() < std::time::Duration::from_secs(2));
            std::fs::rename(&moved, &lib).unwrap();
            assert_eq!(code, 200, "{body}");
            let v: serde_json::Value = serde_json::from_str(&body).unwrap();
            assert_eq!(v["version"], env!("CARGO_PKG_VERSION"));
            assert_eq!(v["rows"].as_array().unwrap().len(), 3, "{body}");
            let row = |t: &str| {
                v["rows"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .find(|r| r["title"] == t)
                    .unwrap()
                    .clone()
            };
            assert_eq!(row("Merged (2002)")["muxed_label"], "mkvmerge 96.0");
            assert_eq!(row("Old (2001)")["muxed_label"], "freemkv 1.6.11");
            assert_eq!(row("Old (2001)")["needs_remux"], true);
            let v = serde_json::json!({"rows": [row("New (2020)")]});
            assert_eq!(v["rows"][0]["kind"], "iso_only");
            let target = v["rows"][0]["target"].as_str().unwrap().to_string();

            let add = |t: &str| {
                let b = serde_json::json!({ "target": t }).to_string();
                roundtrip(&cfg, "POST", "/api/library/queue/add", Some(&b), &[])
            };
            assert!(
                add("/etc/passwd").1.contains("\"queued\":0"),
                "only listed rows"
            );
            assert!(add(&target).1.contains("\"queued\":1"));
            assert!(add(&target).1.contains("\"queued\":0"), "already queued");
            let (_, body) = roundtrip(&cfg, "POST", "/api/library/queue/out-of-date", None, &[]);
            let q: serde_json::Value = serde_json::from_str(&body).unwrap();
            assert_eq!(
                (q["queued"].as_u64(), q["eligible"].as_u64()),
                (Some(2), Some(3)),
                "{body}"
            );
            let (_, body) = roundtrip(&cfg, "POST", "/api/library/queue/clear-queued", None, &[]);
            assert!(body.contains("\"removed\":3"), "{body}");
            let (_, body) = roundtrip(&cfg, "POST", "/api/library/queue/pause", None, &[]);
            assert!(body.contains("\"paused\":true"), "{body}");
            let (code, _) = roundtrip(&cfg, "GET", "/api/library/console", None, &[]);
            assert_eq!(code, 200);
            let (code, _) = roundtrip(
                &cfg,
                "GET",
                "/api/library/log?title=New%20(2020)",
                None,
                &[],
            );
            assert_eq!(code, 200);
            let (code, _) = roundtrip(&cfg, "GET", "/api/library/log", None, &[]);
            assert_eq!(code, 400);
            let (code, _) = roundtrip(&cfg, "GET", "/api/library/nope", None, &[]);
            assert_eq!(code, 404);
            let (code, _) = roundtrip(&cfg, "GET", "/api/library/queue/all", None, &[]);
            assert_eq!(code, 404, "queue actions are POST only");
            let bad = r#"{"library_dir": "relative/path"}"#;
            let (code, _) = roundtrip(&cfg, "POST", "/api/settings", Some(bad), &[]);
            assert_eq!(code, 400);
        }

        #[test]
        fn get_version_dispatches_and_returns_running_version() {
            let cfg = Arc::new(RwLock::new(Config::default()));
            let (code, body) = roundtrip(&cfg, "GET", "/api/version", None, &[]);
            assert_eq!(code, 200);
            assert!(
                body.contains(&format!("\"version\":\"{}\"", crate::server::VERSION_LABEL)),
                "GET /api/version must serve the running version, got: {body}"
            );
        }

        #[test]
        fn unknown_route_returns_404() {
            let cfg = Arc::new(RwLock::new(Config::default()));
            let (code, body) = roundtrip(&cfg, "GET", "/api/nope", None, &[]);
            assert_eq!(code, 404, "an unknown route must 404");
            assert!(body.contains("not found"));
        }

        #[test]
        fn settings_route_gates_on_method() {
            // GET /api/settings serves redacted settings; a DELETE to the same
            // path falls through to 404 (method-gated, not matched).
            let cfg = Arc::new(RwLock::new(Config::default()));
            let (get_code, _) = roundtrip(&cfg, "GET", "/api/settings", None, &[]);
            assert_eq!(get_code, 200, "GET /api/settings must be served");
            let (del_code, _) = roundtrip(&cfg, "DELETE", "/api/settings", None, &[]);
            assert_eq!(del_code, 404, "DELETE /api/settings must not match");
        }

        #[test]
        fn sse_route_is_served_at_events_not_api_sse() {
            // Pin the ACTUAL served route: production serves /events, and
            // /api/sse is NOT a route and must 404. /events streams forever,
            // so it isn't tested here beyond the fact that it doesn't 404.
            let cfg = Arc::new(RwLock::new(Config::default()));
            let (api_sse_code, _) = roundtrip(&cfg, "GET", "/api/sse", None, &[]);
            assert_eq!(
                api_sse_code, 404,
                "/api/sse is not a real route — production serves /events"
            );
        }

        // ── Device-name validation in dispatch ──────────────────────────

        #[test]
        fn rip_route_rejects_invalid_device_name() {
            // A path-traversal device name must be rejected by the dispatch
            // guard (is_valid_device_name) with 400 — never reaching handle_rip.
            let cfg = Arc::new(RwLock::new(Config::default()));
            let (code, body) = roundtrip(&cfg, "POST", "/api/rip/..%2F..%2Fetc", None, &[]);
            assert_eq!(code, 400, "traversal device name must be rejected");
            assert!(body.contains("invalid device name"));
        }

        // A shape-VALID but nonexistent device name must not create state:
        // looping unauthenticated POST /api/scan/<random> used to grow the
        // STATE/JoinHandle maps without bound until OOM-killed.
        #[test]
        fn unauthenticated_scan_of_a_nonexistent_device_does_not_grow_state() {
            // Assert per-device, never on total STATE size: STATE is a
            // process-global, so a parallel sibling test would fail this
            // spuriously — exactly what comparing total length once did.
            let cfg = Arc::new(RwLock::new(Config::default()));
            let devices: Vec<String> = (0..25).map(|i| format!("zznotadrive{i:03}")).collect();
            for dev in &devices {
                let (code, _) = roundtrip(&cfg, "POST", &format!("/api/scan/{dev}"), None, &[]);
                assert_ne!(
                    code, 200,
                    "a nonexistent device must not be accepted for scanning"
                );
                assert!(
                    !crate::server::ripper::device_known(dev),
                    "no STATE entry may be created for unknown device {dev}"
                );
            }
            // Re-check every one after the whole loop: a handler that deferred
            // the insert (spawning a worker that registers later) would pass
            // the per-iteration check above and still leak all 25.
            let leaked: Vec<&String> = devices
                .iter()
                .filter(|d| crate::server::ripper::device_known(d))
                .collect();
            assert!(
                leaked.is_empty(),
                "fabricated devices left in STATE: {leaked:?}"
            );
        }

        // Unknown (never-enumerated) drives get 404 on every state-creating route, not 409 "busy".
        #[test]
        fn state_creating_routes_404_an_unknown_device() {
            let cfg = Arc::new(RwLock::new(Config::default()));
            for route in ["scan", "rip", "eject"] {
                let dev = format!("zzunknown{route}");
                let (code, body) =
                    roundtrip(&cfg, "POST", &format!("/api/{route}/{dev}"), None, &[]);
                assert_eq!(code, 404, "/api/{route} on an unknown device: {body}");
                assert!(!crate::server::ripper::device_known(&dev));
            }
        }

        #[test]
        fn stop_route_rejects_invalid_device_name() {
            let cfg = Arc::new(RwLock::new(Config::default()));
            let (code, _) = roundtrip(&cfg, "POST", "/api/stop/x", None, &[]);
            // "x" is too short (is_valid_device_name requires len 3..=64) -> 400.
            assert_eq!(code, 400, "a 1-char device name must be rejected");
        }

        // ── CSRF gate on POST ───────────────────────────────────────────

        #[test]
        fn cross_origin_post_is_rejected_403() {
            let cfg = Arc::new(RwLock::new(Config::default()));
            let (code, body) = roundtrip(
                &cfg,
                "POST",
                "/api/settings",
                Some("{}"),
                &[("Origin", "http://evil.example.com")],
            );
            assert_eq!(code, 403, "a cross-origin POST must be rejected");
            assert!(body.contains("cross-origin"));
        }

        // ── read_json_body size limit (via handle_settings_post) ────────

        #[test]
        fn oversize_request_body_is_rejected_413() {
            let tmp = tempfile::TempDir::new().unwrap();
            let cfg = cfg_in_tempdir(tmp.path());
            // One byte over MAX_REQUEST_BODY (1 MiB).
            let big = "x".repeat((MAX_REQUEST_BODY as usize) + 1);
            let (code, _) = roundtrip(&cfg, "POST", "/api/settings", Some(&big), &[]);
            assert_eq!(code, 413, "a body over MAX_REQUEST_BODY must be 413");
        }

        #[test]
        fn exact_cap_request_body_is_accepted_not_413() {
            // Exactly MAX_REQUEST_BODY bytes is in-spec, distinguishing
            // read_body_capped's `>` from a `>=` mutant. Pad valid JSON with
            // leading whitespace (serde_json skips it) out to the cap.
            let tmp = tempfile::TempDir::new().unwrap();
            let cfg = cfg_in_tempdir(tmp.path());
            let payload = r#"{"abort_on_lost_secs": 31}"#;
            let padding = " ".repeat((MAX_REQUEST_BODY as usize) - payload.len());
            let body = format!("{padding}{payload}");
            assert_eq!(body.len() as u64, MAX_REQUEST_BODY);
            let (code, resp) = roundtrip(&cfg, "POST", "/api/settings", Some(&body), &[]);
            assert_ne!(
                code, 413,
                "a body of exactly MAX_REQUEST_BODY bytes must not be rejected as too large"
            );
            assert_eq!(
                code, 200,
                "the exact-cap body is valid JSON and must succeed, got: {resp}"
            );
            assert_eq!(cfg.read().unwrap().abort_on_lost_secs, 31);
        }

        #[test]
        fn malformed_json_body_is_rejected_400() {
            let tmp = tempfile::TempDir::new().unwrap();
            let cfg = cfg_in_tempdir(tmp.path());
            let (code, body) = roundtrip(&cfg, "POST", "/api/settings", Some("{not json"), &[]);
            assert_eq!(code, 400);
            assert!(body.contains("invalid json"));
        }

        // ── handle_settings_post: the real save + the sentinel guard ────

        #[test]
        fn settings_post_persists_a_field_to_disk() {
            let tmp = tempfile::TempDir::new().unwrap();
            let cfg = cfg_in_tempdir(tmp.path());
            let (code, body) = roundtrip(
                &cfg,
                "POST",
                "/api/settings",
                Some(r#"{"abort_on_lost_secs": 30}"#),
                &[],
            );
            assert_eq!(code, 200, "a valid settings POST must succeed, got: {body}");
            assert!(body.contains("\"ok\":true"));
            // The in-memory config was mutated...
            assert_eq!(cfg.read().unwrap().abort_on_lost_secs, 30);
            // ...and persisted to settings.json on disk.
            let saved = std::fs::read_to_string(cfg.read().unwrap().settings_file())
                .expect("settings.json must be written");
            assert!(
                saved.contains("\"abort_on_lost_secs\""),
                "the persisted settings.json must carry the field"
            );
        }

        // A present-but-invalid on_read_error must not block the legacy
        // abort_on_error migration: the fallback was gated on key-exists but
        // assignment needed a string, so `null` silently skipped both.
        #[test]
        fn settings_post_null_on_read_error_still_applies_the_legacy_migration() {
            let tmp = tempfile::TempDir::new().unwrap();
            let cfg = cfg_in_tempdir(tmp.path());
            cfg.write().unwrap().on_read_error = "skip".to_string();

            let (code, body) = roundtrip(
                &cfg,
                "POST",
                "/api/settings",
                Some(r#"{"on_read_error": null, "abort_on_error": true}"#),
                &[],
            );

            assert_eq!(code, 200, "the PATCH reports success: {body}");
            assert_eq!(
                cfg.read().unwrap().on_read_error,
                "stop",
                "a null on_read_error carries no policy, so the legacy \
                 abort_on_error=true must still migrate to \"stop\" — \
                 answering 200 while applying neither is a save that looks \
                 like it worked and did nothing"
            );
        }

        #[test]
        fn settings_post_masked_keyserver_url_preserves_stored() {
            // The secret-sentinel guard, MASKED half: a POST carrying the
            // masked keyserver_url (containing SECRET_SENTINEL — the form GET
            // returns) must NOT clobber the stored token-bearing URL.
            let tmp = tempfile::TempDir::new().unwrap();
            let cfg = cfg_in_tempdir(tmp.path());
            let stored = "https://8.8.8.8/mysecrettoken/decode";
            cfg.write().unwrap().keyserver_url = stored.to_string();

            let masked = format!("https://8.8.8.8/{SECRET_SENTINEL}");
            let patch = format!(r#"{{"keyserver_url": "{masked}"}}"#);
            let (code, _) = roundtrip(&cfg, "POST", "/api/settings", Some(&patch), &[]);
            assert_eq!(code, 200);
            assert_eq!(
                cfg.read().unwrap().keyserver_url,
                stored,
                "a masked (sentinel) keyserver_url must leave the stored URL intact"
            );
        }

        // H4/H10: a non-bool webhook flag (or a malformed element) is a 400
        // naming the field; nothing in the patch may land.
        #[test]
        fn settings_post_rejects_non_bool_webhook_flag() {
            let tmp = tempfile::TempDir::new().unwrap();
            let cfg = cfg_in_tempdir(tmp.path());
            let stored = vec![crate::server::config::WebhookEntry {
                url: "https://discord.com/api/webhooks/1/secretA".into(),
                post_rip: true,
                post_mux: true,
                post_move: true,
            }];
            cfg.write().unwrap().webhook_urls = stored.clone();
            for (patch, field) in [
                (
                    r#"{"tmdb_api_key":"changed","webhook_urls":[{"url":"https://example.com/h","post_rip":"yes"}]}"#,
                    "webhook_urls[0].post_rip",
                ),
                (
                    r#"{"tmdb_api_key":"changed","webhook_urls":["https://example.com/a",{"url":"https://example.com/h","post_move":1}]}"#,
                    "webhook_urls[1].post_move",
                ),
                (
                    r#"{"tmdb_api_key":"changed","webhook_urls":[42]}"#,
                    "webhook_urls[0]",
                ),
            ] {
                let (code, body) = roundtrip(&cfg, "POST", "/api/settings", Some(patch), &[]);
                assert_eq!(code, 400, "{patch} must be rejected, got {code}: {body}");
                assert!(body.contains(field), "error must name {field}, got: {body}");
                let c = cfg.read().unwrap();
                assert_eq!(
                    c.webhook_urls, stored,
                    "stored webhooks must survive a rejected POST"
                );
                assert_ne!(
                    c.tmdb_api_key, "changed",
                    "no field may land on a rejected POST"
                );
            }
        }

        #[test]
        fn settings_post_real_keyserver_url_replaces_stored() {
            // The secret-sentinel guard, REAL-VALUE half: a POST with a genuine
            // (sentinel-free, SSRF-valid) keyserver_url replaces the stored one.
            let tmp = tempfile::TempDir::new().unwrap();
            let cfg = cfg_in_tempdir(tmp.path());
            cfg.write().unwrap().keyserver_url = "https://8.8.8.8/old/decode".to_string();

            // Public IP literal validates without DNS.
            let patch = r#"{"keyserver_url": "https://1.1.1.1/newtoken/decode"}"#;
            let (code, _) = roundtrip(&cfg, "POST", "/api/settings", Some(patch), &[]);
            assert_eq!(code, 200);
            assert_eq!(
                cfg.read().unwrap().keyserver_url,
                "https://1.1.1.1/newtoken/decode",
                "a real new keyserver_url must replace the stored one"
            );
        }

        #[test]
        fn settings_post_empty_keyserver_url_clears_it() {
            // The clear half: an empty (no-sentinel) keyserver_url writes
            // through, clearing the stored value (disables the online source).
            let tmp = tempfile::TempDir::new().unwrap();
            let cfg = cfg_in_tempdir(tmp.path());
            cfg.write().unwrap().keyserver_url = "https://8.8.8.8/token/decode".to_string();

            let (code, _) = roundtrip(
                &cfg,
                "POST",
                "/api/settings",
                Some(r#"{"keyserver_url": ""}"#),
                &[],
            );
            assert_eq!(code, 200);
            assert_eq!(
                cfg.read().unwrap().keyserver_url,
                "",
                "an empty keyserver_url must clear the stored value"
            );
        }

        #[test]
        fn settings_post_ssrf_url_is_rejected_400_and_not_stored() {
            // A non-sentinel keyserver_url pointing at an internal/loopback
            // host must be rejected before the write guard — stored value
            // untouched.
            let tmp = tempfile::TempDir::new().unwrap();
            let cfg = cfg_in_tempdir(tmp.path());
            cfg.write().unwrap().keyserver_url = "https://8.8.8.8/keep/decode".to_string();

            let (code, _) = roundtrip(
                &cfg,
                "POST",
                "/api/settings",
                Some(r#"{"keyserver_url": "http://127.0.0.1/admin"}"#),
                &[],
            );
            assert_eq!(code, 400, "an SSRF keyserver_url must be rejected");
            assert_eq!(
                cfg.read().unwrap().keyserver_url,
                "https://8.8.8.8/keep/decode",
                "a rejected keyserver_url must not mutate the stored value"
            );
        }

        #[test]
        fn settings_post_http_keyserver_url_is_rejected_like_the_rip_path() {
            // The rip gates on keysources' https-only validator; save must use the
            // same one, or an http:// URL saves fine and every rip has no online source.
            let tmp = tempfile::TempDir::new().unwrap();
            let cfg = cfg_in_tempdir(tmp.path());
            cfg.write().unwrap().keyserver_url = "https://8.8.8.8/keep/decode".to_string();

            let (code, body) = roundtrip(
                &cfg,
                "POST",
                "/api/settings",
                Some(r#"{"keyserver_url": "http://8.8.8.8/decode"}"#),
                &[],
            );
            assert_eq!(code, 400, "http:// keyserver_url must be rejected at save");
            assert!(body.contains("https://"), "error must say why: {body}");
            assert_eq!(
                cfg.read().unwrap().keyserver_url,
                "https://8.8.8.8/keep/decode"
            );
        }

        #[test]
        fn settings_post_keyserver_url_is_stored_trimmed() {
            // Validation runs on the trimmed URL, so the trimmed URL is what is stored.
            let tmp = tempfile::TempDir::new().unwrap();
            let cfg = cfg_in_tempdir(tmp.path());
            let patch = r#"{"keyserver_url": "  https://1.1.1.1/decode \n"}"#;
            let (code, _) = roundtrip(&cfg, "POST", "/api/settings", Some(patch), &[]);
            assert_eq!(code, 200);
            assert_eq!(cfg.read().unwrap().keyserver_url, "https://1.1.1.1/decode");
        }

        #[test]
        fn settings_post_unresolvable_masked_webhook_leaves_output_dir_unmutated() {
            // Red/green regression for the write-guard early-return defect:
            // resolution runs INSIDE `cfg.write()` after ~20 fields have
            // already mutated the live Config; a 400 must not leave that undone.
            let tmp = tempfile::TempDir::new().unwrap();
            let cfg = cfg_in_tempdir(tmp.path());
            let original_output_dir = cfg.read().unwrap().output_dir.clone();

            // A valid output_dir change ordered BEFORE an unresolvable masked
            // webhook entry in the patch (mirrors field-mutation order inside
            // the guard: output_dir first, webhook_urls near the end).
            let patch = serde_json::json!({
                "output_dir": "/mnt/zz-settings-guard-fixture-4711/output",
                "webhook_urls": ["https://hooks.slack.com/********"],
            })
            .to_string();
            let (code, body) = roundtrip(&cfg, "POST", "/api/settings", Some(&patch), &[]);

            assert_eq!(
                code, 400,
                "an unresolvable masked webhook_urls entry must be rejected, got: {body}"
            );
            assert_eq!(
                cfg.read().unwrap().output_dir,
                original_output_dir,
                "output_dir on the live Config must be UNCHANGED when the save is \
                 rejected — a partial in-memory mutation must never survive a 400"
            );
        }

        // ── handle_stop / handle_scan / handle_rip reach their handlers ──

        #[test]
        fn stop_route_reaches_handle_stop_with_its_own_drive_not_found() {
            // A well-formed device with no STATE entry must reach handle_stop,
            // which answers with ITS OWN "drive not found" body — proving the
            // route is wired to the handler, not just validated then dropped.
            let cfg = Arc::new(RwLock::new(Config::default()));
            let (code, body) = roundtrip(&cfg, "POST", "/api/stop/sr0", None, &[]);
            assert_eq!(code, 404);
            assert!(
                body.contains("drive not found"),
                "must be handle_stop's response, not the dispatch 404; got: {body}"
            );
            // And the dispatch fallthrough body must NOT appear.
            assert!(
                !body.contains("\"error\":\"not found\""),
                "a wired /api/stop/<dev> must not hit the dispatch 404"
            );
        }

        // ── handle_accept_loss: rejected claim must not arm the override ──

        #[test]
        fn accept_loss_rejected_while_busy_does_not_arm_marker() {
            // A rip in flight means try_claim_active loses, and the request
            // must 409 WITHOUT writing `.accept-loss` — before the fix the
            // marker was written first, leaving the override armed on disk.
            let tmp = tempfile::TempDir::new().unwrap();
            let cfg = cfg_in_tempdir(tmp.path());
            let device = "sgacceptloss1";
            let disc_name = "DamagedDisc";

            let staging_dir = {
                let c = cfg.read().unwrap();
                c.staging_device_dir(&crate::server::util::sanitize_path_compact(disc_name))
            };
            let dir = std::path::Path::new(&staging_dir);
            std::fs::create_dir_all(dir).unwrap();
            // Pre-existing terminal markers a real damaged/failed rip would
            // have left behind — these must survive a rejected accept too.
            std::fs::write(dir.join(ripper::staging::FAILED_MARKER), b"loss").unwrap();

            // Mark the device busy (as if a rip were already running) with a
            // known current disc name, exactly like a real in-flight rip.
            ripper::update_state(
                device,
                ripper::RipState {
                    device: device.to_string(),
                    status: "ripping".to_string(),
                    disc_name: disc_name.to_string(),
                    ..Default::default()
                },
            );

            let (code, _) = roundtrip(
                &cfg,
                "POST",
                &format!("/api/accept-loss/{device}"),
                None,
                &[],
            );
            assert_eq!(
                code, 409,
                "accept-loss on a busy device must be rejected, not silently dropped"
            );
            assert!(
                !dir.join(ripper::staging::ACCEPT_LOSS_MARKER).exists(),
                "a rejected accept-loss must NOT arm the one-shot override on disk"
            );
            assert!(
                dir.join(ripper::staging::FAILED_MARKER).exists(),
                "a rejected accept-loss must leave the existing .failed marker intact"
            );
        }

        #[test]
        fn accept_loss_transition_reopens_aborted_dir() {
            // A full-HTTP *successful* handle_accept_loss races its own spawned
            // rip worker against this test's read of state.json, so instead this
            // drives the exact read-modify-write closure it passes onward.
            use ripper::staging::{DiscState, StagingState, read_state, write_state};

            // AbortedLoss + a recorded failure reason + a nonzero restart count
            // — exactly what a real over-threshold abort leaves behind — must
            // reopen to Ripped with both annotations cleared.
            let tmp = tempfile::TempDir::new().unwrap();
            let aborted_dir = tmp.path().join("aborted");
            std::fs::create_dir_all(&aborted_dir).unwrap();
            let mut st = DiscState::new(StagingState::AbortedLoss);
            st.failure_reason = Some("main movie: 42s lost over threshold".to_string());
            st.restart_count = 2;
            write_state(&aborted_dir, &st);

            let mut aborted_after = read_state(&aborted_dir).unwrap();
            ripper::staging::apply_accept_loss_reopen(&mut aborted_after);
            write_state(&aborted_dir, &aborted_after);

            let after = read_state(&aborted_dir).unwrap();
            assert_eq!(
                after.state,
                StagingState::Ripped,
                "an AbortedLoss dir must reopen to Ripped so the resume re-mux \
                 (not the abort gate) picks it up"
            );
            assert_eq!(
                after.failure_reason, None,
                "accept-loss must clear the recorded failure reason"
            );
            assert_eq!(
                after.restart_count, 0,
                "accept-loss must reset the restart counter"
            );

            // A Done dir (already muxed) must NOT be pulled back into Ripped —
            // guards against a match-arm regression reopening every terminal
            // state, not just the abort/failed ones.
            let done_dir = tmp.path().join("done");
            std::fs::create_dir_all(&done_dir).unwrap();
            let mut done_st = DiscState::new(StagingState::Done);
            done_st.restart_count = 1;
            write_state(&done_dir, &done_st);

            let mut done_after = read_state(&done_dir).unwrap();
            ripper::staging::apply_accept_loss_reopen(&mut done_after);
            write_state(&done_dir, &done_after);
            let done_after = read_state(&done_dir).unwrap();

            assert_eq!(
                done_after.state,
                StagingState::Done,
                "a Done dir must NOT be reopened by the accept-loss transition"
            );
        }

        // ── FIX 5: accept-loss must respect the .muxing ownership marker —
        // apply_accept_loss_reopen and write_failed_marker are both
        // lock-free RMWs on state.json, so arming mid-mux can clobber a quarantine.
        #[test]
        fn accept_loss_guard_refuses_while_muxing() {
            use ripper::staging::{DiscState, StagingState, write_state};
            let tmp = tempfile::TempDir::new().unwrap();

            // A dir the worker is actively muxing: the guard must see has_muxing.
            let muxing_dir = tmp.path().join("muxing");
            std::fs::create_dir_all(&muxing_dir).unwrap();
            let mut st = DiscState::new(StagingState::Ripped);
            st.muxing = true;
            write_state(&muxing_dir, &st);
            assert!(
                ripper::staging::muxing_status(&muxing_dir).unwrap(),
                "an actively-muxing dir must report is_muxing so accept-loss refuses (409)"
            );

            // A stable terminal dir (mux finished): the guard must let it through.
            let failed_dir = tmp.path().join("failed");
            std::fs::create_dir_all(&failed_dir).unwrap();
            assert!(ripper::staging::write_failed_marker(&failed_dir, "E6008"));
            assert!(
                !ripper::staging::muxing_status(&failed_dir).unwrap(),
                "a settled (non-muxing) dir must NOT report is_muxing; accept-loss proceeds"
            );
        }

        // ── handle_title_override: a title override with no media_type must
        // fall back to the disc's detected tmdb_media_type, not default to
        // "movie" (which collapses every TV episode into one Show (Year).mkv).
        #[test]
        fn title_override_omitted_media_type_preserves_detected_tv() {
            let cfg = Arc::new(RwLock::new(Config::default()));
            let device = "sgtitleovrtvpreserve1";
            ripper::update_state(
                device,
                ripper::RipState {
                    device: device.to_string(),
                    tmdb_media_type: "tv".to_string(),
                    ..Default::default()
                },
            );

            let (code, _) = roundtrip(
                &cfg,
                "POST",
                &format!("/api/title/{device}"),
                Some(r#"{"title":"Endeavour","tmdb_id":0}"#),
                &[],
            );
            assert_eq!(code, 200, "a known device must accept the override");

            let stored = ripper::take_title_override(device)
                .expect("handle_title_override must record an override");
            assert_eq!(
                stored.media_type, "tv",
                "omitting media_type must preserve the disc's detected \
                 tmdb_media_type (tv), not fall back to movie"
            );

            crate::server::ripper::STATE.lock().unwrap().remove(device);
        }

        #[test]
        fn title_override_omitted_media_type_defaults_movie_when_unknown() {
            let cfg = Arc::new(RwLock::new(Config::default()));
            let device = "sgtitleovrnodetect2";
            ripper::update_state(
                device,
                ripper::RipState {
                    device: device.to_string(),
                    // tmdb_media_type left empty: nothing detected yet.
                    ..Default::default()
                },
            );

            let (code, _) = roundtrip(
                &cfg,
                "POST",
                &format!("/api/title/{device}"),
                Some(r#"{"title":"X","tmdb_id":0}"#),
                &[],
            );
            assert_eq!(code, 200, "a known device must accept the override");

            let stored = ripper::take_title_override(device)
                .expect("handle_title_override must record an override");
            assert_eq!(
                stored.media_type, "movie",
                "with no detected media_type, omission must fall back to movie"
            );

            crate::server::ripper::STATE.lock().unwrap().remove(device);
        }

        #[test]
        fn title_override_explicit_media_type_is_honored() {
            let cfg = Arc::new(RwLock::new(Config::default()));
            let device = "sgtitleovrexplicit3";
            // Detected as movie, but the operator explicitly overrides to tv:
            // the explicit field must win over whatever STATE says.
            ripper::update_state(
                device,
                ripper::RipState {
                    device: device.to_string(),
                    tmdb_media_type: "movie".to_string(),
                    ..Default::default()
                },
            );

            let (code, _) = roundtrip(
                &cfg,
                "POST",
                &format!("/api/title/{device}"),
                Some(r#"{"title":"X","media_type":"tv","tmdb_id":0}"#),
                &[],
            );
            assert_eq!(code, 200, "a known device must accept the override");

            let stored = ripper::take_title_override(device)
                .expect("handle_title_override must record an override");
            assert_eq!(
                stored.media_type, "tv",
                "an explicit media_type in the request body must be honored \
                 over the detected tmdb_media_type"
            );

            crate::server::ripper::STATE.lock().unwrap().remove(device);
        }

        // ── handle_settings_post: pre-write-guard validation rejections ──

        #[test]
        fn settings_post_rejects_invalid_output_format_enum() {
            let tmp = tempfile::TempDir::new().unwrap();
            let cfg = cfg_in_tempdir(tmp.path());
            let before = cfg.read().unwrap().output_format.clone();
            let (code, body) = roundtrip(
                &cfg,
                "POST",
                "/api/settings",
                Some(r#"{"output_format": "garbage"}"#),
                &[],
            );
            assert_eq!(code, 400, "an out-of-vocabulary enum must be rejected");
            assert!(body.contains("invalid value for output_format"));
            assert_eq!(
                cfg.read().unwrap().output_format,
                before,
                "a rejected enum must not mutate the live config"
            );
        }

        #[test]
        fn settings_post_rejects_out_of_range_port() {
            let tmp = tempfile::TempDir::new().unwrap();
            let cfg = cfg_in_tempdir(tmp.path());
            let (code, body) = roundtrip(
                &cfg,
                "POST",
                "/api/settings",
                Some(r#"{"port": 70000}"#),
                &[],
            );
            assert_eq!(code, 400, "a port outside 1..=65535 must be rejected");
            assert!(body.contains("port must be 1..=65535"));
        }

        #[test]
        fn settings_post_rejects_relative_output_dir() {
            let tmp = tempfile::TempDir::new().unwrap();
            let cfg = cfg_in_tempdir(tmp.path());
            let (code, body) = roundtrip(
                &cfg,
                "POST",
                "/api/settings",
                Some(r#"{"output_dir": "relative/path"}"#),
                &[],
            );
            assert_eq!(code, 400, "a non-absolute mount root must be rejected");
            assert!(body.contains("output_dir must be an absolute path"));
        }

        #[test]
        fn settings_post_rejects_parent_dir_in_movie_dir() {
            let tmp = tempfile::TempDir::new().unwrap();
            let cfg = cfg_in_tempdir(tmp.path());
            let (code, body) = roundtrip(
                &cfg,
                "POST",
                "/api/settings",
                Some(r#"{"movie_dir": "../escape"}"#),
                &[],
            );
            assert_eq!(code, 400, "a climbing sub-directory name must be rejected");
            assert!(body.contains("movie_dir must not contain '..'"));
        }

        #[test]
        fn settings_post_rejects_keydb_path_without_cfg_extension() {
            let tmp = tempfile::TempDir::new().unwrap();
            let cfg = cfg_in_tempdir(tmp.path());
            let (code, body) = roundtrip(
                &cfg,
                "POST",
                "/api/settings",
                Some(r#"{"keydb_path": "/data/keys.txt"}"#),
                &[],
            );
            assert_eq!(code, 400, "a non-.cfg keydb path must be rejected");
            assert!(body.contains("keydb_path must be an absolute .cfg path"));
        }

        // ── handle_settings_post: the write-guard field application ──────

        #[test]
        fn settings_post_applies_scalar_and_clamped_fields() {
            let tmp = tempfile::TempDir::new().unwrap();
            let cfg = cfg_in_tempdir(tmp.path());
            let abs_out = tmp.path().join("out");
            let patch = serde_json::json!({
                "auto_eject": true,
                "main_feature": true,
                "capture_without_keys": true,
                "keep_iso": true,
                // Over the ceilings — each must clamp, not persist raw.
                "max_retries": 99,
                "min_length_secs": 40 * 24 * 3600u64,
                "decrypt_threads": 9999,
                "log_retention_days": 99999u64,
                "movie_dir": "films",
                "output_dir": abs_out.to_string_lossy(),
                "port": 9999,
                "output_format": "iso",
            })
            .to_string();
            let (code, body) = roundtrip(&cfg, "POST", "/api/settings", Some(&patch), &[]);
            assert_eq!(code, 200, "a valid multi-field patch must succeed: {body}");
            let c = cfg.read().unwrap();
            assert!(c.auto_eject && c.main_feature && c.capture_without_keys && c.keep_iso);
            assert_eq!(c.max_retries, 10, "max_retries clamps to 10");
            assert_eq!(
                c.min_length_secs,
                30 * 24 * 3600,
                "min_length_secs clamps to MAX_DURATION_SECS (30 days)"
            );
            assert_eq!(c.decrypt_threads, 256, "decrypt_threads clamps to 256");
            assert_eq!(
                c.log_retention_days, 3650,
                "log_retention_days clamps to 10 years"
            );
            assert_eq!(c.movie_dir, "films");
            assert_eq!(c.output_dir, abs_out.to_string_lossy());
            assert_eq!(c.port, 9999);
            assert_eq!(c.output_format, "iso");
        }

        #[test]
        fn settings_post_tmdb_api_key_sentinel_is_ignored_but_real_value_applied() {
            let tmp = tempfile::TempDir::new().unwrap();
            let cfg = cfg_in_tempdir(tmp.path());
            cfg.write().unwrap().tmdb_api_key = "original-secret".to_string();

            // The masked sentinel round-trip must NOT clobber the stored key.
            let masked = format!(r#"{{"tmdb_api_key": "{SECRET_SENTINEL}"}}"#);
            let (code, _) = roundtrip(&cfg, "POST", "/api/settings", Some(&masked), &[]);
            assert_eq!(code, 200);
            assert_eq!(
                cfg.read().unwrap().tmdb_api_key,
                "original-secret",
                "the redaction sentinel must be ignored, not stored"
            );

            // A genuine new value replaces it.
            let (code, _) = roundtrip(
                &cfg,
                "POST",
                "/api/settings",
                Some(r#"{"tmdb_api_key": "brand-new-key"}"#),
                &[],
            );
            assert_eq!(code, 200);
            assert_eq!(cfg.read().unwrap().tmdb_api_key, "brand-new-key");
        }

        #[test]
        fn settings_post_rip_mode_single_zeroes_retries_multi_bumps_to_one() {
            let tmp = tempfile::TempDir::new().unwrap();
            let cfg = cfg_in_tempdir(tmp.path());

            let (code, _) = roundtrip(
                &cfg,
                "POST",
                "/api/settings",
                Some(r#"{"rip_mode": "single", "max_retries": 5}"#),
                &[],
            );
            assert_eq!(code, 200);
            assert_eq!(
                cfg.read().unwrap().max_retries,
                0,
                "single mode forces zero retries"
            );

            let (code, _) = roundtrip(
                &cfg,
                "POST",
                "/api/settings",
                Some(r#"{"rip_mode": "multi", "max_retries": 0}"#),
                &[],
            );
            assert_eq!(code, 200);
            assert_eq!(
                cfg.read().unwrap().max_retries,
                1,
                "multi mode with zero retries clamps up to 1"
            );
        }

        // ── handle_system_info (GET /api/system) ────────────────────────

        #[test]
        fn system_info_serves_queues_and_reflects_a_done_staging_dir() {
            let tmp = tempfile::TempDir::new().unwrap();
            let cfg = cfg_in_tempdir(tmp.path());
            // Point staging at a real dir holding one settled (`.done`) rip so
            // build_queue_views emits a Move-queue row.
            let staging = tmp.path().join("staging");
            let done_dir = staging.join("Some_Movie_2024");
            std::fs::create_dir_all(&done_dir).unwrap();
            std::fs::write(done_dir.join(".done"), b"").unwrap();
            cfg.write().unwrap().staging_dir = staging.to_string_lossy().to_string();

            let (code, body) = roundtrip(&cfg, "GET", "/api/system", None, &[]);
            assert_eq!(code, 200, "GET /api/system must be served: {body}");
            let v: serde_json::Value = serde_json::from_str(&body).expect("system info is JSON");
            assert!(v.get("move_queue").is_some(), "must carry a move_queue");
            assert!(v.get("mux_queue").is_some(), "must carry a mux_queue");
            assert!(
                v.get("debug_enabled").is_some(),
                "must report the debug toggle state"
            );
            let mq = v["move_queue"].as_array().expect("move_queue is an array");
            assert!(
                mq.iter()
                    .any(|e| e.as_str().unwrap_or("").contains("Some Movie 2024")),
                "the settled .done dir must show in the move queue, got: {mq:?}"
            );
        }

        // ── handle_device_log (GET /api/logs/<device>) ──────────────────

        #[test]
        fn device_log_route_serves_logged_lines_for_a_valid_device() {
            // A unique device name so parallel tests can't share the ring.
            let device = "zzweblogdev01";
            crate::server::log::device_log(device, "unit-test-device-log-marker");
            let cfg = Arc::new(RwLock::new(Config::default()));
            let (code, body) = roundtrip(&cfg, "GET", &format!("/api/logs/{device}"), None, &[]);
            assert_eq!(code, 200, "a valid device log must be served");
            assert!(
                body.contains("unit-test-device-log-marker"),
                "the served log must carry the logged line, got: {body}"
            );
        }

        // ── move/mux error clear endpoints ──────────────────────────────

        #[test]
        fn error_clear_endpoints_dispatch() {
            // Clear-all wipes process-global MOVE_ERRORS/MUX_ERRORS; hold the
            // shared lock so parallel mover/muxer/resume tests aren't wiped.
            let _g = crate::server::mover::TEST_STATE_LOCK
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            let cfg = Arc::new(RwLock::new(Config::default()));
            let (c1, b1) = roundtrip(&cfg, "POST", "/api/move-errors/clear-all", None, &[]);
            assert_eq!(c1, 200);
            assert!(b1.contains("\"ok\":true"));
            let (c2, b2) = roundtrip(&cfg, "POST", "/api/mux-errors/clear-all", None, &[]);
            assert_eq!(c2, 200);
            assert!(b2.contains("\"ok\":true"));
            // clear-one with no path= param is a 400.
            let (c3, b3) = roundtrip(&cfg, "POST", "/api/move-errors/clear?x=1", None, &[]);
            assert_eq!(c3, 400);
            assert!(b3.contains("missing path"));
        }

        // ── SSE first frame (GET /events): it loops forever, so roundtrip
        // (reads to EOF) would hang. Run the handler on its own thread, read
        // the first SSE frame, then drop the socket so the next write fails.

        fn read_first_sse_frame(r: &mut impl Read) -> Vec<u8> {
            let mut buf = Vec::new();
            let mut chunk = [0u8; 1024];
            loop {
                let n = r.read(&mut chunk).expect("read first frame");
                if n == 0 {
                    break;
                }
                buf.extend_from_slice(&chunk[..n]);
                let find = |hay: &[u8], pat: &[u8]| hay.windows(pat.len()).position(|w| w == pat);
                if let Some(h) = find(&buf, b"\r\n\r\n") {
                    let body = h + 4;
                    // An SSE frame ends at its blank line, never at a `}`.
                    if let Some(end) = find(&buf[body..], b"\n\n") {
                        buf.truncate(body + end + 2);
                        break;
                    }
                }
            }
            buf
        }

        // Global STATE grows under parallel tests, so the frame can span reads; a read ending
        // on a nested `}` must not be taken as the end of the frame.
        #[test]
        fn read_first_sse_frame_waits_for_the_frame_terminator() {
            struct Chunks(Vec<&'static [u8]>);
            impl Read for Chunks {
                fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
                    if self.0.is_empty() {
                        return Ok(0);
                    }
                    let c = self.0.remove(0);
                    out[..c.len()].copy_from_slice(c);
                    Ok(c.len())
                }
            }
            let mut r = Chunks(vec![
                b"HTTP/1.1 200 OK\r\n\r\ndata: {\"a\":{\"b\":1}",
                b",\"c\":2}\n\n",
                b"data: {}\n\n",
            ]);
            let buf = read_first_sse_frame(&mut r);
            assert_eq!(
                String::from_utf8_lossy(&buf),
                "HTTP/1.1 200 OK\r\n\r\ndata: {\"a\":{\"b\":1},\"c\":2}\n\n"
            );
        }

        #[test]
        fn sse_emits_a_json_state_first_frame() {
            let cfg = Arc::new(RwLock::new(Config::default()));
            let server = Server::http("127.0.0.1:0").expect("bind loopback server");
            let addr = server.server_addr().to_ip().expect("ip addr");

            let handler = std::thread::spawn(move || {
                let request = server.recv().expect("recv request");
                handle_sse(request, &cfg);
            });

            let mut stream = TcpStream::connect(addr).expect("connect");
            stream
                .set_read_timeout(Some(std::time::Duration::from_secs(5)))
                .unwrap();
            stream
                .write_all(b"GET /events HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n")
                .expect("write request");
            stream.flush().ok();

            let buf = read_first_sse_frame(&mut stream);
            drop(stream);
            handler.join().expect("handler thread");

            let raw = String::from_utf8_lossy(&buf);
            assert!(
                raw.contains("text/event-stream"),
                "the response must declare the SSE content type, got: {raw}"
            );
            let frame = raw
                .split("\r\n\r\n")
                .nth(1)
                .expect("an SSE body frame must follow the headers");
            let json = frame
                .trim_start()
                .strip_prefix("data: ")
                .expect("the first frame must be a `data: ` event")
                .trim();
            let _: serde_json::Value = serde_json::from_str(json)
                .expect("the first SSE frame must carry valid state JSON");
        }
    }
}

fn text_response(request: tiny_http::Request, body: &str) {
    let header =
        Header::from_bytes(&b"Content-Type"[..], &b"text/plain; charset=utf-8"[..]).unwrap();
    let response = Response::from_string(body).with_header(header).with_header(
        Header::from_bytes(
            &b"Cache-Control"[..],
            &b"no-store, no-cache, must-revalidate"[..],
        )
        .unwrap(),
    );
    let _ = request.respond(response);
}

// Cached result of build_queue_views, shared across every concurrent
// /events (SSE) client and /api/state poller — trades a small, bounded
// staleness window for turning N concurrent directory scans into one.
struct QueueViewSnapshot {
    computed_at: std::time::Instant,
    mux_queue: Vec<String>,
    move_queue: Vec<String>,
    mux_full: usize,
    move_full: usize,
}

impl QueueViewSnapshot {
    fn views(&self) -> (Vec<String>, Vec<String>, usize, usize) {
        (
            self.mux_queue.clone(),
            self.move_queue.clone(),
            self.mux_full,
            self.move_full,
        )
    }
}

struct QueueViewCache {
    // None only between a key's first scan starting and finishing — nothing to serve yet.
    snapshot: Option<QueueViewSnapshot>,
    // Single-flight marker: a timestamp (not a bool, which only the setter could clear) for
    // when the owning scan started; trusted for QUEUE_VIEW_REFRESH_DEADLINE.
    refresh_started: Option<std::time::Instant>,
    // Threads inside scan_queue_views for this key now; maintained by
    // RefreshGuard and capped at QUEUE_VIEW_MAX_REFRESHERS.
    refreshers: usize,
}

// RAII owner of a key's single-flight marker: Drop (not the happy path)
// releases it, so a scan that panics inside read_dir hands the key back
// instead of stranding it until the deadline.
struct RefreshGuard {
    key: String,
    claimed_at: std::time::Instant,
}

impl Drop for RefreshGuard {
    fn drop(&mut self) {
        {
            let mut map = QUEUE_VIEW_CACHE.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(entry) = map.get_mut(&self.key) {
                entry.refreshers = entry.refreshers.saturating_sub(1);
                // Only clear the marker if it is still OURS. A refresher that
                // was presumed dead and taken over must not clear the marker
                // of the live refresher that replaced it.
                if entry.refresh_started == Some(self.claimed_at) {
                    entry.refresh_started = None;
                }
            }
        }
        QUEUE_VIEW_REFRESHED.notify_all();
    }
}

// Keyed by staging_dir rather than a single slot, since the path can change at runtime and two
// different staging dirs must not evict each other's cached scan.
static QUEUE_VIEW_CACHE: Lazy<std::sync::Mutex<std::collections::HashMap<String, QueueViewCache>>> =
    Lazy::new(|| std::sync::Mutex::new(std::collections::HashMap::new()));

// Signalled when an in-flight scan completes; only the cold-start case (no
// snapshot at all) waits on it — any stale snapshot is served immediately.
static QUEUE_VIEW_REFRESHED: Lazy<std::sync::Condvar> = Lazy::new(std::sync::Condvar::new);

// Safety valve for the cold-start wait: gives up after this long rather than parking forever.
// Deliberately does NOT scan for itself (that abandons single-flight).
const QUEUE_VIEW_COLD_WAIT: std::time::Duration = std::time::Duration::from_secs(5);

// How long the per-key single-flight marker is TRUSTED before a refresher is presumed dead
// (panicked/wedged) and the next caller may take it over.
const QUEUE_VIEW_REFRESH_DEADLINE: std::time::Duration = std::time::Duration::from_secs(30);

// Hard ceiling on threads inside scan_queue_views for ONE key: the original
// plus one retry, so a takeover that also wedges cannot pile up unboundedly.
const QUEUE_VIEW_MAX_REFRESHERS: usize = 2;

// How long a cached queue view is served before the next caller triggers a
// fresh scan; kept under the ~1s SSE tick so staleness stays invisible.
const QUEUE_VIEW_CACHE_TTL: std::time::Duration = std::time::Duration::from_millis(750);

// How long an idle key is RETAINED, far longer than QUEUE_VIEW_CACHE_TTL, so the Phase-3 prune
// doesn't evict a still-active key mid-stale-while-revalidate.
const QUEUE_VIEW_CACHE_RETAIN: std::time::Duration = std::time::Duration::from_secs(300);

// Test-only seam around the staging-dir scan a cache miss performs, keyed by staging dir so
// tests stay isolated. Lets a test slow one dir's scan and count how many scans it received.
#[cfg(test)]
pub(crate) mod queue_scan_probe {
    use std::collections::HashMap;
    use std::sync::Mutex;

    #[derive(Default, Clone, Copy)]
    pub struct Probe {
        pub delay_ms: u64,
        /// Incremented on scan ENTRY (before the delay), so a waiting test
        /// can tell that the slow scan is genuinely in flight.
        pub scans: usize,
        /// Per-dir override of `QUEUE_VIEW_REFRESH_DEADLINE` (0 = use the
        /// production value). Keyed by dir like everything else here so a
        /// timing test cannot perturb another test's staging dir.
        pub refresh_deadline_ms: u64,
        /// Per-dir override of `QUEUE_VIEW_COLD_WAIT` (0 = production value).
        pub cold_wait_ms: u64,
    }

    static PROBES: Mutex<Option<HashMap<String, Probe>>> = Mutex::new(None);

    fn with<R>(f: impl FnOnce(&mut HashMap<String, Probe>) -> R) -> R {
        let mut g = PROBES.lock().unwrap_or_else(|e| e.into_inner());
        f(g.get_or_insert_with(HashMap::new))
    }

    /// Arm the probe for `dir`; every subsequent scan of it sleeps `delay_ms`.
    pub fn arm(dir: &str, delay_ms: u64) {
        with(|m| {
            m.entry(dir.to_string()).or_default().delay_ms = delay_ms;
        });
    }

    /// Number of scans this dir has received since it was first armed.
    pub fn scans(dir: &str) -> usize {
        with(|m| m.get(dir).map(|p| p.scans).unwrap_or(0))
    }

    /// Shorten (or lengthen) how long this dir's in-flight refresh marker is
    /// trusted before it is presumed dead. Lets a test observe the
    /// wedged-refresher takeover in milliseconds instead of the production
    /// deadline.
    pub fn set_refresh_deadline(dir: &str, ms: u64) {
        with(|m| {
            m.entry(dir.to_string()).or_default().refresh_deadline_ms = ms;
        });
    }

    /// Shorten how long a COLD caller (nothing at all to serve) parks before
    /// giving up on the in-flight scan.
    pub fn set_cold_wait(dir: &str, ms: u64) {
        with(|m| {
            m.entry(dir.to_string()).or_default().cold_wait_ms = ms;
        });
    }

    /// `(refresh_deadline_ms, cold_wait_ms)`; 0 means "use production".
    pub fn overrides(dir: &str) -> (u64, u64) {
        with(|m| {
            m.get(dir)
                .map(|p| (p.refresh_deadline_ms, p.cold_wait_ms))
                .unwrap_or((0, 0))
        })
    }

    /// Called from the production scan path. Never holds the probe lock
    /// across the sleep.
    pub fn enter(dir: &str) {
        let delay = with(|m| match m.get_mut(dir) {
            Some(p) => {
                p.scans += 1;
                p.delay_ms
            }
            None => 0,
        });
        if delay > 0 {
            std::thread::sleep(std::time::Duration::from_millis(delay));
        }
    }
}

/// How long an in-flight refresh marker for `dir` is trusted.
#[cfg(not(test))]
fn queue_view_refresh_deadline(_dir: &str) -> std::time::Duration {
    QUEUE_VIEW_REFRESH_DEADLINE
}

#[cfg(test)]
fn queue_view_refresh_deadline(dir: &str) -> std::time::Duration {
    match queue_scan_probe::overrides(dir).0 {
        0 => QUEUE_VIEW_REFRESH_DEADLINE,
        ms => std::time::Duration::from_millis(ms),
    }
}

/// How long a cold caller parks for someone else's first scan of `dir`.
#[cfg(not(test))]
fn queue_view_cold_wait(_dir: &str) -> std::time::Duration {
    QUEUE_VIEW_COLD_WAIT
}

#[cfg(test)]
fn queue_view_cold_wait(dir: &str) -> std::time::Duration {
    match queue_scan_probe::overrides(dir).1 {
        0 => QUEUE_VIEW_COLD_WAIT,
        ms => std::time::Duration::from_millis(ms),
    }
}

/// The scan a cache miss performs, behind a test-only instrumentation seam.
fn scan_queue_views(staging_dir: &str) -> (Vec<String>, Vec<String>, usize, usize) {
    #[cfg(test)]
    queue_scan_probe::enter(staging_dir);
    build_queue_views(staging_dir)
}

// build_queue_views, shared across callers within QUEUE_VIEW_CACHE_TTL, single-flighted with no
// lock held across the scan so a slow staging dir can't park /api/state (the HEALTHCHECK
// probe).
fn build_queue_views_cached(staging_dir: &str) -> (Vec<String>, Vec<String>, usize, usize) {
    // Phase 1 — decide, under the lock, whether THIS caller scans. The lock
    // is never held across `scan_queue_views`: a slow-to-enumerate staging
    // dir must not park `/api/state`, which `--healthcheck` probes.
    enum Decision {
        /// Serve this (possibly stale) snapshot; do not touch the disk.
        Serve(Vec<String>, Vec<String>, usize, usize),
        /// This caller owns the refresh.
        Scan,
        /// Cold key with a scan already in flight — wait for its result.
        Wait,
    }

    let deadline = queue_view_refresh_deadline(staging_dir);
    let cold_wait = queue_view_cold_wait(staging_dir);
    let mut map = QUEUE_VIEW_CACHE.lock().unwrap_or_else(|e| e.into_inner());
    let waited_from = std::time::Instant::now();
    let claimed_at = loop {
        let decision = match map.get_mut(staging_dir) {
            // Never seen: claim the slot and scan.
            None => Decision::Scan,
            Some(entry) => {
                // "A refresh is in flight" is a marker YOUNGER than the
                // deadline. Past it the owner is presumed dead (panicked or
                // wedged in `read_dir`), so its claim no longer justifies waiting.
                let live_refresh = entry
                    .refresh_started
                    .is_some_and(|t| t.elapsed() < deadline);
                // Serve a snapshot if fresh, OR stale with a live refresh in
                // flight — queueing behind someone else's I/O is the stall we're
                // avoiding, and a sub-second-stale view is invisible to a poller.
                let serve = entry
                    .snapshot
                    .as_ref()
                    .filter(|s| s.computed_at.elapsed() < QUEUE_VIEW_CACHE_TTL || live_refresh)
                    .map(|s| s.views());
                // The takeover a dead marker permits is itself capped: past
                // the max threads already inside `read_dir` for this key,
                // adding another only burns another HTTP worker.
                let may_scan = !live_refresh && entry.refreshers < QUEUE_VIEW_MAX_REFRESHERS;
                match serve {
                    Some((mux, mv, mux_full, move_full)) => {
                        Decision::Serve(mux, mv, mux_full, move_full)
                    }
                    // Stale (or cold) with no live refresher: take the key
                    // over, unless we are already at the refresher cap.
                    None if may_scan => Decision::Scan,
                    // Capped out but we have SOMETHING: never block a warm
                    // caller — hand back the stale view.
                    None => match entry.snapshot.as_ref().map(|s| s.views()) {
                        Some((mux, mv, mux_full, move_full)) => {
                            Decision::Serve(mux, mv, mux_full, move_full)
                        }
                        None => Decision::Wait,
                    },
                }
            }
        };
        match decision {
            Decision::Serve(mux, mv, mux_full, move_full) => {
                return (mux, mv, mux_full, move_full);
            }
            Decision::Scan => {
                let claimed_at = std::time::Instant::now();
                let entry = map
                    .entry(staging_dir.to_string())
                    .or_insert_with(|| QueueViewCache {
                        snapshot: None,
                        refresh_started: None,
                        refreshers: 0,
                    });
                entry.refresh_started = Some(claimed_at);
                entry.refreshers += 1;
                break claimed_at;
            }
            // Cold key, live scan in flight: wait for its result instead of
            // launching a duplicate one. The wait RELEASES the map lock, so
            // every other staging dir and warm reader keeps running.
            Decision::Wait => {
                if waited_from.elapsed() >= cold_wait {
                    // Give up on THIS call rather than start a competing scan:
                    // scanning anyway consumes an HTTP worker every `cold_wait`
                    // while wedged, which is how HEALTHCHECK restarts the daemon.
                    tracing::warn!(
                        staging_dir = %staging_dir,
                        waited_ms = waited_from.elapsed().as_millis() as u64,
                        "queue view still cold after waiting for an in-flight staging scan; \
                         serving an empty queue view for this request"
                    );
                    return (Vec::new(), Vec::new(), 0, 0);
                }
                let (m, _) = QUEUE_VIEW_REFRESHED
                    .wait_timeout(map, std::time::Duration::from_millis(50))
                    .unwrap_or_else(|e| e.into_inner());
                map = m;
            }
        }
    };
    drop(map);
    // Marker released on EVERY exit from here on, panic included.
    let _refresh_guard = RefreshGuard {
        key: staging_dir.to_string(),
        claimed_at,
    };

    // Phase 2 — scan with NO lock held.
    let (mux_queue, move_queue, mux_full, move_full) = scan_queue_views(staging_dir);

    // Phase 3 — publish. The single-flight marker is released by
    // `_refresh_guard` as this function returns.
    let mut map = QUEUE_VIEW_CACHE.lock().unwrap_or_else(|e| e.into_inner());
    // Opportunistic prune so ever-changing staging paths (or a test suite's
    // distinct tempdirs) can't grow this map forever. Keep this key, anything
    // scanning, and any snapshot within RETAIN (not the serve TTL — see const).
    map.retain(|k, v| {
        k == staging_dir
            || v.refreshers > 0
            || v.snapshot
                .as_ref()
                .is_some_and(|s| s.computed_at.elapsed() < QUEUE_VIEW_CACHE_RETAIN)
    });
    let entry = map
        .entry(staging_dir.to_string())
        .or_insert_with(|| QueueViewCache {
            snapshot: None,
            refresh_started: None,
            refreshers: 0,
        });
    // A presumed-dead refresher that comes back to life must not overwrite the
    // fresher snapshot its replacement already published. Anything computed
    // after we started is at least as current as what we hold.
    let superseded = entry
        .snapshot
        .as_ref()
        .is_some_and(|s| s.computed_at > claimed_at);
    if !superseded {
        entry.snapshot = Some(QueueViewSnapshot {
            computed_at: std::time::Instant::now(),
            mux_queue: mux_queue.clone(),
            move_queue: move_queue.clone(),
            mux_full,
            move_full,
        });
    }
    drop(map);
    QUEUE_VIEW_REFRESHED.notify_all();
    (mux_queue, move_queue, mux_full, move_full)
}

fn get_state_json(staging_dir: &str) -> String {
    // Recover-and-proceed on poison, like every other STATE consumer. This
    // was the ONE site that bailed with `Err(_) => "{}"`, forever returning
    // a blank dashboard with a permanently green HEALTHCHECK; it's still readable.
    let state = ripper::STATE.lock().unwrap_or_else(|e| e.into_inner());
    // `_move` is now an ARRAY of per-artifact bars (movie file + companion ISO
    // get one each); clone the whole Vec (empty = nothing moving). Recover on
    // poison (MOVE_STATE convention) — `.ok()` would drop live bars process-wide.
    let move_state = crate::server::mover::MOVE_STATE
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone();
    // Mux progress rides on the synthetic `_mux` device key in STATE (a
    // RipState seeded by the mux worker), serialized as part of `state`.
    // There is no separate live MuxState struct.
    let mut obj = serde_json::to_value(&*state).unwrap_or_else(|_| serde_json::json!({}));
    if !move_state.is_empty() {
        obj["_move"] = serde_json::to_value(&move_state).unwrap_or_default();
    }
    // Release the STATE lock before the staging-dir scan below: it does
    // filesystem I/O, and holding STATE across it would serialize the
    // ripper's once-per-tick progress writes against this once-per-second scan.
    drop(state);
    // SINGLE-SOURCE STAGE VIEW (fix C): Mux/Move queues ride the SAME state
    // payload as the per-device tiles, pushed every SSE tick, so all three
    // views derive from one consistent snapshot — no more stale/disagreeing queues.
    let (mux_queue, move_queue, _, _) = build_queue_views_cached(staging_dir);
    obj["_mux_queue"] = serde_json::to_value(&mux_queue).unwrap_or_default();
    obj["_move_queue"] = serde_json::to_value(&move_queue).unwrap_or_default();
    obj.to_string()
}

// Cap on serialized queue entries so a pathological subdir count can't
// produce an unbounded response; shared with handle_system_info's "+N more"
// math so the list and its overflow count can never drift apart.
const QUEUE_DISPLAY_CAP: usize = 100;

// Builds the Mux-queue and Move-queue display lists, shared by get_state_json and
// handle_system_info so both derive from one place. Mutual exclusion comes from state.json
// itself.
fn build_queue_views(staging_dir: &str) -> (Vec<String>, Vec<String>, usize, usize) {
    let active_move_dir = crate::server::mover::ACTIVE_MOVE_DIR
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone();
    // Move queue: staging dirs handed off to the mover (`state == Done`),
    // minus the one actively being moved (shown as live bars, not a queue row).
    let mut move_queue: Vec<String> = std::fs::read_dir(staging_dir)
        .ok()
        .map(|entries| {
            entries
                .filter_map(|e| e.ok())
                .filter(|e| {
                    e.path().is_dir()
                        && crate::server::ripper::staging::read_state(&e.path())
                            .map(|s| s.state == crate::server::ripper::staging::StagingState::Done)
                            .unwrap_or_else(|| e.path().join(".done").exists())
                })
                .filter(|e| {
                    active_move_dir.as_deref() != Some(e.file_name().to_string_lossy().as_ref())
                })
                .map(|e| {
                    let name = e.file_name().to_string_lossy().replace('_', " ");
                    format!("{} (moving)", name)
                })
                .collect()
        })
        .unwrap_or_default();
    // Mux queue: staging dirs with a `.ripped` hand-off and no terminal /
    // move-queue / in-flight marker (see `pending_queue`).
    let mut mux_queue = crate::server::muxer::pending_queue(std::path::Path::new(staging_dir));
    // Uncapped totals captured before truncation so "+N more" math shares this
    // one snapshot with the displayed lists.
    let move_full_count = move_queue.len();
    let mux_full_count = mux_queue.len();
    move_queue.truncate(QUEUE_DISPLAY_CAP);
    mux_queue.truncate(QUEUE_DISPLAY_CAP);
    (mux_queue, move_queue, mux_full_count, move_full_count)
}

fn handle_system_info(request: tiny_http::Request, cfg: &Arc<RwLock<Config>>) {
    // Degrade gracefully on a poisoned lock like every other handler; copy
    // the two paths out and DROP the read guard before the I/O below, or a
    // held RwLock would block a concurrent cfg.write() (Settings-save).
    let (staging_dir, syslog_path) = match cfg.read() {
        Ok(c) => (
            c.staging_dir.clone(),
            format!("{}/device_system.log", c.log_dir()),
        ),
        Err(_) => {
            return json_response(
                request,
                500,
                r#"{"ok":false,"error":"config lock poisoned"}"#,
            );
        }
    };

    // Move + Mux queue lists come from the SAME shared builder the live
    // /api/state SSE payload uses, so the System page and live dashboard
    // can never disagree, and the "+N more" math shares one scan snapshot.
    let (mux_queue, move_queue, mux_full_count, move_full_count) = build_queue_views(&staging_dir);

    // Mover errors: stuck staging dirs the user needs to act on.
    let move_errors: Vec<crate::server::mover::MoverError> = crate::server::mover::MOVE_ERRORS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .values()
        .cloned()
        .collect();

    let truncation_count = move_full_count.saturating_sub(QUEUE_DISPLAY_CAP)
        + mux_full_count.saturating_sub(QUEUE_DISPLAY_CAP);
    let mux_errors: Vec<crate::server::muxer::MuxerError> = crate::server::muxer::MUX_ERRORS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .values()
        .cloned()
        .collect();

    // System log: last 50 lines. Tail from the end with a bounded read
    // rather than slurping the whole file — device_system.log is never
    // rotated and the System page polls this endpoint every few seconds.
    let syslog = tail_file(&syslog_path, SYSLOG_TAIL_BYTES)
        .unwrap_or_default()
        .lines()
        .rev()
        .take(50)
        .collect::<Vec<_>>()
        .join("\n");

    let body = serde_json::json!({
        "move_queue": move_queue,
        "move_errors": move_errors,
        "mux_queue": mux_queue,
        "mux_errors": mux_errors,
        "truncation_count": truncation_count,
        "syslog": syslog,
        // Current runtime debug-logging state, so the System-page toggle
        // reflects reality on load (POST /api/debug flips it).
        "debug_enabled": debug_enabled(),
    });

    json_response(request, 200, &body.to_string());
}

fn handle_device_log(request: tiny_http::Request, _cfg: &Arc<RwLock<Config>>, device: &str) {
    // Single source of truth for device-name validation. Dispatch already
    // gates on is_valid_device_name; re-checking here closes any latent
    // bypass if the handler is ever called directly.
    if !is_valid_device_name(device) {
        text_response(request, "invalid device");
        return;
    }
    let lines = crate::server::log::get_device_log(device, 2000);
    text_response(request, &lines.join("\n"));
}

// Upper bound on trailing bytes read when tailing a log. The JSONL event
// log uses rolling::never so it grows unbounded; 8 MiB comfortably holds
// the 5000-line n cap while keeping per-request allocation bounded.
const DEBUG_TAIL_BYTES: u64 = 8 * 1024 * 1024;

// Same idea for the system log: 50 lines, generously bounded.
const SYSLOG_TAIL_BYTES: u64 = 256 * 1024;

// Read up to the last max_bytes of a file, seeking from the end rather than
// slurping the whole file; a partial first line from a mid-file seek is
// acceptable and dropped by callers that split on \n.
fn tail_file(path: &str, max_bytes: u64) -> std::io::Result<String> {
    use std::io::{Read, Seek, SeekFrom};
    let mut f = std::fs::File::open(path)?;
    let len = f.metadata()?.len();
    let read_from = len.saturating_sub(max_bytes);
    let truncated = read_from > 0;
    f.seek(SeekFrom::Start(read_from))?;
    let mut buf = Vec::with_capacity(len.saturating_sub(read_from).min(max_bytes) as usize);
    f.take(max_bytes).read_to_end(&mut buf)?;
    let mut s = String::from_utf8_lossy(&buf).into_owned();
    // When we seeked into the middle of the file, the first line is a
    // partial record — drop it so callers never parse a half line.
    if truncated && let Some(nl) = s.find('\n') {
        s.drain(..=nl);
    }
    Ok(s)
}

// GET /api/debug?n=N&level=L&device=D&q=substr: last N JSONL events, tailed
// from autorip.jsonl with optional level/device/substring filters. Output
// is raw JSONL (not a JSON array) for streamability, used by the Debug tab.
fn handle_debug_log(request: tiny_http::Request, url: &str) {
    let params = parse_query(url);
    let n: usize = params
        .get("n")
        .and_then(|s| s.parse().ok())
        .unwrap_or(500)
        .min(5000);
    let level = params.get("level").map(|s| s.to_lowercase());
    // Validate the device filter with the same strict predicate as every other
    // device handler; ignore an invalid value rather than letting an arbitrary
    // attacker-supplied substring into the line filter.
    let device = params
        .get("device")
        .filter(|d| is_valid_device_name(d))
        .cloned();
    // Restrict the free-text grep filter to printable ASCII (0x20..=0x7E):
    // the JSONL we grep is ASCII-only, so this keeps an attacker from
    // smuggling control bytes or arbitrary Unicode into the line filter.
    let q = params
        .get("q")
        .filter(|s| s.bytes().all(|b| (0x20..=0x7E).contains(&b)))
        .cloned();

    let path = crate::server::observe::json_log_path();
    let content = match tail_file(&path, DEBUG_TAIL_BYTES) {
        Ok(s) => s,
        Err(e) => {
            // The non-rolling jsonl file may not exist on a fresh boot
            // before the first event flushes. Return empty rather than 404
            // — UI can poll without alerting.
            tracing::debug!(path = %path, error = %e, "debug: jsonl missing");
            return text_response(request, "");
        }
    };

    let levels_at_or_above = |min: &str| -> &'static [&'static str] {
        match min {
            "error" => &["ERROR"],
            "warn" => &["WARN", "ERROR"],
            "info" => &["INFO", "WARN", "ERROR"],
            "debug" => &["DEBUG", "INFO", "WARN", "ERROR"],
            _ => &["TRACE", "DEBUG", "INFO", "WARN", "ERROR"],
        }
    };

    let lines: Vec<&str> = content.lines().collect();
    let start = lines.len().saturating_sub(n);
    let mut out: Vec<String> = Vec::new();
    for line in &lines[start..] {
        if let Some(ref l) = level {
            let allowed = levels_at_or_above(l);
            // tracing-subscriber JSON format puts the level in `"level":"INFO"`.
            if !allowed
                .iter()
                .any(|lv| line.contains(&format!("\"level\":\"{}\"", lv)))
            {
                continue;
            }
        }
        if let Some(ref d) = device {
            // Match `"device":"sg4"` exactly to avoid `sg40` matching `sg4`.
            if !line.contains(&format!("\"device\":\"{}\"", d)) {
                continue;
            }
        }
        if let Some(ref needle) = q
            && !line.contains(needle)
        {
            continue;
        }
        out.push((*line).to_string());
    }
    text_response(request, &out.join("\n"));
}

// Parse ?key=value&key2=v2 into a HashMap. Naive: percent-decodes but does
// NOT translate + to space and has no array-style keys — sufficient for our
// debug filters and easier to audit than a URL parser dep.
fn parse_query(url: &str) -> std::collections::HashMap<String, String> {
    let mut map = std::collections::HashMap::new();
    let q = match url.split_once('?') {
        Some((_, q)) => q,
        None => return map,
    };
    // Bound the work: cap the number of pairs and the length of each key/value
    // so a hostile query string can't blow up the HashMap or the per-request
    // allocation.
    const MAX_PAIRS: usize = 32;
    const MAX_FIELD_LEN: usize = 256;
    // Truncate a &str to at most `n` bytes on a char boundary (raw query
    // fields may carry multibyte UTF-8, so a blind byte slice could panic).
    fn clamp(s: &str, n: usize) -> &str {
        if s.len() <= n {
            return s;
        }
        let mut end = n;
        while end > 0 && !s.is_char_boundary(end) {
            end -= 1;
        }
        &s[..end]
    }
    for pair in q.split('&').take(MAX_PAIRS) {
        if let Some((k, v)) = pair.split_once('=') {
            map.insert(
                percent_decode(clamp(k, MAX_FIELD_LEN)),
                percent_decode(clamp(v, MAX_FIELD_LEN)),
            );
        }
    }
    map
}

#[cfg(test)]
mod parse_query_tests {
    use super::*;

    // parse_query's `clamp` truncates a field to MAX_FIELD_LEN (256) bytes
    // on a char boundary. These pin an off-by-one mutant across three
    // shapes: mid-char cutoff, exact cap, and cutoff already on a boundary.
    #[test]
    fn clamps_query_value_at_char_boundary_when_cutoff_lands_mid_char() {
        // 255 ASCII bytes, then a 2-byte 'é' straddling the 256-byte cutoff
        // (occupies bytes 255..257), then more filler. Byte offset 256 sits
        // inside 'é', so clamp must back up to 255 and drop 'é' entirely.
        let value = format!("{}é{}", "a".repeat(255), "b".repeat(10));
        let url = format!("/x?q={value}");
        let map = parse_query(&url);
        assert_eq!(
            map.get("q").map(String::as_str),
            Some("a".repeat(255).as_str()),
            "must truncate to the last full character before the cutoff, not split 'é'"
        );
    }

    #[test]
    fn query_value_exactly_at_cap_is_not_truncated() {
        // Exactly 256 bytes (128 two-byte 'é' chars) — s.len() <= n, the
        // `<=` early-return branch, no truncation at all.
        let value = "é".repeat(128);
        assert_eq!(value.len(), 256);
        let url = format!("/x?q={value}");
        let map = parse_query(&url);
        assert_eq!(map.get("q").map(String::as_str), Some(value.as_str()));
    }

    #[test]
    fn query_value_over_cap_already_on_boundary_truncates_cleanly() {
        // 260 plain ASCII bytes: over the cap, but byte 256 is already a
        // char boundary, so the backward-scan loop body never executes.
        let value = "a".repeat(260);
        let url = format!("/x?q={value}");
        let map = parse_query(&url);
        assert_eq!(
            map.get("q").map(String::as_str),
            Some("a".repeat(256).as_str())
        );
    }

    #[test]
    fn tail_file_returns_whole_small_file_untruncated() {
        let tmp = tempfile::TempDir::new().unwrap();
        let path = tmp.path().join("log.txt");
        std::fs::write(&path, b"line1\nline2\nline3\n").unwrap();
        let out = tail_file(path.to_str().unwrap(), 4096).expect("tail must succeed");
        assert_eq!(
            out, "line1\nline2\nline3\n",
            "a file smaller than the cap is returned whole, no head drop"
        );
    }

    #[test]
    fn tail_file_seeks_from_end_and_drops_partial_head_line() {
        let tmp = tempfile::TempDir::new().unwrap();
        let path = tmp.path().join("log.txt");
        // 10 lines of "NNNNNNNN\n" (9 bytes each = 90 bytes). Cap the read at 25
        // bytes so it seeks into the middle of a line — the truncated head line
        // must be dropped so callers never parse a half record.
        let mut content = String::new();
        for i in 0..10 {
            content.push_str(&format!("{:08}\n", i));
        }
        std::fs::write(&path, content.as_bytes()).unwrap();
        let out = tail_file(path.to_str().unwrap(), 25).expect("tail must succeed");
        assert!(
            out.len() <= 25,
            "the read must be bounded by max_bytes, got {} bytes",
            out.len()
        );
        assert!(
            !out.starts_with('\n') && out.ends_with("00000009\n"),
            "the tail must end at the last full line and start after a newline, got: {out:?}"
        );
        // Every returned line must be a complete 8-digit record (no partial head).
        for line in out.lines() {
            assert_eq!(line.len(), 8, "no partial line may survive, got: {line:?}");
        }
    }

    #[test]
    fn tail_file_missing_file_is_an_error() {
        let tmp = tempfile::TempDir::new().unwrap();
        let path = tmp.path().join("nope.txt");
        assert!(
            tail_file(path.to_str().unwrap(), 4096).is_err(),
            "a missing file must surface an io error, not empty success"
        );
    }
}

// Deadline for the bounded settings-save: above any reasonable NFS write
// latency, short enough a wedged /config doesn't block the API thread
// forever. On timeout we 503; write-then-rename keeps the old file intact.
const SETTINGS_SAVE_DEADLINE_SECS: u64 = 15;

fn handle_settings_post(request: tiny_http::Request, cfg: &Arc<RwLock<Config>>) {
    let (request, body) = match read_json_body(request) {
        Ok(rb) => rb,
        Err(()) => return,
    };
    let patch: serde_json::Value = match serde_json::from_str(&body) {
        Ok(v) => v,
        Err(_) => {
            json_response(request, 400, r#"{"ok":false,"error":"invalid json"}"#);
            return;
        }
    };

    // Validate every outbound URL/target BEFORE taking the write guard:
    // these do synchronous DNS, and running under `cfg.write()` would block
    // every concurrent `cfg.read()` for the duration (the 0.20.8 lock-stall).
    if let Some(v) = patch.get("keydb_url").and_then(|v| v.as_str()) {
        // SSRF guard at store time (handle_update_keydb re-validates at
        // fetch time). Empty clears the URL; a sentinel-bearing value is a
        // masked "unchanged" placeholder from GET — skip re-validating it.
        if !v.trim().is_empty()
            && !v.contains(SECRET_SENTINEL)
            && let Err(e) = validate_fetch_url(v)
        {
            return json_response(
                request,
                400,
                &serde_json::json!({
                    "ok": false,
                    "error": format!("keydb_url rejected: {e}")
                })
                .to_string(),
            );
        }
    }
    if let Some(v) = patch.get("keyserver_url").and_then(|v| v.as_str()) {
        // The SAME gate the rip applies (https-only + SSRF), so a URL that saves
        // is one the rip will use. Empty disables the online source; a sentinel
        // value skips re-validation.
        if !v.trim().is_empty()
            && !v.contains(SECRET_SENTINEL)
            && let Err(e) = freemkv_keysources::validate_keyserver_url(v.trim())
        {
            return json_response(
                request,
                400,
                &serde_json::json!({
                    "ok": false,
                    "error": format!("keyserver_url rejected: {e}")
                })
                .to_string(),
            );
        }
    }
    if let Some(v) = patch.get("network_target").and_then(|v| v.as_str()) {
        // SSRF guard: at rip time libfreemkv streams decrypted disc content
        // to this bare `host:port`; without a check a POST could beacon
        // plaintext to an internal host. Empty clears the target.
        if !v.trim().is_empty()
            && let Err(e) = validate_network_target(v)
        {
            return json_response(
                request,
                400,
                &serde_json::json!({
                    "ok": false,
                    "error": format!("network_target rejected: {e}")
                })
                .to_string(),
            );
        }
    }
    // webhook_urls are intentionally NOT SSRF-validated (fire-and-forget;
    // LAN targets are intended — see webhook_agent). Resolved here, BEFORE
    // the write guard, so a rejected entry can't leave fields already mutated.
    let webhook_urls_resolved: Option<Vec<WebhookEntry>> = if let Some(arr) =
        patch.get("webhook_urls").and_then(|v| v.as_array())
    {
        // Each element is the modern object, or a bare string (legacy client)
        // treated as fire-on-all; a missing flag defaults to true. A malformed
        // element or non-bool flag is a 400, never a silent drop.
        let mut incoming: Vec<IncomingWebhook> = Vec::with_capacity(arr.len());
        for (i, v) in arr.iter().enumerate() {
            match WebhookEntry::parse(i, v) {
                Ok(e) => incoming.push(IncomingWebhook {
                    url: e.url,
                    post_rip: e.post_rip,
                    post_mux: e.post_mux,
                    post_move: e.post_move,
                }),
                Err(field) => {
                    return json_response(
                        request,
                        400,
                        &serde_json::json!({
                            "ok": false,
                            "error": format!("{field}: flags must be booleans and url a string")
                        })
                        .to_string(),
                    );
                }
            }
        }
        let existing = match cfg.read() {
            Ok(c) => c.webhook_urls.clone(),
            Err(_) => {
                return json_response(
                    request,
                    500,
                    r#"{"ok":false,"error":"config lock poisoned"}"#,
                );
            }
        };
        match resolve_webhook_entries(&incoming, &existing) {
            Ok(urls) => Some(urls),
            Err(_) => {
                // A masked entry matched 0 (deleted row) or >1 (shared
                // origin) stored secrets — refuse to guess. Returned BEFORE
                // the write guard so no other field in the patch mutates either.
                return json_response(
                    request,
                    400,
                    r#"{"ok":false,"error":"ambiguous masked webhook entry; re-enter the full webhook URL"}"#,
                );
            }
        }
    } else {
        None
    };
    if let Some(v) = patch.get("port").and_then(|v| v.as_u64()) {
        // Reject out-of-range BEFORE taking the write guard: validating
        // inside it left a partial update behind when a bad port 400'd. A
        // raw POST can carry any value (e.g. 70000 truncates to 4464 as u16).
        if !(1..=65535).contains(&v) {
            return json_response(
                request,
                400,
                r#"{"ok":false,"error":"port must be 1..=65535"}"#,
            );
        }
    }

    // Validate string-enum fields BEFORE the write guard: a raw POST can
    // carry any value, and silently storing e.g. output_format="garbage"
    // would load cleanly and misbehave downstream. Sets mirror `config::load_saved`.
    for (field, allowed) in [
        ("key_source", &["local", "online"][..]),
        ("on_insert", &["nothing", "scan", "rip", "resume"][..]),
        ("on_read_error", &["stop", "skip"][..]),
        ("output_format", &["mkv", "m2ts", "iso", "network"][..]),
        ("rip_mode", &["single", "multi"][..]),
    ] {
        if let Some(v) = patch.get(field).and_then(|v| v.as_str())
            && !allowed.contains(&v)
        {
            return json_response(
                request,
                400,
                &format!(r#"{{"ok":false,"error":"invalid value for {field}"}}"#),
            );
        }
    }

    // Validate directory-path fields BEFORE the write guard: these become
    // filesystem roots autorip enumerates, so a raw POST must not point them
    // anywhere arbitrary. Require an absolute path with no `..`; empty is unset.
    let has_parent_dir = |p: &std::path::Path| {
        p.components()
            .any(|c| matches!(c, std::path::Component::ParentDir))
    };
    // output_dir / staging_dir are MOUNT ROOTS: they must be absolute (a real
    // mount point) and may not climb with `..`.
    for field in ["output_dir", "staging_dir"] {
        if let Some(v) = patch.get(field).and_then(|v| v.as_str()) {
            if v.is_empty() {
                continue;
            }
            let p = std::path::Path::new(v);
            if !p.is_absolute() || has_parent_dir(p) {
                return json_response(
                    request,
                    400,
                    &format!(
                        r#"{{"ok":false,"error":"{field} must be an absolute path with no '..'"}}"#
                    ),
                );
            }
        }
    }
    // movie_dir / tv_dir are SUB-DIRECTORY names UNDER output_dir. Relative
    // is the norm; only reject `..` so they can't escape the output root.
    // An absolute override is also permitted.
    for field in ["movie_dir", "tv_dir", "iso_dir"] {
        if let Some(v) = patch.get(field).and_then(|v| v.as_str()) {
            if v.is_empty() {
                continue;
            }
            if has_parent_dir(std::path::Path::new(v)) {
                return json_response(
                    request,
                    400,
                    &format!(r#"{{"ok":false,"error":"{field} must not contain '..'"}}"#),
                );
            }
        }
    }

    // The Library folders are absolute (they are not filed under output_dir).
    for field in ["library_dir", "library_iso_dir"] {
        if let Some(v) = patch.get(field).and_then(|v| v.as_str())
            && !v.is_empty()
            && (!std::path::Path::new(v).is_absolute() || has_parent_dir(std::path::Path::new(v)))
        {
            return json_response(
                request,
                400,
                &format!(
                    r#"{{"ok":false,"error":"{field} must be an absolute path with no '..'"}}"#
                ),
            );
        }
    }

    // Validate keydb_path BEFORE the write guard: require an absolute path,
    // no `..`, and a `.cfg` extension. Exempt: "" (unset) and the redacted
    // basename round-trip (GET returns just the filename, which must survive).
    if let Some(v) = patch.get("keydb_path").and_then(|v| v.as_str()) {
        let is_redacted_roundtrip = !v.is_empty() && !v.contains('/') && {
            let stored = cfg
                .read()
                .unwrap_or_else(|e| e.into_inner())
                .keydb_path
                .clone();
            stored.as_deref().is_some_and(|s| {
                std::path::Path::new(s)
                    .file_name()
                    .map(|n| n == std::ffi::OsStr::new(v))
                    .unwrap_or(false)
            })
        };
        if !v.is_empty() && !is_redacted_roundtrip {
            let p = std::path::Path::new(v);
            let bad = !p.is_absolute()
                || p.components()
                    .any(|c| matches!(c, std::path::Component::ParentDir))
                || p.extension().and_then(|e| e.to_str()) != Some("cfg");
            if bad {
                return json_response(
                    request,
                    400,
                    r#"{"ok":false,"error":"keydb_path must be an absolute .cfg path with no '..'"}"#,
                );
            }
        }
    }

    // Numeric clamps applied below mirror `config::load_saved`'s trust-boundary
    // ceilings so the live in-memory value can't diverge from what a restart
    // would load.
    const MAX_DURATION_SECS: u64 = 30 * 24 * 3600; // 30 days
    const MAX_RETENTION_DAYS: u64 = 3650; // 10 years

    // Mutate inside the write guard, then snapshot+drop it BEFORE the save
    // (fs I/O can hang on NFS). `save_gen` is taken under the guard so save
    // order follows mutation order.
    let save_gen: u64;
    let snapshot: Config = {
        let mut c = match cfg.write() {
            Ok(c) => c,
            Err(_) => {
                return json_response(
                    request,
                    500,
                    r#"{"ok":false,"error":"config lock poisoned"}"#,
                );
            }
        };
        if let Some(v) = patch.get("output_dir").and_then(|v| v.as_str()) {
            c.output_dir = v.to_string();
        }
        if let Some(v) = patch.get("staging_dir").and_then(|v| v.as_str()) {
            c.staging_dir = v.to_string();
        }
        if let Some(v) = patch.get("movie_dir").and_then(|v| v.as_str()) {
            c.movie_dir = v.to_string();
        }
        if let Some(v) = patch.get("tv_dir").and_then(|v| v.as_str()) {
            c.tv_dir = v.to_string();
        }
        if let Some(v) = patch.get("iso_dir").and_then(|v| v.as_str()) {
            c.iso_dir = v.to_string();
        }
        if let Some(v) = patch.get("tmdb_api_key").and_then(|v| v.as_str()) {
            // Ignore the redaction sentinel so a round-trip of the GET
            // response doesn't wipe the stored key.
            if v != SECRET_SENTINEL {
                c.tmdb_api_key = v.to_string();
            }
        }
        if let Some(v) = patch.get("keydb_url").and_then(|v| v.as_str()) {
            // Validated above the write guard (SSRF). Ignore any value
            // containing the sentinel — it is the masked form from GET
            // /api/settings and must not clobber the stored token-bearing URL.
            if !v.contains(SECRET_SENTINEL) {
                c.keydb_url = v.to_string();
            }
        }
        if let Some(v) = patch.get("key_source").and_then(|v| v.as_str()) {
            c.key_source = v.to_string();
        }
        if let Some(v) = patch.get("keyserver_url").and_then(|v| v.as_str()) {
            // Validated above the write guard (SSRF). Ignore any value
            // containing the sentinel — it is the masked form from GET
            // /api/settings and must not clobber the stored token-bearing URL.
            if !v.contains(SECRET_SENTINEL) {
                c.keyserver_url = v.trim().to_string();
            }
        }
        if let Some(v) = patch.get("keyserver_secret").and_then(|v| v.as_str())
            && v != SECRET_SENTINEL
        {
            c.keyserver_secret = v.to_string();
        }
        if let Some(v) = patch.get("keydb_path").and_then(|v| v.as_str()) {
            // GET redacts keydb_path to its filename to avoid leaking the
            // container path. A bare value matching the stored basename is
            // that redacted form round-tripping — don't clobber it.
            let is_redacted_roundtrip = !v.is_empty()
                && !v.contains('/')
                && c.keydb_path.as_deref().is_some_and(|stored| {
                    std::path::Path::new(stored)
                        .file_name()
                        .map(|n| n == std::ffi::OsStr::new(v))
                        .unwrap_or(false)
                });
            if !is_redacted_roundtrip {
                c.keydb_path = if v.is_empty() {
                    None
                } else {
                    Some(v.to_string())
                };
            }
        }
        if let Some(v) = patch.get("capture_without_keys").and_then(|v| v.as_bool()) {
            c.capture_without_keys = v;
        }
        if let Some(v) = patch.get("on_insert").and_then(|v| v.as_str()) {
            c.on_insert = v.to_string();
        }
        if let Some(v) = patch.get("main_feature").and_then(|v| v.as_bool()) {
            c.main_feature = v;
        }
        if let Some(v) = patch.get("auto_eject").and_then(|v| v.as_bool()) {
            c.auto_eject = v;
        }
        // Presence is not the question — VALIDITY is. Gating on `.is_some()`
        // while assignment requires `.as_str()` meant a non-string value
        // applied neither the field nor the legacy migration, yet answered 200.
        let on_read_error_in_patch = patch
            .get("on_read_error")
            .and_then(|v| v.as_str())
            .is_some();
        if let Some(v) = patch.get("on_read_error").and_then(|v| v.as_str()) {
            c.on_read_error = v.to_string();
        }
        // Legacy: migrate abort_on_error bool to on_read_error string.
        // An explicit on_read_error in the PATCH always wins (mirrors config.rs::load_saved).
        if !on_read_error_in_patch {
            if let Some(false) = patch.get("abort_on_error").and_then(|v| v.as_bool()) {
                c.on_read_error = "skip".to_string();
            }
            if let Some(true) = patch.get("abort_on_error").and_then(|v| v.as_bool()) {
                c.on_read_error = "stop".to_string();
            }
        }
        if let Some(v) = patch.get("output_format").and_then(|v| v.as_str()) {
            c.output_format = v.to_string();
        }
        if let Some(v) = patch.get("network_target").and_then(|v| v.as_str()) {
            // Validated above the write guard (SSRF); empty clears it.
            c.network_target = v.to_string();
        }
        if let Some(v) = patch.get("min_length_secs").and_then(|v| v.as_u64()) {
            c.min_length_secs = v.min(MAX_DURATION_SECS);
        }
        if let Some(v) = patch.get("port").and_then(|v| v.as_u64()) {
            // Range-validated above the write guard (1..=65535) so a bad
            // value can't leave a partial in-memory mutation behind.
            c.port = v as u16;
        }
        if let Some(v) = patch.get("max_retries").and_then(|v| v.as_u64()) {
            c.max_retries = v.min(10) as u8;
        }
        if let Some(v) = patch.get("library_dir").and_then(|v| v.as_str()) {
            c.library_dir = v.to_string();
        }
        if let Some(v) = patch.get("library_iso_dir").and_then(|v| v.as_str()) {
            c.library_iso_dir = v.to_string();
        }
        if let Some(v) = patch
            .get("library_iso_subfolders")
            .and_then(|v| v.as_bool())
        {
            c.library_iso_subfolders = v;
        }
        if let Some(v) = patch.get("keep_iso").and_then(|v| v.as_bool()) {
            c.keep_iso = v;
        }
        if let Some(v) = patch.get("abort_on_lost_secs").and_then(|v| v.as_u64()) {
            c.abort_on_lost_secs = v.min(MAX_DURATION_SECS);
        }
        if let Some(rip_mode) = patch.get("rip_mode").and_then(|v| v.as_str()) {
            // "multi" with zero retries is meaningless — clamp to at least 1.
            // Do NOT re-derive keep_iso from the mode here: it's handled
            // explicitly above, and clobbering it overrode the operator's choice.
            if rip_mode == "single" {
                c.max_retries = 0;
            } else if c.max_retries == 0 {
                c.max_retries = 1;
            }
        }
        if let Some(urls) = webhook_urls_resolved {
            // Resolved (SSRF-validated + masked-placeholder-resolved) above
            // the write guard, so applying it here is infallible — any
            // ambiguity already returned 400 before any field mutated.
            c.webhook_urls = urls;
        }
        // decrypt_threads + log_retention_days: operator-tunable from the
        // Settings page.
        if let Some(v) = patch.get("decrypt_threads").and_then(|v| v.as_u64()) {
            // Match config::load's .min(256) clamp so the live/on-disk
            // value can't diverge from what a restart would load (and
            // libfreemkv caps the effective pool at 64 regardless).
            c.decrypt_threads = (v as usize).min(256);
        }
        if let Some(v) = patch.get("log_retention_days").and_then(|v| v.as_u64()) {
            c.log_retention_days = v.min(MAX_RETENTION_DAYS);
        }
        save_gen = config::next_save_generation();
        c.clone()
    }; // <-- write guard dropped here; readers unblock immediately

    // Apply the decrypt-thread setting LIVE: swaps libfreemkv's rayon pool;
    // in-flight work uses the old pool, the next rip picks up the new size.
    config::apply_decrypt_threads(snapshot.decrypt_threads);

    // Fail-loud-EARLY destination check: warn NOW if a configured directory
    // is missing/unwritable, rather than only discovering the dead mount
    // hours later at move time. Non-blocking — the save still succeeds.
    for (root, reason) in crate::server::mover::check_configured_destinations(&snapshot) {
        crate::server::log::syslog(&format!(
            "WARNING: configured destination '{root}' is not usable: {reason}. \
             Rips will be PRESERVED in staging (not moved) until this is fixed."
        ));
    }

    // Queue on the coalescing writer and await it with a deadline: a hung
    // write (NFS) parks only that writer, and later saves supersede it.
    let rx = match config::save_coalesced(snapshot, save_gen) {
        Ok(rx) => rx,
        Err(e) => {
            tracing::error!(
                target: "web",
                error = %e,
                "failed to spawn settings-save thread; on-disk settings.json unchanged"
            );
            return json_response(
                request,
                500,
                r#"{"ok":false,"error":"settings save failed: could not spawn save thread"}"#,
            );
        }
    };
    match rx.recv_timeout(std::time::Duration::from_secs(SETTINGS_SAVE_DEADLINE_SECS)) {
        Ok(Ok(())) => json_response(request, 200, r#"{"ok":true}"#),
        Ok(Err(e)) => {
            tracing::error!(
                target: "web",
                error = %e,
                "settings save failed; on-disk settings.json unchanged"
            );
            json_response(
                request,
                500,
                r#"{"ok":false,"error":"settings save failed"}"#,
            )
        }
        Err(_) => {
            tracing::error!(
                target: "web",
                "settings save timed out after {SETTINGS_SAVE_DEADLINE_SECS}s; \
                 in-memory config updated, on-disk result unknown (written only \
                 if storage recovers before autorip restarts)"
            );
            json_response(
                request,
                503,
                r#"{"ok":false,"error":"settings save timed out; on-disk result unknown, it is written only if storage recovers before autorip restarts"}"#,
            )
        }
    }
}

fn handle_sse(request: tiny_http::Request, cfg: &Arc<RwLock<Config>>) {
    // /events holds its thread for the whole client session (1s poll
    // loop). Cap concurrent streams so N clients can't pin N threads and
    // DoS the box; over the cap return 503 and let the thread end.
    let _sse_guard = match ConnGuard::try_acquire(&SSE_CLIENTS, MAX_SSE_CLIENTS) {
        Some(g) => g,
        None => {
            tracing::warn!(
                max = MAX_SSE_CLIENTS,
                "SSE connection rejected: concurrent /events cap reached"
            );
            return json_response(
                request,
                503,
                r#"{"ok":false,"error":"too many SSE clients"}"#,
            );
        }
    };
    // Same-origin only, matching every other route — no ACAO. The service
    // is unauthenticated, so a wildcard would let any page the operator
    // visits cross-origin subscribe and read the full RipState.
    let headers = vec![
        Header::from_bytes(&b"Content-Type"[..], &b"text/event-stream"[..]).unwrap(),
        Header::from_bytes(&b"Cache-Control"[..], &b"no-cache"[..]).unwrap(),
        Header::from_bytes(&b"Connection"[..], &b"keep-alive"[..]).unwrap(),
    ];

    let mut response = Response::empty(200);
    for h in headers {
        response = response.with_header(h);
    }

    let mut stream = request.upgrade("sse", response);

    // Re-read the staging dir each tick (cheap) so a Settings change to
    // the staging path is reflected without restarting the SSE stream.
    let staging_dir = || {
        cfg.read()
            .unwrap_or_else(|e| e.into_inner())
            .staging_dir
            .clone()
    };

    let initial = format!("data: {}\n\n", get_state_json(&staging_dir()));
    if stream.write_all(initial.as_bytes()).is_err() {
        return;
    }
    let _ = stream.flush();

    let mut library = crate::server::library::api::SseCursor::default();
    loop {
        std::thread::sleep(std::time::Duration::from_secs(1));
        let mut frame = format!("data: {}\n\n", get_state_json(&staging_dir()));
        if let Some(lib) = crate::server::library::api::sse_frame(&mut library) {
            frame.push_str(&lib);
        }
        if stream.write_all(frame.as_bytes()).is_err() {
            break;
        }
        if stream.flush().is_err() {
            break;
        }
    }
}

fn handle_scan(request: tiny_http::Request, cfg: &Arc<RwLock<Config>>, device: &str) {
    if !ripper::device_known(device) {
        return json_response(request, 404, UNKNOWN_DEVICE_BODY);
    }
    // Check-and-claim: `try_claim_active_checked` reads thread liveness
    // FIRST outside the STATE lock, then folds status-check+set into a
    // single STATE lock, closing the TOCTOU of two concurrent scan POSTs.
    let Some(claim_gen) = ripper::try_claim_active_checked(device, false) else {
        json_response(request, 409, r#"{"ok":false,"error":"busy"}"#);
        return;
    };

    let dev = device.to_string();
    let dev_path = format!("/dev/{}", device);
    let cfg = Arc::clone(cfg);
    ripper::update_state(
        &dev,
        ripper::RipState {
            device: dev.clone(),
            status: "scanning".to_string(),
            disc_present: true,
            ..Default::default()
        },
    );
    let dev_for_register = dev.clone();
    if let Err(e) = ripper::spawn_rip_thread(&dev_for_register, "scan", move || {
        // Catch the panic, as the rip spawn site and poll loop do — without
        // it a panic in `scan_disc` leaves the claim standing forever
        // (`status="scanning"`, every route 409ing until restart).
        if std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            ripper::scan_disc(&cfg, &dev, &dev_path);
        }))
        .is_err()
        {
            crate::server::log::device_log(&dev, "Scan thread panicked");
            ripper::update_state(
                &dev,
                ripper::RipState {
                    device: dev.clone(),
                    status: "error".to_string(),
                    disc_present: true,
                    last_error: "Internal error (panic)".to_string(),
                    ..Default::default()
                },
            );
        }
    }) {
        tracing::error!(device = %dev_for_register, error = %e, "failed to spawn scan thread");
        // Roll the device state back to idle so a failed spawn doesn't wedge
        // the busy-check forever. Shared helper so poll loop + handlers can't drift.
        ripper::rollback_failed_spawn(&dev_for_register, claim_gen);
        json_response(
            request,
            500,
            r#"{"ok":false,"error":"thread spawn failed"}"#,
        );
        return;
    }
    json_response(request, 200, r#"{"ok":true}"#);
}

// POST /api/rip/{device}[?resume=yes|no] — the ONLY path that starts disk
// work. resume=yes re-muxes an existing staging ISO (404 if none); resume=no
// wipes staging first; no param does a fresh sweep+mux, keeping any staging dir.
fn handle_rip(request: tiny_http::Request, cfg: &Arc<RwLock<Config>>, device: &str, query: &str) {
    if !ripper::device_known(device) {
        return json_response(request, 404, UNKNOWN_DEVICE_BODY);
    }
    let resume_mode = parse_resume_param(query);

    // Check-and-claim — see `handle_scan` for the ordering. Closes the TOCTOU
    // of two concurrent rip POSTs. Also marks the device "scanning": resume
    // is decided by the worker itself, keeping scan logic in one place.
    let Some(claim_gen) = ripper::try_claim_active_checked(device, false) else {
        json_response(request, 409, r#"{"ok":false,"error":"already ripping"}"#);
        return;
    };
    let _ = spawn_rip_after_claim(request, cfg, device, resume_mode, claim_gen);
}

// Spawn the rip worker for `device`, assuming the caller ALREADY won the
// claim. Shared by handle_rip and handle_accept_loss. Returns true if
// spawned, so accept_loss can disarm its pre-written marker if unused.
#[must_use]
fn spawn_rip_after_claim(
    request: tiny_http::Request,
    cfg: &Arc<RwLock<Config>>,
    device: &str,
    resume_mode: ResumeMode,
    claim_gen: u64,
) -> bool {
    let dev = device.to_string();
    let dev_path = format!("/dev/{}", device);
    let cfg = Arc::clone(cfg);

    let dev_for_register = dev.clone();
    ripper::register_halt(&dev_for_register, libfreemkv::Halt::new());
    if let Err(e) = ripper::spawn_rip_thread(&dev_for_register, "rip", move || {
        if std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            ripper::handle_rip_request(&cfg, &dev, &dev_path, resume_mode);
        }))
        .is_err()
        {
            crate::server::log::device_log(&dev, "Rip thread panicked");
            ripper::update_state(
                &dev,
                ripper::RipState {
                    device: dev.clone(),
                    status: "error".to_string(),
                    last_error: "Internal error (panic)".to_string(),
                    ..Default::default()
                },
            );
        }
        ripper::unregister_halt(&dev);
    }) {
        tracing::error!(device = %dev_for_register, error = %e, "failed to spawn rip thread");
        // Roll the device state back to idle so a failed spawn doesn't wedge
        // the busy-check forever. Shared helper so poll loop + handlers can't drift.
        ripper::rollback_failed_spawn(&dev_for_register, claim_gen);
        json_response(
            request,
            500,
            r#"{"ok":false,"error":"thread spawn failed"}"#,
        );
        return false;
    }

    json_response(request, 200, r#"{"ok":true}"#);
    true
}

// Entry verdict for handle_accept_loss's ownership gates, factored out so
// they're unit-testable without a live Request; reverting the .muxing 409
// guard flips MuxInProgress and fails the pinned test.
#[derive(Debug, PartialEq, Eq)]
enum AcceptLossEntry {
    /// No staging dir on disk for this device — 404.
    NoStagingDir,
    /// The mux worker owns the dir (`.muxing`). Refuse (409): this handler's
    /// lock-free state.json write would otherwise clobber the worker's terminal
    /// quarantine, silently dropping the operator's Accept.
    MuxInProgress,
    /// Present and unowned — proceed to arm the override.
    Proceed,
    /// Dir or .muxing state unreadable (not NotFound: EACCES, ESTALE) — 503.
    StagingUnreadable,
}

// Only NotFound is "gone" (metadata follows links, so a dangling symlink is
// 404); any other stat/read error, including on the .muxing state, fails
// closed as StagingUnreadable (503, retry).
fn accept_loss_entry_for(dir: &std::path::Path) -> AcceptLossEntry {
    match std::fs::metadata(dir) {
        Ok(_) => match crate::server::ripper::staging::muxing_status(dir) {
            Ok(muxing) => accept_loss_entry_verdict(true, muxing),
            Err(_) => AcceptLossEntry::StagingUnreadable,
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            accept_loss_entry_verdict(false, false)
        }
        Err(_) => AcceptLossEntry::StagingUnreadable,
    }
}

fn accept_loss_entry_verdict(dir_exists: bool, is_muxing: bool) -> AcceptLossEntry {
    if !dir_exists {
        AcceptLossEntry::NoStagingDir
    } else if is_muxing {
        AcceptLossEntry::MuxInProgress
    } else {
        AcceptLossEntry::Proceed
    }
}

// POST /api/accept-loss/{device}: accept a recorded over-threshold loss and
// deliver the existing rip. Writes .accept-loss, clears terminal/abort
// markers, then re-muxes the EXISTING ISO — fixing the exhaust->.failed loop.
fn handle_accept_loss(request: tiny_http::Request, cfg: &Arc<RwLock<Config>>, device: &str) {
    // Resolve via the one naming rule, not the title alone: with a boxset
    // in the drive, Accept must arm the marker on THIS disc's dir, not a
    // sibling disc that happens to share its title.
    let staging = {
        let c = match cfg.read() {
            Ok(c) => c,
            Err(e) => e.into_inner(),
        };
        match ripper::staging_basename_for_device(&c, device) {
            Some(base) => c.staging_device_dir(&base),
            None => {
                drop(c);
                json_response(
                    request,
                    404,
                    r#"{"ok":false,"error":"no disc state for device"}"#,
                );
                return;
            }
        }
    };
    let dir = std::path::Path::new(&staging);
    // Ownership gates factored into pure accept_loss_entry_verdict above;
    // refusing on .muxing avoids clobbering a just-written quarantine, since
    // this handler isn't otherwise serialized against the mux worker's RMWs.
    match accept_loss_entry_for(dir) {
        AcceptLossEntry::StagingUnreadable => {
            json_response(
                request,
                503,
                r#"{"ok":false,"error":"staging dir unreadable; retry"}"#,
            );
            return;
        }
        AcceptLossEntry::NoStagingDir => {
            json_response(
                request,
                404,
                r#"{"ok":false,"error":"no staging dir to accept"}"#,
            );
            return;
        }
        AcceptLossEntry::MuxInProgress => {
            json_response(
                request,
                409,
                r#"{"ok":false,"error":"mux in progress; retry after it finishes"}"#,
            );
            return;
        }
        AcceptLossEntry::Proceed => {}
    }
    // Claim the device BEFORE touching any on-disk marker: a rejected 409
    // must leave the staging dir untouched, or the next legitimate rip
    // would silently mux a loss that was never actually accepted.
    let Some(claim_gen) = ripper::try_claim_active_checked(device, false) else {
        json_response(request, 409, r#"{"ok":false,"error":"already ripping"}"#);
        return;
    };
    // Arm the one-shot override and move the dir off its terminal/abort
    // state to the re-muxable hand-off state, so resume re-mux proceeds
    // instead of being refused as failed.
    ripper::staging::write_accept_loss_marker(dir);
    ripper::staging::mutate_state_if_present(dir, ripper::staging::apply_accept_loss_reopen);
    // Legacy pre-migration dirs: strip the marker files the same way.
    let _ = std::fs::remove_file(dir.join(ripper::staging::FAILED_MARKER));
    ripper::staging::clear_aborted_loss_marker(dir);
    ripper::staging::clear_restart_count(dir);
    crate::server::log::device_log(
        device,
        "Accept-damage requested — re-muxing the existing ISO with the loss override.",
    );
    // Delegate to the already-claimed spawn path (resume_remux consumes
    // `.accept-loss`); do NOT go through handle_rip, which would try to
    // claim a second time and always lose against the claim just above.
    if !spawn_rip_after_claim(request, cfg, device, ResumeMode::Require, claim_gen) {
        // The OS refused the thread, so NOTHING will consume the override
        // just armed — the same stale-override hazard the claim-before-write
        // ordering above prevents, reached by the other door. Disarm.
        ripper::staging::clear_accept_loss_marker(dir);
        crate::server::log::device_log(
            device,
            "Accept-damage override disarmed: the rip thread could not be spawned, so no run will consume it.",
        );
    }
}

/// Resume-mode chosen by the caller of `/api/rip`. The dispatch logic
/// in `ripper::handle_rip_request` reads this and routes to the
/// appropriate code path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResumeMode {
    /// `?resume=yes` — require an existing resumable staging dir,
    /// fail if none.
    Require,
    /// `on_insert=resume` — Default's guards, then resume eligible state, else as `Fresh`.
    Prefer,
    /// `?resume=no` — operator clean slate: wipe any existing staging dir first.
    Wipe,
    /// no `resume=` query param — fresh sweep+mux in place, unless the disc's
    /// staging is finished, held, loss-aborted or mux-owned.
    Default,
    /// `on_insert=rip` — Default's guards, then wipe stale partial/failed staging and sweep.
    Fresh,
}

fn parse_resume_param(query: &str) -> ResumeMode {
    for kv in query.split('&') {
        let (k, v) = match kv.split_once('=') {
            Some((k, v)) => (k, v),
            None => (kv, ""),
        };
        if k == "resume" {
            return match v {
                "yes" | "true" | "1" => ResumeMode::Require,
                "no" | "false" | "0" => ResumeMode::Wipe,
                _ => ResumeMode::Default,
            };
        }
    }
    ResumeMode::Default
}

#[cfg(test)]
mod stop_report_tests {
    use super::stop_report;

    // Catches the mutation making a timed-out Stop report the clean-stop
    // answer (idle + ok:true): a failure rendered as success. handle_stop
    // used to reset to "idle" regardless of drain, leaving every later route 409ing.
    #[test]
    fn a_stop_that_did_not_drain_is_not_reported_as_a_clean_stop() {
        let clean = stop_report(true);
        assert_eq!(
            clean.status, "idle",
            "a drained stop leaves the device idle"
        );
        assert!(clean.last_error.is_empty(), "no error on the clean path");
        assert!(
            clean.body.contains(r#""ok":true"#),
            "a drained stop answers ok:true; got {}",
            clean.body
        );

        let timed_out = stop_report(false);
        assert_ne!(
            timed_out.status, "idle",
            "a stop whose worker is still running must NOT publish idle — the \
             device is still held and every route will refuse it"
        );
        assert!(
            !timed_out.last_error.is_empty(),
            "the reason the device is still busy must reach the state row the \
             dashboard renders, not just the server log"
        );
        assert!(
            timed_out.body.contains(r#""ok":false"#),
            "a stop that stopped nothing must not answer ok:true; got {}",
            timed_out.body
        );
    }
}

#[cfg(test)]
mod worker_panic_tests {
    // Catches the mutation removing catch_unwind from ANY worker spawn site.
    // The claim is taken before spawn, so the worker owns clearing it even
    // on panic; the scan site once lacked this, 409ing a device forever.
    #[test]
    fn every_worker_spawn_site_catches_its_panic() {
        let src = crate::server::util::source_lf(include_str!("web.rs"));
        // Match the production call shape only. (This file's test modules are
        // interleaved with production code, so a bare name match would also
        // count this test's own string literals.)
        const SITE: &str = "if let Err(e) = ripper::spawn_rip_thread(";
        let mut sites = 0;
        for (idx, _) in src.match_indices(SITE) {
            sites += 1;
            // The closure body follows the call; a spawn site's panic handling
            // must appear before the next spawn site (or the end of the file).
            let rest = &src[idx..];
            let window_end = rest[1..].find(SITE).map(|i| i + 1).unwrap_or(rest.len());
            assert!(
                rest[..window_end].contains("catch_unwind"),
                "a worker spawn site with no catch_unwind leaves the device's \
                 claim set forever if the worker panics; site {sites}"
            );
        }
        assert!(sites >= 2, "expected both web spawn sites; found {sites}");
    }
}

#[cfg(test)]
mod accept_loss_spawn_failure_tests {
    // Catches the mutation dropping the disarm branch when
    // spawn_rip_after_claim fails: .accept-loss is armed BEFORE spawning, so
    // a refused thread must disarm it or the next rip inherits stale consent.
    #[test]
    fn a_failed_spawn_disarms_the_accept_loss_override() {
        let src = crate::server::util::source_lf(include_str!("web.rs"));
        let start = src
            .find("fn handle_accept_loss(")
            .expect("handle_accept_loss must exist");
        let rest = &src[start..];
        let end = rest.find("\n}\n").expect("function must end");
        let body: String = rest[..end]
            .lines()
            .filter(|l| !l.trim_start().starts_with("//"))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            body.contains("if !spawn_rip_after_claim("),
            "handle_accept_loss must CHECK whether the spawn succeeded — a \
             fire-and-forget call cannot disarm the override it armed"
        );
        assert!(
            body.contains("clear_accept_loss_marker("),
            "a spawn failure must disarm `.accept-loss`, or the override sits \
             on disk with no run to consume it and the NEXT rip of this disc \
             silently inherits the operator's consent"
        );
    }
}

#[cfg(test)]
mod dashboard_button_tests {
    // Catches the mutation putting a bare fetch(...) back into any
    // device-action button's onclick: no .then/.catch discarded the server's
    // answer, so a 409 (claim held by an unwinding worker) read as success.
    #[test]
    fn no_device_action_button_discards_the_servers_answer() {
        let src = crate::server::util::source_lf(include_str!("web.rs"));
        let code: Vec<&str> = src
            .lines()
            .filter(|l| !l.trim_start().starts_with("//"))
            .filter(|l| !l.trim_start().starts_with('*'))
            .collect();
        let mut checked = 0;
        for line in code {
            let is_device_action = [
                "/api/scan/",
                "/api/rip/",
                "/api/eject/",
                "/api/stop/",
                "/api/accept-loss/",
            ]
            .iter()
            .any(|ep| line.contains(ep));
            if !line.contains("onclick=") || !is_device_action {
                continue;
            }
            checked += 1;
            assert!(
                !line.contains("fetch("),
                "a device-action button must not call fetch() directly — it \
                 discards the status code, so a 409 renders as a success. Use \
                 apiPost(url, this, label). Offending line:\n{line}"
            );
            assert!(
                line.contains("apiPost("),
                "every device-action button must go through apiPost, which \
                 surfaces the failure and re-enables the button. Offending \
                 line:\n{line}"
            );
        }
        assert!(
            checked >= 8,
            "expected to inspect the whole drive-card button set; only found \
             {checked} — the matcher has drifted away from the buttons"
        );
    }
}

#[cfg(test)]
mod resume_param_tests {
    use super::{ResumeMode, parse_resume_param};

    // ?resume=no selects Wipe, DELETING an existing staging dir before a
    // fresh sweep — the only unauthenticated param that can destroy a rip.
    // Had no test; a mutation flipped the key comparison and stayed green.
    #[test]
    fn resume_param_maps_only_the_documented_values() {
        // The destructive one. Every spelling of it.
        for q in ["resume=no", "resume=false", "resume=0"] {
            assert_eq!(
                parse_resume_param(q),
                ResumeMode::Wipe,
                "{q} must select Wipe"
            );
        }
        for q in ["resume=yes", "resume=true", "resume=1"] {
            assert_eq!(
                parse_resume_param(q),
                ResumeMode::Require,
                "{q} must select Require"
            );
        }

        // Anything else is Default — never the destructive mode. An
        // unrecognised value must not be read as "no".
        for q in [
            "resume=maybe",
            "resume=",
            "resume",
            "foo=bar",
            "",
            "RESUME=no", // the key match is case-sensitive
            "resume=NO", // ...and so is the value
        ] {
            assert_eq!(
                parse_resume_param(q),
                ResumeMode::Default,
                "{q:?} must fall through to Default, never Wipe"
            );
        }
    }

    // The scan must match on the KEY, not the first value that looks like
    // one — with == flipped to != a URL like ?title=no&resume=yes would wipe
    // the staging dir the operator asked to keep.
    #[test]
    fn resume_param_reads_the_resume_key_not_a_neighbouring_one() {
        assert_eq!(
            parse_resume_param("title=no&resume=yes"),
            ResumeMode::Require
        );
        assert_eq!(parse_resume_param("a=1&b=2&resume=no"), ResumeMode::Wipe);
        // A key that merely contains "resume" is not the resume key.
        assert_eq!(parse_resume_param("presume=no"), ResumeMode::Default);
        assert_eq!(parse_resume_param("resumed=no"), ResumeMode::Default);
        // First match wins and stops the scan.
        assert_eq!(
            parse_resume_param("resume=yes&resume=no"),
            ResumeMode::Require
        );
    }
}

/// Shared cap for all three KEYDB download paths (startup, daily refresh, web
/// handler). All paths use `read_capped_keydb_body` so overflow is detected
/// rather than silently truncating at the cap.
pub(crate) const KEYDB_MAX_BYTES: u64 = 100 * 1024 * 1024;

/// Why a capped keydb body read failed.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum KeydbReadError {
    /// The underlying reader errored.
    Io,
    /// The body exceeded the byte cap (oversized plain-text keydb).
    TooLarge,
}

// Read a keydb body, rejecting anything over max_bytes. Read::take(max)
// would silently truncate an oversized body into a half-valid file; reading
// one byte past the cap instead makes an oversized body detectable.
pub(crate) fn read_capped_keydb_body<R: std::io::Read>(
    reader: R,
    max_bytes: u64,
) -> std::result::Result<Vec<u8>, KeydbReadError> {
    let mut buf = Vec::new();
    reader
        .take(max_bytes + 1)
        .read_to_end(&mut buf)
        .map_err(|_| KeydbReadError::Io)?;
    if buf.len() as u64 > max_bytes {
        return Err(KeydbReadError::TooLarge);
    }
    Ok(buf)
}

fn handle_update_keydb(request: tiny_http::Request, cfg: &Arc<RwLock<Config>>) {
    // Serialize: only one keydb download may be in flight at a time. Each one
    // buffers the whole file into memory, so concurrent unauthenticated calls
    // could allocate many large buffers at once. A second caller gets 429.
    static KEYDB_UPDATE_IN_FLIGHT: AtomicBool = AtomicBool::new(false);
    if KEYDB_UPDATE_IN_FLIGHT
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        return json_response(
            request,
            429,
            r#"{"ok":false,"error":"A KEYDB update is already in progress."}"#,
        );
    }
    // Release the in-flight flag on every exit path.
    struct InFlightGuard;
    impl Drop for InFlightGuard {
        fn drop(&mut self) {
            KEYDB_UPDATE_IN_FLIGHT.store(false, Ordering::Release);
        }
    }
    let _in_flight = InFlightGuard;

    let keydb_url = cfg
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .keydb_url
        .clone();
    if keydb_url.is_empty() {
        json_response(
            request,
            400,
            r#"{"ok":false,"error":"No KEYDB URL configured. Set it in Settings."}"#,
        );
        return;
    }

    // SSRF guard at fetch time (defence-in-depth on the store-time check):
    // resolve+validate once, then pin the connection to those IPs so DNS
    // rebinding can't redirect the fetch between validation and connect.
    let pinned = match validate_fetch_url(&keydb_url) {
        Ok(addrs) => addrs,
        Err(e) => {
            let msg = serde_json::json!({
                "ok": false,
                "error": format!("KEYDB URL rejected: {e}")
            })
            .to_string();
            json_response(request, 400, &msg);
            return;
        }
    };
    // Cap is the shared KEYDB_MAX_BYTES (100 MiB); read_capped_keydb_body
    // returns 413 on an oversized body rather than silently truncating.
    let keydb_cap = KEYDB_MAX_BYTES;

    // Download via ureq (supports HTTPS) then save via libfreemkv
    let body = match keydb_update_call(pinned, &keydb_url, KEYDB_FETCH_TIMEOUT, STALL_TIMEOUT) {
        Ok(resp) => match read_capped_keydb_body(resp.into_body().into_reader(), keydb_cap) {
            Ok(buf) => buf,
            Err(KeydbReadError::Io) => {
                json_response(
                    request,
                    500,
                    r#"{"ok":false,"error":"Failed to read response body."}"#,
                );
                return;
            }
            Err(KeydbReadError::TooLarge) => {
                json_response(
                    request,
                    413,
                    r#"{"ok":false,"error":"KEYDB too large (>100 MB plain-text); use a gzip/zip URL"}"#,
                );
                return;
            }
        },
        Err(ureq::Error::StatusCode(code)) => {
            let msg = format!(
                r#"{{"ok":false,"error":"Server returned HTTP {}. Check the URL in Settings."}}"#,
                code
            );
            json_response(request, 502, &msg);
            return;
        }
        Err(e) => {
            // Do NOT echo the configured KEYDB origin/hostname to the client —
            // that leaks server config. Keep detail in the log only, through
            // `ureq_error_kind`, since `ureq::Error`'s Display isn't URL-free.
            tracing::warn!(
                origin = %crate::server::webhook::webhook_url_origin(&keydb_url),
                error_kind = %ureq_error_kind(&e),
                "keydb update: could not connect to configured KEYDB server"
            );
            json_response(
                request,
                502,
                r#"{"ok":false,"error":"Could not connect to the configured KEYDB server. Check the URL in Settings."}"#,
            );
            return;
        }
    };

    // Write to the service-canonical keydb path, NOT libfreemkv's exe-local
    // default — otherwise "Update KEYDB" reports success while every AACS
    // rip keeps failing because the read side looks elsewhere.
    let saved = crate::server::keysource::save_keydb(cfg, &body);
    match saved {
        Ok(result) => {
            let body = serde_json::json!({
                "ok": true,
                "entries": result.entries,
                "bytes": result.bytes,
            });
            json_response(request, 200, &body.to_string());
        }
        Err(e) if e.code() == libfreemkv::error::E_KEYDB_WRITE => {
            // A write/persist failure is an environment problem (disk full,
            // permissions on the keys dir) — not invalid content. Surface it
            // distinctly so the operator fixes the right thing.
            json_response(
                request,
                500,
                r#"{"ok":false,"error":"Failed to save KEYDB to disk (check space/permissions)"}"#,
            );
        }
        Err(_) => {
            json_response(
                request,
                500,
                r#"{"ok":false,"error":"Downloaded file is not a valid KEYDB. Check the URL."}"#,
            );
        }
    }
}

fn handle_eject(request: tiny_http::Request, device: &str) {
    if !ripper::device_known(device) {
        return json_response(request, 404, UNKNOWN_DEVICE_BODY);
    }
    // Gate on rip status (BU40N is slot-loading; eject mid-rip is
    // irreversible), enforced server-side since POST is unauthenticated.
    // Claim first, closing the busy-check/eject TOCTOU via one STATE lock.
    if ripper::try_claim_active_checked(device, false).is_none() {
        return json_response(
            request,
            409,
            r#"{"ok":false,"error":"drive busy; stop the rip before ejecting"}"#,
        );
    }
    let device_path = format!("/dev/{}", device);
    crate::server::ripper::eject_drive(&device_path);
    ripper::update_state(
        device,
        ripper::RipState {
            device: device.to_string(),
            status: "idle".to_string(),
            ..Default::default()
        },
    );
    json_response(request, 200, r#"{"ok":true}"#);
}

fn handle_stop(request: tiny_http::Request, cfg: &Arc<RwLock<Config>>, device: &str) {
    // Stop signals threads to abort, drains the rip thread, drops the SCSI
    // session, and collapses state to idle. Stop preserves partial staging
    // state for resume (pre-0.21.10 wiped it, once nuking an 85 GB ISO).
    let _ = cfg;

    // Cancel the halt and drain (ripper::stop_and_drain). A timed-out drain
    // is not a stop — the worker still owns the drive — so report what
    // actually happened instead of the old "reset to idle either way".
    let drained = ripper::stop_and_drain(device, std::time::Duration::from_secs(60));
    if !drained {
        tracing::error!(
            device = %device,
            "rip thread did not drain within 60s of stop — the worker is still \
             running and this device stays held until it exits (a worker wedged \
             in a blocking drive ioctl needs a container restart)"
        );
        crate::server::log::device_log(
            device,
            "Stop: the rip thread did not exit within 60s. It is still running, so \
             this drive stays busy until it does — scan/rip/eject will be refused. \
             If it never exits, restart the container.",
        );
    }

    // Recover-and-proceed on poison (house convention): a poisoned STATE must
    // not turn a Stop into a silent 404.
    let mut state = ripper::STATE.lock().unwrap_or_else(|e| e.into_inner());
    let report = stop_report(drained);
    let existed = state
        .get_mut(device)
        .map(|rs| {
            // Full reset: keep device id + disc_present, drop everything else.
            let disc_still_in = rs.disc_present;
            *rs = ripper::RipState {
                device: device.to_string(),
                status: report.status.to_string(),
                disc_present: disc_still_in,
                last_error: report.last_error.to_string(),
                ..Default::default()
            };
            true
        })
        .unwrap_or(false);
    drop(state);

    if !existed {
        json_response(request, 404, r#"{"ok":false,"error":"drive not found"}"#);
        return;
    }
    ripper::set_stop_cooldown(device);
    json_response(request, 200, report.body);
}

// What a Stop reports, as a function of whether the rip thread ACTUALLY
// drained. Split out of handle_stop so "a stop that stopped nothing must
// not render as success" is testable without the real 60s drain budget.
struct StopReport {
    /// `RipState::status` to publish.
    status: &'static str,
    /// `RipState::last_error` to publish (empty on the clean path).
    last_error: &'static str,
    /// The JSON response body. Always HTTP 200: the Stop WAS delivered (the
    /// `Halt` is cancelled either way) and the UI must not spin — but `ok` is
    /// false when the worker is still running, so a client that checks the
    /// field cannot read a timed-out drain as a completed stop.
    body: &'static str,
}

fn stop_report(drained: bool) -> StopReport {
    if drained {
        StopReport {
            status: "idle",
            last_error: "",
            body: r#"{"ok":true}"#,
        }
    } else {
        StopReport {
            // NOT idle: the device is still held by a live worker. "error" is
            // the status the dashboard renders together with `last_error`, so
            // the reason lands on the card instead of only in the log.
            status: "error",
            last_error: "Stop timed out: the rip thread is still running; the drive stays busy until it exits",
            body: r#"{"ok":false,"error":"stop timed out: the rip thread is still running"}"#,
        }
    }
}

pub(crate) fn percent_decode(s: &str) -> String {
    let mut result = Vec::new();
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        // Need two hex digits AFTER '%' (i+3 <= len); the old `i+2 < len`
        // guard was off by one, dropping a trailing `%XX`. Both bytes must be
        // hex digits — `from_str_radix` alone accepts a sign, so `%+3` misdecodes.
        if bytes[i] == b'%'
            && i + 3 <= bytes.len()
            && bytes[i + 1].is_ascii_hexdigit()
            && bytes[i + 2].is_ascii_hexdigit()
            && let Ok(byte) = u8::from_str_radix(&String::from_utf8_lossy(&bytes[i + 1..i + 3]), 16)
        {
            result.push(byte);
            i += 3;
            continue;
        }
        result.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&result).to_string()
}

/// Toggle debug logging on/off at runtime. POST body can be empty or contain {"enabled":true/false}.
fn handle_debug_toggle(request: tiny_http::Request) {
    let (request, body) = match read_json_body(request) {
        Ok(rb) => rb,
        Err(()) => return,
    };

    // `{"enabled": <bool>}` sets the level; any other body (missing,
    // non-bool, or invalid JSON) defaults to OFF — a malformed/empty POST
    // must not silently turn on verbose debug logging.
    let enabled = match serde_json::from_str::<serde_json::Value>(&body) {
        Ok(v) => v.get("enabled").and_then(|b| b.as_bool()).unwrap_or(false),
        Err(_) => false,
    };

    *DEBUG_ENABLED
        .write()
        .unwrap_or_else(|poisoned| poisoned.into_inner()) = enabled;

    // Swap the EnvFilter so libfreemkv's `tracing::debug!` events actually
    // surface in docker logs while debug is on — without this the toggle only
    // flips autorip's own checks and the library stays at warn.
    let filter_swapped = crate::server::observe::set_debug(enabled);

    tracing::info!(enabled, filter_swapped, "debug logging toggled");
    json_response(
        request,
        200,
        &serde_json::json!({
            "ok": true,
            "enabled": enabled,
            "filter_swapped": filter_swapped,
        })
        .to_string(),
    );
}
