import {test} from 'node:test';
import assert from 'node:assert/strict';
import {readFile} from 'node:fs/promises';
import vm from 'node:vm';

async function system() {
  const context = vm.createContext({});
  const source = await readFile(new URL('../../src/server/web/assets/system.js', import.meta.url), 'utf8');
  const module = new vm.SourceTextModule(source + '\nexport {awaitRestart, requestReboot};', {context});
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

// A fake clock: each sleep advances it, and `answers` scripts what each poll gets.
function harness(answers, {staleAfter = Infinity} = {}) {
  let clock = 0, polls = 0;
  const notes = [];
  return {
    notes,
    get polls() { return polls; },
    deps: {
      get: async () => {
        const a = answers(polls++, clock);
        if (a instanceof Error) throw a;
        return a;
      },
      sleep: async ms => { clock += ms; },
      now: () => clock,
      stale: () => clock > staleAfter,
      note: stage => notes.push([stage, clock]),
    },
  };
}

test('the wait ends only when a server with a new boot id answers', async () => {
  const {awaitRestart} = await system();
  const h = harness(n => n < 2 ? new Error('the server did not answer')
    : n < 4 ? {version: '1', boot: 'old'} : {version: '1', boot: 'new'});
  const v = await awaitRestart('old', h.deps);
  assert.equal(v.boot, 'new');
  assert.equal(h.polls, 5);
  assert.deepEqual(h.notes, []);
});

test('a slow restart says so after a minute, and a lost one after six', async () => {
  const {awaitRestart} = await system();
  const h = harness((n, clock) => clock < 400000 ? new Error('the server did not answer') : {boot: 'new'});
  await awaitRestart('old', h.deps);
  assert.deepEqual(h.notes.map(([stage]) => stage), ['slow', 'lost']);
  assert.ok(h.notes[0][1] >= 60000 && h.notes[0][1] < 62001);
  assert.ok(h.notes[1][1] >= 360000 && h.notes[1][1] < 362001);
});

test('leaving the page stops the wait', async () => {
  const {awaitRestart} = await system();
  const h = harness(() => ({boot: 'old'}), {staleAfter: 10000});
  assert.equal(await awaitRestart('old', h.deps), null);
  assert.ok(h.polls <= 5);
});

test('the boot id to wait past is read before the reboot is requested', async () => {
  const {requestReboot} = await system();
  const calls = [];
  const before = await requestReboot(async (method, url) => {
    calls.push(method + ' ' + url);
    return url === '/api/version' ? {version: '1', boot: 'old'} : {ok: true, boot: 'old'};
  });
  assert.deepEqual(calls, ['GET /api/version', 'POST /api/system/reboot']);
  assert.deepEqual({...before}, {version: '1', boot: 'old'});
});

test('a reboot answer lost to the restart still waits for the new server', async () => {
  const {requestReboot} = await system();
  const before = await requestReboot(async (method) => {
    if (method === 'GET') return {version: '1', boot: 'old'};
    throw new Error('the server did not answer');
  });
  assert.equal(before.boot, 'old');
});

test('a refused reboot is reported, not waited for', async () => {
  const {requestReboot} = await system();
  await assert.rejects(requestReboot(async (method) => {
    if (method === 'GET') return {version: '1', boot: 'old'};
    throw new Error('Reboot is unavailable');
  }), /unavailable/);
});
