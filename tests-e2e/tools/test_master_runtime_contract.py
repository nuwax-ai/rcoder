import importlib.util
from pathlib import Path
import tempfile
import unittest


def module(name):
    path = Path(__file__).resolve().parents[2] / 'docker' / (name + '.py')
    spec = importlib.util.spec_from_file_location(name, path)
    loaded = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(loaded)
    return loaded


base = module('master-base-contract')
binaries = module('verify-runtime-binaries')


class MasterRuntimeContractTests(unittest.TestCase):
    def test_missing_and_changed_provenance_require_explicit_rebuild(self):
        for labels in (None, {}, {base.LABEL: 'previous-source'}):
            with self.subTest(labels=labels):
                with self.assertRaisesRegex(ValueError, 'make docker-build-master-base'):
                    base.check_labels(labels, 'current-source')
        base.check_labels({base.LABEL: 'current-source'}, 'current-source')

    def test_each_actual_base_build_input_changes_fingerprint(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            for name in base.SOURCE_FILES:
                path = root / name
                path.parent.mkdir(parents=True, exist_ok=True)
                path.write_text('original')
            original = base.source_fingerprint(root)
            for name in base.SOURCE_FILES:
                path = root / name
                path.write_text('changed')
                self.assertNotEqual(original, base.source_fingerprint(root))
                path.write_text('original')

    def test_both_actual_processes_must_execute_and_report_version(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            for name in binaries.BINARY_NAMES:
                path = root / name
                path.write_text(f'#!/bin/sh\necho "{name} 1.0"\n')
                path.chmod(0o755)
            binaries.verify(root)
            for name in binaries.BINARY_NAMES:
                path = root / name
                good = path.read_text()
                for failing in ('echo "GLIBC_2.39 not found" >&2; exit 127', 'exit 0'):
                    with self.subTest(binary=name, failing=failing):
                        path.write_text('#!/bin/sh\n' + failing + '\n')
                        with self.assertRaisesRegex(RuntimeError, name):
                            binaries.verify(root)
                path.write_text(good)


if __name__ == '__main__':
    unittest.main()
