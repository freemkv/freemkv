// The Titles and Confirm episodes dialog (design §4.2-4.4): every title of a disc, its flags,
// and the operator's choice. A Movie is one feature; TV is ticked episodes in order, previewed
// as the names they file under. The same dialog serves a drive, a held rip and a remux.

import { esc, api, act, toast, modal, bytes } from './ui.js';

// ── Pure choice logic (tests/web/titles-dialog.test.mjs) ───────────────────

/* The ticks a dialog opens with: the saved pick, else the suggestion to confirm (none when
   several groups qualify), else what the plan runs on its own. Uncaptured titles never. */
function initialPick(data) {
  const usable = i => (data.titles || []).some(t => t.index === i && t.swept !== false);
  const start = data.pick && data.pick.length ? data.pick
    : data.confirm ? (data.confirm.suggested || [])
    : (data.automatic || []);
  const pick = start.filter(usable);
  return data.kind === 'movie' ? pick.slice(0, 1) : pick;
}

/* Tick or untick `index`: a Movie keeps one feature; TV appends to the end of the order. */
function togglePick(pick, index, kind) {
  if (kind === 'movie') return pick[0] === index ? [] : [index];
  return pick.includes(index) ? pick.filter(i => i !== index) : [...pick, index];
}

/* Move a ticked title one place up (-1) or down (+1) in the episode order. */
function movePick(pick, index, delta) {
  const at = pick.indexOf(index), to = at + delta;
  if (at < 0 || to < 0 || to >= pick.length) return pick;
  const next = [...pick];
  [next[at], next[to]] = [next[to], next[at]];
  return next;
}

/* The engine's numbering: the First episode, else disc N starts at (N-1)*count+1. */
function firstEpisode(count, disc, start) {
  if (start) return start;
  return disc ? (disc - 1) * count + 1 : 1;
}

const pad2 = n => String(n).padStart(2, '0');

/* The names the ticked episodes file under ("Show S01E01"; TMDB adds episode names). */
function episodeNames(show, season, disc, start, count) {
  const first = firstEpisode(count, disc, start);
  return Array.from({ length: count }, (_, i) => (show || 'Show') + ' S' + pad2(season || 1) + 'E' + pad2(first + i));
}

/* A typed Season, Disc or First episode: null when blank, undefined when not 1..65535. */
function typedNumber(raw, badInput) {
  const text = String(raw ?? '').trim();
  if (badInput) return undefined;
  if (text === '') return null;
  const n = Number(text);
  return Number.isInteger(n) && n >= 1 && n <= 65535 ? n : undefined;
}

const NUMBERS = [['season', 'Season'], ['disc', 'Disc'], ['episode_start', 'First episode']];

/* Why the choice cannot be saved yet, or null. `where`: the dialog that sets First episode,
   when this one cannot. */
function pickProblem(pick, kind, episodeStart, where = '') {
  if (kind === 'movie') return pick.length === 1 ? null : 'Choose the one title that is the movie.';
  if (!pick.length) return 'Tick at least one episode.';
  if (pick.length === 1 && !episodeStart) {
    return 'One episode needs its First episode number' + (where ? ': set it with ' + where + ', then choose again.' : '.');
  }
  return null;
}

/* Unticked titles in display groups: each suggested cluster (when the dialog asks the operator
   to choose among them), then every other title in scan order. */
function titleGroups(data, pick) {
  const rest = (data.titles || []).filter(t => !pick.includes(t.index));
  const clusters = data.confirm && !(data.confirm.suggested || []).length ? data.confirm.clusters || [] : [];
  const groups = [];
  const placed = new Set();
  clusters.forEach((cluster, n) => {
    const rows = rest.filter(t => cluster.includes(t.index));
    rows.forEach(t => placed.add(t.index));
    if (rows.length) groups.push({ label: 'Similar titles, group ' + (n + 1), rows });
  });
  const others = rest.filter(t => !placed.has(t.index));
  if (others.length) groups.push({ label: groups.length || pick.length ? 'Other titles' : '', rows: others });
  return groups;
}

const REASONS = {
  'missing-episode-roster': 'This disc does not list its episodes.',
  'incomplete-episode-roster': "This disc's episode list is incomplete.",
  'conflicting-episode-roster': "This disc's episode list contradicts itself.",
  'invalid-episode-roster': "This disc's episode list is invalid.",
};

