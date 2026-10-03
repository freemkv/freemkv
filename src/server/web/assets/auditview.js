// The media and audit sections of the Library's details dialog, as HTML strings:
// what each track is, what a better release of the tier would have, and what the
// deep audit verified or found. Pure functions of the listing row, so they test
// without a page.

import { esc, runtime, when } from './ui.js';

const CHANNELS = { 8: '7.1', 7: '6.1', 6: '5.1', 3: '2.1', 2: '2.0', 1: '1.0' };

/** "7.1" for 8 channels; "?" when the file does not say. */
export function chLabel(n) {
  if (!n) return '?';
  return CHANNELS[n] || n + 'ch';
}

const TIERS = { uhd: '4K UHD', bluray: 'Blu-ray', sd: 'SD' };

export function tierLabel(t) { return TIERS[t] || ''; }

/** "DV P8 + HDR10", "HDR10+", "SDR"; "" when nothing is known. */
export function hdrLabel(h) {
  if (!h || !h.format) return '';
  const base = h.hdr10plus === true && h.format === 'hdr10' ? 'HDR10+'
    : { hdr10: 'HDR10', hlg: 'HLG', sdr: 'SDR' }[h.format] || '';
  if (!h.dv) return base || (h.format === 'unknown' ? 'HDR unknown' : '');
  const dv = 'DV P' + h.dv.profile;
  return base && base !== 'SDR' ? dv + ' + ' + base : dv;
}

/** The Dolby Vision profile as players name it: 8.1, 8.4, 7, 5. */
export function dvProfile(dv) {
  return dv.profile === 8 && dv.compat_id ? '8.' + dv.compat_id : String(dv.profile);
}

const FORMATS = { 'E-AC-3': 'DD+', 'AC-3': 'Dolby Digital' };

/** "TrueHD Atmos 7.1", "DD+ 5.1", "DTS-HD MA 5.1". */
export function audioLabel(a) {
  return (FORMATS[a.format] || a.format) + (a.atmos === true ? ' Atmos' : '') + ' ' + chLabel(a.channels);
}

/** The badges after an audio track: lossless or lossy, Atmos, default, best, a false claim. */
export function audioBadge(a, { best = false, isDefault = false } = {}) {
  const b = [];
  b.push(a.lossless === true ? '<span class="badge badge-ok">lossless</span>'
    : a.lossless === false ? '<span class="badge badge-muted">lossy</span>'
    : '<span class="badge badge-muted" title="The first frames could not be read">lossless?</span>');
  if (a.atmos === true) b.push('<span class="badge badge-teal">Atmos</span>');
  if (best) b.push('<span class="badge badge-ok">best</span>');
  if (isDefault) b.push('<span class="badge badge-muted">default</span>');
  if (a.lossy_claim) b.push('<span class="badge badge-warn" title="The track title says lossless; the stream is lossy">says lossless, is lossy</span>');
  return b.join(' ');
}

/** The upgrade radar: what a better release of this tier would carry. */
export function radar(d) {
  if (!d || !d.tier) return '';
  const miss = (d.radar || []).map(x => '<span class="badge badge-warn">' + esc(x) + '</span>');
  const unk = (d.radar_unknown || []).map(x => '<span class="badge badge-muted" title="Could not be read from the file">' + esc(x) + '?</span>');
  if (!miss.length && !unk.length) return '<span class="badge badge-ok">nothing missing for ' + esc(tierLabel(d.tier)) + '</span>';
  return miss.concat(unk).join(' ');
}

/** h:mm:ss, the way a seek bar reads. */
export function clock(s) {
  if (s == null) return '?';
  const neg = s < 0;
  s = Math.round(Math.abs(s));
  const out = Math.floor(s / 3600) + ':' + String(Math.floor(s / 60) % 60).padStart(2, '0') + ':' + String(s % 60).padStart(2, '0');
  return (neg ? '-' : '') + out;
}

/** The declared length against the last frame, in one line. */
export function timelineText(t) {
  if (!t) return '';
  const parts = [];
  if (t.declared_secs != null) parts.push('declared ' + clock(t.declared_secs));
  if (t.last_frame_secs != null) parts.push('last frame ' + clock(t.last_frame_secs));
  if (t.last_cue_secs != null) parts.push('last index point ' + clock(t.last_cue_secs));
  const gap = t.delta_secs == null ? '' : ' (' + (t.delta_secs >= 0 ? '+' : '') + t.delta_secs.toFixed(1) + ' s)';
  const verdict = {
    ok: 'content ends where the file says',
    overrun: 'content runs past the declared end' + gap,
    short: 'content ends before the declared end' + gap,
  }[t.state] || 'the end could not be read';
  return parts.join(' · ') + (parts.length ? ' · ' : '') + verdict;
}

