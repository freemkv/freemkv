// The remux Console: a terminal that follows whichever job is running and
// shows what `freemkv iso://… mkv://…` would print for it. Per-title logs
// open in the same terminal with the finished transcript.

import { api, esc, terminal, speed, hms, toast } from './ui.js';
import { subscribe } from './bus.js';

function statusHtml(r, queued) {
  if (!r) return null;
  const pct = r.pct == null ? '' : '<b>' + r.pct.toFixed(1) + '%</b> · ';
  const rate = r.speed_bps ? speed(r.speed_bps) + ' · ' : '';
  const eta = r.eta_secs != null ? 'ETA ' + hms(r.eta_secs) + ' · ' : '';
  const stall = r.stalled_secs >= 60 ? ' · <b style="color:#fb923c">no progress for ' + hms(r.stalled_secs) + '</b>' : '';
  return '<b>' + esc(r.title) + '</b> · ' + pct + rate + eta + esc(r.phase) + (queued ? ' · ' + queued + ' queued' : '') + stall;
}

function idleText(queue) {
  if (!queue) return 'idle';
  if (queue.paused) return 'queue paused · ' + queue.queued + ' queued';
  return queue.queued ? 'waiting for the next job · ' + queue.queued + ' queued' : 'idle · nothing queued';
}

/** Open the Console. It clears and follows each new job as the queue advances. */
export async function openConsole() {
  const t = terminal({ title: 'freemkv — console' });
  let job = null;
  let seen = 0;
  let queued = 0;
  let queue = null;
  const show = (running) => {
    t.status(statusHtml(running, queued), running ? running.pct : 0);
    if (running) {
      t.setTitle('freemkv — ' + running.title);
      t.progress(running.line, running.pct);
      t.idle(null);
    } else {
      t.progress(null);
      t.idle(idleText(queue));
    }
  };
  try {
    const c = await api('GET', '/api/library/console');
    job = c.job;
    if (c.title) t.setTitle('freemkv — ' + c.title);
    seen = c.lines.length ? c.lines[c.lines.length - 1].seq : 0;
    queue = c.queue;
    queued = c.queue ? c.queue.queued : 0;
    t.append(c.lines);
    if (!c.lines.length && !c.running) t.setInfo('No remux has run since the server started.');
    show(c.running);
    t.scrollEnd();
  } catch (e) {
    t.idle('could not load the console: ' + e.message);
  }
  const off = subscribe('library', (f) => {
    queued = f.queued;
    queue = { paused: f.paused, queued: f.queued };
    const lines = (f.lines || []).filter(l => l.seq > seen);
    if (lines.length) seen = lines[lines.length - 1].seq;
    for (const l of lines) {
      if (l.job !== job) {
        job = l.job;
        t.clear();
      }
    }
    t.append(lines.filter(l => l.job === job));
    if (f.job_title) t.setTitle('freemkv — ' + f.job_title);
    if (f.running && f.running.job_id !== job) {
      job = f.running.job_id;
      t.clear();
    }
    show(f.running);
  });
  t.onClose(off);
}

/** A title's last remux transcript; live when that title is running now. */
export async function openTitleLog(title) {
  const t = terminal({ title: 'freemkv — ' + title + ' (log)' });
  try {
    const c = await api('GET', '/api/library/log?title=' + encodeURIComponent(title));
    t.append(c.lines);
    if (!c.lines.length) t.idle('No remux log for this title yet.');
    t.setInfo('<a href="/api/library/log?raw=1&title=' + encodeURIComponent(title) + '" target="_blank" style="color:inherit">raw log</a>');
    t.scrollEnd();
  } catch (e) {
    toast('Could not load the log: ' + e.message, 'bad');
    t.idle('could not load the log');
    return;
  }
  let reloading = false;
  const reload = async () => {
    if (reloading) return;
    reloading = true;
    try {
      const c = await api('GET', '/api/library/log?title=' + encodeURIComponent(title));
      t.clear();
      t.append(c.lines);
    } catch (e) { /* keep what is shown */ }
    setTimeout(() => { reloading = false; }, 1000);
  };
  const off = subscribe('library', (f) => {
    const r = f.running;
    if (r && r.title === title) {
      if ((f.lines || []).some(l => l.job === r.job_id)) reload();
      t.status(statusHtml(r, 0), r.pct);
      t.progress(r.line, r.pct);
    } else {
      t.status(null);
      t.progress(null);
    }
  });
  t.onClose(off);
}
