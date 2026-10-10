import { test } from 'node:test';
import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import vm from 'node:vm';

async function harness(kind = 'movie', searchApi, queued = 1, savedMedia = { title: 'Old', season: 1 }, candidates = []) {
  const source = await readFile(new URL('../../src/server/web/assets/remux.js', import.meta.url), 'utf8');
  const context = vm.createContext({});
  const calls = [], nodes = {};
  const legacyNodes = candidates.map(c => ({ checked: false, dataset: { legacyOutput: String(c.id) } }));
  nodes['confirm-ownership'] = { checked: false };
  for (const key of ['results', 'kind', 'search', 'query', 'season', 'disc', 'episode-start']) nodes[key] = { value: '' };
  nodes.query.value = 'Cast Away'; nodes.season.value = '1';
  let closed = false, refreshed = 0, body = '', closeHook;
  const choice = { title: '<Cast Away>', year: 2000, tmdb_id: 8358, media_type: kind };
  const supplied = {
    esc: value => String(value ?? '').replaceAll('&', '&amp;').replaceAll('<', '&lt;').replaceAll('>', '&gt;').replaceAll('"', '&quot;'),
    act: async (_button, operation) => operation(),
    api: async (method, url, payload) => {
      calls.push({ method, url, payload });
      if (method === 'POST') return url === '/api/library/queue/add'
        ? { ok: true, queued }
        : { ok: true, queued, saved: { revision: payload.expected_revision + 1, media: payload.media } };
      if (url.startsWith('/api/tmdb/search')) return searchApi ? searchApi(url) : [choice];
      return { saved: { revision: 7, media: savedMedia }, owned_outputs: ['/tv/<old>.mkv'], preview_token: 'server-snapshot', candidates };
    },
    modal: options => {
      body = options.body;
      for (const key of ['season', 'disc', 'episode-start']) {
        nodes[key].value = body.match(new RegExp('data-' + key + ' value="([^"]*)"'))?.[1] || '';
      }
      return { el: { querySelector: selector => nodes[selector.slice(6, -1)], querySelectorAll: () => legacyNodes.filter(n => n.checked) },
        onClose: hook => { closeHook = hook; }, close: () => { closed = true; closeHook?.(); } };
    },
    refreshNow: () => { refreshed++; }, toast: () => {}, stagedActions: () => 'retained',
  };
  const module = new vm.SourceTextModule(source + '\nexport { changeMatch, actHtml };', { context });
  const imports = new Map([...source.matchAll(/import\s*\{([^}]+)\}\s*from\s*'([^']+)'/g)]
    .map(m => [m[2], m[1].split(',').map(s => s.trim())]));
  await module.link(async specifier => {
    const names = imports.get(specifier);
    return new vm.SyntheticModule(names, function () {
      for (const name of names) this.setExport(name, supplied[name] || (() => {}));
    }, { context });
  });
  await module.evaluate();
  return { ui: module.namespace, nodes, legacyNodes, calls, get body() { return body; }, get closed() { return closed; }, get refreshed() { return refreshed; } };
}

for (const kind of ['movie', 'tv']) test(`change match submits explicit ${kind} identity and revision`, async () => {
  const h = await harness(kind);
  await h.ui.changeMatch({ iso: '/iso/<source>.iso', title: 'Old TV', target: '/tv/old.mkv' }, {});
  assert.ok(h.body.includes('&lt;source&gt;'));
  assert.ok(h.body.includes('/tv/&lt;old&gt;.mkv'));
  await h.nodes.search.onclick();
  assert.ok(h.nodes.results.innerHTML.includes('&lt;Cast Away&gt;'));
  assert.ok(!h.nodes.results.innerHTML.includes('<Cast Away>'));
  h.nodes.season.value = '2'; h.nodes.disc.value = '3'; h.nodes['episode-start'].value = '4';
  await h.nodes.results.onclick({ target: { closest: () => ({ dataset: { pick: '0' } }) } });
  const post = h.calls.find(c => c.method === 'POST');
  assert.equal(post.url, '/api/library/match/remux');
  assert.equal(post.payload.expected_revision, 7);
  assert.equal(post.payload.media.kind, kind);
  assert.equal(post.payload.media.season, kind === 'tv' ? 2 : null);
  assert.equal(post.payload.media.disc, kind === 'tv' ? 3 : null);
  assert.equal(post.payload.media.episode_start, kind === 'tv' ? 4 : null);
  assert.equal(post.payload.ownership.preview_token, 'server-snapshot');
  assert.equal(post.payload.ownership.selected_candidates.length, 0);
  assert.equal(h.calls.some(c => c.url === '/api/library/queue/add'), false);
  assert.equal(h.closed, true);
  assert.equal(h.refreshed, 1);
});

