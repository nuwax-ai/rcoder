import argparse
import importlib.util
from pathlib import Path
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[2]
spec = importlib.util.spec_from_file_location('production_preflight', ROOT / 'tools/build/production_preflight.py')
gate = importlib.util.module_from_spec(spec)
spec.loader.exec_module(gate)
COMMIT = 'c' * 40


class ProductionPreflightTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        (self.root / 'makefiles').mkdir()
        (self.root / 'versions.mk').write_text(
            'PINGAP_VERSION ?= 0.14.3\nPINGAP_COMMIT ?= ' + COMMIT + '\nPNPM_MAJOR ?= 10\n')
        self.source = self.root / 'selected-source'
        src = self.source / 'crates/app-cli/src'
        src.mkdir(parents=True)
        (src / 'devtool.rs').write_text(
            'const DEFAULT_PINGAP_VERSION: &str = "0.14.3";\n'
            'const DEFAULT_PINGAP_COMMIT: &str = "' + COMMIT + '";\n')
        (src.parent / 'Cargo.toml').write_text('[dependencies]\npingap-config = {rev="' + COMMIT + '"}\n')
        self.args = argparse.Namespace(root=self.root, source=[self.source],
                                       pingap_version='0.14.3', pingap_commit=COMMIT,
                                       download_version='0.14.3', node_version='22.23.2',
                                       ttyd_version='1.7.7', go_version='1.26.4', deno_version='2.9.7',
                                       pnpm_major='10', rust_base_tag='trixie')

    def test_current_inputs_and_selected_source_match(self):
        self.assertEqual(gate.verify(self.args), ('0.14.3', COMMIT))

    def test_node_major_22_is_preserved(self):
        self.args.node_version = '24.1.0'
        with self.assertRaisesRegex(ValueError, 'Node must remain'):
            gate.verify(self.args)

    def test_download_override_must_match_config(self):
        self.args.download_version = '0.15.0'
        with self.assertRaisesRegex(ValueError, 'download version'):
            gate.verify(self.args)

    def test_missing_source_has_no_adjacent_repository_fallback(self):
        self.args.source = [self.root / 'missing']
        with self.assertRaisesRegex(ValueError, 'authority missing'):
            gate.verify(self.args)

    def test_conflicting_authority_is_rejected(self):
        (self.source / 'crates/app-cli/src/other.rs').write_text(
            'const DEFAULT_PINGAP_VERSION: &str = "0.15.0";\n'
            'const DEFAULT_PINGAP_COMMIT: &str = "' + COMMIT + '";\n')
        with self.assertRaisesRegex(ValueError, 'conflicting'):
            gate.verify(self.args)

    def test_cargo_config_rev_must_match_authority(self):
        (self.source / 'crates/app-cli/Cargo.toml').write_text('[dependencies]\npingap-config = {rev="' + 'b' * 40 + '"}\n')
        with self.assertRaisesRegex(ValueError, 'pingap-config rev'):
            gate.verify(self.args)

    def test_component_preflight_does_not_depend_on_pingap_source(self):
        self.args.source = None
        self.args.component = ['node', 'deno']
        self.args.pingap_version = None
        self.args.pingap_commit = None
        self.assertIsNone(gate.verify(self.args))

    def test_component_preflight_requires_every_declared_version(self):
        self.args.component = ['node', 'deno']
        self.args.deno_version = None
        with self.assertRaisesRegex(ValueError, 'missing actual deno'):
            gate.verify(self.args)

    def test_pnpm_major_must_not_change_or_fall_back_when_missing(self):
        for value in ['11', '', None]:
            with self.subTest(value=value):
                self.args.pnpm_major = value
                with self.assertRaisesRegex(ValueError, 'pnpm must retain'):
                    gate.verify(self.args)

    def test_pnpm_expected_major_comes_from_single_dependency_source(self):
        path = self.root / 'versions.mk'
        path.write_text(path.read_text().replace('PNPM_MAJOR ?= 10', 'PNPM_MAJOR ?= 11'))
        self.args.pnpm_major = '11'
        self.assertEqual(gate.verify(self.args), ('0.14.3', COMMIT))

    def test_rust_toolchain_is_stable_not_a_fixed_old_version(self):
        for value in ['1.91', '', None]:
            with self.subTest(value=value):
                self.args.rust_base_tag = value
                with self.assertRaisesRegex(ValueError, 'moving stable'):
                    gate.verify(self.args)


if __name__ == '__main__':
    unittest.main()
