// Folder health as the Library, Remux and System pages show it: the banner for a folder
// that stopped answering, why a queued remux waits, and the System page's Folders rows.
// Pure functions of the listing and the system payload, so they test without a page.

import { esc, ago, when, bytes } from './ui.js';

/** The Library's folders (output, source ISO, remux staging) whose last check failed. A
    folder still being checked (slow, not failed) is not among them. */
export function unhealthy(folders) {
  return (folders || []).filter(f => f.health && f.health.state && f.health.state !== 'ok' && f.health.state !== 'checking');
}

/** The Library's folders whose check is still out: slow to answer, work for them waits. */
export function checking(folders) {
  return (folders || []).filter(f => f.health && f.health.state === 'checking');
}

// A failed folder is an alert; a slow one a notice, as on the System page.
function bannerOne(message, path, since, hint, waits, slow) {
  return '<div class="banner ' + (slow ? 'warn" role="status">' : 'bad" role="alert">⚠ ') + '<span><b>' + esc(message) + '</b> '
    + '<span class="mono">' + esc(path) + '</span>' + (since ? ' · since ' + esc(ago(since)) : '') + '. '
    + esc(hint || 'Remount the share on the host.') + (waits ? ' Remuxes wait in the queue and start on their own.' : '')
    + '</span></div>';
}

/** The hold's role while the queue file could not be read: a file, and nothing waits on it. */
export const QUEUE_FILE_ROLE = 'queue file';

/** One banner per failed folder, then one per folder still being checked, or the queue's hold
    when a preflight found the problem before the next check did; '' when every folder answers.
    A queue file that cannot be read is its own hold: its banner comes first, beside any folder's,
    and nothing then waits to start on its own. */
export function folderBanner(folders, hold, waits) {
  const fileHeld = !!hold && hold.role === QUEUE_FILE_ROLE;
  const own = waits && !fileHeld;
  const bad = unhealthy(folders).map(f => bannerOne(f.message || 'The ' + f.role + ' folder cannot be used.', f.path, f.health.since, f.hint, own, false));
  const slow = checking(folders).map(f => bannerOne(f.message || 'The ' + f.role + ' folder has not answered yet — still checking.', f.path, f.health.since, f.hint, own, true));
  const held = hold && (fileHeld || !(bad.length || slow.length))
    ? [bannerOne(hold.message, hold.path, hold.since, hold.hint, own, hold.state === 'checking')] : [];
  return held.concat(bad, slow).join('');
}

/** The Library's Remux link while remuxes wait: its tooltip and its badge's label. A held
    queue file is named even while a folder fails too. */
export function remuxLinkLabel(hold, bad) {
  if (hold && hold.role === QUEUE_FILE_ROLE) {
    const also = bad ? ' Remuxes are also waiting: ' + bad.message : '';
    return { title: 'The remux queue is held: ' + hold.message + also, aria: 'The remux queue is held until its file can be read' };
  }
  return { title: 'Remuxes are waiting: ' + (bad || hold).message, aria: 'Remuxes are waiting for a folder' };
}

const NOTE = {
  restarted: 'after a restart', preempted: 'a rip took the slot', interrupted: 'interrupted', stalled: 'stalled',
  waiting_for_folder: 'waiting for the output folder', staged_waiting: 'staged, waiting for the output folder',
};

/** Why a queued job has not started: its note, the queue's hold, and when a retry is due. */
export function queuedNote(job, hold, now) {
  const bits = [];
  if (hold && hold.role === QUEUE_FILE_ROLE) bits.push('queue held: its file cannot be read');
  else if (job.note === 'waiting_for_folder' && hold) bits.push('waiting for the ' + hold.role + ' folder');
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
  const tip = ' title="' + esc(job.failure ? job.failure.message : stagedNote(job)) + '"';
  const due = job.state === 'queued' && job.not_before && job.not_before > now ? ' · retry ' + when(job.not_before) : '';
  const size = job.staged_bytes != null ? ' (' + bytes(job.staged_bytes) + ')' : '';
  const n = job.staged_attempts || 0;
  const text = 'Finished locally' + size + (n ? ' · ' + n + (n === 1 ? ' failed copy' : ' failed copies') : '') + due;
  return '<span class="pill ' + tone + '" data-keep="1"' + tip + '>' + esc(text) + '</span>';
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
