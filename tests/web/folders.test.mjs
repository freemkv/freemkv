// Run with: node --experimental-vm-modules --test tests/web/*.test.mjs
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import vm from 'node:vm';

const asset = (name) => readFile(new URL('../../src/server/web/assets/' + name, import.meta.url), 'utf8');

// The real `esc` from ui.js; `ago` and `when` are fixed so the output is stable.
async function load() {
  const context = vm.createContext({});
  const ui = await asset('ui.js');
  const at = ui.indexOf('export function esc(');
  const esc = vm.runInContext('(' + ui.slice(at + 'export '.length, ui.indexOf('\n', at)) + ')', context);
  const stub = new vm.SyntheticModule(['esc', 'ago', 'when'], function () {
    this.setExport('esc', esc);
    this.setExport('ago', (ts) => (ts ? 'ago(' + ts + ')' : 'never'));
    this.setExport('when', (ts) => 'at ' + ts);
  }, { context });
  const mod = new vm.SourceTextModule(await asset('folders.js'), { context });
  await mod.link(async () => stub);
  await mod.evaluate();
  return mod.namespace;
}

const stale = {
  role: 'output', path: '/mnt/media/<movies>',
  message: 'The output folder is unreachable — the network share needs to be remounted (stale file handle).',
  hint: 'Remount the share on the host (or bring the NAS back); waiting work resumes on its own.',
  health: { state: 'unhealthy', since: 100 },
};
const fine = { role: 'source ISO', path: '/isos', health: { state: 'ok', since: 1 } };

test('no banner while every folder answers', async () => {
  const f = await load();
  assert.equal(f.folderBanner([fine], null, true), '');
  assert.equal(f.folderBanner(undefined, null, true), '');
  assert.deepEqual(f.unhealthy([fine, { role: 'x', path: '/x', health: null }]), []);
});

test('a banner names the folder, the reason and the fix, escaped', async () => {
  const f = await load();
  const html = f.folderBanner([stale, fine], null, true);
  assert.equal((html.match(/class="banner bad"/g) || []).length, 1);
  assert.match(html, /The output folder is unreachable/);
  assert.match(html, /\/mnt\/media\/&lt;movies&gt;/);
  assert.ok(!html.includes('<movies>'), 'the path is escaped');
  assert.match(html, /since ago\(100\)/);
  assert.match(html, /Remount the share on the host/);
  assert.match(html, /Remuxes wait in the queue/);
  assert.ok(!f.folderBanner([stale], null, false).includes('Remuxes wait'), 'the Library page says less');
});

test('a hold a preflight found shows before the next check does', async () => {
  const f = await load();
  const hold = { role: 'output', path: '/nas', message: 'The output folder is not responding — x.', hint: 'Remount it.', since: 5 };
  const html = f.folderBanner([fine], hold, true);
  assert.match(html, /not responding/);
  assert.match(html, /Remount it\./);
});

test('a queued job says why it waits and when it retries', async () => {
  const f = await load();
  const hold = { role: 'output' };
  assert.equal(f.queuedNote({ note: null }, null, 0), '');
  assert.equal(f.queuedNote({ note: 'preempted' }, null, 0), 'a rip took the slot');
  assert.equal(f.queuedNote({ note: null }, hold, 0), 'waiting for the output folder');
  assert.equal(f.queuedNote({ note: 'staged_waiting' }, hold, 0), 'staged, waiting for the output folder');
  assert.equal(
    f.queuedNote({ note: 'waiting_for_folder', not_before: 500, attempts: 1 }, null, 100),
    'waiting for the output folder · retry 2 at 500');
  assert.equal(f.queuedNote({ note: 'waiting_for_folder', not_before: 50, attempts: 1 }, null, 100),
    'waiting for the output folder', 'a backoff that has passed is not shown');
});

test('the System Folders row shows state, last good access and last error', async () => {
  const f = await load();
  const ok = f.folderHealthRow({ role: 'Library', path: '/lib', state: 'ok', ok: true, last_ok: 9 });
  assert.match(ok, /dot-ok/);
  assert.match(ok, /ago\(9\)/);
  assert.match(ok, /<td><span class="muted small">–<\/span><\/td>/);
  const bad = f.folderHealthRow({
    role: 'Movies', path: '/m', state: 'unresponsive', ok: false, since: 7,
    message: 'The movies folder is not responding <x>', last_ok: null,
    last_error: 'not responding after 5s', last_error_at: 8,
  });
  assert.match(bad, /dot-warn/);
  assert.match(bad, /not responding &lt;x&gt;/);
  assert.match(bad, /since ago\(7\)/);
  assert.match(bad, /never/);
  assert.match(bad, /not responding after 5s.*ago\(8\)/);
  // An old payload without the new fields still renders.
  assert.match(f.folderHealthRow({ role: 'Old', path: '/o', ok: false, problem: 'missing' }), /dot-bad/);
});
