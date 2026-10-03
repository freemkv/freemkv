// Run with: node --experimental-vm-modules --test tests/web/*.test.mjs
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import vm from 'node:vm';

const asset = (name) => readFile(new URL('../../src/server/web/assets/' + name, import.meta.url), 'utf8');

// The real esc/runtime/bytes from ui.js (the rest of it needs a page); `when` is fixed.
async function uiStub(context) {
  const ui = await asset('ui.js');
  const pick = (name) => {
    const at = ui.indexOf('export function ' + name + '(');
    let depth = 0, i = ui.indexOf('{', at);
    for (; i < ui.length; i++) {
      if (ui[i] === '{') depth++;
      else if (ui[i] === '}' && --depth === 0) break;
    }
    return ui.slice(at + 'export '.length, i + 1);
  };
  const fns = vm.runInContext('(() => {' + ['esc', 'runtime', 'bytes'].map(pick).join('\n') + '\nreturn { esc, runtime, bytes }; })()', context);
  const names = ['esc', 'runtime', 'bytes', 'when', 'modal', 'api', 'act', 'toast', 'confirmButton'];
  return new vm.SyntheticModule(names, function () {
    for (const n of names) this.setExport(n, fns[n] || (n === 'when' ? (ts) => 'at ' + ts : () => {}));
  }, { context });
}

async function load(entry) {
  const context = vm.createContext({});
  const stubs = {
    './ui.js': await uiStub(context),
    './console.js': new vm.SyntheticModule(['openTitleLog'], function () { this.setExport('openTitleLog', () => {}); }, { context }),
    './libdata.js': new vm.SyntheticModule(['refreshNow'], function () { this.setExport('refreshNow', () => {}); }, { context }),
  };
  const modules = new Map();
  async function link(name) {
    if (stubs[name]) return stubs[name];
    if (!modules.has(name)) {
      const mod = new vm.SourceTextModule(await asset(name.replace('./', '')), { context });
      modules.set(name, mod);
      await mod.link(link);
    }
    return modules.get(name);
  }
  const mod = await link(entry);
  await mod.evaluate();
  return mod.namespace;
}

const v = await load('./auditview.js');
const d = await load('./details.js');

const uhdDetail = () => ({
  version: 1,
  tier: 'uhd',
  hdr: { format: 'hdr10', hdr10plus: false, dv: { profile: 8, level: 6, compat_id: 1, rpu: true, el: false, bl: true } },
  video: [{ number: 1, codec: 'HEVC', width: 3840, height: 2160, fps: 23.976, bit_depth: 10, transfer: 16, max_cll: 1000, max_fall: 400, dv: { profile: 8, level: 6, compat_id: 1, rpu: true, el: false, bl: true }, hdr10plus: false, title: '', language: 'und', default: true }],
  audio: [
    { number: 2, format: 'AC-3', channels: 6, lossless: false, atmos: false, language: 'eng', title: 'Dolby Digital 5.1', default: true, forced: false, lossy_claim: false },
    { number: 3, format: 'TrueHD', channels: 8, lossless: true, atmos: true, language: 'eng', title: 'TrueHD Atmos <7.1>', default: false, forced: false, lossy_claim: false },
    { number: 4, format: 'DTS', channels: 6, lossless: false, atmos: false, language: 'fra', title: 'DTS-HD MA 5.1', default: false, forced: false, lossy_claim: true },
  ],
  subtitles: [{ number: 5, format: 'PGS', language: 'eng', title: 'Forced', default: false, forced: true }],
  best_audio: 1,
  default_audio: 0,
  default_not_best: true,
  radar: [],
  radar_unknown: [],
  timeline: { declared_secs: 7392, last_cue_secs: 7390, last_frame_secs: 7391.96, delta_secs: -0.04, state: 'ok' },
});

test('hdrLabel names HDR the way a player does, and never guesses', () => {
  assert.equal(v.hdrLabel({ format: 'hdr10', dv: { profile: 8, compat_id: 1 } }), 'DV P8 + HDR10');
  assert.equal(v.hdrLabel({ format: 'hdr10', dv: { profile: 7, compat_id: 6 } }), 'DV P7 + HDR10');
  assert.equal(v.hdrLabel({ format: 'dv', dv: { profile: 5, compat_id: 0 } }), 'DV P5');
  assert.equal(v.hdrLabel({ format: 'hdr10', hdr10plus: true }), 'HDR10+');
  assert.equal(v.hdrLabel({ format: 'hlg' }), 'HLG');
  assert.equal(v.hdrLabel({ format: 'sdr' }), 'SDR');
  assert.equal(v.hdrLabel({ format: 'sdr', dv: { profile: 8, compat_id: 2 } }), 'DV P8');
  assert.equal(v.hdrLabel({ format: 'unknown', hdr10plus: null }), 'HDR unknown');
  assert.equal(v.hdrLabel(null), '');
  assert.equal(v.dvProfile({ profile: 8, compat_id: 1 }), '8.1');
  assert.equal(v.dvProfile({ profile: 7, compat_id: 6 }), '7');
});

