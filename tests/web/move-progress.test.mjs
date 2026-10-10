import { test } from 'node:test';
import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import vm from 'node:vm';

async function render(moves, sys = {}) {
  const context = vm.createContext({});
  const source = await readFile(new URL('../../src/server/web/assets/ripper.js', import.meta.url), 'utf8');
  const module = new vm.SourceTextModule(source + '\nexport { moveHtml };', { context });
  await module.link(specifier => {
    const names = specifier === './ui.js'
      ? ['esc', 'escLinks', '$', 'put', 'fill', 'api', 'act', 'toast', 'twoStep', 'terminal', 'modal', 'bytes', 'plural', 'ICON']
      : specifier === './bus.js' ? ['subscribe']
      : ['connections', 'subscribeConnections', 'driveState', 'busyDriveCount'];
    return new vm.SyntheticModule(names, function () {
      for (const name of names) this.setExport(name, name === 'esc' ? String : () => {});
    }, { context });
  });
  await module.evaluate();
  return module.namespace.moveHtml({ _move: moves }, sys);
}

test('completed bytes finalize without stale speed; delivered rows collapse', async () => {
  const html = await render([
    {name: 'Finished episode', phase: 'delivered', progress_pct: 100, speed_mbs: 45},
    {name: 'Source ISO', phase: 'finalizing', progress_pct: 100, speed_mbs: 47, eta: '1:00'},
  ]);
  assert.doesNotMatch(html, /Finished episode|MB\/s|remaining/);
  assert.match(html, /Source ISO/);
  assert.match(html, /Finalizing/);
  assert.match(html, /1 file delivered/);
});

test('blocked and cleanup-needed transfers never display a live rate', async () => {
  const html = await render([
    {name: 'Failed ISO', phase: 'blocked', progress_pct: 33, speed_mbs: 47},
    {name: 'Delivered MKV', phase: 'cleanup_needed', progress_pct: 100, speed_mbs: 45},
  ]);
  assert.match(html, /Blocked/);
  assert.match(html, /staging cleanup needed/);
  assert.doesNotMatch(html, /MB\/s/);
});

test('a held delivery offers Retry rather than dismissing the error', async () => {
  const html = await render([], {move_errors: [{path: '/staging/disc', reason: 'sync failed', retry_held: true}]});
  assert.match(html, /Automatic retries stopped/);
  assert.match(html, />Retry<\/button>/);
  assert.match(html, />Retry all<\/button>/);
  assert.doesNotMatch(html, /Clear all|Clear this error/);
  assert.match(html, /sync failed/);
});

test('a stalled active filesystem call keeps its reason and disables Retry', async () => {
  const html = await render([{name: 'Disc', phase: 'blocked', speed_mbs: 100}], {
    move_errors: [{path: '/stage/job', reason: 'writing destination stalled: /nas/disc.iso', worker_active: true, retry_held: true}],
  });
  assert.match(html, /writing destination stalled: \/nas\/disc.iso/);
  assert.match(html, /Waiting for filesystem — retry unavailable/);
  assert.match(html, /disabled data-clear="move"/);
  assert.doesNotMatch(html, /MB\/s/);
});
