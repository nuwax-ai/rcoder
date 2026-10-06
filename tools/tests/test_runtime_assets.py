import concurrent.futures
import importlib.util
import io
import json
from pathlib import Path
import tarfile
import tempfile
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


class RuntimeAssetsTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.calls = []

    def downloader(self, urls, target):
        self.calls.append(urls[0])
        arch = 'arm64' if any(x in urls[0] for x in ('arm64', 'aarch64')) else 'amd64'
        data = executable(arch)
        if 'deno' in urls[0]:
            with zipfile.ZipFile(target, 'w') as archive:
                archive.writestr('deno', data)
        elif '.tar.gz' in urls[0]:
            name = urls[0].split('/')[-1][:-7]
            member = 'go/bin/go' if name.startswith('go') else name + '/bin/node' if name.startswith('node') else name
            with tarfile.open(target, 'w:gz') as archive:
                item = tarfile.TarInfo(member)
                item.size = len(data)
                archive.addfile(item, io.BytesIO(data))
        else:
            target.write_bytes(data)

    def prepare(self, component='node', version='22.23.2'):
        return assets.prepare(component, version, self.root / 'cache', downloader=self.downloader)

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
                manifest = assets.verify(self.prepare(component, version))
                self.assertEqual(len(manifest['files']), 2)
                self.assertEqual(manifest['identity']['version'], version)


if __name__ == '__main__':
    unittest.main()
