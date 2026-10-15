import json
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest

from tools.build.pingap_identity import parse_pingap_version, source_identity


class PingapIdentityTests(unittest.TestCase):
    def fixture(self, root):
        cargo = root / 'crates/app-cli/Cargo.toml'
        cargo.parent.mkdir(parents=True)
        cargo.write_text('[dependencies]\npingap-config = {git="https://github.com/vicanso/pingap",rev="' + 'a' * 40 + '"}\n')
        devtool = root / 'crates/app-cli/src/build_deploy/devtool.rs'
        devtool.parent.mkdir(parents=True)
        devtool.write_text('const DEFAULT_PINGAP_VERSION: &str = "0.15.0";\nconst DEFAULT_PINGAP_COMMIT: &str = "' + 'a' * 40 + '";\n')
        return cargo, devtool

    def test_source_identity_checks_dependency_and_constants(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            cargo, devtool = self.fixture(root)
            self.assertEqual(source_identity(root), {'version': '0.15.0', 'commit': 'a' * 40})
            devtool.write_text(devtool.read_text().replace('a' * 40, 'b' * 40))
            with self.assertRaisesRegex(ValueError, 'disagree'):
                source_identity(root)
            devtool.write_text(devtool.read_text().replace('b' * 40, 'a' * 40))
            cargo.write_text(cargo.read_text().replace('https://github.com/vicanso/pingap', 'https://example.com/fake'))
            with self.assertRaisesRegex(ValueError, 'official repository'):
                source_identity(root)

    def test_unsupported_or_duplicate_dependency_formats_fail_explicitly(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            cargo, _ = self.fixture(root)
            original = cargo.read_text()
            for invalid in [original + original.split('\n')[1] + '\n',
                            '[dependencies.pingap-config]\ngit="https://github.com/vicanso/pingap"\nrev="' + 'a' * 40 + '"\n']:
                cargo.write_text(invalid)
                with self.assertRaisesRegex(ValueError, 'supported inline'):
                    source_identity(root)

    def test_catalog_cannot_disagree_with_source_pin(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            self.fixture(root)
            catalog = root / 'tools/build/pingap-assets.json'
            catalog.parent.mkdir(parents=True)
            catalog.write_text(json.dumps({'repository': 'vicanso/pingap', 'releases': {'0.15.0': {'tag': 'v0.15.0', 'commit': 'b' * 40}}}))
            with self.assertRaisesRegex(ValueError, 'trusted official'):
                source_identity(root)

    def test_cli_fields_read_the_selected_source_without_building(self):
        script = Path(__file__).resolve().parents[1] / 'build/pingap_identity.py'
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            self.fixture(root)
            for field, expected in [('version', '0.15.0'), ('commit', 'a' * 40), ('pair', '0.15.0 ' + 'a' * 40)]:
                with self.subTest(field=field):
                    result = subprocess.run([sys.executable, str(script), '--repo-root', str(root), '--field', field], capture_output=True, text=True)
                    self.assertEqual(result.returncode, 0, result.stderr)
                    self.assertEqual(result.stdout.strip(), expected)
            self.assertFalse((root / '.cache').exists())
            cargo = root / 'crates/app-cli/Cargo.toml'
            cargo.write_text(cargo.read_text().replace('a' * 40, 'b' * 40))
            result = subprocess.run([sys.executable, str(script), '--repo-root', str(root), '--field', 'version'], capture_output=True, text=True)
            self.assertNotEqual(result.returncode, 0)
            self.assertEqual(result.stdout, '')
            self.assertIn('disagree', result.stderr)

    def test_version_parser_matches_whole_output(self):
        self.assertEqual(parse_pingap_version('pingap 0.15.0\n'), '0.15.0')
        self.assertEqual(parse_pingap_version('pingap 0.15.0-rc.1+build.2'), '0.15.0-rc.1+build.2')
        for output in ['pingap 0.15.00', 'pingap 0.15.0evil', 'pingap 0.15.0 extra', 'warning\npingap 0.15.0', '0.15.0', 'pingap 00.15.0', 'pingap 0.15.0-01']:
            with self.subTest(output=output), self.assertRaises(ValueError):
                parse_pingap_version(output)


if __name__ == '__main__':
    unittest.main()
