// Pieces the Library and Remux pages both draw: the muxed-with cell, the
// audit verdict, and the per-title details dialog.

import { esc, modal, bytes, runtime, when, api, act, toast, confirmButton } from './ui.js';
import { openTitleLog } from './console.js';
import { refreshNow } from './libdata.js';

/** Out of date: an MKV written by anything but this freemkv. The one
    definition both pages count with. */
export function outdated(r) {
  return !!(r.mkv && r.probed && r.muxed_with && (r.muxed_with.state === 'older' || r.muxed_with.state === 'other'));
}

/** Video codecs, each once; a second HEVC stream is the Dolby Vision layer. */
export function videoLabel(codecs) {
  const hevc = codecs.filter(c => c === 'HEVC').length;
  const rest = [...new Set(codecs.filter(c => c !== 'HEVC'))];
  const out = hevc ? [hevc > 1 ? 'HEVC + DV' : 'HEVC'] : [];
  return out.concat(rest);
}

/** "freemkv 1.6.11" with an out-of-date badge; "probing…" until read. */
export function muxedHtml(r, plain = false) {
  if (!r.mkv) return r.kind === 'iso_only' ? '<span class="badge badge-warn">no MKV yet</span>' : '';
  if (!r.probed) return '<span class="muted small">probing…</span>';
  const m = r.muxed_with;
  const label = esc(r.muxed_label || (m.version ? 'freemkv ' + m.version : ''));
  const tip = esc(r.writing_app || 'no writing-app stamp');
  if (m.state === 'current' || (plain && m.state !== 'unknown')) return '<span class="one" title="' + tip + '"><span class="ell">' + label + '</span></span>';
  if (m.state === 'unknown') return '<span class="muted" title="The file carries no writing-app stamp">unknown</span>';
  return '<span class="one" title="' + tip + ' (not muxed by this freemkv)"><span class="ell">' + label + '</span><span class="badge badge-warn">out of date</span></span>';
}

const ISSUE_TEXT = {
  not_mkv: 'Not an MKV file',
  unreadable: "The file's header can't be read",
  no_video: 'No video track',
  no_duration: "The file doesn't say how long it is",
  no_cues: 'No index for skipping through the video',
  runtime_mismatch: 'The video stops before the length the file claims (cut short?)',
};

export function issueText(i) {
  let t = ISSUE_TEXT[i.kind] || i.kind.replace(/_/g, ' ');
  if (i.kind === 'runtime_mismatch') t += ': ' + runtime(i.runtime_secs) + ' of ' + runtime(i.duration_secs);
  if (i.kind === 'unreadable' && i.code != null) t += ' (E' + i.code + ')';
  return t;
}

/** [dotClass, glyph, tooltip, sortRank] for a row's audit. */
export function auditState(r) {
  if (!r.mkv) return ['dot-idle', '', 'No MKV', 9];
  const a = r.audit;
  if (!a) return ['dot-warn', '●', 'Not checked yet', 3];
  if (!a.ok) return ['dot-bad', '●', a.issues.map(issueText).join('; '), 5];
  if (a.issues.length) return ['dot-warn', '●', a.issues.map(issueText).join('; '), 2];
  return ['dot-ok', '✓', 'Checks out', 0];
}

export function auditHtml(r) {
  const [cls, glyph, tip] = auditState(r);
  if (!r.mkv) return '';
  const color = cls === 'dot-ok' ? 'var(--ok)' : cls === 'dot-bad' ? 'var(--bad)' : '#d97706';
  return '<span title="' + esc(tip) + '" style="color:' + color + ';font-size:1rem">' + glyph + '</span>';
}

function trackRows(a) {
  const v = (a.video || []).map((c, i) => '<tr><td>' + (i + 1) + '</td><td>Video</td><td>' + esc(c) + '</td><td></td></tr>');
  const au = (a.audio || []).map(t => '<tr><td></td><td>Audio</td><td>' + esc(t.codec) + '</td><td>' + esc(t.language) + '</td></tr>');
  const s = (a.subtitles || []).map(l => '<tr><td></td><td>Subtitle</td><td></td><td>' + esc(l) + '</td></tr>');
  return v.concat(au, s).join('');
}

