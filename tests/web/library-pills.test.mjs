// Run with: node --experimental-vm-modules --test tests/web/*.test.mjs
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import vm from 'node:vm';

const asset = (name) => readFile(new URL('../../src/server/web/assets/' + name, import.meta.url), 'utf8');

// Lift one top-level function's source out of an asset module.
function pick(src, name) {
  const at = src.indexOf('function ' + name + '(');
  let depth = 0, i = src.indexOf('{', at);
  for (; i < src.length; i++) {
    if (src[i] === '{') depth++;
    else if (src[i] === '}' && --depth === 0) break;
  }
  return src.slice(at, i + 1);
}

async function trackPills() {
  const ui = await asset('ui.js');
  const lib = await asset('library.js');
  const ctx = vm.createContext({});
  return vm.runInContext('(() => {' + [pick(ui, 'esc'), pick(ui, 'plural'), pick(lib, 'trackPills')].join('\n')
    + '\nconst videoLabel = () => []; const issueText = () => "";\nreturn trackPills; })()', ctx);
}

const row = (audio, subtitles) => ({ mkv: true, audit: { video: null, audio, subtitles, issues: [] } });

test('a single subtitle track is counted in the singular', async () => {
  const pills = await trackPills();
  assert.match(pills(row([{}], [{}])), /1 audio · 1 sub</);
  assert.match(pills(row([{}, {}], [{}, {}, {}])), /2 audio · 3 subs</);
  assert.doesNotMatch(pills(row([{}], [])), /sub/);
});
