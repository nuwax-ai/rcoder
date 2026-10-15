#!/usr/bin/env python3
"""Prune archived e2e report runs. Dry-run by default; --apply deletes.

Run directories are pruned by age beyond --days while always keeping the
newest --keep runs. Content-addressed binaries under reports/_bin are
garbage-collected only when no remaining run (including in-progress ones,
which carry manifest.json) references their sha. Deletion never touches
anything outside the reports root.
"""
import argparse
import hashlib
import json
import os
import shutil
import sys
import time
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
RUN_ROOT = Path(os.environ.get('E2E_RUN_ROOT') or ROOT / 'reports')


def run_directories(root):
    """Archived run dirs: marker-bearing entries; _bin and dot dirs excluded."""
    runs = []
    if not root.is_dir():
        return runs
    for entry in root.iterdir():
        if not entry.is_dir() or entry.name.startswith(('_', '.')):
            continue
        summary = entry / 'summary.json'
        marker = summary if summary.exists() else entry / 'manifest.json'
        if marker.exists():
            runs.append({'path': entry, 'mtime': marker.stat().st_mtime})
    return runs


def select_prunable(runs, *, now, days, keep):
    """Split into (prunable, protected). The newest `keep` runs are always
    protected regardless of age; older runs are prunable once past `days`."""
    ordered = sorted(runs, key=lambda run: run['mtime'], reverse=True)
    protected = ordered[:keep]
    prunable = [run for run in ordered[keep:] if (now - run['mtime']) > days * 86400]
    return prunable, protected


def referenced_binaries(runs):
    """shas referenced by the given runs (summary preferred, manifest
    fallback so in-progress runs keep their frozen binaries alive).
    None = identity evidence unreadable; caller must skip GC, not guess."""
    referenced = set()
    for run in runs:
        for name in ('summary.json', 'manifest.json'):
            path = run['path'] / name
            if not path.exists():
                continue
            try:
                data = json.loads(path.read_text())
            except (OSError, ValueError):
                return None
            shas = data.get('test_binary_sha256')
            if isinstance(shas, dict):
                referenced.update(value for value in shas.values() if isinstance(value, str))
    return referenced


def directory_size(path):
    total = 0
    for item in path.rglob('*'):
        if item.is_file():
            try:
                total += item.stat().st_size
            except OSError:
                pass
    return total


def file_sha256(path):
    digest = hashlib.sha256()
    with path.open('rb') as handle:
        for chunk in iter(lambda: handle.read(1 << 20), b''):
            digest.update(chunk)
    return digest.hexdigest()


def dedupe_existing_binaries(runs, *, store_root, now, apply, recent_grace_s=7200):
    """Convert per-run bin/ copies into content-addressed hardlinks.

    Runs whose marker changed within the grace window are skipped so a live
    run is never rebased mid-flight. Each copy is content-hashed; matching
    canonical content replaces the copy with a hardlink (freed bytes), and
    unseen content moves into the store then links back (shared, not freed).
    Returns (freed_bytes, moved_bytes, skipped_recent, examined).
    """
    bin_root = Path(store_root)
    freed = moved = 0
    skipped_recent = examined = 0
    for run in sorted(runs, key=lambda item: item['mtime'], reverse=True):
        if now - run['mtime'] < recent_grace_s:
            skipped_recent += 1
            continue
        bin_dir = run['path'] / 'bin'
        if not bin_dir.is_dir():
            continue
        for frozen in bin_dir.iterdir():
            if not frozen.is_file():
                continue
            examined += 1
            try:
                sha = file_sha256(frozen)
                size = frozen.stat().st_size
            except OSError:
                continue
            canonical_dir = bin_root / sha
            canonical = canonical_dir / frozen.name
            if canonical.exists():
                try:
                    if os.path.samefile(canonical, frozen):
                        continue  # already linked
                except OSError:
                    pass
                freed += size
                if apply:
                    frozen.unlink()
                    os.link(canonical, frozen)
            else:
                moved += size
                if apply:
                    canonical_dir.mkdir(parents=True, exist_ok=True)
                    shutil.move(str(frozen), str(canonical))
                    os.link(canonical, frozen)
    return freed, moved, skipped_recent, examined


def main():
    parser = argparse.ArgumentParser(
        description='Prune archived e2e report runs (dry-run unless --apply)')
    parser.add_argument('--days', type=float, default=14.0,
                        help='Runs older than this many days are prunable (default 14)')
    parser.add_argument('--keep', type=int, default=30,
                        help='Always keep at least this many newest runs (default 30)')
    parser.add_argument('--apply', action='store_true',
                        help='Actually delete; without it only print what would go')
    parser.add_argument('--dedupe', action='store_true',
                        help='Also convert existing per-run bin/ copies into '
                             'content-addressed hardlinks (main reclaim path for '
                             'recent duplicated binaries)')
    args = parser.parse_args()
    runs = run_directories(RUN_ROOT)
    if not runs:
        print(f'no archived runs under {RUN_ROOT}')
        return 0
    prunable, protected = select_prunable(runs, now=time.time(),
                                          days=args.days, keep=args.keep)
    mode = 'APPLY' if args.apply else 'DRY-RUN'
    print(f'{mode}: {len(runs)} archived runs, protecting newest {len(protected)}, '
          f'{len(prunable)} prunable (>{args.days:g} days old)')
    reclaimable = 0
    pruned_paths = set()
    for run in sorted(prunable, key=lambda item: item['mtime']):
        size = directory_size(run['path'])
        reclaimable += size
        pruned_paths.add(str(run['path']))
        if args.apply:
            shutil.rmtree(run['path'])
        print(f'  {"deleted" if args.apply else "would delete"} '
              f'{run["path"].name} ({size / 1e6:.1f} MB)')
    remaining = [run for run in runs if str(run['path']) not in pruned_paths]
    if args.dedupe:
        freed, moved, skipped, examined = dedupe_existing_binaries(
            remaining, store_root=RUN_ROOT / '_bin', now=time.time(),
            apply=args.apply)
        verb = 'deduped' if args.apply else 'would dedupe'
        print(f'  {verb}: {examined} frozen binaries examined — '
              f'{freed / 1e9:.2f} GB duplicate copies reclaimed, '
              f'{moved / 1e9:.2f} GB moved into the shared store '
              f'({skipped} recent runs skipped)')
        reclaimable += freed
    bin_root = RUN_ROOT / '_bin'
    if bin_root.is_dir():
        referenced = referenced_binaries(remaining)
        if referenced is None:
            print('  _bin GC skipped: run identity evidence unreadable')
        else:
            for entry in sorted(bin_root.iterdir()):
                if entry.name in referenced:
                    continue
                size = directory_size(entry)
                reclaimable += size
                if args.apply:
                    shutil.rmtree(entry)
                print(f'  {"deleted" if args.apply else "would delete"} '
                      f'_bin/{entry.name} ({size / 1e6:.1f} MB)')
    suffix = '' if args.apply else ' (pass --apply to delete)'
    print(f'{mode}: {reclaimable / 1e9:.2f} GB reclaimable{suffix}')
    return 0


if __name__ == '__main__':
    sys.exit(main())
