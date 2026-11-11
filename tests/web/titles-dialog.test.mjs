import { test } from 'node:test';
import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import vm from 'node:vm';

// titles.js with its ui.js imports stubbed: the pure choice logic, and the dialog driven
// through a minimal fake modal.
async function load(api = async () => ({})) {
  const source = await readFile(new URL('../../src/server/web/assets/titles.js', import.meta.url), 'utf8');
  const context = vm.createContext({});
  const calls = [], toasts = [], modals = [];
  const handlers = {};
  const list = { innerHTML: '' };
  let bodyHtml = '';
  const body = { writes: 0, get innerHTML() { return bodyHtml; }, set innerHTML(html) { bodyHtml = html; this.writes++; },
    addEventListener: (event, fn) => { handlers[event] = fn; }, querySelector: sel => sel === '[data-tl-list]' ? list : null };
  const buttons = { '[data-tl-save]': { disabled: true }, '[data-tl-auto]': { disabled: true }, '[data-rv-name]': {} };
  const inputs = {};
  for (const key of ['season', 'disc', 'episode_start']) inputs[key] = { value: '' };
  let closed = 0;
  const supplied = {
    esc: s => String(s ?? ''),
    bytes: n => n + ' B',
    toast: text => { toasts.push(text); },
    act: async (_button, operation, label) => {
      try { return await operation(); } catch (e) { toasts.push(label + ': ' + e.message); return undefined; }
    },
    api: async (method, url, payload) => { calls.push({ method, url, payload }); return api(method, url, payload); },
    modal: options => {
      modals.push(options);
      // The number inputs exist once the body names them, with the value it rendered.
      const typed = sel => {
        const key = /data-tl-num="(\w+)"/.exec(sel)?.[1];
        if (!key || !body.innerHTML.includes('data-tl-num="' + key + '"')) return null;
        if (!('rendered' in inputs[key])) inputs[key].value = body.innerHTML.match(new RegExp('data-tl-num="' + key + '" value="([^"]*)"'))[1];
        inputs[key].rendered = true;
        return inputs[key];
      };
      return { el: { querySelector: sel => sel === '[data-tl-body]' ? body : buttons[sel] || typed(sel) }, close() { closed++; } };
    },
  };
  const module = new vm.SourceTextModule(source
    + '\nexport { initialPick, togglePick, movePick, firstEpisode, episodeNames, pickProblem, titleGroups, confirmLead, typedNumber };', { context });
  await module.link(async specifier => {
    const names = [...source.matchAll(/import\s*\{([^}]+)\}\s*from\s*'([^']+)'/g)]
      .find(m => m[2] === specifier)[1].split(',').map(s => s.trim());
    return new vm.SyntheticModule(names, function () {
      for (const name of names) this.setExport(name, supplied[name]);
    }, { context });
  });
  await module.evaluate();
  const target = (selector, dataset) => ({ closest: s => s === selector ? { dataset } : null });
  return { t: module.namespace, calls, toasts, body, list, buttons, inputs, handlers, target, modals,
    html: () => body.innerHTML + list.innerHTML, get closed() { return closed; } };
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
  assert.deepEqual([...t.initialPick({ ...suggested, automatic: [5] })], [3, 1, 2], 'the suggestion wins over the plan');
  assert.deepEqual([...t.initialPick({ ...ambiguous, automatic: [5] })], [], 'a hold ticks nothing of the plan');
  assert.deepEqual([...t.initialPick({ ...suggested, pick: [] })], [3, 1, 2], 'an empty pick is automatic');
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
  assert.equal(t.pickProblem([1], 'tv', null), 'One episode needs its First episode number.');
  assert.equal(t.pickProblem([1], 'tv', null, 'Change title'), 'One episode needs its First episode number: set it with Change title, then choose again.');
  assert.equal(t.pickProblem([1, 2], 'tv', null), null);
});

