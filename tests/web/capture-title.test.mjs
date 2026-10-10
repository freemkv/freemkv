import { test } from 'node:test';
import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import vm from 'node:vm';

async function harness(searchApi) {
  const source = await readFile(new URL('../../src/server/web/assets/ripper.js', import.meta.url), 'utf8');
  const nodes = {}, calls = [];
  let body;
  const context = vm.createContext({
    esc: s => String(s ?? ''), toast: () => {}, ownDevice: s => s, deviceOwner: () => '',
    ownerUrl: (_owner, url) => url, twoStep: () => {}, parseName: title => ({ title, year: 0 }),
    act: (_button, operation) => operation(), api: async (method, url, payload) => { calls.push({ method, url, payload }); return method === 'GET' && searchApi ? searchApi(url) : {}; },
    modal: options => {
      body = options.body;
      for (const id of ['tp-q', 'tp-res', 'tp-search', 'tp-manual', 'tp-episode', 'rv-proceed', 'rv-cancel']) {
        nodes[id] = { value: '', select() {}, focus() {}, addEventListener(event, fn) { this[event] = fn; } };
      }
      nodes['tp-q'].value = 'Show Season 1 Disc 2';
      nodes['tp-episode'].value = body.match(/id="tp-episode"[^>]*value="([^"]*)"/)?.[1] || '';
      return { el: { querySelector: selector => nodes[selector.slice(1)] }, close() {} };
    },
  });
  vm.runInContext(source.slice(source.indexOf('function titlePicker('), source.indexOf('/* A device log line:')), context);
  return { context, nodes, calls };
}

test('capture manual TV correction preserves identity and confirmed numbering', async () => {
  const h = await harness();
  h.context.changeTitle('sr0', { job_id: 'stable-job', media_type: 'tv', episode_start: 4 });
  assert.equal(h.nodes['tp-episode'].value, '4');
  await h.nodes['tp-manual'].onclick({ currentTarget: {} });
  assert.equal(h.calls[0].payload.job_id, 'stable-job');
  assert.equal(h.calls[0].payload.episode_start, 4);
  assert.equal(h.calls[0].payload.media_type, 'tv');
});

test('capture numbering rejects invalid input and clears it for movies', async () => {
  const h = await harness();
  h.context.changeTitle('sr0', { media_type: 'tv' });
  for (const raw of ['0', '-1', '65536', '1.5']) {
    h.nodes['tp-episode'].value = raw;
    await h.nodes['tp-manual'].onclick({ currentTarget: {} });
  }
  assert.equal(h.calls.length, 0);
  h.context.changeTitle('sr0', { media_type: 'movie', episode_start: 4 });
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
  h.context.changeTitle('sr0', { media_type: 'tv', episode_start: 4 });
  h.nodes['tp-episode'].value = '';
  h.nodes['tp-episode'].validity = { badInput: true };
  await h.nodes['tp-manual'].onclick({ currentTarget: {} });
  assert.equal(h.calls.length, 0);
});

test('an older capture lookup cannot replace the newest results', async () => {
  const pending = [];
  const h = await harness(() => new Promise(resolve => pending.push(resolve)));
  h.context.changeTitle('sr0', { media_type: 'tv', episode_start: 4 });
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