function kv(rows) {
  const body = rows.filter(r => r && r[1] !== '' && r[1] != null).map(([k, v]) => '<dt>' + esc(k) + '</dt><dd>' + v + '</dd>').join('');
  return body ? '<dl class="kv">' + body + '</dl>' : '';
}

function yes(b) { return b ? '✓' : ''; }

function videoRows(v, d, i) {
  const size = v.width && v.height ? v.width + '×' + v.height : '';
  const codec = esc(v.codec) + (v.bit_depth ? ', ' + v.bit_depth + '-bit' : '');
  const light = [];
  if (v.max_cll) light.push('MaxCLL ' + v.max_cll);
  if (v.max_fall) light.push('MaxFALL ' + v.max_fall);
  if (v.mastering_max_nits) light.push('mastered at ' + v.mastering_max_nits + ' nits');
  const dv = v.dv ? 'Profile ' + dvProfile(v.dv) + ', level ' + v.dv.level
    + ' (' + ['bl', 'el', 'rpu'].filter(k => v.dv[k]).map(k => k.toUpperCase()).join(', ') + ')' : '';
  return [
    ['Track', i ? String(v.number) + (v.dv && v.dv.el ? ' (Dolby Vision layer)' : '') : ''],
    ['Resolution', esc(size) + (i === 0 && d.tier ? ' <span class="badge badge-teal">' + esc(tierLabel(d.tier)) + '</span>' : '')],
    ['Codec', codec],
    ['Frame rate', v.fps ? esc(v.fps + ' fps') + (v.interlaced ? ' interlaced' : '') : ''],
    ['HDR', i === 0 ? esc(hdrLabel(d.hdr)) : ''],
    ['HDR10+', i === 0 && d.hdr && d.hdr.format !== 'sdr' ? (d.hdr.hdr10plus === true ? 'present' : d.hdr.hdr10plus === false ? 'not present' : '<span class="muted">unknown</span>') : ''],
    ['Dolby Vision', esc(dv)],
    ['Light levels', light.length ? esc(light.join(' · ')) : ''],
  ];
}

export function videoSection(d) {
  if (!d.video || !d.video.length) return '';
  return '<h3>Video</h3>' + d.video.map((v, i) => kv(videoRows(v, d, i))).join('<hr style="border:0;border-top:1px solid var(--line)">');
}

export function audioSection(d) {
  if (!d.audio || !d.audio.length) return '<h3>Audio</h3><p class="small muted">No audio track.</p>';
  const rows = d.audio.map((a, i) => '<tr><td>' + a.number + '</td><td>' + esc(a.language) + '</td><td>' + esc(audioLabel(a))
    + '</td><td>' + esc(a.title) + '</td><td>' + audioBadge(a, { best: i === d.best_audio, isDefault: i === d.default_audio }) + '</td></tr>').join('');
  let warn = '';
  if (d.default_not_best && d.audio[d.default_audio] && d.audio[d.best_audio]) {
    warn = '<div class="issue"><span class="dot dot-warn"></span>' + esc('A player starts with ' + audioLabel(d.audio[d.default_audio]) + ', not the better ' + audioLabel(d.audio[d.best_audio])) + '</div>';
  }
  return '<h3>Audio</h3><div class="det-scroll"><table class="det"><thead><tr><th>#</th><th>Language</th><th>Format</th><th>Title</th><th></th></tr></thead><tbody>' + rows + '</tbody></table></div>' + warn;
}

export function subtitleSection(d) {
  if (!d.subtitles || !d.subtitles.length) return '';
  const rows = d.subtitles.map(s => '<tr><td>' + s.number + '</td><td>' + esc(s.language) + '</td><td>' + esc(s.format) + '</td><td>' + esc(s.title)
    + '</td><td>' + yes(s.default) + '</td><td>' + yes(s.forced) + '</td></tr>').join('');
  return '<h3>Subtitles</h3><div class="det-scroll"><table class="det"><thead><tr><th>#</th><th>Language</th><th>Format</th><th>Title</th><th>Default</th><th>Forced</th></tr></thead><tbody>' + rows + '</tbody></table></div>';
}

/** Findings the media read adds to the audit's own: false lossless claims, a cut-off end. */
export function mediaFindings(d) {
  const out = [];
  (d.audio || []).filter(a => a.lossy_claim).forEach(a => out.push(['dot-warn', 'Audio track ' + a.number + ' is titled "' + a.title + '" but the stream is lossy (' + audioLabel(a) + ')']));
  const t = d.timeline;
  if (t && (t.state === 'short' || t.state === 'overrun')) out.push(['dot-warn', 'The ' + timelineText(t)]);
  return out;
}

