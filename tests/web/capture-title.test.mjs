import { test } from 'node:test';
import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import vm from 'node:vm';

async function harness(searchApi) {
  const source = await readFile(new URL('../../src/server/web/assets/ripper.js', import.meta.url), 'utf8');
  const nodes = {}, calls = [], toasts = [], dialogs = [];
  let body, foot;
  const context = vm.createContext({
    esc: s => String(s ?? ''), toast: (text) => { toasts.push(text); }, ownDevice: s => s, deviceOwner: () => '',
    ownerUrl: (owner, url) => owner ? '/api/peers/' + owner + url : url, twoStep: () => {}, parseName: title => ({ title, year: 0 }),
    act: (_button, operation) => operation(), api: async (method, url, payload) => { calls.push({ method, url, payload }); return method === 'GET' && searchApi ? searchApi(url) : {}; },
    openTitlesDialog: options => { dialogs.push(options); },
    modal: options => {
      body = options.body;
      foot = options.foot;
      for (const key of Object.keys(nodes)) delete nodes[key];
      for (const id of ['tp-q', 'tp-res', 'tp-search', 'tp-manual', 'tp-episode', 'tp-season', 'tp-disc', 'tp-kind', 'tp-tv', 'rv-proceed', 'rv-cancel', 'rv-episodes']) {
        if (id === 'rv-episodes' && !foot.includes('id="rv-episodes"')) continue;
        nodes[id] = { value: '', style: {}, select() {}, focus() {}, addEventListener(event, fn) { this[event] = fn; } };
      }
      nodes['tp-q'].value = 'Show Season 1 Disc 2';
      for (const id of ['tp-episode', 'tp-season', 'tp-disc']) {
        nodes[id].value = body.match(new RegExp('id="' + id + '"[^>]*value="([^"]*)"'))?.[1] || '';
      }
      return { el: { querySelector: selector => nodes[selector.slice(1)] }, close() { nodes.closed = true; } };
    },
  });
  vm.runInContext(source.slice(source.indexOf('function titlePicker('), source.indexOf('/* A device log line:')), context);
  return { context, nodes, calls, toasts, dialogs, get foot() { return foot; } };
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

test('a typed movie name ignores the hidden TV numbers, even invalid ones', async () => {
  const h = await harness();
  h.context.changeTitle('sr0', { job_id: 'job', media_kind: 'tv' });
  h.nodes['tp-season'].value = '0';
  h.nodes['tp-kind'].value = 'movie';
  h.nodes['tp-kind'].onchange();
  await h.nodes['tp-manual'].onclick({ currentTarget: {} });
  const { media_type, season, disc, episode_start } = h.calls[0].payload;
  assert.deepEqual([media_type, season, disc, episode_start], ['movie', null, null, null]);
  assert.equal(h.toasts.some(t => /must be/.test(t)), false);
});

// A held rip waiting on its episodes opens the titles dialog on its saved image.
test('a held rip with episodes to confirm opens on its titles and saves them to its dir', async () => {
  const h = await harness();
  let reloaded = 0;
  h.context.reviewDialog({ dir: 'job-7', title: 'Show', media_type: 'tv', episode_confirm: { suggested: [1, 2] } }, () => { reloaded++; }, 'p9');
  assert.equal(h.dialogs.length, 1);
  const d = h.dialogs[0];
  assert.equal(d.heading, 'Confirm episodes · Show');
  assert.equal(d.canClear, false, 'a held rip has no automatic to go back to');
  assert.equal(d.rename, 'Change name');
  await d.load();
  assert.equal(h.calls[0].url, '/api/peers/p9/api/review/titles?dir=job-7');
  const ids = [{ playlist: 'p2' }, { playlist: 'p1' }];
  await d.save({ revision: 3 }, ids);
  assert.equal(h.calls[1].method, 'POST');
  assert.equal(h.calls[1].url, '/api/peers/p9/api/review/resolve');
  assert.deepEqual(JSON.parse(JSON.stringify(h.calls[1].payload)), { dir: 'job-7', action: 'episodes', revision: 3, titles: ids });
  d.onSaved();
  assert.equal(reloaded, 1);
  // Change name… swaps to the name picker for the same rip.
  const rvName = { onclick: null };
  let closed = false;
  d.onExtra({ el: { querySelector: sel => sel === '[data-rv-name]' ? rvName : null }, close() { closed = true; } });
  rvName.onclick();
  assert.equal(closed, true);
  assert.match(h.foot, /id="rv-proceed"/);
});

test('the name picker offers Episodes… for a TV rip only', async () => {
  const h = await harness();
  h.context.reviewDialog({ dir: 'job-7', title: 'Film', media_type: 'movie' }, () => {});
  assert.doesNotMatch(h.foot, /rv-episodes/);
  assert.equal(h.dialogs.length, 0);
  h.context.reviewDialog({ dir: 'job-8', title: 'Show', media_type: 'tv' }, () => {});
  assert.match(h.foot, /id="rv-episodes"/);
  h.nodes['rv-episodes'].onclick();
  assert.equal(h.nodes.closed, true);
  assert.equal(h.dialogs.length, 1);
  await h.dialogs[0].load();
  assert.equal(h.calls[0].url, '/api/review/titles?dir=job-8');
});

async function ripperSlice(from, to, extra = {}) {
  const source = await readFile(new URL('../../src/server/web/assets/ripper.js', import.meta.url), 'utf8');
  const context = vm.createContext({ esc: s => String(s ?? ''), escLinks: s => String(s ?? ''), ...extra });
  vm.runInContext(source.slice(source.indexOf('const ACTIVE ='), source.indexOf('// ── Formatting'))
    + source.slice(source.indexOf('const discIn ='), source.indexOf('function stateLabel('))
    + source.slice(source.indexOf(from), source.indexOf(to)), context);
  return context;
}

test('a held rip waiting on its episodes is labelled Confirm episodes in the list', async () => {
  const { reviewHtml } = await ripperSlice('function reviewHtml(', 'function parseName(');
  const html = reviewHtml([{ dir: 'a', title: 'Show', episode_confirm: { suggested: [] } }, { dir: 'b', title: 'Film' }]);
  assert.match(html, /data-review="a">Confirm episodes…</);
  assert.match(html, /data-review="b">Review…</);
});

test('a drive offers Titles… before the rip and a Confirm episodes banner while it holds', async () => {
  const { bannersHtml, actionsHtml } = await ripperSlice('function bannersHtml(', '// ── A drive card');
  const card = { job_id: 'j', tmdb_title: 'Show', status: 'idle' };
  for (const status of ['idle', 'error']) assert.match(actionsHtml('sg0', { ...card, status }), /data-titles>Titles…/, status);
  for (const status of ['ripping', 'scanning', 'done']) assert.doesNotMatch(actionsHtml('sg0', { ...card, status }), /data-titles/, status);
  assert.doesNotMatch(actionsHtml('sg0', { ...card, job_id: '' }), /data-titles/, 'no job, nothing to save it with');
  assert.doesNotMatch(actionsHtml('sg0', { job_id: 'j', status: 'idle' }), /data-titles/, 'not scanned yet');
  assert.doesNotMatch(actionsHtml('sg0', { ...card, loss_aborted: true }), /data-titles/, 'a loss-aborted rip is past choosing');
  const confirm = { ...card, confirm: { suggested: [1, 2] } };
  assert.match(bannersHtml(confirm), /data-titles>Confirm episodes…/);
  assert.doesNotMatch(bannersHtml({ ...confirm, status: 'ripping' }), /data-titles/);
  assert.doesNotMatch(bannersHtml({ ...confirm, job_id: '' }), /data-titles/);
  assert.doesNotMatch(bannersHtml(card), /data-titles/);
});
