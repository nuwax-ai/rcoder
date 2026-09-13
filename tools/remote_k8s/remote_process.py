"""Heartbeat-bound remote process groups: SSH cancellation must not orphan builds."""
import json
import os
from pathlib import Path
import shlex
import subprocess
import time

WORKER = r'''
import fcntl,json,os,select,signal,subprocess,sys,time
ident,token,command=sys.argv[1],sys.argv[2],json.loads(sys.argv[3])
base='/tmp/rcoder-remote-k8s-'+ident
operation=os.fdopen(os.open(base+'.operation',os.O_CREAT|os.O_RDWR|os.O_NOFOLLOW,0o600),'w')
fcntl.flock(operation,fcntl.LOCK_EX|fcntl.LOCK_NB)
def check_owner():
 with open(base+'.active') as stream: owner=json.load(stream)
 if owner['token']!=token: raise RuntimeError('Environment ownership changed')
 os.kill(owner['pid'],0)
check_owner()
p=subprocess.Popen(command,start_new_session=True,stdin=subprocess.DEVNULL)
last=time.monotonic()
try:
 while p.poll() is None:
  if select.select([sys.stdin],[],[],1)[0]:
   if not os.read(sys.stdin.fileno(),4096): raise RuntimeError('SSH client disconnected')
   last=time.monotonic()
  if time.monotonic()-last>35: raise RuntimeError('Client heartbeat expired')
  check_owner()
 sys.exit(p.returncode)
finally:
 if p.poll() is None:
  os.killpg(p.pid,signal.SIGTERM)
  try:p.wait(timeout=10)
  except subprocess.TimeoutExpired:os.killpg(p.pid,signal.SIGKILL);p.wait()
'''


def execute(c, args, budget, log):
    command = shlex.join(['python3', '-c', WORKER, c.id, c.lock_token, json.dumps(args)])
    ssh = ['ssh', '-o', 'BatchMode=yes', '-o', 'ConnectTimeout=10', '-o', 'ServerAliveInterval=10',
           '-o', 'ServerAliveCountMax=2', c.host, command]
    log.parent.mkdir(parents=True, exist_ok=True)
    print('Log: ' + str(log), flush=True)
    with log.open('w') as stream:
        log.chmod(0o600)
        p = subprocess.Popen(ssh, stdin=subprocess.PIPE, stdout=stream, stderr=subprocess.STDOUT)
        deadline = time.monotonic() + budget
        heartbeat = 0
        try:
            while p.poll() is None:
                if c.lock_process.poll() is not None:
                    raise RuntimeError('Environment lock disconnected')
                if time.monotonic() >= deadline:
                    raise TimeoutError('Remote build deadline exceeded')
                if time.monotonic() >= heartbeat:
                    p.stdin.write(b'alive\n')
                    p.stdin.flush()
                    heartbeat = time.monotonic() + 5
                time.sleep(.2)
            if p.returncode:
                raise RuntimeError('Remote build failed; see ' + str(log) + '\n' + log.read_text()[-1600:])
        finally:
            try:p.stdin.close()
            except BrokenPipeError:pass
            try:p.wait(timeout=15)
            except subprocess.TimeoutExpired:
                p.terminate()
                try:p.wait(timeout=5)
                except subprocess.TimeoutExpired:p.kill();p.wait()