test('audio labels and badges carry format, channels, lossless and Atmos', () => {
  const [ac3, thd, dts] = uhdDetail().audio;
  assert.equal(v.audioLabel(thd), 'TrueHD Atmos 7.1');
  assert.equal(v.audioLabel(ac3), 'Dolby Digital 5.1');
  assert.equal(v.audioLabel({ format: 'E-AC-3', channels: 6, atmos: true }), 'DD+ Atmos 5.1');
  assert.equal(v.audioLabel({ format: 'DTS-HD MA', channels: null }), 'DTS-HD MA ?');
  assert.equal(v.chLabel(8), '7.1');
  assert.equal(v.chLabel(10), '10ch');
  const best = v.audioBadge(thd, { best: true });
  assert.match(best, /badge-ok">lossless/);
  assert.match(best, /Atmos/);
  assert.match(best, />best</);
  assert.match(v.audioBadge(ac3, { isDefault: true }), /lossy.*default/);
  assert.match(v.audioBadge(dts), /says lossless, is lossy/);
  assert.match(v.audioBadge({ lossless: null }), /lossless\?/);
});

test('the radar lists what the tier lacks, and what could not be read', () => {
  assert.equal(v.radar({ tier: 'uhd', radar: [], radar_unknown: [] }), '<span class="badge badge-ok">nothing missing for 4K UHD</span>');
  const bd = v.radar({ tier: 'bluray', radar: ['lossless', 'UHD'], radar_unknown: [] });
  assert.match(bd, /badge-warn">lossless<.*badge-warn">UHD</);
  assert.match(v.radar({ tier: 'uhd', radar: ['DV'], radar_unknown: ['Atmos'] }), /badge-muted"[^>]*>Atmos\?</);
  assert.equal(v.radar({ tier: null }), '');
});

test('the timeline and the deep audit read as sentences', () => {
  assert.equal(v.clock(7391.96), '2:03:12');
  assert.equal(v.timelineText({ declared_secs: 7392, last_frame_secs: 7200, delta_secs: -192, state: 'short' }),
    'declared 2:03:12 · last frame 2:00:00 · content ends before the declared end (-192.0 s)');
  assert.equal(v.timelineText({ state: 'unknown' }), 'the end could not be read');

  const old = v.deepWhy({ reason: 'decode_errors', stage: 'decode' });
  assert.match(old.what, /damaged frames.*decoding stage/);
  assert.match(old.where, /before the timeline was sampled/);
  assert.match(old.advice, /Remux it from its ISO/);
  const sampled = v.deepWhy({
    reason: 'bitstream_corruption', stage: 'decode',
    forensic: { window_secs: 5, buckets: ['payload_bitstream'], windows: [{ at_secs: 300, payload_errors: 0 }, { at_secs: 900, payload_errors: 4 }, { at_secs: 4500, payload_errors: 1 }] },
  });
  assert.equal(sampled.where, 'Around 0:15:00, 1:15:00 (2 of 3 sampled 5-second windows)');
  assert.match(sampled.advice, /re-rip the disc/);
  assert.match(v.forensicTable({ windows: [{ at_secs: 900, payload_errors: 4 }] }), /dot-bad.*0:15:00/);

  const clean = v.deepVerified({ reached_secs: 7391, scanned: 99, peak_rss_mib: 812 }, { audio_tracks: 3, duration_secs: 7392 });
  assert.equal(clean, 'Copied every stream and decoded the main video and 3 audio tracks through 2:03:11 of 2:03:12, at 99; peak memory 812 MiB.');
});

test('the dialog shows the media sections, escaped', () => {
  const row = {
    title: 'Arrival', kind: 'remux', mkv: '/m/Arrival.mkv', size_bytes: 61e9, probed: true,
    muxed_with: { state: 'current', version: '1.8.0' },
    audit: { ok: true, issues: [], duration_secs: 7392, audio_tracks: 3, video: ['HEVC'], audio: [], subtitles: [], detail: uhdDetail() },
    deep: { state: 'clean', verdict: { clean: true, completed: true, reason: 'clean', scanned: 5, reached_secs: 7391 } },
  };
  const html = d.detailsBody(row);
  for (const want of ['4K UHD', 'DV P8 + HDR10', 'TrueHD Atmos 7.1', '<h3>Video</h3>', '<h3>Audio</h3>', '<h3>Subtitles</h3>',
    '<h3>Audit findings</h3>', '<h3>Deep audit</h3>', '<h3>Upgrade radar</h3>', '<h3>Files</h3>', 'data-raw', 'Profile 8.1, level 6 (BL, RPU)',
    'not the better TrueHD Atmos 7.1', 'is titled &quot;DTS-HD MA 5.1&quot; but the stream is lossy', 'Copied every stream']) {
    assert.ok(html.includes(want), 'missing ' + want);
  }
  assert.ok(html.includes('TrueHD Atmos &lt;7.1&gt;'), 'titles are escaped');
  assert.ok(!html.includes('Details pending'));
});

test('a report stored before the media detail shows its tracks and says details are pending', () => {
  const row = {
    title: 'Heat', kind: 'remux', mkv: '/m/Heat.mkv', probed: true, muxed_with: { state: 'current' },
    audit: { ok: true, issues: [], duration_secs: 10200, video: ['AVC'], audio: [{ codec: 'DTS', language: 'eng' }], subtitles: ['eng'] },
  };
  const html = d.detailsBody(row);
  assert.ok(html.includes('Details pending'));
  assert.ok(html.includes('<h3>Tracks</h3>') && html.includes('DTS'));
  assert.ok(!html.includes('<h3>Upgrade radar</h3>') && !html.includes('data-raw'));
});

test('the raw report escapes every field', () => {
  const html = v.rawHtml([{ title: 'Track 1 (video)', fields: [['Name', '<b>x</b>']] }]);
  assert.ok(html.includes('&lt;b&gt;x&lt;/b&gt;'));
  assert.match(v.rawHtml([]), /No header fields/);
});

test('every raw report section shares one fixed-column table, so values line up', () => {
  const html = v.rawHtml([
    { title: 'Track 1 (video)', fields: [['Video › DisplayWidth', '3840']] },
    { title: 'Track 2 (audio)', fields: [['TrackNumber', '2']] },
  ]);
  assert.equal((html.match(/<table class="det raw">/g) || []).length, 2);
});
