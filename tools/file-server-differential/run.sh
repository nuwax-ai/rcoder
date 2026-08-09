#!/usr/bin/env bash

set -euo pipefail
TOOL_DIR="$(cd "$(dirname "$0")" && pwd)"
cleanup() { "${TOOL_DIR}/stop.sh" || true; }
trap cleanup EXIT INT TERM
"${TOOL_DIR}/prepare.sh"
"${TOOL_DIR}/start.sh"
node "${TOOL_DIR}/test.mjs"
