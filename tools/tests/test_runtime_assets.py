import concurrent.futures
import importlib.util
import gzip
import hashlib
import io
import json
from pathlib import Path
import tarfile
import tempfile
import threading
import unittest
from unittest.mock import patch
import zipfile

ROOT = Path(__file__).resolve().parents[2]
spec = importlib.util.spec_from_file_location('runtime_assets', ROOT / 'tools/build/runtime_assets.py')
assets = importlib.util.module_from_spec(spec)
spec.loader.exec_module(assets)


def executable(arch):
    data = bytearray(128)
    data[:6] = b'\x7fELF\x02\x01'
    data[18:20] = (62 if arch == 'amd64' else 183).to_bytes(2, 'little')
    return bytes(data)


def archive_bytes(url, arch, suffix=b''):
    """Deterministic tarballs whose trusted digest can be supplied by fixtures."""
    name = url.split('/')[-1][:-7]
    member = 'go/bin/go' if name.startswith('go') else name + '/bin/node' if name.startswith('node') else name
    data = executable(arch) + suffix
    output = io.BytesIO()
    with gzip.GzipFile(fileobj=output, mode='wb', mtime=0) as compressed:
        with tarfile.open(fileobj=compressed, mode='w') as archive:
            item = tarfile.TarInfo(member)
            item.size = len(data)
            archive.addfile(item, io.BytesIO(data))
    return output.getvalue()


def fixture_catalog():
    catalog = json.loads((Path(assets.__file__).with_name('pingap-assets.json')).read_text())
    for version, release in catalog['releases'].items():
        for arch, record in release['assets'].items():
            url = f'https://github.com/vicanso/pingap/releases/download/v{version}/' + record['name']
            record['sha256'] = hashlib.sha256(archive_bytes(url, arch)).hexdigest()
    return catalog


class RuntimeAssetsTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.calls = []
        self.catalog = fixture_catalog()

    def downloader(self, urls, target):
        self.calls.append(urls[0])
        arch = 'arm64' if any(x in urls[0] for x in ('arm64', 'aarch64')) else 'amd64'
        data = executable(arch)
        if 'deno' in urls[0]:
            with zipfile.ZipFile(target, 'w') as archive:
                archive.writestr('deno', data)
        elif '.tar.gz' in urls[0]:
            target.write_bytes(archive_bytes(urls[0], arch))
        else:
            target.write_bytes(data)

    def prepare(self, component='node', version='22.23.2'):
        return assets.prepare(component, version, self.root / 'cache', downloader=self.downloader, trusted_catalog=self.catalog)

    def test_cache_hit_and_version_change(self):
        first = self.prepare()
        self.assertEqual(self.prepare(), first)
        self.assertEqual(len(self.calls), 2)
        second = self.prepare(version='22.24.0')
        self.assertNotEqual(first, second)
        self.assertEqual(len(self.calls), 4)
        self.assertTrue(first.exists())

    def test_corrupt_archive_rebuilds_new_generation(self):
        first = self.prepare()
        next((first / 'cache').glob('*x64*')).write_bytes(b'truncated')
        second = self.prepare()
        self.assertNotEqual(first, second)
        assets.verify(second)
        self.assertEqual(len(self.calls), 4)

    def test_failed_download_leaves_previous_assets_and_no_ref(self):
        first = self.prepare()
        def failed(urls, target):
            target.write_bytes(b'partial')
            raise RuntimeError('controlled interrupted download')
        with self.assertRaises(RuntimeError):
            assets.prepare('node', '22.24.0', self.root / 'cache', downloader=failed)
        assets.verify(first)
        self.assertEqual(len(list((self.root / 'cache/v1/refs').glob('*.json'))), 1)

    def test_copy_failure_preserves_all_old_files(self):
        entry = self.prepare()
        context = self.root / 'context'
        assets.distribute(entry, [context])
        old = {str(p): p.read_bytes() for p in context.rglob('*') if p.is_file()}
        copy = assets.shutil.copy2
        count = 0
        def failed(source, target):
            nonlocal count
            count += 1
            if count == 2:
                raise OSError('controlled copy failure')
            return copy(source, target)
        with patch.object(assets.shutil, 'copy2', side_effect=failed):
            with self.assertRaises(OSError):
                assets.distribute(entry, [context])
        self.assertEqual(old, {str(p): p.read_bytes() for p in context.rglob('*') if p.is_file()})

    def test_concurrent_callers_publish_only_one_generation(self):
        with concurrent.futures.ThreadPoolExecutor(2) as pool:
            entries = list(pool.map(lambda _: self.prepare(), range(2)))
        self.assertEqual(entries[0], entries[1])
        self.assertEqual(len(self.calls), 2)

    def test_publication_failure_restores_all_previous_assets(self):
        first = self.prepare('ttyd', '1.7.7')
        context = self.root / 'publication-context'
        assets.distribute(first, [context])
        old = {str(p): p.read_bytes() for p in context.rglob('*') if p.is_file()}
        def newer(urls, target):
            arch = 'arm64' if 'aarch64' in urls[0] else 'amd64'
            target.write_bytes(executable(arch) + b'new version')
        second = assets.prepare('ttyd', '1.7.8', self.root / 'cache', downloader=newer)
        replace = assets.os.replace
        publications = 0
        def failed(source, target):
            nonlocal publications
            publications += 1
            if publications == 2:
                raise OSError('controlled second publication failure')
            return replace(source, target)
        with patch.object(assets.os, 'replace', side_effect=failed):
            with self.assertRaises(OSError):
                assets.distribute(second, [context])
        self.assertEqual(old, {str(p): p.read_bytes() for p in context.rglob('*') if p.is_file()})
        assets.verify(first)
        assets.verify(second)

    def test_deno_generations_do_not_reuse_old_binary_identity(self):
        first = self.prepare('deno', '2.9.7')
        second = self.prepare('deno', '2.9.8')
        self.assertNotEqual(first, second)
        self.assertEqual(set(assets.verify(second)['files']), {'cache/deno-amd64', 'cache/deno-arm64'})

    def test_bad_architecture_and_broken_zip_are_rejected(self):
        def wrong(urls, target):
            target.write_bytes(executable('arm64'))
        with self.assertRaises(ValueError):
            assets.prepare('ttyd', '1.7.7', self.root / 'cache', downloader=wrong)
        self.assertFalse(list((self.root / 'cache/v1/refs').glob('*')))

    def test_all_components_have_verified_pair(self):
        for component, version in [('ttyd', '1.7.7'), ('node', '22.23.2'), ('deno', '2.9.7'), ('go', '1.26.4'), ('pingap', '0.14.3')]:
            with self.subTest(component=component):
                manifest = assets.verify(self.prepare(component, version), self.catalog)
                self.assertEqual(len(manifest['files']), 2)
                self.assertEqual(manifest['identity']['version'], version)


    def test_pingap_catalog_matches_reviewed_release_assets(self):
        release = assets.trusted_pingap_release('0.15.0', ('amd64', 'arm64'))
        self.assertEqual(release['tag'], 'v0.15.0')
        self.assertEqual(release['commit'], '8270a1ebb7a238ea86fa220215714613410378bb')
        self.assertEqual(release['assets']['amd64']['id'], 602311135)
        self.assertEqual(release['assets']['amd64']['sha256'], '0539eeac37e9bf81307885983a4906e41e03aba1bf093b3c6bc5952e809e8334')
        self.assertEqual(release['assets']['arm64']['id'], 602308525)
        self.assertEqual(release['assets']['arm64']['sha256'], '25b583c9820d8da260dfaeb0af0e9c602c26dfba950f082a527057244f94cedd')

    def test_pingap_fixture_requires_explicit_trusted_catalog(self):
        with self.assertRaisesRegex(ValueError, 'trusted Pingap asset checksum'):
            assets.prepare('pingap', '0.15.0', self.root / 'cache', downloader=self.downloader)
        self.assertFalse(list((self.root / 'cache/v1/refs').glob('*')))
        with self.assertRaisesRegex(ValueError, 'no trusted Pingap assets'):
            assets.prepare('pingap', 'unreviewed', self.root / 'cache', downloader=self.downloader)

    def test_pingap_manifest_records_official_asset_identity_and_expected_digest(self):
        entry = self.prepare('pingap', '0.15.0')
        manifest = assets.verify(entry, self.catalog)
        release = manifest['identity']['trusted_release']
        self.assertEqual(release, assets.trusted_pingap_release('0.15.0', ('amd64', 'arm64'), self.catalog))
        for arch, relative, _, _ in assets.specs('pingap', '0.15.0', ('amd64', 'arm64')):
            self.assertEqual(manifest['files'][relative], release['assets'][arch]['sha256'])

    def test_pingap_corrupt_cache_cannot_retrust_its_own_modified_manifest(self):
        first = self.prepare('pingap', '0.15.0')
        manifest = json.loads((first / 'manifest.json').read_text())
        relative = next(relative for relative in manifest['files'] if 'x86' in relative)
        (first / relative).write_bytes(b'corrupt tarball')
        manifest['files'][relative] = assets.digest(first / relative)
        assets.atomic_json(first / 'manifest.json', manifest)
        with self.assertRaisesRegex(ValueError, 'trusted Pingap asset checksum'):
            assets.verify(first, self.catalog)
        second = self.prepare('pingap', '0.15.0')
        self.assertNotEqual(first, second)
        self.assertTrue(first.exists(), 'do not remove previous immutable generations')
        self.assertEqual(len(self.calls), 4)
        assets.verify(second, self.catalog)

    def test_pingap_rejects_cache_without_trusted_source_identity(self):
        first = self.prepare('pingap', '0.15.0')
        manifest = json.loads((first / 'manifest.json').read_text())
        del manifest['identity']['trusted_release']
        assets.atomic_json(first / 'manifest.json', manifest)
        with self.assertRaisesRegex(ValueError, 'trusted release identity'):
            assets.verify(first, self.catalog)
        self.assertNotEqual(self.prepare('pingap', '0.15.0'), first)

    def test_pingap_changed_trusted_source_creates_a_new_cache_identity(self):
        first = self.prepare('pingap', '0.15.0')
        new_catalog = json.loads(json.dumps(self.catalog))
        new_catalog['releases']['0.15.0']['assets']['amd64']['id'] += 1
        second = assets.prepare('pingap', '0.15.0', self.root / 'cache', downloader=self.downloader, trusted_catalog=new_catalog)
        self.assertNotEqual(first, second)
        self.assertEqual(len(list((self.root / 'cache/v1/refs').glob('*.json'))), 2)
        assets.verify(first, self.catalog)
        assets.verify(second, new_catalog)

    def test_pingap_wrong_download_digest_publishes_nothing(self):
        def corrupt(urls, target):
            self.downloader(urls, target)
            target.write_bytes(target.read_bytes() + b'wrong upstream bytes')
        with self.assertRaisesRegex(ValueError, 'trusted Pingap asset checksum'):
            assets.prepare('pingap', '0.15.0', self.root / 'cache', downloader=corrupt, trusted_catalog=self.catalog)
        self.assertFalse(list((self.root / 'cache/v1/refs').glob('*')))
        self.assertEqual(list((self.root / 'cache/v1/entries').glob('*')), [])

    def test_pingap_architecture_is_validated_even_when_fixture_digest_matches(self):
        catalog = json.loads(json.dumps(self.catalog))
        wrong = {}
        for arch, _, _, urls in assets.specs('pingap', '0.15.0', ('amd64', 'arm64')):
            data = archive_bytes(urls[0], 'arm64' if arch == 'amd64' else 'amd64')
            wrong[urls[0]] = data
            catalog['releases']['0.15.0']['assets'][arch]['sha256'] = hashlib.sha256(data).hexdigest()
        def download_wrong(urls, target):
            target.write_bytes(wrong[urls[0]])
        with self.assertRaisesRegex(ValueError, 'invalid Linux amd64 executable'):
            assets.prepare('pingap', '0.15.0', self.root / 'cache', downloader=download_wrong, trusted_catalog=catalog)
        self.assertFalse(list((self.root / 'cache/v1/refs').glob('*')))

    def test_concurrent_pingap_callers_share_one_verified_generation(self):
        with concurrent.futures.ThreadPoolExecutor(2) as workers:
            entries = list(workers.map(lambda _: self.prepare('pingap', '0.15.0'), range(2)))
        self.assertEqual(entries[0], entries[1])
        self.assertEqual(len(self.calls), 2)
        assets.verify(entries[0], self.catalog)

    def test_failed_parallel_pingap_generation_preserves_other_version(self):
        first = self.prepare('pingap', '0.14.3')
        attempted = threading.Event()
        proceed = threading.Event()
        def failed(urls, target):
            attempted.set()
            if not proceed.wait(5):
                raise RuntimeError('controlled wait timed out')
            target.write_bytes(b'wrong new release')
        with concurrent.futures.ThreadPoolExecutor(2) as workers:
            failure = workers.submit(assets.prepare, 'pingap', '0.15.0', self.root / 'cache', downloader=failed, trusted_catalog=self.catalog)
            self.assertTrue(attempted.wait(5))
            self.assertEqual(self.prepare('pingap', '0.14.3'), first)
            proceed.set()
            with self.assertRaisesRegex(ValueError, 'trusted Pingap asset checksum'):
                failure.result(timeout=5)
        assets.verify(first, self.catalog)
        self.assertEqual(len(list((self.root / 'cache/v1/refs').glob('*.json'))), 1)


if __name__ == '__main__':
    unittest.main()
