import {test} from 'node:test';
import assert from 'node:assert/strict';
import {readFile} from 'node:fs/promises';
import vm from 'node:vm';

async function summary(data) {
  const context = vm.createContext({});
  const source = await readFile(new URL('../../src/server/web/assets/system.js', import.meta.url), 'utf8');
  const module = new vm.SourceTextModule(source + '\nexport {storageSummary};', {context});
  await module.link(name => {
    const names = name === './ui.js' ? ['esc', '$', 'put', 'api', 'act', 'toast', 'bytes', 'ago', 'twoStep']
      : name === './ripper.js' ? ['openDeviceTerminal'] : ['stagedLine'];
    return new vm.SyntheticModule(names, function () {
      for (const key of names) this.setExport(key, () => {});
    }, {context});
  });
  await module.evaluate();
  return module.namespace.storageSummary(data);
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
