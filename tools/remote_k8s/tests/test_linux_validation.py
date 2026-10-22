import importlib.util
from pathlib import Path
import unittest

SPEC = importlib.util.spec_from_file_location(
    'linux_validation', Path(__file__).resolve().parents[1] / 'linux_validation.py')
validation = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(validation)


class LinuxValidationTests(unittest.TestCase):
    def output(self, summary):
        return f'test {validation.CASE} ... ok\n\n{summary}\n'

    def test_exact_case_must_execute_and_pass(self):
        validation.validate_result(0, self.output(
            'test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; '
            '300 filtered out; finished in 0.25s'))

    def test_empty_skipped_or_multiple_matches_are_rejected(self):
        for counts in ['0 passed; 0 failed; 0 ignored',
                       '0 passed; 0 failed; 1 ignored',
                       '2 passed; 0 failed; 0 ignored']:
            with self.subTest(counts=counts), self.assertRaises(RuntimeError):
                validation.validate_result(0, self.output(
                    f'test result: ok. {counts}; 0 measured; '
                    '300 filtered out; finished in 0.25s'))

    def test_failure_or_wrong_case_cannot_reuse_a_passing_summary(self):
        output = self.output('test result: ok. 1 passed; 0 failed; 0 ignored; '
                             '0 measured; 300 filtered out; finished in 0.25s')
        with self.assertRaises(RuntimeError):
            validation.validate_result(1, output)
        with self.assertRaises(RuntimeError):
            validation.validate_result(0, output.replace(validation.CASE, 'other_case'))
