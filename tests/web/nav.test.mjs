import test from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';

const read = (f) => readFileSync(new URL('../../src/server/web/assets/' + f, import.meta.url), 'utf8');

test('Remux has no tab in the header; the Library page links to it', () => {
  const nav = read('index.html').match(/<nav class="nav-links"[\s\S]*?<\/nav>/)[0];
  assert.ok(!nav.includes('/remux'), 'the header nav must not list Remux');
  assert.ok(nav.includes('href="/library"'));
  assert.match(read('library.js'), /id="to-remux" href="\/remux" data-link/);
  assert.match(read('app.js'), /'\/remux': \(\) => import\('\.\/remux\.js'\)/, 'the route still exists');
});

test('the Remux link never borrows the buttons\' busy style, which hides its label', () => {
  const lib = read('library.js');
  assert.ok(!/classList\.toggle\('busy'/.test(lib.slice(lib.indexOf('paintRemuxLink'))), 'no busy class on the link');
  assert.ok(read('remux.js').includes('function rowPills'), 'the kept-file pill leads the Remux row');
});
