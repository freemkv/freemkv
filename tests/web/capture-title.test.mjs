import { test } from 'node:test';
import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import vm from 'node:vm';

async function harness(searchApi) {
  const source = await readFile(new URL('../../src/server/web/assets/ripper.js', import.meta.url), 'utf8');
  const nodes = {}, calls = [], toasts = [];
  let body;
  const context = vm.createContext({
    esc: s => String(s ?? ''), toast: (text) => { toasts.push(text); }, ownDevice: s => s, deviceOwner: () => '',
    ownerUrl: (_owner, url) => url, twoStep: () => {}, parseName: title => ({ title, year: 0 }),
    act: (_button, operation) => operation(), api: async (method, url, payload) => { calls.push({ method, url, payload }); return method === 'GET' && searchApi ? searchApi(url) : {}; },
    modal: options => {
      body = options.body;
      for (const id of ['tp-q', 'tp-res', 'tp-search', 'tp-manual', 'tp-episode', 'tp-season', 'tp-disc', 'tp-kind', 'tp-tv', 'rv-proceed', 'rv-cancel']) {
        nodes[id] = { value: '', style: {}, select() {}, focus() {}, addEventListener(event, fn) { this[event] = fn; } };
      }
      nodes['tp-q'].value = 'Show Season 1 Disc 2';
      for (const id of ['tp-episode', 'tp-season', 'tp-disc']) {
        nodes[id].value = body.match(new RegExp('id="' + id + '"[^>]*value="([^"]*)"'))?.[1] || '';
      }
      return { el: { querySelector: selector => nodes[selector.slice(1)] }, close() {} };
    },
  });
  vm.runInContext(source.slice(source.indexOf('function titlePicker('), source.indexOf('/* A device log line:')), context);
  return { context, nodes, calls, toasts };
}

test('capture manual TV correction preserves identity and confirmed numbering', async () => {
  const h = await harness();
  h.context.changeTitle('sr0', { job_id: 'stable-job', media_kind: 'tv', episode_start: 4 });
  assert.equal(h.nodes['tp-episode'].value, '4');
  await h.nodes['tp-manual'].onclick({ currentTarget: {} });
  assert.equal(h.calls[0].payload.job_id, 'stable-job');
  assert.equal(h.calls[0].payload.episode_start, 4);
  assert.equal(h.calls[0].payload.media_type, 'tv');
});

test('capture numbering rejects invalid input and clears it for movies', async () => {
  const h = await harness();
  h.context.changeTitle('sr0', { media_kind: 'tv' });
  for (const raw of ['0', '-1', '65536', '1.5']) {
    h.nodes['tp-episode'].value = raw;
    await h.nodes['tp-manual'].onclick({ currentTarget: {} });
  }
  assert.equal(h.calls.length, 0);
  h.context.changeTitle('sr0', { media_kind: 'movie', episode_start: 4 });
  await h.nodes['tp-manual'].onclick({ currentTarget: {} });
  assert.equal(h.calls[0].payload.episode_start, null);
});

test('held capture numbering can be confirmed without a drive', async () => {
  const h = await harness();
  h.context.reviewDialog({ dir: 'stable-job', media_type: 'tv', episode_start: 4 }, () => {});
  await h.nodes['rv-proceed'].onclick({ currentTarget: {} });
  assert.equal(h.calls[0].url, '/api/review/resolve');
  assert.equal(h.calls[0].payload.dir, 'stable-job');
  assert.equal(h.calls[0].payload.episode_start, 4);
});

test('invalid browser number input cannot silently clear confirmed numbering', async () => {
  const h = await harness();
  h.context.changeTitle('sr0', { media_kind: 'tv', episode_start: 4 });
  h.nodes['tp-episode'].value = '';
  h.nodes['tp-episode'].validity = { badInput: true };
  await h.nodes['tp-manual'].onclick({ currentTarget: {} });
  assert.equal(h.calls.length, 0);
});

test('an older capture lookup cannot replace the newest results', async () => {
  const pending = [];
  const h = await harness(() => new Promise(resolve => pending.push(resolve)));
  h.context.changeTitle('sr0', { media_kind: 'tv', episode_start: 4 });
  h.nodes['tp-q'].value = 'old';
  const old = h.nodes['tp-search'].onclick({ currentTarget: {} });
  h.nodes['tp-q'].value = 'new';
  const newest = h.nodes['tp-search'].onclick({ currentTarget: {} });
  pending[1]([{ title: 'New result', tmdb_id: 2, media_type: 'tv' }]);
  await newest;
  pending[0]([{ title: 'Stale result', tmdb_id: 1, media_type: 'tv' }]);
  await old;
  assert.match(h.nodes['tp-res'].innerHTML, /New result/);
  assert.doesNotMatch(h.nodes['tp-res'].innerHTML, /Stale result/);
  await h.nodes['tp-res'].click({ target: { closest: () => ({ dataset: { pick: '0' } }) } });
  assert.equal(h.calls.find(c => c.method === 'POST').payload.tmdb_id, 2);
});

// R1: a typed name can be marked TV without TMDB; its season, disc and First episode go with
// it, and the kind is always sent (the toggle starts at the disc's resolved kind).
test('a typed name marked TV sends its kind, season, disc and First episode', async () => {
  const h = await harness();
  h.context.changeTitle('sr0', { job_id: 'job', tmdb_media_type: 'movie', season: 1, disc: 2 });
  assert.equal(h.nodes['tp-kind'].value, 'movie', 'the resolved kind, else TMDB\'s');
  assert.equal(h.nodes['tp-tv'].style.display, 'none');
  h.nodes['tp-kind'].value = 'tv';
  h.nodes['tp-kind'].onchange();
  assert.equal(h.nodes['tp-tv'].style.display, '');
  h.nodes['tp-episode'].value = '5';
  await h.nodes['tp-manual'].onclick({ currentTarget: {} });
  const { media_type, season, disc, episode_start, tmdb_id } = h.calls[0].payload;
  assert.deepEqual([media_type, season, disc, episode_start, tmdb_id], ['tv', 1, 2, 5, 0]);
});

test('a disc that is TV by its label opens as TV, and a typed movie sends movie', async () => {
  const h = await harness();
  h.context.changeTitle('sr0', { job_id: 'job', media_kind: 'tv', tmdb_media_type: 'movie' });
  assert.equal(h.nodes['tp-kind'].value, 'tv');
  h.nodes['tp-kind'].value = 'movie';
  await h.nodes['tp-manual'].onclick({ currentTarget: {} });
  assert.equal(h.calls[0].payload.media_type, 'movie');
  assert.equal(h.calls[0].payload.episode_start, null);
});

test('TV numbers are refused, not dropped, for a movie picked from TMDB', async () => {
  const h = await harness(async () => [{ title: 'A Film', tmdb_id: 3, media_type: 'movie' }]);
  h.context.changeTitle('sr0', { job_id: 'job', media_kind: 'tv', episode_start: 4 });
  await h.nodes['tp-search'].onclick({ currentTarget: {} });
  await h.nodes['tp-res'].click({ target: { closest: () => ({ dataset: { pick: '0' } }) } });
  assert.equal(h.calls.filter(c => c.method === 'POST').length, 0);
  assert.match(h.toasts.at(-1), /TV only/);
});
