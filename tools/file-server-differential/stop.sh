#!/usr/bin/env bash

set -euo pipefail
source "$(cd "$(dirname "$0")" && pwd)/lib.sh"

for implementation in ts rust; do
  pid_file="${DIFF_RUNTIME}/${implementation}/server.pid"
  [[ -f "${pid_file}" ]] || continue
  pid="$(<"${pid_file}")"
  if [[ "${pid}" =~ ^[0-9]+$ ]] && kill -0 "${pid}" 2>/dev/null; then
    kill "${pid}" 2>/dev/null || true
    for _ in $(seq 1 30); do kill -0 "${pid}" 2>/dev/null || break; sleep 0.1; done
    kill -9 "${pid}" 2>/dev/null || true
  fi
  rm -f "${pid_file}"
done
