import {test} from 'node:test';
import assert from 'node:assert/strict';
import {readFile} from 'node:fs/promises';
import vm from 'node:vm';

async function load() {
  const context = vm.createContext({});
  const source = await readFile(new URL('../../src/server/web/assets/system.js', import.meta.url), 'utf8');
  const module = new vm.SourceTextModule(source + '\nexport {storageSummary, storageFolderRow, stagedClearToast};', {context});
  await module.link(name => {
    const names = name === './ui.js' ? ['esc', '$', 'put', 'api', 'act', 'toast', 'bytes', 'ago', 'twoStep']
      : name === './ripper.js' ? ['openDeviceTerminal'] : ['stagedLine'];
    return new vm.SyntheticModule(names, function () {
      for (const key of names) this.setExport(key, key === 'esc' ? esc : key === 'ago' ? (t => 'at ' + t) : key === 'bytes' ? String : () => {});
    }, {context});
  });
  await module.evaluate();
  return module.namespace;
}

const esc = v => String(v).replace(/&/g, '&amp;').replace(/</g, '&lt;').replace(/>/g, '&gt;').replace(/"/g, '&quot;');

async function summary(data) {
  return (await load()).storageSummary(data);
}

test('successful access checks cannot hide a delivery failure', async () => {
  const html = await summary({mounts: [{ok: true}], move_errors: [{reason: 'sync failed'}]});
  assert.match(html, /file delivery needs attention/);
  assert.doesNotMatch(html, /every folder answering/);
});

test('missing folder results do not claim good health', async () => {
  assert.equal(await summary({}), 'checking folders');
  assert.equal(await summary({mounts: [{ok: true}]}), 'every folder answering');
});

test('a folder still being checked is slow, not failed', async () => {
  const slow = await summary({mounts: [{ok: true}, {ok: false, state: 'checking'}]});
  assert.match(slow, /slow to answer/);
  assert.doesNotMatch(slow, /every folder answering/);
  const bad = await summary({mounts: [{ok: false, state: 'checking'}, {ok: false, state: 'unresponsive'}]});
  assert.match(bad, /needs attention/);
});

test('a folder row: a slow check is a warning, never a failure', async () => {
  const {storageFolderRow} = await load();
  const row = m => storageFolderRow({role: 'Library', path: '/nas', ...m});
  assert.match(row({state: 'ok', last_ok: 5}), /dot-ok/);
  assert.match(row({state: 'checking', message: 'still checking'}), /dot-warn/);
  assert.match(row({state: 'unresponsive', message: 'not responding'}), /dot-warn/);
  assert.match(row({state: 'missing', message: 'folder is missing'}), /dot-bad/);
});

test('a folder row escapes its message and last error, and says never without a good access', async () => {
  const {storageFolderRow} = await load();
  const html = storageFolderRow({role: 'Library', path: '/nas', state: 'missing', message: '<b>gone</b>', last_error: 'x<script>', last_error_at: 9});
  assert.doesNotMatch(html, /<b>gone|<script>/);
  assert.match(html, /&lt;b&gt;gone/);
  assert.match(html, /x&lt;script&gt;/);
  assert.match(html, />never</);
  assert.match(storageFolderRow({role: 'Library', path: '/nas', state: 'ok', last_ok: 7}), />at 7</);
});

test('clearing staging says how many items could not be removed', async () => {
  const {stagedClearToast} = await load();
  assert.deepEqual([...stagedClearToast({discarded: 3, failed: 0})], ['3 staging items cleared', 'info']);
  const [text, tone] = stagedClearToast({discarded: 1, failed: 2});
  assert.match(text, /1 staging item cleared/);
  assert.match(text, /2 could not be removed/);
  assert.equal(tone, 'warn');
});
