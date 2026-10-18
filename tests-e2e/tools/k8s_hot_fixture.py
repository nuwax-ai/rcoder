#!/usr/bin/env python3
"""Run-owned artifact download barriers; no runtime/AI response emulation.

Only original built B/C/D ZIP bytes are served; C is read-only and unblocked. Barrier control is filesystem-only
through kubectl exec into this exact fixture Pod, never an unauthenticated HTTP
write endpoint. Every blocked GET has its own finite deadline.
"""
import http.server
import json
import os
from pathlib import Path
import threading
import time
import urllib.parse

ARTIFACTS = Path('/fixture')
CONTROL = Path('/control')
MAX_BARRIER_SECONDS = 180


class State:
    def __init__(self, root):
        self.root = Path(root)
        self.root.mkdir(parents=True, exist_ok=True)
        self.lock = threading.Lock()
        self.counts = {'B': 0, 'D': 0}
        self.static_counts = {'C': 0}
        self.events = []

    def record(self, version, event):
        with self.lock:
            if event == 'entered':
                if version in self.counts:
                    self.counts[version] += 1
                else:
                    self.static_counts[version] += 1
            self.events.append({'version': version, 'event': event,
                                'monotonic': time.monotonic()})
            temp = self.root / 'state.json.pending'
            temp.write_text(json.dumps({'counts': self.counts, 'static_counts': self.static_counts, 'events': self.events}))
            os.replace(temp, self.root / 'state.json')


def handler(state, artifacts=ARTIFACTS, barrier_seconds=MAX_BARRIER_SECONDS):
    class Handler(http.server.BaseHTTPRequestHandler):
        def do_GET(self):
            path = urllib.parse.urlsplit(self.path).path
            if path == '/health':
                self.send_response(200)
                self.end_headers()
                self.wfile.write(b'healthy')
                return
            if path not in ('/B.zip', '/C.zip', '/D.zip'):
                self.send_error(404)
                return
            version = path[1]
            state.record(version, 'entered')
            deadline = time.monotonic() + barrier_seconds
            while version in state.counts and not (state.root / (version + '.release')).exists():
                if time.monotonic() >= deadline:
                    state.record(version, 'deadline')
                    self.send_error(504, 'Owned artifact barrier deadline exceeded')
                    return
                time.sleep(.02)
            body = (Path(artifacts) / (version + '.zip')).read_bytes()
            self.send_response(200)
            self.send_header('Content-Type', 'application/zip')
            self.send_header('Content-Length', str(len(body)))
            self.end_headers()
            try:
                self.wfile.write(body)
                state.record(version, 'served')
            except (BrokenPipeError, ConnectionResetError):
                state.record(version, 'observer_disconnected')

        def log_message(self, *_args):
            # No request headers, environment or credentials enter fixture logs.
            pass
    return Handler


def main():
    state = State(CONTROL)
    server = http.server.ThreadingHTTPServer(('0.0.0.0', 8080), handler(state))
    server.daemon_threads = True
    server.serve_forever()


if __name__ == '__main__':
    main()
