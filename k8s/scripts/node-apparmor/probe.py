import os, subprocess, time
p = subprocess.Popen(["sleep", "5"])
time.sleep(0.3)
try:
    os.kill(p.pid, 15)
    print("SIGNAL_OK")
except PermissionError:
    print("SIGNAL_DENIED")
