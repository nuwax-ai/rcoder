#!/usr/bin/env python3
"""仅协议测试：真实 ACP 子进程与可控屏障；不调用或伪装真实 AI。"""
import json
import hashlib
import os
import signal
import sys
import threading
import time
from pathlib import Path

root = Path(os.environ['ACP_FIXTURE_DIR'])
pid = os.getpid()
project = os.environ.get('AGENT_PROJECT_ID', '')
session = os.environ.get('ACP_FIXTURE_SESSION_PREFIX', 'fixture-') + str(pid)
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
spawn_details = {}
if os.environ.get('ACP_FIXTURE_HTTP_CONTRACT') == '1':
    spawn_details['launch_environment'] = {key: os.environ.get(key) for key in
        ('AGENT_PROJECT_ID', 'AGENT_WORKING_DIR', 'LOG_DIR')}
log('spawn', cwd=os.getcwd(), **spawn_details)


def prompt(message):
    request_id = message.get('params', {}).get('_meta', {}).get('request_id', '')
    received_sid = message.get('params', {}).get('sessionId')
    if os.environ.get('ACP_FIXTURE_HTTP_CONTRACT') == '1' and received_sid != session:
        log('prompt_wrong_session', request_id=request_id, received_sid=received_sid)
        send({'jsonrpc': '2.0', 'id': message['id'], 'error': {
            'code': -32602, 'message': 'controlled prompt session does not match its actual owner'}})
        return
    text = ''.join(block.get('text', '') for block in message['params'].get('prompt', []))
    cancelled.clear()
    details = {'received_sid': received_sid} if os.environ.get('ACP_FIXTURE_HTTP_CONTRACT') == '1' else {}
    log('prompt', request_id=request_id, text=text, **details)
    # Opt-in HTTP entry acceptance. Existing native/SDK tests keep their original
    # behavior; this barrier is not a product delivery/retention guarantee.
    if os.environ.get('ACP_FIXTURE_HTTP_CONTRACT') == '1':
        last_probe = 0.0
        while not (root / (request_id + '.release')).exists() and not cancelled.is_set():
            probe = root / (request_id + '.subscriber-probe')
            if probe.exists() and time.monotonic() - last_probe >= 0.1:
                send({'jsonrpc': '2.0', 'method': 'session/update', 'params': {
                    'sessionId': session, 'update': {
                        'sessionUpdate': 'available_commands_update',
                        'availableCommands': [{'name': probe.read_text(), 'description': 'controlled subscriber witness'}]},
                    '_meta': {'request_id': request_id}}})
                last_probe = time.monotonic()
            time.sleep(0.01)
        if not cancelled.is_set():
            output = Path(os.environ.get('ACP_FIXTURE_SOURCE_FILE', root / 'source.txt')).read_text()
            send({'jsonrpc': '2.0', 'method': 'session/update', 'params': {
                'sessionId': session, 'update': {
                    'sessionUpdate': 'agent_message_chunk',
                    'content': {'type': 'text', 'text': output}}}})
            log('source_output', request_id=request_id, output=output)
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
        history = root / (hashlib.sha256(project.encode()).hexdigest() + '.sessions.json')
        if os.environ.get('ACP_FIXTURE_HTTP_CONTRACT') == '1' and method == 'session/load':
            known = json.loads(history.read_text()) if history.exists() else []
            if message['params']['sessionId'] not in known:
                log('load_error', requested_session=message['params']['sessionId'])
                send({'jsonrpc': '2.0', 'id': message['id'], 'error': {
                    'code': -32603, 'message': 'controlled fixture history does not contain this project session'}})
                continue
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
        if os.environ.get('ACP_FIXTURE_HTTP_CONTRACT') == '1' and method == 'session/new' and session not in ('', 'pending'):
            known = json.loads(history.read_text()) if history.exists() else []
            history.write_text(json.dumps(known + [session]))
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
