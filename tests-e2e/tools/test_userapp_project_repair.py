"""Offline evidence-integrity regressions. No Docker or LLM calls."""
import json
import unittest

from userapp_project_repair import Redactor, connection_refused, tool_evidence


class EvidenceTests(unittest.TestCase):
    def test_jsonl_redaction_preserves_escaped_urls_and_hides_credentials(self):
        secret = "fixture-private-api-key"
        command = 'curl "https://user:password@example.invalid/tasks/1?token=private"; echo "done"'
        event = {"type": "tool_use", "part": {"state": {"input": {"command": command},
                 "output": json.dumps({"message": command, "api_key": secret})}}}
        redactor = Redactor([secret])
        output = redactor.artifact(json.dumps(event) + "\n" + json.dumps({"type": "step_finish"}), json_lines=True)
        parsed = [json.loads(line) for line in output.splitlines()]
        self.assertEqual(len(parsed), 2)
        self.assertEqual(parsed[1]["type"], "step_finish")
        clean_command = parsed[0]["part"]["state"]["input"]["command"]
        self.assertTrue(clean_command.endswith('; echo "done"'))
        self.assertEqual(json.loads(parsed[0]["part"]["state"]["output"])["message"], clean_command)
        for credential in (secret, "user:password", "token=private"):
            self.assertNotIn(credential, output)
        structured = redactor.artifact({"command": command, "api_key": secret})
        self.assertEqual(json.loads(structured)["command"], clean_command)

    def test_observation_errors_do_not_count_as_closed_or_successful_reads(self):
        refused = {"observed": True, "errno": 111, "refused_errno": 111, "errno_name": "ECONNREFUSED"}
        self.assertTrue(connection_refused(refused))
        for observation in (
            {}, {"observed": False, "error_type": "DockerExecError"},
            {**refused, "observed": False},
            {**refused, "errno": 110, "errno_name": "ETIMEDOUT"},
            {**refused, "errno": 0, "errno_name": "CONNECTED"},
            {**refused, "errno": "111"},
        ):
            self.assertFalse(connection_refused(observation), observation)
        failed_read = {"type": "tool_use", "part": {"tool": "bash", "state": {
            "status": "completed", "input": {"command": "cat userapp-dev-guide/SKILL.md"},
            "output": "SOURCE_WORKSPACE repair_target", "metadata": {"exit": 1}}}}
        self.assertFalse(tool_evidence([failed_read])["guide"])
        failed_read["part"]["state"]["metadata"]["exit"] = 0
        self.assertTrue(tool_evidence([failed_read])["guide"])


if __name__ == "__main__":
    unittest.main()