/** The details dialog for one row. */
export function openDetails(r, ctx = {}) {
  const a = r.audit;
  const [cls] = auditState(r);
  const verdict = !r.mkv ? '<span class="badge badge-muted">no MKV</span>'
    : !a ? '<span class="badge badge-warn">not checked yet</span>'
    : cls === 'dot-bad' ? '<span class="badge badge-bad">issue</span>'
    : cls === 'dot-warn' ? '<span class="badge badge-warn">checks out, with a note</span>'
    : '<span class="badge badge-ok">✓ checks out</span>';
  let body = '<div class="chips" style="margin-bottom:1rem">' + verdict
    + (a && a.duration_secs ? ' <span class="badge badge-muted">' + runtime(a.duration_secs) + '</span>' : '')
    + (r.size_bytes ? ' <span class="badge badge-muted">' + bytes(r.size_bytes) + '</span>' : '')
    + (r.muxed_with && (r.muxed_with.state === 'older' || r.muxed_with.state === 'other') ? ' <span class="badge badge-warn">made by an older version or another program</span>' : '')
    + '</div>';
  if (a && a.issues.length) {
    body += '<h3>Findings</h3>' + a.issues.map(i => '<div class="issue"><span class="dot ' + (i.kind === 'no_cues' ? 'dot-warn' : 'dot-bad') + '"></span>' + esc(issueText(i)) + '</div>').join('');
  }
  if (a) {
    body += '<h3>Tracks</h3><table class="det"><thead><tr><th>#</th><th>Type</th><th>Codec</th><th>Language</th></tr></thead><tbody>' + trackRows(a) + '</tbody></table>';
  }
  body += '<h3>Files</h3><dl class="kv">'
    + (r.mkv ? '<dt>MKV</dt><dd class="mono">' + esc(r.mkv) + '</dd>' : '')
    + (r.mkv ? '<dt>Made with</dt><dd>' + esc(r.writing_app || 'not recorded') + '</dd>' : '')
    + (r.modified ? '<dt>Modified</dt><dd>' + esc(when(r.modified)) + '</dd>' : '')
    + '<dt>Source ISO</dt><dd class="mono">' + (r.iso ? esc(r.iso) + (r.linked ? ' <span class="badge badge-teal" title="Recorded when this app ripped it">linked</span>' : '') : '<span class="muted">' + esc(noteText(r)) + '</span>') + '</dd>'
    + (r.target && !r.mkv ? '<dt>Remux creates</dt><dd class="mono">' + esc(r.target) + '</dd>' : '')
    + '</dl>';
  const res = r.result;
  if (res) {
    body += '<h3>Last remux</h3><p class="small" style="margin:0">'
      + (res.outcome === 'done'
        ? '✓ ' + bytes(res.size_bytes) + ' in ' + runtime(res.secs) + ', ' + esc(when(res.finished_at))
        : '<span style="color:var(--bad)">✗ ' + esc(res.message) + '</span>, ' + esc(when(res.finished_at)))
      + '</p>';
  }
  const can = ctx.remux !== false && (r.kind === 'remux' || r.kind === 'iso_only');
  const busy = r.job && (r.job.state === 'queued' || r.job.state === 'running');
  const foot = (r.mkv ? '<button class="btn btn-ghost btn-sm" data-a="reaudit">Re-audit</button>' : '')
    + (ctx.remux !== false && (r.job || r.result) ? '<button class="btn btn-ghost btn-sm" data-a="log">Remux log</button>' : '')
    + (can ? '<button class="btn btn-primary btn-sm" data-a="remux"' + (busy ? ' disabled' : '') + '>' + (busy ? 'Queued' : 'Remux') + '</button>' : '');
  const m = modal({ title: esc(r.title), body, foot, wide: true });
  const q = (s) => m.el.querySelector(s);
  if (q('[data-a=log]')) q('[data-a=log]').onclick = () => openTitleLog(r.title);
  if (q('[data-a=reaudit]')) q('[data-a=reaudit]').onclick = (e) => act(e.currentTarget, async () => {
    await api('POST', '/api/library/reaudit', { paths: [r.mkv] });
    toast('Re-auditing ' + r.title, 'info');
    refreshNow();
  }, 'Re-audit');
  const rb = q('[data-a=remux]');
  if (rb && !busy) confirmButton(rb, (b) => queueOne(r, b).then(ok => { if (ok) m.close(); }));
  return m;
}

export function noteText(r) {
  if (r.note && r.note.kind === 'several_isos') return r.note.count + ' ISOs match: rename one';
  if (r.note && r.note.kind === 'several_mkvs') return r.note.count + ' MKVs match';
  return 'no ISO';
}

/** Queue one row; toasts the outcome. Resolves true when it was queued. */
export async function queueOne(r, btn) {
  const res = await act(btn, () => api('POST', '/api/library/queue/add', { target: r.target }), 'Remux');
  if (!res) return false;
  if (res.queued) toast('Queued ' + r.title, 'ok');
  else toast(r.title + ' is already queued', 'info');
  refreshNow();
  return res.queued > 0;
}
