#!/usr/bin/env bash

set -euo pipefail
source "$(cd "$(dirname "$0")" && pwd)/lib.sh"

[[ -d "${DIFF_RUNTIME}/ts/project_init" ]] || { echo "runtime is not prepared; run prepare.sh first" >&2; exit 1; }
"${DIFF_TOOL_DIR}/stop.sh" >/dev/null 2>&1 || true

launch_with_env() {
  local implementation="$1" port_key="$2" port="$3" log_file="$4"
  shift 4
  local root
  root="$(service_root "${implementation}")"
  env \
    NODE_ENV=production \
    DEPLOYMENT_MODE=docker-compose \
    INIT_PROJECT_NAME_REACT=react-vite-template \
    INIT_PROJECT_NAME_VUE3=vue3-vite-template \
    INIT_PROJECT_DIR="${root}/project_init" \
    PROJECT_SOURCE_DIR="${root}/project_workspace" \
    UPLOAD_PROJECT_DIR="${root}/project_zips" \
    DIST_TARGET_DIR="${root}/project_nginx" \
    LOG_BASE_DIR="${root}/logs/project_logs" \
    COMPUTER_WORKSPACE_DIR="${root}/computer-project-workspace" \
    COMPUTER_LOG_DIR="${root}/logs/computer_logs" \
    FILE_SERVER_LOG_DIR="${root}/logs/file-server" \
    TEMPLATE_CACHE_DIR="${root}/cache/templates" \
    NODE_MODULES_LOCAL_DIR="${root}/cache/node-modules" \
    GIT_ENABLED=true \
    GIT_DEFAULT_AUTHOR_NAME="Nuwax File Server" \
    GIT_DEFAULT_AUTHOR_EMAIL=git@nuwax.com \
    PNPM_PRUNE_ENABLED=false \
    LOG_CONSOLE_ENABLED=true \
    RUST_LOG=file_server=info \
    "${port_key}=${port}" \
    "$@" >"${log_file}" 2>&1 &
}

echo "building Rust file-server..."
CARGO_TARGET_DIR="${DIFF_CARGO_TARGET}" CARGO_INCREMENTAL=0 \
  cargo build --quiet -p file-server --bin file-server --manifest-path "${RCODER_ROOT}/Cargo.toml"

launch_with_env ts PORT "${TS_PORT}" "${DIFF_RUNTIME}/ts/logs/server.log" \
  node "${TS_SERVER_ROOT}/src/server.js"
echo "$!" >"${DIFF_RUNTIME}/ts/server.pid"

launch_with_env rust FILE_SERVER_PORT "${RUST_PORT}" "${DIFF_RUNTIME}/rust/logs/server.log" \
  "${DIFF_CARGO_TARGET}/debug/file-server"
echo "$!" >"${DIFF_RUNTIME}/rust/server.pid"

wait_for_health ts "${TS_PORT}"
wait_for_health rust "${RUST_PORT}"
echo "TS and Rust services are healthy on ${TS_PORT} and ${RUST_PORT}"
