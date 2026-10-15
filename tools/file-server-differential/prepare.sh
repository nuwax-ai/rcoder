#!/usr/bin/env bash

set -euo pipefail
source "$(cd "$(dirname "$0")" && pwd)/lib.sh"

require_ts_source

require_command node
require_command cargo
require_command curl
require_command git

case "${DIFF_RUNTIME}" in
  "${DIFF_TOOL_DIR}"/runtime) ;;
  *) echo "refusing to reset unexpected runtime path: ${DIFF_RUNTIME}" >&2; exit 1 ;;
esac
rm -rf "${DIFF_RUNTIME}"

for implementation in ts rust; do
  root="$(service_root "${implementation}")"
  mkdir -p "${root}/project_init" "${root}/project_workspace" \
    "${root}/project_zips" "${root}/project_nginx" \
    "${root}/computer-project-workspace" "${root}/logs/project_logs" \
    "${root}/logs/computer_logs" "${root}/cache/templates" \
    "${root}/cache/node-modules"
  cp "${RCODER_ROOT}/tmp/template/react-vite-template.zip" "${root}/project_init/"
  cp "${RCODER_ROOT}/tmp/template/vue3-vite-template.zip" "${root}/project_init/"
done
mkdir -p "${DIFF_RUNTIME}/report"
echo "differential runtime prepared at ${DIFF_RUNTIME}"