test('saved TV first episode is displayed and submitted without re-entry', async () => {
  const h = await harness('tv', undefined, 1, { title: 'Show', season: 1, disc: 2, episode_start: 4 });
  await h.ui.changeMatch({ iso: '/iso/source.iso', title: 'Show' }, {});
  assert.equal(h.nodes['episode-start'].value, '4');
  assert.ok(h.body.includes('First episode'));
  await h.nodes.search.onclick();
  await h.nodes.results.onclick({ target: { closest: () => ({ dataset: { pick: '0' } }) } });
  assert.equal(h.calls.find(c => c.method === 'POST').payload.media.episode_start, 4);
});

test('older saved TV match without episode override submits explicit null, not a disc-derived offset', async () => {
  const h = await harness('tv', undefined, 1, { title: 'Show', season: 1, disc: 2 });
  await h.ui.changeMatch({ iso: '/iso/source.iso', title: 'Show' }, {});
  assert.equal(h.nodes['episode-start'].value, '');
  await h.nodes.search.onclick();
  await h.nodes.results.onclick({ target: { closest: () => ({ dataset: { pick: '0' } }) } });
  assert.equal(h.calls.find(c => c.method === 'POST').payload.media.episode_start, null);
});

for (const value of ['1', '65535', '']) test(`TV first episode accepts boundary or cleared value ${JSON.stringify(value)}`, async () => {
  const h = await harness('tv', undefined, 1, { title: 'Show', season: 1, episode_start: 4 });
  await h.ui.changeMatch({ iso: '/iso/source.iso', title: 'Show' }, {});
  await h.nodes.search.onclick();
  h.nodes['episode-start'].value = value;
  await h.nodes.results.onclick({ target: { closest: () => ({ dataset: { pick: '0' } }) } });
  assert.equal(h.calls.find(c => c.method === 'POST').payload.media.episode_start, value ? Number(value) : null);
});

test('browser number-input badInput is not mistaken for a cleared override', async () => {
  const h = await harness('tv');
  await h.ui.changeMatch({ iso: '/iso/source.iso', title: 'Show' }, {});
  await h.nodes.search.onclick();
  h.nodes['episode-start'].validity = { badInput: true };
  await h.nodes.results.onclick({ target: { closest: () => ({ dataset: { pick: '0' } }) } });
  assert.equal(h.calls.filter(c => c.method === 'POST').length, 0);
  assert.equal(h.closed, false);
});

test('older TV lookup response cannot override entered first episode', async () => {
  const pending = [];
  const h = await harness('tv', () => new Promise(resolve => pending.push(resolve)));
  await h.ui.changeMatch({ iso: '/iso/source.iso', title: 'Show' }, {});
  const first = h.nodes.search.onclick();
  const second = h.nodes.search.onclick();
  h.nodes['episode-start'].value = '4';
  pending[1]([{ title: 'Correct show', tmdb_id: 2, media_type: 'tv' }]);
  await second;
  pending[0]([{ title: 'Stale show', tmdb_id: 1, media_type: 'tv', episode_start: 2 }]);
  await first;
  await h.nodes.results.onclick({ target: { closest: () => ({ dataset: { pick: '0' } }) } });
  const media = h.calls.find(c => c.method === 'POST').payload.media;
  assert.equal(media.tmdb_id, 2);
  assert.equal(media.episode_start, 4);
});

for (const value of ['0', '-1', '65536', '1.5', 'invalid']) test(`invalid first episode ${value} refuses save`, async () => {
  const h = await harness('tv');
  await h.ui.changeMatch({ iso: '/iso/source.iso', title: 'Show' }, {});
  await h.nodes.search.onclick();
  h.nodes['episode-start'].value = value;
  await h.nodes.results.onclick({ target: { closest: () => ({ dataset: { pick: '0' } }) } });
  assert.equal(h.calls.filter(c => c.method === 'POST').length, 0);
  assert.equal(h.closed, false);
});