const WHAT = {
  demux_errors: 'Copying its packets failed: the container itself is damaged.',
  decode_errors: 'Decoding it found damaged frames.',
  bitstream_corruption: 'The video decoder lost its place in the picture data again and again.',
  memory_runaway: 'Decoding the video used memory without limit and was stopped.',
  stderr_overflow: 'The decoder reported errors without end and was stopped.',
  nonzero_exit: 'The decoder stopped with an error.',
};

const ADVICE = {
  container_framing: 'Packets fail to copy: a muxing fault. Remux it from its ISO.',
  payload_bitstream: 'Frames decode damaged while the container copies cleanly: the damage is in the encoded video, usually from the source or the rip. Remux one more time from the ISO; if the damage survives a clean remux, re-rip the disc.',
  timestamp: 'Timestamps run backwards: a muxing fault. Remux it from its ISO.',
  inconclusive: 'No sampled window showed it, so the damage is sparse. The decoder lines below show what failed.',
};

const ADVICE_BY_REASON = {
  demux_errors: ADVICE.container_framing,
  decode_errors: 'Remux it from its ISO; if the damage survives a clean remux, re-rip the disc.',
  bitstream_corruption: ADVICE.payload_bitstream,
  memory_runaway: 'Remux it from its ISO; if it happens again, re-rip the disc.',
  stderr_overflow: 'Remux it from its ISO; if it happens again, re-rip the disc.',
  nonzero_exit: 'Re-audit it; if it fails again, remux it from its ISO.',
};

/** Why a deep audit failed: what, where in the movie, and what to do. */
export function deepWhy(v) {
  if (!v) return null;
  const f = v.forensic;
  const what = (WHAT[v.reason] || 'The deep audit failed.') + (v.stage ? ' (' + (v.stage === 'demux' ? 'reading' : 'decoding') + ' stage)' : '');
  let where = 'Not located: this verdict was reached before the timeline was sampled.';
  let advice = ADVICE_BY_REASON[v.reason] || '';
  if (f && f.windows) {
    const hit = f.windows.filter(w => w.payload_errors || w.container_errors || w.timestamp_errors);
    where = hit.length
      ? 'Around ' + hit.map(w => clock(w.at_secs)).join(', ') + ' (' + hit.length + ' of ' + f.windows.length + ' sampled ' + f.window_secs + '-second windows)'
      : 'None of ' + f.windows.length + ' sampled windows showed it.';
    advice = (f.buckets || []).map(b => ADVICE[b]).filter(Boolean).join(' ') || advice;
  }
  return { what, where, advice };
}

/** What a clean deep audit verified. */
export function deepVerified(v, a) {
  if (!v) return '';
  const n = a && a.audio_tracks;
  const decoded = 'decoded the main video' + (n ? ' and ' + n + (n === 1 ? ' audio track' : ' audio tracks') : '');
  const len = a && a.duration_secs;
  const through = v.reached_secs && len ? ' through ' + clock(v.reached_secs) + ' of ' + clock(len) : len ? ' through ' + runtime(len) : '';
  return 'Copied every stream and ' + decoded + through + (v.scanned ? ', ' + when(v.scanned) : '') + (v.peak_rss_mib ? '; peak memory ' + v.peak_rss_mib + ' MiB' : '') + '.';
}

export function forensicTable(f) {
  if (!f || !f.windows || !f.windows.length) return '';
  const rows = f.windows.map(w => {
    const bad = w.payload_errors || w.container_errors || w.timestamp_errors;
    return '<tr><td><span class="dot ' + (bad ? 'dot-bad' : 'dot-ok') + '"></span> ' + clock(w.at_secs) + '</td><td>' + (w.payload_errors || '') + '</td><td>' + (w.container_errors || '') + '</td><td>' + (w.timestamp_errors || '') + '</td></tr>';
  }).join('');
  return '<details class="adv"><summary class="small">Sampled windows</summary><div class="det-scroll"><table class="det"><thead><tr><th>At</th><th>Damaged frames</th><th>Packet errors</th><th>Timestamp errors</th></tr></thead><tbody>' + rows + '</tbody></table></div></details>';
}

/** The raw report: every header field, per section. */
export function rawHtml(sections) {
  if (!sections || !sections.length) return '<p class="small muted">No header fields were recorded.</p>';
  return sections.map(s => '<h3>' + esc(s.title) + '</h3><div class="det-scroll"><table class="det"><tbody>'
    + s.fields.map(([k, v]) => '<tr><td class="muted">' + esc(k) + '</td><td class="mono">' + esc(v) + '</td></tr>').join('')
    + '</tbody></table></div>').join('');
}
