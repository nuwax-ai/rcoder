import unittest

from tools.verify_kani import classify_result


class ClassifyResultTests(unittest.TestCase):
    harness = "pg_utils::kani_proofs::pg_identifier_symbolic_grammar"

    def test_successful_unwind_check_is_not_misclassified(self) -> None:
        output = (
            "Checking harness pg_identifier_symbolic_grammar\n"
            "Check 1: proof.unwind.0\n"
            " - Status: SUCCESS\n"
            ' - Description: "unwinding assertion loop 0"\n'
            "VERIFICATION:- SUCCESSFUL\n"
        )
        self.assertEqual(
            classify_result(self.harness, 0, output, False, ""),
            ("PASS", "proved"),
        )

    def test_failed_unwinding_check_is_reported_separately(self) -> None:
        output = (
            "Checking harness pg_identifier_symbolic_grammar\n"
            "Failed Checks: unwinding assertion loop 0\n"
            "VERIFICATION:- FAILED\n"
        )
        status, _ = classify_result(self.harness, 1, output, False, "")
        self.assertEqual(status, "UNWINDING")

    def test_failed_assertion_is_failure(self) -> None:
        output = (
            "Checking harness pg_identifier_symbolic_grammar\n"
            "Failed Checks: assertion failed: actual == expected\n"
            "VERIFICATION:- FAILED\n"
        )
        status, _ = classify_result(self.harness, 1, output, False, "")
        self.assertEqual(status, "FAILURE")

    def test_undetermined_is_not_a_pass(self) -> None:
        output = (
            "Checking harness pg_identifier_symbolic_grammar\n"
            "VERIFICATION:- UNDETERMINED\n"
        )
        status, _ = classify_result(self.harness, 1, output, False, "")
        self.assertEqual(status, "UNDETERMINED")

    def test_timeout_is_reported_separately(self) -> None:
        status, _ = classify_result(self.harness, None, "", True, "")
        self.assertEqual(status, "TIMEOUT")

    def test_missing_harness_is_not_run(self) -> None:
        status, _ = classify_result(self.harness, 1, "cargo compilation failed", False, "")
        self.assertEqual(status, "NOT_RUN")


if __name__ == "__main__":
    unittest.main()