test('a saved match is not reported as queued when admission fails', async () => {
  const h = await harness('movie', undefined, 0);
  await h.ui.changeMatch({ iso: '/iso/source.iso', title: 'Old', target: '/old.mkv' }, {});
  await h.nodes.search.onclick();
  const pick = { target: { closest: () => ({ dataset: { pick: '0' } }) } };
  await h.nodes.results.onclick(pick);
  assert.equal(h.closed, true);
  await h.nodes.results.onclick(pick);
  const saves = h.calls.filter(c => c.method === 'POST' && c.url === '/api/library/match/remux');
  assert.deepEqual(saves.map(c => c.payload.expected_revision), [7]);
});

test('legacy candidates start unchecked and require separate ownership affirmation', async () => {
  const h = await harness('movie', undefined, 1, undefined, [{ id: 2, path: '/tv/<episode>.mkv', size_bytes: 10 }]);
  await h.ui.changeMatch({ iso: '/iso/source.iso', title: 'Old' }, {});
  assert.ok(h.body.includes('/tv/&lt;episode&gt;.mkv'));
  assert.ok(!/data-legacy-output="2"[^>]*checked/.test(h.body));
  assert.equal(h.legacyNodes[0].checked, false);
  await h.nodes.search.onclick();
  const pick = { target: { closest: () => ({ dataset: { pick: '0' } }) } };
  h.legacyNodes[0].checked = true;
  await h.nodes.results.onclick(pick);
  assert.equal(h.calls.filter(c => c.method === 'POST').length, 0);
  h.nodes['confirm-ownership'].checked = true;
  await h.nodes.results.onclick(pick);
  const ownership = h.calls.find(c => c.method === 'POST').payload.ownership;
  assert.deepEqual(JSON.parse(JSON.stringify(ownership)), {
    preview_token: 'server-snapshot', selected_candidates: [2], confirm_ownership: true,
  });
});

test('an older search cannot overwrite the newest match results', async () => {
  const pending = [];
  const h = await harness('movie', () => new Promise(resolve => pending.push(resolve)));
  await h.ui.changeMatch({ iso: '/iso/source.iso', title: 'Old' }, {});
  const first = h.nodes.search.onclick();
  h.nodes.query.value = 'Correct movie';
  const second = h.nodes.search.onclick();
  pending[1]([{ title: 'Correct movie', tmdb_id: 2, media_type: 'movie' }]);
  await second;
  pending[0]([{ title: 'Stale movie', tmdb_id: 1, media_type: 'movie' }]);
  await first;
  assert.ok(h.nodes.results.innerHTML.includes('Correct movie'));
  assert.ok(!h.nodes.results.innerHTML.includes('Stale movie'));
  await h.nodes.results.onclick({ target: { closest: () => ({ dataset: { pick: '0' } }) } });
  assert.equal(h.calls.find(c => c.method === 'POST').payload.media.tmdb_id, 2);
});

test('invalid TV season does not save or close the dialog', async () => {
  const h = await harness('tv');
  await h.ui.changeMatch({ iso: '/iso/source.iso', title: 'Old' }, {});
  await h.nodes.search.onclick();
  h.nodes.season.value = '0';
  await h.nodes.results.onclick({ target: { closest: () => ({ dataset: { pick: '0' } }) } });
  assert.equal(h.calls.filter(c => c.method === 'POST').length, 0);
  assert.equal(h.closed, false);
});

test('active and retained jobs do not offer Change match', async () => {
  const h = await harness();
  for (const job of [{ state: 'queued' }, { state: 'running' }, { state: 'failed', staged: '/kept.mkv' }]) {
    assert.ok(!h.ui.actHtml({ kind: 'remux', title: 'Old', job }).includes('data-match'));
  }
  assert.ok(h.ui.actHtml({ kind: 'iso_only', title: 'Old' }).includes('data-match'));
});

test('an ambiguous row with an exact ISO offers correction but not automatic remux', async () => {
  const h = await harness();
  const html = h.ui.actHtml({ kind: 'ambiguous', iso: '/iso/one.iso', title: 'Old episodes' });
  assert.ok(html.includes('data-match'));
  assert.ok(!html.includes('data-remux'));
});
