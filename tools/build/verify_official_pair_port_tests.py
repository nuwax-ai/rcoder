#!/usr/bin/env python3
"""Socket precheck regressions; these do not run app-cli or Pingap."""
import socket
import unittest
from verify_official_pair import verify_unused_business_port


class BusinessPortPrecheckTests(unittest.TestCase):
    def test_live_listener_still_rejected(self):
        with socket.socket() as listener:
            listener.bind(("127.0.0.1", 0))
            listener.listen(1)
            with self.assertRaises(OSError):
                verify_unused_business_port(listener.getsockname()[1])

    @unittest.skipIf(hasattr(socket, "SO_EXCLUSIVEADDRUSE"), "Windows uses exclusive bind instead of Unix TIME_WAIT reuse")
    def test_time_wait_after_clean_close_is_reusable(self):
        with socket.socket() as listener:
            listener.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
            listener.bind(("127.0.0.1", 0))
            port = listener.getsockname()[1]
            listener.listen(1)
            with socket.create_connection(("127.0.0.1", port), timeout=3) as client:
                accepted, _ = listener.accept()
                with accepted:
                    accepted.shutdown(socket.SHUT_WR)
                    self.assertEqual(client.recv(1), b"")
                    client.shutdown(socket.SHUT_WR)
                    self.assertEqual(accepted.recv(1), b"")
        verify_unused_business_port(port)


if __name__ == "__main__":
    unittest.main()
