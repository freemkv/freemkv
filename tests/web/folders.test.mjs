// Run with: node --experimental-vm-modules --test tests/web/*.test.mjs
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import vm from 'node:vm';

const asset = (name) => readFile(new URL('../../src/server/web/assets/' + name, import.meta.url), 'utf8');

// The real `esc` from ui.js; `ago`, `when` and `bytes` are fixed so the output is stable.
async function load() {
  const context = vm.createContext({});
  const ui = await asset('ui.js');
  const at = ui.indexOf('export function esc(');
  const esc = vm.runInContext('(' + ui.slice(at + 'export '.length, ui.indexOf('\n', at)) + ')', context);
  const stub = new vm.SyntheticModule(['esc', 'ago', 'when', 'bytes'], function () {
    this.setExport('esc', esc);
    this.setExport('ago', (ts) => (ts ? 'ago(' + ts + ')' : 'never'));
    this.setExport('when', (ts) => 'at ' + ts);
    this.setExport('bytes', (b) => (b / 1e9).toFixed(1) + ' GB');
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

test('a job with a kept file says it finished locally, its size and failed copies', async () => {
  const f = await load();
  assert.equal(f.stagedNote({ state: 'queued' }), '');
  assert.equal(f.stagedNote(null), '');
  const job = { state: 'queued', staged: '/stage/A.staged.mkv', staged_bytes: 4.2e9, staged_attempts: 2 };
  assert.equal(f.stagedNote(job), 'Finished locally (4.2 GB) — waiting for the output folder to copy it · 2 failed copies');
  assert.equal(f.stagedNote({ ...job, staged_attempts: 1, state: 'failed' }),
    'Finished locally (4.2 GB) — the copy into the output folder failed · 1 failed copy');
  const pill = f.stagedPill({ ...job, not_before: 500, failure: { message: 'E5000 <stale>' } }, 100);
  assert.match(pill, /class="pill warn"/);
  assert.match(pill, /retry at 500/);
  assert.match(pill, /title="E5000 &lt;stale&gt;"/);
  assert.ok(!pill.includes('<stale>'), 'the failure is escaped');
  assert.match(f.stagedPill({ ...job, state: 'failed', not_before: 500 }, 100), /class="pill bad"/);
  assert.ok(!f.stagedPill({ ...job, state: 'failed', not_before: 500 }, 100).includes('retry'), 'a failed one waits for the user');
});

test('a kept file offers Retry now and Discard, never while it is being copied in', async () => {
  const f = await load();
  const r = { title: 'A <b>', job: { state: 'failed', staged: '/s/A.staged.mkv' } };
  const html = f.stagedActions(r);
  assert.match(html, /data-staged-retry/);
  assert.match(html, /Discard staged file/);
  assert.match(html, /A &lt;b&gt;/);
  assert.ok(!html.includes('A <b>'));
  assert.equal(f.stagedActions({ ...r, job: { ...r.job, state: 'running' } }), '');
  assert.equal(f.stagedActions({ title: 'B', job: { state: 'queued' } }), '');
});

test('the System page counts the kept staged files', async () => {
  const f = await load();
  assert.equal(f.stagedLine(undefined), 'Kept staged files: none');
  assert.equal(f.stagedLine({ count: 0, bytes: 0 }), 'Kept staged files: none');
  const line = f.stagedLine({ count: 2, bytes: 8.4e9, dir: '/stage/<x>' });
  assert.match(line, /Kept staged files: 2 \(8\.4 GB\)/);
  assert.match(line, /\/stage\/&lt;x&gt;/);
  assert.match(line, /waiting for the output folder/);
});

test('the Remux row pill is short; the reason stays in its tooltip', async () => {
  const f = await load();
  const job = { state: 'failed', staged: '/s/A.staged.mkv', staged_bytes: 52.8e9, staged_attempts: 2,
    failure: { message: 'E9073 timed out while copying' } };
  const pill = f.stagedPill(job, 100);
  assert.match(pill, />Finished locally \(52\.8 GB\) · 2 failed copies</);
  assert.ok(!pill.includes('—'), 'the long explanation is not in the pill text');
  assert.match(pill, /title="E9073 timed out while copying"/);
  assert.match(f.stagedPill({ ...job, failure: null }, 100), /title="Finished locally/);
});

test('a folder still being checked is a notice, not an alert', async () => {
  const f = await load();
  const slow = {
    role: 'output', path: '/mnt/nas',
    message: 'The output folder has not answered for 7s — still checking.',
    hint: 'Nothing to do yet.',
    health: { state: 'checking', since: 9 },
  };
  assert.deepEqual(f.unhealthy([slow, fine]), []);
  const html = f.folderBanner([slow, fine], null, true);
  assert.match(html, /class="banner warn" role="status"/);
  assert.ok(!html.includes('role="alert"'), html);
  assert.match(html, /still checking/);
  const both = f.folderBanner([slow, stale], null, true);
  assert.equal((both.match(/role="alert"/g) || []).length, 1);
  assert.equal((both.match(/role="status"/g) || []).length, 1);
  const held = f.folderBanner([fine], { state: 'checking', role: 'output', path: '/n', message: 'slow', since: 1 }, true);
  assert.match(held, /banner warn/);
});

test('a queue file that cannot be read is a held queue, not a folder to wait for', async () => {
  const f = await load();
  const hold = {
    role: 'queue file', path: '/config/library-queue.json', since: 3,
    message: 'The remux queue file could not be read.',
    hint: 'Nothing in the remux queue runs or changes until this file can be read.',
  };
  const html = f.folderBanner([fine], hold, true);
  assert.match(html, /could not be read/);
  assert.ok(!html.includes('start on their own'), html);
  const why = f.queuedNote({ note: null }, hold, 0);
  assert.ok(!why.includes('folder'), why);
  const label = f.remuxLinkLabel(hold, null);
  assert.ok(!label.aria.includes('folder'), label.aria);
  assert.ok(!label.title.includes('waiting'), label.title);
  const folder = f.remuxLinkLabel({ role: 'output', message: 'gone' }, null);
  assert.equal(folder.aria, 'Remuxes are waiting for a folder');
  assert.equal(folder.title, 'Remuxes are waiting: gone');
});

test('a held queue file is shown beside a failed or slow folder, never hidden by it', async () => {
  const f = await load();
  const hold = {
    role: 'queue file', path: '/config/library-queue.json', since: 3,
    message: 'The remux queue file could not be read.',
    hint: 'Nothing in the remux queue runs or changes until this file can be read.',
  };
  const slow = { role: 'source ISO', path: '/isos', health: { state: 'checking', since: 2 } };
  for (const folders of [[stale], [slow], [stale, slow]]) {
    const html = f.folderBanner(folders, hold, true);
    assert.match(html, /queue file could not be read/, html);
    assert.match(html, /library-queue\.json/, html);
    assert.ok(!html.includes('start on their own'), html);
  }
  // The failed or slow folder's own banner is still rendered beside the hold.
  assert.match(f.folderBanner([stale], hold, true), /remounted \(stale file handle\)/);
  assert.match(f.folderBanner([slow], hold, true), /source ISO folder has not answered yet/);
  const label = f.remuxLinkLabel(hold, stale);
  assert.equal(label.aria, 'The remux queue is held until its file can be read');
  assert.match(label.title, /queue is held/);
  assert.match(label.title, /stale file handle/);
});
