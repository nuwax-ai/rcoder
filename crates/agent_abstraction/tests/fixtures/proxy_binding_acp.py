#!/usr/bin/env python3
"""ACP protocol fixture only: no network and no AI responses."""
import json
import os
import sys
import time
from pathlib import Path

root = Path(os.environ["ACP_PROXY_FIXTURE_DIR"])
session = "binding-session-" + str(os.getpid())


def log(event):
    with (root / "events.jsonl").open("a") as output:
        output.write(json.dumps({"event": event, "pid": os.getpid()}) + "\n")


def reply(request, result):
    print(json.dumps({"jsonrpc": "2.0", "id": request["id"], "result": result}), flush=True)


log("spawn")
for line in sys.stdin:
    request = json.loads(line)
    method = request.get("method")
    if method == "initialize":
        reply(request, {"protocolVersion": 1, "agentCapabilities": {"loadSession": True}})
    elif method in ("session/new", "session/load"):
        log("session_request")
        while (root / "hold-new").exists() and not (root / "release-new").exists():
            time.sleep(0.01)
        if method == "session/load":
            session = request["params"]["sessionId"]
        reply(request, {"sessionId": session})
    elif method == "session/prompt":
        log("prompt")
        reply(request, {"stopReason": "end_turn"})
    elif "id" in request:
        reply(request, {})
