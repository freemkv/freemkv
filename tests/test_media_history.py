"""Release drift must be explained without replacing historical answers."""

import json
from pathlib import Path
import shutil
import subprocess
import tempfile
import unittest
from unittest.mock import patch

import media_history as history


class ComparisonTests(unittest.TestCase):
    @unittest.skipUnless(shutil.which('ffmpeg'), 'ffmpeg')
    def test_parallel_snapshot_preserves_exact_sequential_hashes(self):
        fixture = Path(__file__).with_name('media-fixtures') / 'cfr.mkv'
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            expected = {}
            for name in ('a.mkv', 'b.mkv', 'c.mkv'):
                path = root / name
                shutil.copyfile(fixture, path)
                expected[name] = {'bytes': path.stat().st_size,
                                  'sha256': history.sha256(path),
                                  'content_timing_sha256': history.content_hash(path)}
            self.assertEqual(history.snapshot(root), expected)
            self.assertEqual(list(history.snapshot(root)), sorted(expected))

    def test_build_stamp_changes_do_not_hide_content_or_timing_changes(self):
        old = {'clip.mkv': {'sha256': 'old-container', 'content_timing_sha256': 'same'}}
        new = {'clip.mkv': {'sha256': 'new-container', 'content_timing_sha256': 'same'}}
        result = history.compare(old, new, {})
        self.assertEqual(result['unexpected'], [])
        self.assertEqual(set(result['container_only']), {'clip.mkv'})
        new['clip.mkv']['content_timing_sha256'] = 'changed-timestamps'
        self.assertEqual(history.compare(old, new, {})['unexpected'], ['clip.mkv'])

    def test_removed_added_and_unhashable_changed_files_fail(self):
        old = {'old.clpi': {'sha256': '1'}, 'bad.m2ts': {'sha256': '2', 'error': 'invalid'}}
        new = {'new.clpi': {'sha256': '1'}, 'bad.m2ts': {'sha256': '3', 'error': 'invalid'}}
        self.assertEqual(set(history.compare(old, new, {})['unexpected']),
                         {'old.clpi', 'new.clpi', 'bad.m2ts'})
        self.assertEqual(history.compare(old, old, {})['unexpected'], [])

    def test_exception_requires_both_exact_answers_and_reason(self):
        old, new = {'x.clpi': {'sha256': '1'}}, {'x.clpi': {'sha256': '2'}}
        allowed = {'x.clpi': {'baseline': '1', 'candidate': '2', 'reason': 'restore original header'}}
        self.assertEqual(history.compare(old, new, allowed)['unexpected'], [])
        allowed['x.clpi']['candidate'] = '3'
        self.assertEqual(history.compare(old, new, allowed)['unexpected'], ['x.clpi'])
        self.assertEqual(history.compare(old, old, allowed)['stale_exceptions'], ['x.clpi'])

    def test_added_and_removed_metadata_require_an_exact_explanation(self):
        old, new = {'x.inf': {'sha256': '1'}}, {'x.bak': {'sha256': '1'}}
        allowed = {'x.inf': {'baseline': '1', 'candidate': None, 'reason': 'rename'},
                   'x.bak': {'baseline': None, 'candidate': '1', 'reason': 'rename'}}
        self.assertEqual(history.compare(old, new, allowed)['unexpected'], [])
        new['x.bak']['sha256'] = 'corrupt'
        self.assertEqual(history.compare(old, new, allowed)['unexpected'], ['x.bak'])

    def test_archive_creates_an_orphan_tag_without_updating_a_branch(self):
        calls = []

        def post(endpoint, body):
            calls.append((endpoint, body))
            return {'sha': 'a' * 40}

        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            history.save(root / '1.8.4/manifest.json', {'version': '1.8.4'})
            history.archive(root, '123', '2', post)
        self.assertEqual(calls[1][1]['parents'], [])
        self.assertEqual(calls[-1][1]['ref'], 'refs/tags/media-history/123/2')
        self.assertEqual([entry['path'] for entry in calls[0][1]['tree']], ['1.8.4/manifest.json'])

    @unittest.skipUnless(shutil.which('ffmpeg'), 'ffmpeg')
    def test_real_media_hash_ignores_container_tags_but_detects_changed_timing(self):
        fixture = Path(__file__).with_name('media-fixtures') / 'cfr.mkv'
        with tempfile.TemporaryDirectory() as directory:
            same, shifted = Path(directory) / 'same.mkv', Path(directory) / 'shifted.mkv'
            for output, options in [(same, []), (shifted, ['-itsoffset', '0.2'])]:
                subprocess.run(['ffmpeg', '-nostdin', '-v', 'error', *options, '-i', str(fixture),
                                '-map', '0', '-c', 'copy', '-metadata', 'title=new build', str(output)],
                               check=True)
            self.assertNotEqual(history.sha256(fixture), history.sha256(same))
            self.assertEqual(history.content_hash(fixture), history.content_hash(same))
            self.assertNotEqual(history.content_hash(fixture), history.content_hash(shifted))

    @unittest.skipUnless(shutil.which('ffmpeg'), 'ffmpeg')
    def test_changed_track_language_cannot_hide_behind_equal_packet_content(self):
        fixture = Path(__file__).with_name('media-fixtures') / 'cfr.mkv'
        with tempfile.TemporaryDirectory() as directory:
            output = Path(directory) / 'language.mkv'
            subprocess.run(['ffmpeg', '-nostdin', '-v', 'error', '-i', str(fixture), '-map', '0',
                            '-c', 'copy', '-metadata:s:a:0', 'language=fra', str(output)], check=True)
            self.assertNotEqual(history.content_hash(fixture), history.content_hash(output))

    @unittest.skipUnless(shutil.which('ffmpeg'), 'ffmpeg')
    def test_chapter_names_and_boundaries_are_part_of_the_answer(self):
        fixture = Path(__file__).with_name('media-fixtures') / 'cfr.mkv'
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            answers = []
            for i, (name, boundary) in enumerate([('Intro', 100), ('Changed', 100), ('Intro', 200)]):
                metadata = root / f'{i}.txt'
                metadata.write_text(';FFMETADATA1\n[CHAPTER]\nTIMEBASE=1/1000\nSTART=0\n'
                                    f'END={boundary}\ntitle={name}\n[CHAPTER]\nTIMEBASE=1/1000\n'
                                    f'START={boundary}\nEND=300\ntitle=Main\n')
                output = root / f'{i}.mkv'
                subprocess.run(['ffmpeg', '-nostdin', '-v', 'error', '-i', str(fixture),
                                '-f', 'ffmetadata', '-i', str(metadata), '-map', '0',
                                '-map_metadata', '0', '-map_chapters', '1', '-c', 'copy', str(output)],
                               check=True)
                answers.append(history.content_hash(output))
            self.assertNotEqual(answers[0], answers[1], 'a changed chapter name must be detected')
            self.assertNotEqual(answers[0], answers[2], 'a changed chapter boundary must be detected')

    def test_baseline_is_the_complete_released_184_set(self):
        spec = json.loads(Path(history.__file__).with_name('media-baseline.json').read_text())
        self.assertEqual([entry['version'] for entry in spec['history']], ['1.8.4'])
        entry = spec['history'][0]
        self.assertTrue(history.gate.valid_revisions(entry['revisions']))
        self.assertEqual(entry['expected_differences'], {})
        self.assertEqual(set(entry['manifests']),
                         {'x86_64-unknown-linux-musl', 'x86_64-pc-windows-msvc'})

    def test_historical_manifest_requires_exact_pinned_bytes_and_compatible_inputs(self):
        with tempfile.TemporaryDirectory() as directory:
            source = Path(directory) / 'source.json'
            report = {'schema': 1, 'target': 'linux', 'version': '1.8.4',
                      'revisions': {'freemkv': 'a'}, 'fixtures': {'bd.iso': 'pinned'},
                      'harness': {'sha': 'b'}, 'hash_method': 'method', 'rustc': 'rustc',
                      'ffmpeg': 'ffmpeg', 'suite_exit': 0, 'artifacts': {'x.clpi': {'sha256': 'c'}}}
            history.save(source, report)
            pin = history.sha256(source)
            spec = {'version': '1.8.4', 'revisions': report['revisions'],
                    'manifests': {report['target']: {'sha256': pin}}}
            identity = {field: report[field] for field in
                        ('schema', 'target', 'fixtures', 'harness', 'hash_method', 'rustc', 'ffmpeg')}
            def fetch(bucket, key, destination):
                destination.write_bytes(source.read_bytes())
            with patch.object(history, 'download_manifest', fetch):
                dest = Path(directory) / 'manifest.json'
                self.assertEqual(history.historical_manifest('bucket', spec, report['target'],
                                                             identity, dest)['artifacts'], report['artifacts'])
                changed = dict(identity, ffmpeg='different ffmpeg')
                with self.assertRaisesRegex(ValueError, 'historical ffmpeg differs'):
                    history.historical_manifest('bucket', spec, report['target'], changed, dest)
                spec['manifests'][report['target']]['sha256'] = '0' * 64
                with self.assertRaisesRegex(ValueError, 'historical SHA-256 mismatch'):
                    history.historical_manifest('bucket', spec, report['target'], identity, dest)

    def test_publish_requires_both_successful_full_disc_reports(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            for platform in ('linux', 'windows'):
                path = root / f'release-comparison-{platform}' / 'candidate'
                history.save(path / 'manifest.json', {'version': '1.8.5', 'run_id': '123',
                             'target': platform, 'suite_exit': 0, 'artifacts': {'x': {}}})
                history.save(path / 'full-disc-qa.json', {'families': {'bd': {}, 'uhd': {}}})
            with patch.object(history.subprocess, 'run') as put:
                put.return_value.returncode = 0
                history.publish(root, 'bucket')
                self.assertEqual(put.call_count, 2)
                self.assertTrue(all('--if-none-match' in call.args[0]
                                    for call in put.call_args_list))
                (root / 'release-comparison-windows/candidate/full-disc-qa.json').write_text(
                    '{"error":"decryption failed"}')
                with self.assertRaisesRegex(ValueError, 'no successful full-disc report'):
                    history.publish(root, 'bucket')

    def test_failed_suite_keeps_its_historical_answers(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            harness = root / 'suite.sh'
            harness.write_text('mkdir -p "$TMPDIR/freemkv-cli-acceptance.fixed"\n'
                               'echo header > "$TMPDIR/freemkv-cli-acceptance.fixed/x.clpi"\nexit 3\n')
            result = history.run_suite(Path(__file__), harness, root / 'answers', {'version': '1.8.4'})
            self.assertEqual(result['suite_exit'], 3)
            self.assertEqual(set(result['artifacts']), {'x.clpi'})
            self.assertTrue((root / 'answers/manifest.json').is_file())


if __name__ == '__main__':
    unittest.main()
