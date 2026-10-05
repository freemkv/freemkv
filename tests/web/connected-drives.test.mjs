// Run with: node --experimental-vm-modules --test tests/web/*.test.mjs
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import vm from 'node:vm';

const context = vm.createContext({ Date });
const source = await readFile(new URL('../../src/server/web/assets/ripper.js', import.meta.url), 'utf8');
const module = new vm.SourceTextModule(source, { context });
await module.link(name => {
  const names = name === './bus.js' ? ['subscribe'] : ['esc', 'escLinks', '$', 'put', 'fill', 'api', 'act', 'toast', 'twoStep', 'terminal', 'modal', 'bytes', 'plural', 'ICON'];
  return new vm.SyntheticModule(names, function () { for (const n of names) this.setExport(n, () => {}); }, { context });
});
await module.evaluate();
const reconcile = module.namespace.reconcilePeers;
const good = (id, progress = 25) => ({ peer: { id, name: id }, snapshot: { sg0: { status: 'ripping' }, _move: [{ progress_pct: progress }] }, sys: { move_errors: [] }, reviews: [{ dir: 'held' }] });
const failed = id => ({ peer: { id, name: id }, error: 'timeout' });

test('brief failures retain complete pipeline and recover without an offline transition', () => {
  const health = new Map(), first = good('p1');
  reconcile([first], health, 0);
  const gap = reconcile([failed('p1')], health, 3000)[0];
  assert.equal(gap.snapshot, first.snapshot);
  assert.equal(gap.reviews, first.reviews);
  assert.equal(gap.offline, false);
  assert.equal(gap.error, null);
  const next = good('p1', 40);
  assert.equal(reconcile([next], health, 6000)[0], next);
  assert.equal(reconcile([failed('p1')], health, 30000)[0].offline, false, 'recovery resets the grace timer');
});

test('sustained outage is marked offline without removing its last snapshot; other owners stay live', () => {
  const health = new Map(), first = good('p1');
  reconcile([first, good('p2')], health, 0);
  reconcile([failed('p1'), good('p2')], health, 3000);
  assert.equal(reconcile([failed('p1'), good('p2')], health, 17999)[0].offline, false);
  const result = reconcile([failed('p1'), good('p2', 90)], health, 18000);
  assert.equal(result[0].offline, true);
  assert.equal(result[0].snapshot, first.snapshot);
  assert.equal(result[1].snapshot._move[0].progress_pct, 90);
  assert.equal(reconcile([good('p1')], health, 21000)[0].error, undefined);
  assert.equal(health.has('p2'), false, 'explicit disconnection removes its cached data');
});

test('a never-reachable peer gets a grace period and empty safe collections', () => {
  const health = new Map();
  assert.equal(reconcile([failed('p1')], health, 0)[0].offline, false);
  const result = reconcile([failed('p1')], health, 15000)[0];
  assert.equal(result.offline, true);
  assert.equal(result.reviews.length, 0);
  assert.equal(Object.keys(result.snapshot).length, 0);
});
