import { test } from 'node:test';
import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import vm from 'node:vm';

async function discHtml() {
  const source = await readFile(new URL('../../src/server/web/assets/ripper.js', import.meta.url), 'utf8');
  const context = vm.createContext({
    esc: s => String(s ?? ''), ICON: { disc: '' }, discIn: () => true,
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
