import json
import os
import tempfile
import time
import unittest
from pathlib import Path

import prune_reports
from prune_reports import (
    dedupe_existing_binaries,
    referenced_binaries,
    run_directories,
    select_prunable,
)


def make_run(root, name, *, age_s, binaries=None, summary=None, manifest=None):
    run = Path(root) / name
    (run / 'bin').mkdir(parents=True)
    for binary_name, content in (binaries or {}).items():
        (run / 'bin' / binary_name).write_bytes(content)
    payload = summary if summary is not None else (manifest if manifest is not None else {})
    marker = 'summary.json' if summary is not None else 'manifest.json'
    (run / marker).write_text(json.dumps(payload))
    stamp = time.time() - age_s
    os.utime(run / marker, (stamp, stamp))
    return run


class RunDirectoryTests(unittest.TestCase):
    def test_only_marker_bearing_dirs_are_runs_and_private_dirs_excluded(self):
        with tempfile.TemporaryDirectory() as root:
            make_run(root, 'aaa', age_s=0)
            (Path(root) / '_bin').mkdir()
            (Path(root) / '.hidden').mkdir()
            (Path(root) / 'no-marker').mkdir()
            names = [entry['path'].name for entry in run_directories(Path(root))]
        self.assertEqual(names, ['aaa'])


class SelectPrunableTests(unittest.TestCase):
    def setUp(self):
        self.now = 1_000_000.0

    def runs(self, *ages):
        return [{'path': Path(f'r{index}'), 'mtime': self.now - age}
                for index, age in enumerate(ages)]

    def test_runs_within_keep_are_protected_regardless_of_age(self):
        prunable, protected = select_prunable(
            self.runs(1, 100 * 86400), now=self.now, days=14, keep=2)
        self.assertEqual({r['path'].name for r in protected}, {'r0', 'r1'})
        self.assertEqual(prunable, [])

    def test_old_runs_beyond_keep_and_days_are_prunable(self):
        prunable, protected = select_prunable(
            self.runs(1, 15 * 86400, 13 * 86400), now=self.now, days=14, keep=1)
        self.assertEqual([r['path'].name for r in prunable], ['r1'])
        self.assertEqual([r['path'].name for r in protected], ['r0'])

    def test_boundary_age_is_not_prunable(self):
        prunable, _ = select_prunable(
            self.runs(0, 14 * 86400), now=self.now, days=14, keep=1)
        self.assertEqual(prunable, [])


class ReferencedBinariesTests(unittest.TestCase):
    def test_summary_and_manifest_keep_binaries_alive(self):
        with tempfile.TemporaryDirectory() as root:
            finished = make_run(root, 'done', age_s=99, summary={'test_binary_sha256': {'suite': 'sha-a'}})
            in_progress = make_run(root, 'live', age_s=5, manifest={'test_binary_sha256': {'suite': 'sha-b'}})
            # 同上：断言留在 with 内
            runs = run_directories(Path(root))
            self.assertEqual(referenced_binaries(runs), {'sha-a', 'sha-b'})
            self.assertIsNotNone(finished)
            self.assertIsNotNone(in_progress)

    def test_unreadable_identity_returns_none_instead_of_guessing(self):
        with tempfile.TemporaryDirectory() as root:
            make_run(root, 'broken', age_s=99, summary=None, manifest={'x': 1})
            (Path(root) / 'broken' / 'summary.json').write_text('{not json')
            # 断言必须在 with 内：TemporaryDirectory 退出即删除，出块后
            # 再读标识文件只能得到空集（先前的假失败根因）
            runs = run_directories(Path(root))
            self.assertIsNone(referenced_binaries(runs))


class DedupeTests(unittest.TestCase):
    def test_identical_copies_become_shared_hardlinks_and_free_bytes(self):
        with tempfile.TemporaryDirectory() as root:
            store = Path(root) / '_bin'
            make_run(root, 'run1', age_s=3 * 3600, binaries={'suite': b'binary-content'})
            make_run(root, 'run2', age_s=3 * 3600, binaries={'suite': b'binary-content'})
            runs = run_directories(Path(root))
            freed, moved, skipped, examined = dedupe_existing_binaries(
                runs, store_root=store, now=time.time(), apply=True)
            first = Path(root) / 'run1' / 'bin' / 'suite'
            second = Path(root) / 'run2' / 'bin' / 'suite'
            self.assertTrue(os.path.samefile(first, second))
            self.assertEqual(examined, 2)
            self.assertEqual(freed, len(b'binary-content'))
            self.assertEqual(moved, len(b'binary-content'))

    def test_unique_content_moves_into_store_without_freeing(self):
        with tempfile.TemporaryDirectory() as root:
            store = Path(root) / '_bin'
            make_run(root, 'run1', age_s=3 * 3600, binaries={'suite': b'unique'})
            freed, moved, _, _ = dedupe_existing_binaries(
                run_directories(Path(root)), store_root=store, now=time.time(), apply=True)
            self.assertEqual(freed, 0)
            self.assertEqual(moved, len(b'unique'))
            self.assertTrue(list(store.iterdir()))

    def test_recent_runs_are_skipped_entirely(self):
        with tempfile.TemporaryDirectory() as root:
            store = Path(root) / '_bin'
            make_run(root, 'live', age_s=60, binaries={'suite': b'x'})
            freed, moved, skipped, examined = dedupe_existing_binaries(
                run_directories(Path(root)), store_root=store, now=time.time(), apply=True)
            self.assertEqual((freed, moved, examined), (0, 0, 0))
            self.assertEqual(skipped, 1)

    def test_dry_run_reports_without_touching_files(self):
        with tempfile.TemporaryDirectory() as root:
            store = Path(root) / '_bin'
            make_run(root, 'run1', age_s=3 * 3600, binaries={'suite': b'binary'})
            first = Path(root) / 'run1' / 'bin' / 'suite'
            before_inode = first.stat().st_ino
            dedupe_existing_binaries(
                run_directories(Path(root)), store_root=store, now=time.time(), apply=False)
            self.assertEqual(first.stat().st_ino, before_inode)
            self.assertFalse(store.exists())


if __name__ == '__main__':
    unittest.main()
