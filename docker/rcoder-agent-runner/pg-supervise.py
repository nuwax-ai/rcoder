#!/usr/bin/python3
"""Linux image-only owner of PostgreSQL and its one-shot bootstrap process tree.

Do not poll()/wait() a child before its group is stopped. waitid(WNOWAIT)
observes exit without reaping the group leader, so its PID cannot be reused
between observation and killpg. Bootstrap never signals a remembered parent PID.
"""
import os
from pathlib import Path
import signal
import subprocess
import sys
import time


def exited(child):
    return os.waitid(os.P_PID, child.pid, os.WEXITED | os.WNOHANG | os.WNOWAIT)


def signal_group(child, sig):
    # Every child starts its own session. Its unreaped leader pins the group ID.
    try:
        os.killpg(child.pid, sig)
    except ProcessLookupError:
        pass


def stop_children(children):
    for child, sig in children:
        signal_group(child, sig)
    deadline = time.monotonic() + 25  # supervisor stopwaitsecs=30, including reap
    while time.monotonic() < deadline:
        if all(exited(child) is not None for child, _ in children):
            break
        time.sleep(0.05)
    # Also stop remaining descendants when their leader exited first. Reap only
    # after sending the last group signal; never signal this numeric ID again.
    for child, _ in children:
        signal_group(child, signal.SIGKILL)
    for child, _ in children:
        child.wait()


def supervise(postgres_command, bootstrap_command):
    stopping = False

    def request_stop(_sig, _frame):
        nonlocal stopping
        stopping = True

    old_handlers = {sig: signal.signal(sig, request_stop)
                    for sig in (signal.SIGTERM, signal.SIGINT)}
    children = []
    try:
        postgres = subprocess.Popen(postgres_command, start_new_session=True)
        children.append((postgres, signal.SIGINT))  # PG fast, not smart shutdown
        if stopping:
            return 0
        bootstrap = subprocess.Popen(bootstrap_command, start_new_session=True)
        children.append((bootstrap, signal.SIGTERM))
        while not stopping:
            status = exited(postgres)
            if status is not None:
                return status.si_status if status.si_code == os.CLD_EXITED else 128 + status.si_status
            if bootstrap is not None:
                status = exited(bootstrap)
                if status is not None:
                    if status.si_code != os.CLD_EXITED or status.si_status != 0:
                        print('[pg] database bootstrap failed; stopping PostgreSQL', file=sys.stderr)
                        return 1
                    # A completed bootstrap must not leave any descendants.
                    signal_group(bootstrap, signal.SIGKILL)
                    bootstrap.wait()
                    children.pop()
                    bootstrap = None
            time.sleep(0.05)
        return 0
    finally:
        stop_children(children)
        for sig, handler in old_handlers.items():
            signal.signal(sig, handler)


def main():
    if not all(hasattr(os, name) for name in ('waitid', 'WNOWAIT', 'P_PID')):
        raise RuntimeError('PostgreSQL image supervisor requires Linux waitid support')
    if len(sys.argv) != 2:
        raise ValueError('Expected PostgreSQL identity helper path')
    postgres = str(Path(os.environ['PG_BIN']) / 'postgres')
    return supervise(
        [postgres, '-D', os.environ['PGDATA']],
        ['sh', '-c', '. "$1"; pg_admin_identity_load && pg_bootstrap_database',
         'pg-bootstrap', sys.argv[1]],
    )


if __name__ == '__main__':
    try:
        sys.exit(main())
    except Exception as error:
        # Environment/argv may include credentials: log only the exception type.
        print('[pg] supervision failed: ' + type(error).__name__, file=sys.stderr)
        sys.exit(1)
