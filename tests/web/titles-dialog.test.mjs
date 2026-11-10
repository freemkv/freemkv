import { test } from 'node:test';
import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import vm from 'node:vm';

// titles.js with its ui.js imports stubbed: the pure choice logic, and the dialog driven
// through a minimal fake modal.
async function load(api = async () => ({})) {
  const source = await readFile(new URL('../../src/server/web/assets/titles.js', import.meta.url), 'utf8');
  const context = vm.createContext({});
  const calls = [], toasts = [];
  const handlers = {};
  const body = { innerHTML: '', addEventListener: (event, fn) => { handlers[event] = fn; } };
  const buttons = { '[data-tl-save]': { disabled: true }, '[data-tl-auto]': {} };
  const supplied = {
    esc: s => String(s ?? ''),
    bytes: n => n + ' B',
    toast: text => { toasts.push(text); },
    act: async (_button, operation) => operation(),
    api: async (method, url, payload) => { calls.push({ method, url, payload }); return api(method, url, payload); },
    modal: () => ({ el: { querySelector: sel => sel === '[data-tl-body]' ? body : buttons[sel] || null, querySelectorAll: () => [] }, close() {} }),
  };
  const module = new vm.SourceTextModule(source
    + '\nexport { initialPick, togglePick, movePick, firstEpisode, episodeNames, pickProblem, titleGroups };', { context });
  await module.link(async specifier => {
    const names = [...source.matchAll(/import\s*\{([^}]+)\}\s*from\s*'([^']+)'/g)]
      .find(m => m[2] === specifier)[1].split(',').map(s => s.trim());
    return new vm.SyntheticModule(names, function () {
      for (const name of names) this.setExport(name, supplied[name]);
    }, { context });
  });
  await module.evaluate();
  const target = (selector, dataset) => ({ closest: s => s === selector ? { dataset } : null });
  return { t: module.namespace, calls, toasts, body, buttons, handlers, target };
}

const title = (index, extra = {}) => ({ index, id: { playlist: 'p' + index, playlist_id: index, extents: [[index, 1]] }, duration: '0h 23m', tracks: '', ...extra });

test('the suggestion opens pre-ticked in play order; ambiguous clusters open with nothing ticked', async () => {
  const { t } = await load();
  const titles = [0, 1, 2, 3, 4, 5].map(i => title(i));
  const suggested = { kind: 'tv', titles, confirm: { reason: 'missing-episode-roster', suggested: [3, 1, 2], clusters: [[3, 1, 2]], alternates: [] }, automatic: [] };
  assert.deepEqual([...t.initialPick(suggested)], [3, 1, 2]);
  const ambiguous = { kind: 'tv', titles, confirm: { reason: 'missing-episode-roster', suggested: [], clusters: [[0, 1], [3, 4]], alternates: [] }, automatic: [] };
  assert.deepEqual([...t.initialPick(ambiguous)], []);
  const groups = t.titleGroups(ambiguous, []);
  assert.deepEqual(JSON.parse(JSON.stringify(groups.map(g => [g.label, g.rows.map(r => r.index)]))),
    [['Similar titles, group 1', [0, 1]], ['Similar titles, group 2', [3, 4]], ['Other titles', [2, 5]]]);
  // A saved pick wins over the suggestion; without either, the automatic plan is ticked.
  assert.deepEqual([...t.initialPick({ ...suggested, pick: [5, 4] })], [5, 4]);
  assert.deepEqual([...t.initialPick({ kind: 'movie', titles, automatic: [2] })], [2]);
});

test('alternates and uncaptured titles start unticked', async () => {
  const { t } = await load();
  const titles = [title(0), title(1, { alternate_of: 0 }), title(2, { swept: false }), title(3)];
  const data = { kind: 'tv', titles, confirm: { reason: 'missing-episode-roster', suggested: [0, 2, 3], clusters: [], alternates: [{ index: 1, of: 0 }] } };
  assert.deepEqual([...t.initialPick(data)], [0, 3]);
});

test('TV ticks append in order and move up and down; a Movie keeps one choice', async () => {
  const { t } = await load();
  let pick = [];
  for (const i of [4, 2, 7]) pick = t.togglePick(pick, i, 'tv');
  assert.deepEqual([...pick], [4, 2, 7]);
  assert.deepEqual([...t.movePick(pick, 7, -1)], [4, 7, 2]);
  assert.deepEqual([...t.movePick(pick, 4, -1)], [4, 2, 7], 'the first cannot move up');
  assert.deepEqual([...t.movePick(pick, 7, 1)], [4, 2, 7], 'the last cannot move down');
  assert.deepEqual([...t.togglePick(pick, 2, 'tv')], [4, 7]);
  let movie = t.togglePick([], 3, 'movie');
  movie = t.togglePick(movie, 5, 'movie');
  assert.deepEqual([...movie], [5]);
  assert.deepEqual([...t.togglePick(movie, 5, 'movie')], []);
  assert.deepEqual([...t.initialPick({ kind: 'movie', titles: [title(0), title(1)], pick: [1, 0] })], [1]);
});

test('names preview the engine numbering and the save rules match the server', async () => {
  const { t } = await load();
  assert.deepEqual([...t.episodeNames('Samurai Champloo', null, null, null, 2)], ['Samurai Champloo S01E01', 'Samurai Champloo S01E02']);
  assert.deepEqual([...t.episodeNames('Show', 2, 3, null, 4)].slice(0, 1), ['Show S02E09']);
  assert.deepEqual([...t.episodeNames('Show', 1, 3, 5, 1)], ['Show S01E05']);
  assert.equal(t.pickProblem([1, 2], 'movie', null) !== null, true);
  assert.equal(t.pickProblem([1], 'movie', null), null);
  assert.equal(t.pickProblem([], 'tv', null) !== null, true);
  assert.equal(t.pickProblem([1], 'tv', null) !== null, true, 'one episode needs its First episode');
  assert.equal(t.pickProblem([1], 'tv', 3), null);
  assert.equal(t.pickProblem([1, 2], 'tv', null), null);
});

test('the drive dialog saves identities in the chosen order with its job and revision', async () => {
  const titles = [0, 1, 2, 3].map(i => title(i));
  const list = { job_id: 'job-1', revision: 4, kind: 'tv', show: 'Show', titles, automatic: [],
    confirm: { reason: 'missing-episode-roster', suggested: [1, 2], clusters: [[1, 2]], alternates: [] } };
  const h = await load(async method => method === 'GET' ? list : { ok: true, revision: 5 });
  await h.t.driveTitles('sg0', { confirm: list.confirm, tmdb_title: 'Show' });
  assert.equal(h.calls[0].url, '/api/titles/sg0');
  assert.match(h.body.innerHTML, /Show S01E01/);
  assert.equal(h.buttons['[data-tl-save]'].disabled, false);
  h.handlers.change({ target: h.target('[data-tl-tick]', { tlTick: '3' }) });
  h.handlers.click({ target: h.target('[data-tl-move]', { tlIndex: '3', tlMove: '-1' }) });
  await h.buttons['[data-tl-save]'].onclick({ currentTarget: {} });
  const post = h.calls[1];
  assert.equal(post.method, 'POST');
  assert.equal(post.url, '/api/titles/sg0');
  assert.equal(post.payload.job_id, 'job-1');
  assert.equal(post.payload.revision, 4);
  assert.deepEqual([...post.payload.titles.map(id => id.playlist_id)], [1, 3, 2]);
  await h.buttons['[data-tl-auto]'].onclick({ currentTarget: {} });
  assert.deepEqual([...h.calls[2].payload.titles], [], 'Automatic clears the pick');
});
