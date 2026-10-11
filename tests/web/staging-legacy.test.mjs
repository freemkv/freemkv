import {test} from 'node:test';
import assert from 'node:assert/strict';
import {readFile} from 'node:fs/promises';
import vm from 'node:vm';

async function system() {
  const context = vm.createContext({});
  const source = await readFile(new URL('../../src/server/web/assets/system.js', import.meta.url), 'utf8');
  const module = new vm.SourceTextModule(source + '\nexport {staleLegacyNote};', {context});
  await module.link(name => {
    const names = name === './ui.js' ? ['esc', '$', 'put', 'api', 'act', 'toast', 'bytes', 'ago', 'twoStep']
      : name === './ripper.js' ? ['openDeviceTerminal'] : ['stagedLine'];
    return new vm.SyntheticModule(names, function () {
      for (const key of names) this.setExport(key, () => {});
    }, {context});
  });
  await module.evaluate();
  return module.namespace;
}

test('Clear staging names the 1.8.3 folders it keeps', async () => {
  const {staleLegacyNote} = await system();
  assert.match(staleLegacyNote(['Heat']), /Kept 1 staging folder from 1\.8\.3 .*\(Heat\)/);
  const many = staleLegacyNote(['a', 'b', 'c', 'd', 'e', 'f']);
  assert.match(many, /Kept 6 staging folders/);
  assert.match(many, /a, b, c, d, e, …/);
});
