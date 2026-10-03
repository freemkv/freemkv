// Folder health as the Library, Remux and System pages show it: the banner for a folder
// that stopped answering, why a queued remux waits, and the System page's Folders rows.
// Pure functions of the listing and the system payload, so they test without a page.

import { esc, ago, when, bytes } from './ui.js';

const STATE = { ok: 'ok', unhealthy: 'unhealthy', unresponsive: 'not responding' };

/** The Library's folders (output, source ISO, remux staging) whose last check failed. */
export function unhealthy(folders) {
  return (folders || []).filter(f => f.health && f.health.state && f.health.state !== 'ok');
}

function bannerOne(message, path, since, hint, waits) {
  return '<div class="banner bad" role="alert">⚠ <span><b>' + esc(message) + '</b> '
    + '<span class="mono">' + esc(path) + '</span>' + (since ? ' · since ' + esc(ago(since)) : '') + '. '
    + esc(hint || 'Remount the share on the host.') + (waits ? ' Remuxes wait in the queue and start on their own.' : '')
    + '</span></div>';
}

/** One banner per unhealthy folder, or the queue's hold when a preflight found the problem
    before the next check did; '' when every folder answers. */
export function folderBanner(folders, hold, waits) {
  const bad = unhealthy(folders);
  if (bad.length) return bad.map(f => bannerOne(f.message || 'The ' + f.role + ' folder cannot be used.', f.path, f.health.since, f.hint, waits)).join('');
  return hold ? bannerOne(hold.message, hold.path, hold.since, hold.hint, waits) : '';
}

const NOTE = {
  restarted: 'after a restart', preempted: 'a rip took the slot', interrupted: 'interrupted', stalled: 'stalled',
  waiting_for_folder: 'waiting for the output folder', staged_waiting: 'staged, waiting for the output folder',
};

/** Why a queued job has not started: its note, the queue's hold, and when a retry is due. */
export function queuedNote(job, hold, now) {
  const bits = [];
  if (job.note === 'waiting_for_folder' && hold) bits.push('waiting for the ' + hold.role + ' folder');
  else if (job.note) bits.push(NOTE[job.note] || job.note);
  else if (hold) bits.push('waiting for the ' + hold.role + ' folder');
  if (job.not_before && job.not_before > now) bits.push('retry ' + (job.attempts + 1) + ' ' + when(job.not_before));
  return bits.join(' · ');
}

/** A job holding a finished MKV kept on local staging: what it waits for, its size, and how
    many copies into the output folder failed. Plain text; '' for any other job. */
export function stagedNote(job) {
  if (!job || !job.staged) return '';
  const size = job.staged_bytes != null ? ' (' + bytes(job.staged_bytes) + ')' : '';
  const n = job.staged_attempts || 0;
  const what = job.state === 'failed' ? ' — the copy into the output folder failed' : ' — waiting for the output folder to copy it';
  return 'Finished locally' + size + what + (n ? ' · ' + n + (n === 1 ? ' failed copy' : ' failed copies') : '');
}

/** The Remux row's pill for such a job, with the reason in its tooltip and a retry that is due. */
export function stagedPill(job, now) {
  const tone = job.state === 'failed' ? 'bad' : 'warn';
  const tip = job.failure ? ' title="' + esc(job.failure.message) + '"' : '';
  const due = job.state === 'queued' && job.not_before && job.not_before > now ? ' · retry ' + when(job.not_before) : '';
  return '<span class="pill ' + tone + '" data-keep="1"' + tip + '>' + esc(stagedNote(job) + due) + '</span>';
}

/** Retry now and Discard for a row whose job keeps a finished file and is not running. */
export function stagedActions(r) {
  if (!r.job || !r.job.staged || r.job.state === 'running') return '';
  const t = esc(r.title);
  return '<button class="btn btn-primary btn-sm" data-staged-retry aria-label="Copy the finished file of ' + t + ' in now">Retry now</button>'
    + '<button class="btn btn-ghost btn-sm" data-staged-discard aria-label="Discard the finished file of ' + t + '">Discard staged file</button>';
}

/** The System page's line on finished files kept on local staging. */
export function stagedLine(s) {
  if (!s || !s.count) return 'Kept staged files: none';
  return 'Kept staged files: ' + esc(s.count) + ' (' + esc(bytes(s.bytes)) + ')'
    + (s.dir ? ' in <span class="mono">' + esc(s.dir) + '</span>' : '') + ', waiting for the output folder';
}

/** The System page's Folders row: role, path, state, last good access and last error. */
export function folderHealthRow(m) {
  const state = m.state || (m.ok ? 'ok' : 'unhealthy');
  const tone = state === 'ok' ? 'ok' : state === 'unresponsive' ? 'warn' : 'bad';
  const label = STATE[state] || state;
  return '<tr><td class="title"><b>' + esc(m.role) + '</b><span class="path mono" title="' + esc(m.path) + '">' + esc(m.path) + '</span></td>'
    + '<td><span class="dot dot-' + tone + '"></span> ' + (state === 'ok' ? esc(label) : '<span style="color:var(--bad)">' + esc(m.message || label) + '</span>')
    + (state !== 'ok' && m.since ? '<span class="note">since ' + esc(ago(m.since)) + '</span>' : '') + '</td>'
    + '<td class="nowrap"><span class="muted small">' + esc(m.last_ok ? ago(m.last_ok) : 'never') + '</span></td>'
    + '<td>' + (m.last_error ? '<span class="small">' + esc(m.last_error) + '</span><span class="note">' + esc(ago(m.last_error_at)) + '</span>' : '<span class="muted small">–</span>') + '</td></tr>';
}
