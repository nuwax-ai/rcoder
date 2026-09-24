#!/usr/bin/env bash
set -euo pipefail
umask 077

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
repo_root="$(cd "${script_dir}/../.." && pwd)"
cd "${repo_root}"
ts_source="${AB_TS_SOURCE:-/Users/soddy/Documents/git-workspace/nuwax-file-server}"
ts_ref="${AB_TS_REF:-HEAD}"
report_root="${AB_REPORT_ROOT:-${repo_root}/tests-e2e/reports/file-server-ab}"
run_id="$(date -u +%Y%m%dT%H%M%SZ | tr '[:upper:]' '[:lower:]')-$(python3 -c 'import uuid; print(uuid.uuid4().hex[:8])')"
work_root="${AB_WORK_ROOT:-${repo_root}/target/file-server-ab}/${run_id}"
runtime_dir="${work_root}/runtime"
ts_context="${work_root}/ts-context"
compose_file="${script_dir}/compose.yaml"
project="file-server-ab-${run_id}"
image_tag="${run_id}"
node_image="${AB_NODE_IMAGE:-node:22-bookworm-slim}"
rust_builder_image="${AB_RUST_BUILDER_IMAGE:-rust:1.98-bookworm}"
rust_port="${AB_RUST_PORT:-}"
ts_port="${AB_TS_PORT:-}"
pnpm_version="${AB_PNPM_VERSION:-10.34.5}"
report_dir="${report_root}/${run_id}"
keep="${AB_KEEP:-0}"
phase="initialize"

export RCODER_ROOT="${repo_root}"
export AB_TS_SOURCE="${ts_source}"
export AB_TS_REF="${ts_ref}"
export AB_TS_CONTEXT="${ts_context}"
export AB_RUNTIME_DIR="${runtime_dir}"
export AB_RUST_PORT="${rust_port}"
export AB_TS_PORT="${ts_port}"
export AB_PNPM_VERSION="${pnpm_version}"
export AB_IMAGE_TAG="${image_tag}"
export AB_RUST_BUILDER_IMAGE="${rust_builder_image}"
export AB_REPORT_ROOT="${report_root}"
export AB_RUN_ID="${run_id}"

cleanup() {
  status=$?
  mkdir -p "${report_dir}/logs" || true
  docker compose -p "${project}" -f "${compose_file}" logs --no-color > "${report_dir}/logs/compose.log" 2>&1 || true
  if [[ "${status}" -ne 0 && ! -f "${report_dir}/summary.md" ]]; then
    failure_kind="setup"
    case "${phase}" in
      resolve-node-image|resolve-rust-builder-image|build-rust-image|build-ts-image|start-containers)
        failure_kind="environment"
        ;;
      run-core-suite)
        failure_kind="comparison-runner"
        ;;
    esac
    cat > "${report_dir}/runner-failure.json" <<EOF
{"phase":"${phase}","failure_kind":"${failure_kind}","exit_code":${status}}
EOF
    cat > "${report_dir}/summary.md" <<EOF
# File-server A/B run ${run_id}

Runner failed during phase ${phase} with exit code ${status} (failure kind: ${failure_kind}). No comparison result was produced. See runner-failure.json and logs/compose.log.
EOF
  fi
  if [[ "${keep}" != "1" ]]; then
    docker compose -p "${project}" -f "${compose_file}" down --remove-orphans --rmi local >/dev/null 2>&1 || true
    docker image rm -f "rcoder-file-server-ab-rust:${image_tag}" "rcoder-file-server-ab-ts:${image_tag}" >/dev/null 2>&1 || true
    rm -rf -- "${ts_context}"
    if [[ "${status}" -eq 0 ]]; then
      rm -rf -- "${runtime_dir}"
    fi
  else
    echo "Kept A/B containers and runtime; project=${project} runtime=${runtime_dir}" >&2
  fi
  return "${status}"
}
trap cleanup EXIT

rust_source_identity() {
  local revision dirty
  revision="$(git -C "${repo_root}" rev-parse HEAD)"
  dirty="$( { git -C "${repo_root}" diff --binary HEAD; git -C "${repo_root}" ls-files --others --exclude-standard -z | xargs -0 shasum -a 256; } | shasum -a 256 | awk '{print $1}')"
  printf '%s dirty=%s' "${revision}" "${dirty}"
}