/* The dialog's lead: why it asks, and what the ticks already say. */
function confirmLead(confirm) {
  if (!confirm) return '';
  const why = REASONS[confirm.reason] || REASONS['missing-episode-roster'];
  const advice = (confirm.suggested || []).length
    ? 'The suggested episodes are ticked in play order. Check the ticks and the order, then confirm.'
    : (confirm.clusters || []).length > 1
      ? 'Several groups of similar titles could be the episodes. Tick the episodes yourself, in order.'
      : 'Tick the episodes, in order.';
  return why + ' ' + advice;
}

// ── The dialog ─────────────────────────────────────────────────────────────

/** Open the dialog. `load()` answers a title list (`/api/titles/<dev>` and kin); `save(data,
 *  ids, numbers)` stores the choice (ids in order; [] = automatic). `numbers` makes Season/Disc/
 *  First episode editable (a remux saves them with its match). `canClear` offers Automatic;
 *  `onExtra(m)` wires `extraFoot`, even when the list fails to load. `rename`: where to rename. */
export async function openTitlesDialog({ heading, load, save, canClear = true, numbers = false, saveLabel = 'Save titles', onSaved = () => {}, extraFoot = '', onExtra, rename = 'Change title' }) {
  const m = modal({
    title: esc(heading), wide: true,
    body: '<div data-tl-body><div class="muted small">Loading the titles…</div></div>',
    foot: extraFoot + (canClear ? '<button class="btn btn-ghost btn-sm" data-tl-auto disabled>Automatic</button>' : '')
      + '<button class="btn btn-primary btn-sm" data-tl-save disabled>' + esc(saveLabel) + '</button>',
  });
  const body = m.el.querySelector('[data-tl-body]');
  const saveBtn = m.el.querySelector('[data-tl-save]');
  if (onExtra) onExtra(m);
  let data;
  try { data = await load(); } catch (e) {
    body.innerHTML = '<div class="banner bad">⚠ <span>Could not load the titles: ' + esc(e.message) + '</span></div>';
    return m;
  }
  let pick = initialPick(data);
  const kind = data.kind === 'movie' ? 'movie' : 'tv';
  const byIndex = new Map((data.titles || []).map(t => [t.index, t]));
  const num = (key, label) => '<label>' + label + ' <input class="txt" type="number" min="1" max="65535" step="1" data-tl-num="' + key + '" value="' + esc(data[key] ?? '') + '"></label>';
  // A Movie shows no number inputs: its numbers stay as the list has them.
  const typed = key => {
    const input = numbers && m.el.querySelector('[data-tl-num="' + key + '"]');
    return input ? typedNumber(input.value, input.validity?.badInput) : data[key] ?? null;
  };
  const value = key => typed(key) ?? null;
  const numberProblem = () => {
    const bad = NUMBERS.find(([key]) => typed(key) === undefined);
    return bad ? bad[1] + ' must be an integer between 1 and 65535, or blank.' : null;
  };
  const uncaptured = (data.titles || []).some(t => t.swept === false);
  const head = (data.confirm ? '<p class="small" style="margin:0 0 .6rem">' + esc(confirmLead(data.confirm)) + '</p>' : '')
    + '<p class="small muted" style="margin:0 0 .6rem">' + (kind === 'movie'
      ? 'This disc is filed as a movie: choose the one title that is the feature.'
      : 'This disc is filed as TV: tick the episodes and put them in order. Each one is filed under the name shown.')
      + ' To change the name or Movie/TV, use ' + esc(rename) + '.</p>'
    + (uncaptured ? '<p class="small muted" style="margin:0 0 .6rem">Only titles this capture read can be chosen here. To rip the others, insert the disc and choose them on its drive.</p>' : '')
    + (numbers && kind === 'tv' ? '<div class="hook">' + num('season', 'Season') + num('disc', 'Disc') + num('episode_start', 'First episode') + '</div>' : '');
  // The numbers and the lead stay put; only the list repaints, so typing keeps its focus.
  body.innerHTML = head + '<div data-tl-list></div>';
  const list = body.querySelector('[data-tl-list]');
  const flags = t => [
    t.in_roster ? '<span class="badge badge-ok">Episode on the disc\'s list</span>' : '',
    t.play_all ? '<span class="badge badge-warn">Play all</span>' : '',
    t.alternate_of != null ? '<span class="badge badge-muted">Alternate of #' + (t.alternate_of + 1) + '</span>' : '',
    t.swept === false ? '<span class="badge badge-bad">Not captured</span>' : '',
  ].join(' ');
  const rowHtml = (t, position, name) => {
    const ticked = position != null;
    const control = '<input type="' + (kind === 'movie' ? 'radio' : 'checkbox') + '" name="tl-pick" data-tl-tick="' + t.index + '"'
      + (ticked ? ' checked' : '') + (t.swept === false ? ' disabled' : '') + ' aria-label="Choose title ' + (t.index + 1) + '">';
    const order = ticked && kind === 'tv'
      ? '<button class="btn btn-ghost btn-sm" data-tl-move="-1" data-tl-index="' + t.index + '"' + (position === 0 ? ' disabled' : '') + ' aria-label="Earlier">↑</button>'
        + '<button class="btn btn-ghost btn-sm" data-tl-move="1" data-tl-index="' + t.index + '"' + (position === pick.length - 1 ? ' disabled' : '') + ' aria-label="Later">↓</button>'
      : '';
    return '<div class="pipe-row">' + control + '<div class="grow"><div class="name">'
      + (ticked && kind === 'tv' ? '<span class="mono">' + (position + 1) + '.</span> ' : '')
      + 'Title ' + (t.index + 1) + (t.playlist ? ' <span class="mono small">' + esc(t.playlist) + '</span>' : '') + ' ' + flags(t) + '</div>'
      + '<div class="sub">' + [esc(t.duration), t.size_bytes ? bytes(t.size_bytes) : '', esc(t.tracks || '')].filter(Boolean).join(' · ') + '</div>'
      + (name ? '<div class="sub">→ <span class="mono">' + esc(name) + '</span></div>' : '') + '</div>' + order + '</div>';
  };
  const paint = () => {
    const start = value('episode_start');
    const names = kind === 'tv' ? episodeNames(data.show, value('season'), value('disc'), start, pick.length) : [];
    const chosen = pick.map((i, at) => byIndex.get(i) ? rowHtml(byIndex.get(i), at, names[at]) : '').join('');
    const groups = titleGroups(data, pick).map(g => (g.label ? '<div class="small muted" style="margin-top:.8rem;font-weight:700">' + esc(g.label) + '</div>' : '')
      + g.rows.map(t => rowHtml(t, null, '')).join('')).join('');
    const problem = numberProblem() || pickProblem(pick, kind, start, numbers ? '' : rename);
    list.innerHTML = (byIndex.size ? '' : '<p class="small muted">This disc has no titles to choose from.</p>')
      + (pick.length ? '<div class="small muted" style="font-weight:700">' + (kind === 'movie' ? 'The movie' : 'Episodes, in order') + '</div>' + chosen : '')
      + groups
      + (problem ? '<p class="small" style="color:var(--warn);margin:.6rem 0 0">' + esc(problem) + '</p>' : '');
    saveBtn.disabled = !!problem;
  };
  body.addEventListener('change', e => {
    const tick = e.target.closest('[data-tl-tick]');
    if (tick) { pick = togglePick(pick, +tick.dataset.tlTick, kind); paint(); }
  });
  body.addEventListener('input', e => { if (e.target.closest('[data-tl-num]')) paint(); });
  body.addEventListener('click', e => {
    const mv = e.target.closest('[data-tl-move]');
    if (mv) { pick = movePick(pick, +mv.dataset.tlIndex, +mv.dataset.tlMove); paint(); }
  });
  const store = async (btn, ids) => {
    const nums = { season: value('season'), disc: value('disc'), episode_start: value('episode_start') };
    const r = await act(btn, () => save(data, ids, nums), saveLabel);
    if (r === undefined) return;
    toast(ids.length ? 'Titles saved' : 'Titles back to automatic', 'ok');
    m.close();
    onSaved();
  };
  saveBtn.onclick = e => store(e.currentTarget, pick.map(i => byIndex.get(i).id));
  const auto = m.el.querySelector('[data-tl-auto]');
  if (auto) { auto.disabled = false; auto.onclick = e => store(e.currentTarget, []); }
  paint();
  return m;
}

/** The drive card's dialog: the scanned disc's titles, saved with the card's job. */
export function driveTitles(dev, s) {
  return openTitlesDialog({
    heading: (s.confirm ? 'Confirm episodes' : 'Titles') + ' · ' + (s.tmdb_title || s.disc_name || dev),
    saveLabel: s.confirm ? 'Confirm episodes' : 'Save titles',
    load: () => api('GET', '/api/titles/' + dev),
    save: (data, ids) => api('POST', '/api/titles/' + dev, { job_id: data.job_id, revision: data.revision, titles: ids }),
  });
}
