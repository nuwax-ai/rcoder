import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import time
import unittest
import uuid
sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
from remote_process import WORKER
from common import run


class RemoteProcessTests(unittest.TestCase):
    def test_local_test_stops_when_environment_lock_is_lost(self):
        with tempfile.TemporaryDirectory() as temp:
            pid_file = Path(temp) / 'pid'
            def guard():
                if pid_file.exists():
                    raise RuntimeError('lock lost')
            with self.assertRaisesRegex(RuntimeError, 'lock lost'):
                run([sys.executable, '-c', 'import os,pathlib,sys,time;pathlib.Path(sys.argv[1]).write_text(str(os.getpid()));time.sleep(60)', str(pid_file)],
                    timeout=10, log=Path(temp) / 'test.log', guard=guard)
            with self.assertRaises(ProcessLookupError):
                os.kill(int(pid_file.read_text()), 0)

    def test_disconnect_terminates_remote_process_group(self):
        ident, token = 'test-' + uuid.uuid4().hex, uuid.uuid4().hex
        base = Path('/tmp/rcoder-remote-k8s-' + ident)
        active = Path(str(base) + '.active')
        operation = Path(str(base) + '.operation')
        active.write_text(json.dumps({'pid': os.getpid(), 'token': token}))
        worker = None
        try:
            with tempfile.TemporaryDirectory() as temp:
                pid_file = Path(temp) / 'pid'
                child = [sys.executable, '-c', 'import os,pathlib,sys,time;pathlib.Path(sys.argv[1]).write_text(str(os.getpid()));time.sleep(60)', str(pid_file)]
                worker = subprocess.Popen([sys.executable, '-c', WORKER, ident, token, json.dumps(child)], stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE)
                worker.stdin.write(b'alive\n'); worker.stdin.flush()
                deadline = time.monotonic() + 5
                while not pid_file.exists() and time.monotonic() < deadline:
                    time.sleep(.05)
                self.assertTrue(pid_file.exists())
                pid = int(pid_file.read_text())
                worker.stdin.close()
                self.assertNotEqual(worker.wait(timeout=15), 0)
                with self.assertRaises(ProcessLookupError):
                    os.kill(pid, 0)
        finally:
            if worker:
                if worker.poll() is None:
                    worker.kill();worker.wait()
                worker.stdout.close();worker.stderr.close()
            active.unlink(missing_ok=True)
            operation.unlink(missing_ok=True)


if __name__ == '__main__':
    unittest.main()