phase="prepare-fixtures"
mkdir -p "${report_dir}/logs" "${runtime_dir}/rust" "${runtime_dir}/typescript"
for side in rust typescript; do
  mkdir -p "${runtime_dir}/${side}"/{project-workspace,project-zips,project-nginx,computer-workspace,userapp-workspace,logs/project,logs/computer,logs/file-server,cache/templates,cache/node-modules,init-project}
  cp "${repo_root}/tmp/template/react-vite-template.zip" "${runtime_dir}/${side}/init-project/react-vite-template.zip"
  cp "${repo_root}/tmp/template/vue3-vite-template.zip" "${runtime_dir}/${side}/init-project/vue3-vite-template.zip"
done

phase="prepare-ts-source"
"${script_dir}/prepare-ts-context.sh"

phase="resolve-node-image"
if ! docker image inspect "${node_image}" >/dev/null 2>&1; then
  docker pull "${node_image}"
fi
node_digest="$(docker image inspect --format '{{index .RepoDigests 0}}' "${node_image}")"
if [[ -z "${node_digest}" || "${node_digest}" == "<no value>" ]]; then
  echo "Could not resolve a registry digest for ${node_image}" >&2
  exit 1
fi
export AB_NODE_IMAGE_DIGEST="${node_digest}"

phase="resolve-rust-builder-image"
if ! docker image inspect "${rust_builder_image}" >/dev/null 2>&1; then
  docker pull "${rust_builder_image}"
fi
rust_builder_digest="$(docker image inspect --format '{{index .RepoDigests 0}}' "${rust_builder_image}")"
if [[ -n "${rust_builder_digest}" && "${rust_builder_digest}" != "<no value>" ]]; then
  export AB_RUST_BUILDER_IMAGE="${rust_builder_digest}"
fi

phase="capture-rust-source"
rust_source_before="$(rust_source_identity)"

phase="build-rust-image"
docker compose -p "${project}" -f "${compose_file}" build --pull=false rust
phase="build-ts-image"
docker compose -p "${project}" -f "${compose_file}" build --pull=false typescript

phase="check-rust-source-stability"
rust_source_after="$(rust_source_identity)"
if [[ "${rust_source_before}" != "${rust_source_after}" ]]; then
  echo "RCoder source changed while the A/B image was building; no comparison was run." >&2
  exit 1
fi

phase="start-containers"
docker compose -p "${project}" -f "${compose_file}" up -d --wait
rust_mapping="$(docker compose -p "${project}" -f "${compose_file}" port rust 60000)"
ts_mapping="$(docker compose -p "${project}" -f "${compose_file}" port typescript 60000)"
rust_host_port="${rust_mapping##*:}"
ts_host_port="${ts_mapping##*:}"

ts_revision="$(cat "${ts_context}/.ab-source-revision")"
export AB_RUST_SOURCE="${rust_source_before}"
export AB_TS_SOURCE="${ts_revision}"
rust_image_id="$(docker image inspect --format '{{.Id}}' "rcoder-file-server-ab-rust:${image_tag}")"
ts_image_id="$(docker image inspect --format '{{.Id}}' "rcoder-file-server-ab-ts:${image_tag}")"
export AB_RUST_IMAGE="${rust_image_id} (builder=${rust_builder_digest:-${rust_builder_image}}, runtime=${node_digest})"
export AB_TS_IMAGE="${ts_image_id} (runtime=${node_digest})"
export AB_NODE_VERSION="$(docker compose -p "${project}" -f "${compose_file}" exec -T rust node --version)"
export AB_NODE_ARCH="$(docker compose -p "${project}" -f "${compose_file}" exec -T rust node -p 'process.arch')"
export AB_PNPM_VERSION="$(docker compose -p "${project}" -f "${compose_file}" exec -T rust pnpm --version)"
export AB_GIT_VERSION="$(docker compose -p "${project}" -f "${compose_file}" exec -T rust git --version)"

phase="run-core-suite"
cargo run -p file-server-ab -- run --run-id "${run_id}" \
	 --rules "${repo_root}/tests/file-server-ab/diff-rules.json" \
	 --rust-url "http://127.0.0.1:${rust_host_port}" \
	 --ts-url "http://127.0.0.1:${ts_host_port}" \
  --rust-root "${runtime_dir}/rust" \
  --ts-root "${runtime_dir}/typescript" \
  --fixtures "${repo_root}/tmp/template" \
  --report-root "${report_root}"

phase="complete"
echo "A/B report: ${report_dir}"
