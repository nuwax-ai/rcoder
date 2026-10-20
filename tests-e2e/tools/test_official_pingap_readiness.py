"""Offline acceptance-harness timing checks; these do not run Docker E2E."""
import unittest
from unittest.mock import Mock, patch

from official_pingap_lifecycle import Acceptance


class StopReadinessTests(unittest.TestCase):
    def test_settled_stop_waits_for_the_observation_cache_to_expire(self):
        acceptance = Acceptance("builtin", "test-source")
        acceptance.save = Mock()
        acceptance.operate = Mock(return_value={"operation_id": "original-stop"})
        acceptance.proxy_receipt = Mock(side_effect=[
            {"instance_id": "previous-proxy"},
            {"instance_id": "standby-proxy", "operation_id": "original-stop"},
        ])
        acceptance.request = Mock(side_effect=[
            (200, {"data": {"confirmation": {
                "confirmed": True, "phase": "standby", "operation_id": "original-stop",
            }}}),
            # Stop's durable terminal precedes the server's Idle publication;
            # an already cached stopping observation can still be valid here.
            (200, {"data": {"status": "stopping", "ready": False}}),
            (200, {"data": {"status": "stopped", "ready": False}}),
        ])

        with patch("official_pingap_lifecycle.time.sleep"):
            stopped = acceptance.stop_once()

        self.assertEqual(stopped["operation_id"], "original-stop")
        acceptance.operate.assert_called_once_with("stop", midwindow_readiness=True)
        self.assertEqual(acceptance.request.call_count, 3)
        settled = acceptance.evidence["checks"][-1]
        self.assertEqual(settled["name"], "settled Stop reports stopped")
        self.assertTrue(settled["passed"])
        self.assertEqual(settled["response"]["data"]["status"], "stopped")


if __name__ == "__main__":
    unittest.main()
