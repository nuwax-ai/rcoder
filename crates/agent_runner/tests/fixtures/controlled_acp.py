#!/usr/bin/env python3
"""仅协议测试：真实 ACP 子进程与可控屏障；不调用或伪装真实 AI。"""
import json
import os
import signal
import sys
import threading
import time
from pathlib import Path

root = Path(os.environ['ACP_FIXTURE_DIR'])
pid = os.getpid()
project = os.environ.get('AGENT_PROJECT_ID', '')
session = 'fixture-' + str(pid)
lock = threading.Lock()
cancelled = threading.Event()


def log(event, **details):
    row = dict(event=event, pid=pid, project=project, session=session, **details)
    with (root / 'protocol.jsonl').open('a') as output:
        output.write(json.dumps(row) + '\n')


def send(message):
    with lock:
        print(json.dumps(message), flush=True)


exit_cause = None


def exit_process(cause):
    global exit_cause
    # TERM may arrive while the main thread is already holding its EOF exit.
    # Keep the first real cause and one barrier; SIGKILL remains uncatchable.
    if exit_cause is not None:
        return
    exit_cause = cause
    log('exit_started', cause=cause)
    if (root / (project + '.exit-barrier')).exists():
        while not (root / (project + '.exit-release')).exists():
            time.sleep(0.01)
    if (root / (project + '.exit-delay')).exists():
        time.sleep(float((root / (project + '.exit-delay')).read_text()))
    log('exit', cause=cause)
    raise SystemExit(0)


def term(_signal, _frame):
    log('term_received')
    exit_process('sigterm')


signal.signal(signal.SIGTERM, term)
log('spawn', cwd=os.getcwd())


def prompt(message):
    request_id = message.get('params', {}).get('_meta', {}).get('request_id', '')
    text = ''.join(block.get('text', '') for block in message['params'].get('prompt', []))
    cancelled.clear()
    log('prompt', request_id=request_id, text=text)
    if (root / (project + '.prompt-error')).exists():
        delay = (root / (project + '.prompt-error')).read_text()
        if delay == 'hold':
            while not (root / (project + '.prompt-error-release')).exists():
                time.sleep(0.01)
        else:
            time.sleep(float(delay))
        send({'jsonrpc': '2.0', 'id': message['id'], 'error': {
            'code': 1002, 'message': 'controlled agent application error'}})
        log('prompt_error', request_id=request_id)
        return
    if 'hold' in text:
        while not (root / (project + '.release')).exists() and not cancelled.is_set():
            time.sleep(0.01)
    send({'jsonrpc': '2.0', 'id': message['id'], 'result': {
        'stopReason': 'cancelled' if cancelled.is_set() else 'end_turn'}})
    log('prompt_end', request_id=request_id, cancelled=cancelled.is_set())


for line in sys.stdin:
    message = json.loads(line)
    method = message.get('method')
    if method == 'initialize':
        send({'jsonrpc': '2.0', 'id': message['id'], 'result': {
            'protocolVersion': 1, 'agentCapabilities': {'loadSession': True}}})
    elif method in ('session/new', 'session/load'):
        if method == 'session/new' and (root / (project + '.new-timeout')).exists():
            log('new_timeout')
            continue
        if method == 'session/load' and (root / (project + '.load-delay')).exists():
            time.sleep(float((root / (project + '.load-delay')).read_text()))
        if method == 'session/load' and (root / (project + '.load-error')).exists():
            log('load_error', requested_session=message['params']['sessionId'])
            send({'jsonrpc': '2.0', 'id': message['id'], 'error': {
                'code': -32603, 'message': 'controlled historical load failure'}})
            continue
        if method == 'session/load' and (root / (project + '.load-timeout')).exists():
            log('load_timeout', requested_session=message['params']['sessionId'])
            continue
        if (root / (project + '.fail')).exists():
            send({'jsonrpc': '2.0', 'id': message['id'], 'error': {
                'code': -32603, 'message': 'controlled create failure'}})
            continue
        if method == 'session/load':
            session = message['params']['sessionId']
        elif (root / (project + '.new-session-id')).exists():
            session = (root / (project + '.new-session-id')).read_text()
        early = root / (project + '.early-notification-id')
        if method == 'session/new' and early.exists():
            for sid in [early.read_text(), session]:
                send({'jsonrpc': '2.0', 'method': 'session/update', 'params': {
                    'sessionId': sid, 'update': {
                        'sessionUpdate': 'available_commands_update', 'availableCommands': []}}})
            log('early_notifications_sent')
            while not (root / (project + '.early-notification-release')).exists():
                time.sleep(0.01)
        log('load' if method == 'session/load' else 'new', params=message['params'])
        send({'jsonrpc': '2.0', 'id': message['id'], 'result': {'sessionId': session}})
    elif method == 'session/prompt':
        threading.Thread(target=prompt, args=(message,), daemon=True).start()
    elif method == 'session/cancel':
        if (root / (project + '.cancel-abnormal-exit')).exists():
            log('cancel_abnormal_exit', target=message['params']['sessionId'])
            os.kill(pid, signal.SIGKILL)
        if (root / (project + '.cancel-delay')).exists():
            log('cancel_received', target=message['params']['sessionId'])
            time.sleep(float((root / (project + '.cancel-delay')).read_text()))
        log('cancel', target=message['params']['sessionId'])
        cancelled.set()
    elif method == 'session/set_mode':
        send({'jsonrpc': '2.0', 'id': message['id'], 'result': {}})
    elif 'id' in message:
        send({'jsonrpc': '2.0', 'id': message['id'], 'error': {
            'code': -32601, 'message': 'fixture method unsupported'}})

# Closing ACP stdin is a normal production cancellation path too.
exit_process('eof')
