#!/usr/bin/env bash

set -euo pipefail

DIFF_TOOL_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
RCODER_ROOT="$(cd "${DIFF_TOOL_DIR}/../.." && pwd)"
DIFF_RUNTIME="${DIFF_TOOL_DIR}/runtime"
TS_SERVER_ROOT="${TS_SERVER_ROOT:-/Users/soddy/Documents/git-workspace/nuwax-file-server}"
TS_PORT="${TS_PORT:-61000}"
RUST_PORT="${RUST_PORT:-61001}"
DIFF_CARGO_TARGET="${DIFF_CARGO_TARGET:-/tmp/rcoder-file-server-target}"

export DIFF_TOOL_DIR RCODER_ROOT DIFF_RUNTIME TS_SERVER_ROOT TS_PORT RUST_PORT DIFF_CARGO_TARGET

require_command() {
  if ! command -v "$1" >/dev/null 2>&1; then
    echo "required command not found: $1" >&2
    exit 1
  fi
}

service_root() { printf '%s/%s' "${DIFF_RUNTIME}" "$1"; }

wait_for_health() {
  local name="$1" port="$2" attempt
  local pid_file="${DIFF_RUNTIME}/${name}/server.pid"
  for attempt in $(seq 1 60); do
    if [[ -f "${pid_file}" ]] && ! kill -0 "$(<"${pid_file}")" 2>/dev/null; then
      echo "${name} exited before becoming healthy" >&2
      tail -n 80 "${DIFF_RUNTIME}/${name}/logs/server.log" >&2 || true
      exit 1
    fi
    if curl --silent --fail --max-time 2 "http://127.0.0.1:${port}/health" >/dev/null; then return 0; fi
    sleep 1
  done
  echo "${name} health check timed out on port ${port}" >&2
  tail -n 80 "${DIFF_RUNTIME}/${name}/logs/server.log" >&2 || true
  exit 1
}
