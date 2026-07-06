#!/bin/bash
set -e

CHROME_BIN="${CHROME_BIN:-chromium}"

echo "[start] launching headless chromium on 127.0.0.1:9222 ..."
"$CHROME_BIN" \
  --headless=new \
  --remote-debugging-port=9222 \
  --remote-debugging-address=127.0.0.1 \
  --no-sandbox \
  --disable-dev-shm-usage \
  --disable-gpu \
  --no-first-run \
  --no-default-browser-check \
  --hide-scrollbars \
  --user-data-dir=/tmp/chrome-canvas-poc \
  about:blank &
CHROME_PID=$!

# 容器退出时顺手收掉 chrome
trap 'kill -TERM $CHROME_PID 2>/dev/null || true' EXIT INT TERM

echo "[start] waiting for chrome :9222 to be ready ..."
READY=0
for i in $(seq 1 60); do
  if curl -sf http://127.0.0.1:9222/json/version >/dev/null 2>&1; then
    READY=1
    echo "[start] chrome ready (probe #$i)"
    break
  fi
  sleep 0.5
done
if [ "$READY" != "1" ]; then
  echo "[start] ERROR: chrome did not become ready on :9222" >&2
  exit 1
fi

echo "[start] starting viewer-relay on :9223 ..."
exec node /app/relay/viewer-relay.js
