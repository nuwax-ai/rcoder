import re
from pathlib import Path
import unittest
from storage_contract_cases import PREFIX, TURSO_CASES, TURSO_EXTRA_CASES, PG_USERAPP_CASE, PG_EXTRA_TARGETS, passed_exactly_one
from contracts import REQUIRED
import run


class StorageContractCatalogTests(unittest.TestCase):
    def test_pg_catalog_matches_source_and_required_assertions(self):
        from pg_contract import CASES
        source = (Path(__file__).resolve().parents[2] /
                  'crates/rcoder-storage/src/pg/project_store/lifecycle_tests.rs').read_text()
        tests = set(re.findall(r'#\[tokio::test\]\s*async fn lifecycle_contract_(\w+)\(', source))
        self.assertEqual(len(CASES), len(set(CASES)))
        self.assertFalse(set(CASES) - tests)
        self.assertTrue({'PG ' + case for case in CASES} <= REQUIRED['pg_storage_lifecycle_contract'])

    def test_extra_pg_cases_are_real_tests_and_required(self):
        root = Path(__file__).resolve().parents[2] / 'crates/rcoder-storage/src'
        self.assertEqual(len(PG_EXTRA_TARGETS), 11)
        self.assertTrue(set(PG_EXTRA_TARGETS) <= REQUIRED['pg_storage_lifecycle_contract'])
        for target in PG_EXTRA_TARGETS.values():
            module, name = target.rsplit('::', 1)
            source = (root / (module.replace('::', '/') + '.rs')).read_text()
            self.assertRegex(source, r'async fn ' + re.escape(name) + r'\(')

    def test_required_turso_cases_are_explicit_source_tests(self):
        source = (Path(__file__).resolve().parents[2] /
                  'crates/rcoder-storage/src/userapp_lifecycle/tests.rs').read_text()
        source_root = Path(__file__).resolve().parents[2] / 'crates/rcoder-storage/src'
        backend_sources = {
            'userapp_lifecycle::common::local_tests': (source_root / 'userapp_lifecycle/common/local_tests.rs').read_text(),
            'db::tests': (source_root / 'db/tests.rs').read_text(),
        }
        tests = set(re.findall(r'#\[tokio::test\]\s*async fn (\w+)\(', source))
        backend_tests = {module + '::' + name for module, text in backend_sources.items()
                         for name in re.findall(r'#\[tokio::test(?:\([^]]*\))?\]\s*async fn (\w+)\(', text)}
        self.assertEqual(len(TURSO_CASES), len(set(TURSO_CASES)))
        self.assertFalse(set(TURSO_CASES) - tests)
        self.assertTrue(
            set(TURSO_EXTRA_CASES.values()) <= backend_tests)
        self.assertIn('turso_storage_contract', run.GROUPS['userapp'])
        self.assertTrue({'Turso ' + case for case in TURSO_CASES} <=
                        REQUIRED['turso_storage_lifecycle_contract'])
        self.assertTrue({'Turso ' + case for case in TURSO_EXTRA_CASES} <=
                        REQUIRED['turso_storage_lifecycle_contract'])
        self.assertEqual(PG_USERAPP_CASE, PREFIX + 'postgres_real_transactions_and_restart_contract')
        self.assertIn('PG userApp transactions and restart', REQUIRED['pg_storage_lifecycle_contract'])

    def test_zero_ignored_failed_and_duplicate_terminals_do_not_pass(self):
        valid = 'test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 190 filtered out; finished in 0.05s\n'
        self.assertTrue(passed_exactly_one(0, valid))
        for text in ('', valid.replace('1 passed', '0 passed'),
                     valid.replace('0 ignored', '1 ignored'),
                     valid.replace('0 failed', '1 failed'), valid + valid):
            with self.subTest(text=text):
                self.assertFalse(passed_exactly_one(0, text))
        self.assertFalse(passed_exactly_one(1, valid))


if __name__ == '__main__':
    unittest.main()
