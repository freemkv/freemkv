import { test } from 'node:test';
import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import vm from 'node:vm';

async function discHtml() {
  const source = await readFile(new URL('../../src/server/web/assets/ripper.js', import.meta.url), 'utf8');
  const context = vm.createContext({
    esc: s => String(s ?? ''), ICON: { disc: '' }, discIn: () => true, fmtMs: ms => (ms / 1000) + 's',
  });
  vm.runInContext(source.slice(source.indexOf('const pad2 ='), source.indexOf('function stepsHtml(')), context);
  return context.discHtml;
}

const tv = (episodes) => ({
  tmdb_title: 'Show', tmdb_media_type: 'tv', duration: '0h 51m', status: 'ripping', episodes,
});

test('a TV card lists its planned episodes in place of one title length', async () => {
  const html = (await discHtml())(tv([
    { season: 2, episode: 4, name: 'Dunes', duration: '0h 51m' },
    { season: 2, episode: 5, name: '', duration: '0h 52m' },
  ]));
  assert.match(html, /2 episodes · S02E04–E05/);
  assert.match(html, /E04<\/span><span class="grow">Dunes/);
  assert.match(html, /E05<\/span><span class="grow">Episode 5/);
  assert.doesNotMatch(html, /<span>0h 51m<\/span>/);
});

test('a card without an episode plan keeps the title length', async () => {
  const html = (await discHtml())(tv(undefined));
  assert.match(html, /<span>0h 51m<\/span>/);
  assert.doesNotMatch(html, /class="eps"/);
});

async function ripFigures() {
  const source = await readFile(new URL('../../src/server/web/assets/ripper.js', import.meta.url), 'utf8');
  const context = vm.createContext({
    esc: s => String(s ?? ''), fmtMs: ms => (ms / 1000) + 's', fmtBytes: b => b + 'B',
    fmtSpeed: v => v + 'MB/s', plural: (n, w) => n + ' ' + w,
  });
  vm.runInContext(source.slice(source.indexOf('function ripFigures('), source.indexOf('function bannersHtml(')), context);
  return context.ripFigures;
}

const damaged = { bytes_maybe: 100, bytes_lost: 0, main_at_risk_ms: 4000, pass_progress_pct: 10 };

test('the at-risk pill sums a TV plan over its episodes', async () => {
  const { pills } = (await ripFigures())({ ...damaged, episodes: [{ at_risk_ms: 2000 }, { at_risk_ms: 3000 }] });
  assert.match(pills, /~5s of the episodes/);
  assert.doesNotMatch(pills, /movie/);
});

test('the at-risk pill reads the main title for a movie', async () => {
  const { pills } = (await ripFigures())(damaged);
  assert.match(pills, /~4s of the movie/);
});

test('an episode row shows its own at-risk time', async () => {
  const html = (await discHtml())(tv([
    { season: 1, episode: 1, name: 'A', duration: '0h 51m', at_risk_ms: 12000 },
    { season: 1, episode: 2, name: 'B', duration: '0h 51m', at_risk_ms: 0 },
  ]));
  assert.match(html, /A<\/span><span class="warn"[^>]*>~12s at risk/);
  assert.doesNotMatch(html, /B<\/span><span class="warn"/);
});