test('the drive dialog saves identities in the chosen order with its job and revision', async () => {
  const titles = [0, 1, 2, 3].map(i => title(i));
  const list = { job_id: 'job-1', revision: 4, kind: 'tv', show: 'Show', titles, automatic: [],
    confirm: { reason: 'missing-episode-roster', suggested: [1, 2], clusters: [[1, 2]], alternates: [] } };
  const h = await load(async method => method === 'GET' ? list : { ok: true, revision: 5 });
  await h.t.driveTitles('sg0', { confirm: list.confirm, tmdb_title: 'Show' });
  assert.equal(h.calls[0].url, '/api/titles/sg0');
  assert.match(h.html(), /Show S01E01/);
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

const tvList = (extra = {}) => ({ job_id: 'job-1', revision: 4, kind: 'tv', show: 'Show', titles: [0, 1, 2, 3].map(i => title(i)), automatic: [], ...extra });
const open = (h, options = {}) => h.t.openTitlesDialog({
  heading: 'Titles', load: () => h.calls.push({ method: 'LOAD' }) && options.list,
  save: (data, ids, numbers) => { h.calls.push({ method: 'SAVE', ids, numbers }); return options.saved ? options.saved() : { ok: true }; },
  ...options,
});

test('a list that fails to load says why, saves nothing, and keeps the extra action', async () => {
  const h = await load();
  let wired = 0;
  await h.t.openTitlesDialog({ heading: 'Confirm episodes', load: async () => { throw new Error('not a held rip'); },
    save: async () => assert.fail('nothing to save'), extraFoot: '<button data-rv-name>Change name…</button>', onExtra: () => { wired++; } });
  assert.match(h.body.innerHTML, /Could not load the titles: not a held rip/);
  assert.equal(h.buttons['[data-tl-save]'].disabled, true);
  assert.equal(h.buttons['[data-tl-auto]'].onclick, undefined);
  assert.match(h.modals[0].foot, /data-tl-auto disabled/, 'Automatic waits for the list');
  assert.equal(wired, 1, 'Change name stays one click away');
});

test('a refused save keeps the dialog open and reports nothing as saved', async () => {
  const h = await load();
  let saved = 0;
  await open(h, { list: tvList({ pick: [0, 1] }), saveLabel: 'Confirm episodes', onSaved: () => { saved++; },
    saved: () => { throw new Error('refused (409)'); } });
  await h.buttons['[data-tl-save]'].onclick({ currentTarget: {} });
  assert.equal(h.closed, 0);
  assert.equal(saved, 0);
  assert.deepEqual(h.toasts, ['Confirm episodes: refused (409)']);
});

test('an empty title list explains itself and cannot be saved', async () => {
  for (const kind of ['tv', 'movie']) {
    const h = await load();
    await open(h, { list: { kind, titles: [], automatic: [] } });
    assert.match(h.list.innerHTML, /no titles to choose from/);
    assert.equal(h.buttons['[data-tl-save]'].disabled, true, kind);
  }
});

test('a Movie is one radio choice, with no order or names, and saves exactly that title', async () => {
  const h = await load();
  await open(h, { list: { kind: 'movie', show: 'Film', titles: [0, 1, 2].map(i => title(i)), automatic: [1] } });
  assert.match(h.list.innerHTML, /type="radio"/);
  assert.doesNotMatch(h.list.innerHTML, /type="checkbox"|data-tl-move|→|Film S01/);
  assert.match(h.list.innerHTML, /The movie/);
  h.handlers.change({ target: h.target('[data-tl-tick]', { tlTick: '2' }) });
  assert.equal(h.buttons['[data-tl-save]'].disabled, false);
  await h.buttons['[data-tl-save]'].onclick({ currentTarget: {} });
  assert.deepEqual([...h.calls.at(-1).ids.map(id => id.playlist_id)], [2]);
});

test('the order buttons stop at the top and the bottom', async () => {
  const h = await load();
  await open(h, { list: tvList({ pick: [2, 0, 3] }) });
  const disabled = dir => [...h.list.innerHTML.matchAll(new RegExp('data-tl-move="' + dir + '" data-tl-index="(\\d)"( disabled)?', 'g'))]
    .map(m => [Number(m[1]), !!m[2]]);
  assert.deepEqual(disabled('-1'), [[2, true], [0, false], [3, false]]);
  assert.deepEqual(disabled('1'), [[2, false], [0, false], [3, true]]);
  assert.match(h.list.innerHTML, /1\.<\/span> Title 3[\s\S]*2\.<\/span> Title 1[\s\S]*3\.<\/span> Title 4/);
  h.handlers.click({ target: h.target('[data-tl-move]', { tlIndex: '2', tlMove: '-1' }) });
  h.handlers.click({ target: h.target('[data-tl-move]', { tlIndex: '3', tlMove: '1' }) });
  await h.buttons['[data-tl-save]'].onclick({ currentTarget: {} });
  assert.deepEqual([...h.calls.at(-1).ids.map(id => id.playlist_id)], [2, 0, 3], 'out-of-range moves change nothing');
});

test('typed numbers drive the preview and the save; invalid ones block it; typing keeps the inputs', async () => {
  const h = await load();
  await open(h, { numbers: true, rename: 'Change match', list: tvList({ season: 2, disc: 3, pick: [0, 1] }) });
  assert.match(h.body.innerHTML, /use Change match\./);
  assert.match(h.list.innerHTML, /Show S02E05[\s\S]*Show S02E06/, 'disc 3 of two episodes starts at 5');
  const writes = h.body.writes;
  const type = (key, value, validity) => {
    h.inputs[key].value = value; h.inputs[key].validity = validity;
    h.handlers.input({ target: h.target('[data-tl-num]', { tlNum: key }) });
  };
  type('episode_start', '7');
  assert.equal(h.body.writes, writes, 'the inputs are not re-rendered while typing');
  assert.match(h.list.innerHTML, /Show S02E07[\s\S]*Show S02E08/);
  for (const [key, raw, validity] of [['season', '0'], ['disc', '1.5'], ['episode_start', '65536'], ['season', '', { badInput: true }]]) {
    type(key, raw, validity);
    assert.equal(h.buttons['[data-tl-save]'].disabled, true, key + '=' + raw);
    assert.match(h.list.innerHTML, /must be an integer between 1 and 65535, or blank/);
    type(key, key === 'season' ? '2' : key === 'disc' ? '3' : '7');
  }
  type('disc', '');
  assert.equal(h.buttons['[data-tl-save]'].disabled, false);
  await h.buttons['[data-tl-save]'].onclick({ currentTarget: {} });
  assert.deepEqual({ ...h.calls.at(-1).numbers }, { season: 2, disc: null, episode_start: 7 });
});

test('a Movie with editable numbers opens without them and saves the list\'s', async () => {
  const h = await load();
  await open(h, { numbers: true, list: { kind: 'movie', titles: [title(0)], automatic: [0], season: null, disc: null, episode_start: null } });
  assert.doesNotMatch(h.body.innerHTML, /data-tl-num/);
  assert.equal(h.buttons['[data-tl-save]'].disabled, false);
  await h.buttons['[data-tl-save]'].onclick({ currentTarget: {} });
  assert.deepEqual({ ...h.calls.at(-1).numbers }, { season: null, disc: null, episode_start: null });
});

test('the lead says why it asks and what the ticks mean', async () => {
  const { t } = await load();
  assert.match(t.confirmLead({ reason: 'incomplete-episode-roster', suggested: [1, 2], clusters: [[1, 2]] }),
    /episode list is incomplete\. The suggested episodes are ticked in play order/);
  assert.match(t.confirmLead({ reason: 'conflicting-episode-roster', suggested: [], clusters: [[0, 1], [2, 3]] }),
    /contradicts itself\. Several groups of similar titles/);
  assert.match(t.confirmLead({ reason: 'something-new', suggested: [], clusters: [] }),
    /does not list its episodes\. Tick the episodes, in order\./);
  assert.equal(t.confirmLead(null), '');
});

test('a ticked suggestion is not regrouped; unticking moves a title to the other titles', async () => {
  const { t } = await load();
  const data = tvList({ confirm: { reason: 'missing-episode-roster', suggested: [1, 2], clusters: [[1, 2]], alternates: [] } });
  const groups = pick => JSON.parse(JSON.stringify(t.titleGroups(data, pick).map(g => [g.label, g.rows.map(r => r.index)])));
  assert.deepEqual(groups([1, 2]), [['Other titles', [0, 3]]]);
  assert.deepEqual(groups([2]), [['Other titles', [0, 1, 3]]]);
  assert.deepEqual([...t.titleGroups({ titles: [title(0)] }, []).map(g => g.label)], [''], 'one plain list needs no heading');
});

test('alternates and uncaptured titles show unticked; uncaptured ones cannot be ticked', async () => {
  const h = await load();
  const titles = [title(0, { in_roster: true }), title(1, { alternate_of: 0 }), title(2, { swept: false }), title(3, { play_all: true })];
  await open(h, { list: { kind: 'tv', show: 'Show', titles, pick: [0, 2], automatic: [], episode_start: 1 } });
  const row = i => h.list.innerHTML.split('<div class="pipe-row">').find(r => r.includes('data-tl-tick="' + i + '"'));
  assert.match(row(0), / checked/);
  assert.match(row(0), /Episode on the disc's list/);
  assert.match(row(1), /Alternate of #1/);
  assert.doesNotMatch(row(1), / checked/);
  assert.match(row(2), / disabled/);
  assert.match(row(2), /Not captured/);
  assert.doesNotMatch(row(2), / checked/, 'a saved pick of an uncaptured title opens unticked');
  assert.match(row(3), /Play all/);
  assert.match(h.body.innerHTML, /Only titles this capture read can be chosen here/);
});

test('the drive dialog names itself by the hold and saves to the drive', async () => {
  const h = await load(async () => tvList({ pick: [0, 1] }));
  await h.t.driveTitles('sg0', { tmdb_title: 'Show' });
  assert.equal(h.modals[0].title, 'Titles · Show');
  assert.match(h.body.innerHTML, /To change the name or Movie\/TV, use Change title\./);
  h.handlers.change({ target: h.target('[data-tl-tick]', { tlTick: '1' }) });
  assert.match(h.list.innerHTML, /set it with Change title, then choose again/, 'one episode is numbered elsewhere');
  assert.match(h.modals[0].foot, />Save titles</);
  const held = await load(async () => tvList());
  await held.t.driveTitles('sg0', { confirm: {}, disc_name: 'SHOW_D1' });
  assert.equal(held.modals[0].title, 'Confirm episodes · SHOW_D1');
  assert.match(held.modals[0].foot, />Confirm episodes</);
});

// ui.js routes a remote drive's title list and pick through its peer.
test('a remote drive\'s titles go through its peer', async () => {
  const source = await readFile(new URL('../../src/server/web/assets/ui.js', import.meta.url), 'utf8');
  const fetched = [];
  const context = vm.createContext({ fetch: async (url, opt) => { fetched.push([url, opt.method]); return { ok: true, status: 200, text: async () => '{"ok":true}' }; }, JSON });
  vm.runInContext(source.slice(source.indexOf('export async function api(')).replace('export ', '').split('/** Run `fn`')[0], context);
  await context.api('GET', '/api/titles/p1a2b:sg0');
  await context.api('POST', '/api/titles/p1a2b%3Aioreg%3A12', { titles: [] });
  await context.api('GET', '/api/titles/sg0');
  assert.deepEqual(fetched, [['/api/peers/p1a2b/api/titles/sg0', 'GET'], ['/api/peers/p1a2b/api/titles/ioreg%3A12', 'POST'], ['/api/titles/sg0', 'GET']]);
});
