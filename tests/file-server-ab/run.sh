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
image_tag="pending"
docker_mirror="${AB_DOCKER_MIRROR:-}"
if [[ -n "${docker_mirror}" && "${docker_mirror}" != */ ]]; then
  docker_mirror="${docker_mirror}/"
fi
if [[ -n "${docker_mirror}" ]]; then
  default_node_image="${docker_mirror}library/node:22-trixie-slim"
  default_rust_builder_image="${docker_mirror}rust:trixie"
else
  default_node_image="node:22-trixie-slim"
  default_rust_builder_image="rust:trixie"
fi
node_image="${AB_NODE_IMAGE:-${default_node_image}}"
rust_builder_image="${AB_RUST_BUILDER_IMAGE:-${default_rust_builder_image}}"
# Compose profile validation runs before the image digest lookup. Use the selected
# image reference there; this value is replaced with the resolved digest before build.
export AB_NODE_IMAGE_DIGEST="${node_image}"
rust_port="${AB_RUST_PORT:-}"
ts_port="${AB_TS_PORT:-}"
pnpm_version="${AB_PNPM_VERSION:-10.34.5}"
pnpm_registry="${AB_PNPM_REGISTRY:-https://registry.npmmirror.com}"
pnpm_network_concurrency="${AB_PNPM_NETWORK_CONCURRENCY:-32}"
pnpm_lock_mode="${AB_PNPM_LOCK_MODE:-pinned}"
if [[ "${pnpm_lock_mode}" != "pinned" && "${pnpm_lock_mode}" != "source" ]]; then
  echo "AB_PNPM_LOCK_MODE must be 'pinned' or 'source'" >&2
  exit 2
fi
if [[ ! "${pnpm_network_concurrency}" =~ ^[1-9][0-9]{0,2}$ ]] || ((pnpm_network_concurrency > 128)); then
  echo "AB_PNPM_NETWORK_CONCURRENCY must be between 1 and 128" >&2
  exit 2
fi
host_uid="$(id -u)"
host_gid="$(id -g)"
docker_arch="$(docker info --format '{{.Architecture}}')"
case "${docker_arch}" in
  aarch64|arm64) docker_arch="arm64" ;;
  x86_64|amd64) docker_arch="amd64" ;;
esac
cache_platform="${DOCKER_DEFAULT_PLATFORM:-linux/${docker_arch}}"
cache_platform="${cache_platform//\//-}"
cache_platform="${cache_platform//:/-}"
volume_prefix="${AB_PNPM_CACHE_VOLUME_PREFIX:-rcoder-file-server-ab-pnpm}"
volume_prefix="$(printf '%s' "${volume_prefix}" | sed 's/[^A-Za-z0-9_.-]/-/g')"
volume_suffix="$(printf '%s-%s-u%s-g%s' "${pnpm_version}" "${cache_platform}" \
  "${host_uid}" "${host_gid}" | sed 's/[^A-Za-z0-9_.-]/-/g')"
pnpm_volume_base="${volume_prefix}-${volume_suffix}"
pnpm_rust_store_volume="${pnpm_volume_base}-rust-store"
pnpm_rust_metadata_cache_volume="${pnpm_volume_base}-rust-metadata"
pnpm_ts_store_volume="${pnpm_volume_base}-typescript-store"
pnpm_ts_metadata_cache_volume="${pnpm_volume_base}-typescript-metadata"
suite="${AB_SUITE:-core}"
report_dir="${report_root}/${run_id}"
keep="${AB_KEEP:-0}"
builder="${AB_BUILDER:-}"
phase="initialize"

export RCODER_ROOT="${repo_root}"
export AB_TS_SOURCE="${ts_source}"
export AB_TS_REF="${ts_ref}"
export AB_TS_CONTEXT="${ts_context}"
export AB_RUNTIME_DIR="${runtime_dir}"
export AB_PNPM_RUST_STORE_VOLUME="${pnpm_rust_store_volume}"
export AB_PNPM_RUST_METADATA_CACHE_VOLUME="${pnpm_rust_metadata_cache_volume}"
export AB_PNPM_TS_STORE_VOLUME="${pnpm_ts_store_volume}"
export AB_PNPM_TS_METADATA_CACHE_VOLUME="${pnpm_ts_metadata_cache_volume}"
export AB_RUST_PORT="${rust_port}"
export AB_TS_PORT="${ts_port}"
export AB_PNPM_VERSION="${pnpm_version}"
export AB_PNPM_REGISTRY="${pnpm_registry}"
export AB_PNPM_NETWORK_CONCURRENCY="${pnpm_network_concurrency}"
export AB_PNPM_LOCK_MODE="${pnpm_lock_mode}"
export AB_IMAGE_TAG="${image_tag}"
ts_image_ref="rcoder-file-server-ab-ts:${image_tag}"
export AB_TS_IMAGE_REF="${ts_image_ref}"
export AB_RUST_IMAGE_REF="rcoder-file-server-ab-rust:${image_tag}"
export AB_TS_DOCKERFILE="Dockerfile"
export AB_DOCKER_BUILDER="${builder:-docker-cli-selected}"
export AB_RUST_BUILDER_IMAGE="${rust_builder_image}"
export AB_REPORT_ROOT="${report_root}"
export AB_RUN_ID="${run_id}"
export AB_UID="${host_uid}"
export AB_GID="${host_gid}"

cleanup() {
  status=$?
  mkdir -p "${report_dir}/logs" || true
  if [[ "${AB_BUILD_ONLY:-0}" != "1" ]]; then
    docker compose -p "${project}" -f "${compose_file}" logs --no-color > "${report_dir}/logs/compose.log" 2>&1 || true
  fi
  if [[ "${status}" -ne 0 && ! -f "${report_dir}/summary.md" ]]; then
    # Conservative attribution: only registry/container/network phases are counted as
    # environment failures. An image build can fail from a source defect in either
    # implementation, so it keeps its own kind and requires reading logs/build.log;
    # the comparison phase is never auto-classified as an environment failure.
    failure_kind="setup"
    case "${phase}" in
      resolve-node-image|resolve-rust-builder-image|prepare-pnpm-volumes|start-containers|validate-pnpm-store|validate-pnpm-concurrency|validate-pnpm-registry|validate-runtime-home|validate-runtime-parity)
        failure_kind="environment"
        ;;
      build-image-set)
        failure_kind="image-build"
        ;;
      run-selected-suites)
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
    if [[ "${AB_BUILD_ONLY:-0}" != "1" ]]; then
      docker compose -p "${project}" -f "${compose_file}" down --remove-orphans --volumes >/dev/null 2>&1 || true
    fi
    # Reports are stored outside work_root; delete the generated TS snapshot
    # and runtime even when strict comparison exits non-zero. AB_KEEP=1 is the
    # explicit opt-in for retaining workspaces after a failed diff.
    rm -rf -- "${ts_context}" "${runtime_dir}"
    rmdir -- "${work_root}" 2>/dev/null || true
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
mkdir -p "${report_dir}/logs" "${runtime_dir}/rust" "${runtime_dir}/typescript" \
  "${runtime_dir}/fixtures"
if [[ "${AB_BUILD_ONLY:-0}" != "1" ]]; then
for volume_name in \
  "${pnpm_rust_store_volume}" "${pnpm_rust_metadata_cache_volume}" \
  "${pnpm_ts_store_volume}" "${pnpm_ts_metadata_cache_volume}"; do
  docker volume create "${volume_name}" >/dev/null
done
cp "${repo_root}/tmp/template/react-vite-template.zip" "${runtime_dir}/fixtures/react-vite-template.zip"
cp "${repo_root}/tmp/template/vue3-vite-template.zip" "${runtime_dir}/fixtures/vue3-vite-template.zip"
python3 "${script_dir}/prepare-fixtures.py" "${runtime_dir}/fixtures" \
  "${pnpm_registry}" "${pnpm_network_concurrency}" "${pnpm_lock_mode}"
for side in rust typescript; do
  mkdir -p "${runtime_dir}/${side}"/{project-workspace,project-zips,project-nginx,computer-workspace,userapp-workspace,logs/project,logs/computer,logs/file-server,cache/templates,cache/node-modules,init-project}
  cp "${runtime_dir}/fixtures/react-vite-template.zip" "${runtime_dir}/${side}/init-project/react-vite-template.zip"
  cp "${runtime_dir}/fixtures/vue3-vite-template.zip" "${runtime_dir}/${side}/init-project/vue3-vite-template.zip"
done

fi

phase="prepare-ts-source"
"${script_dir}/prepare-ts-context.sh"
ts_revision="$(cat "${ts_context}/.ab-source-revision")"

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

toolchain_tag="$({
  shasum -a 256 "${script_dir}/Dockerfile.base"
  printf '%s\n' "${rust_builder_digest:-${rust_builder_image}}" "${node_digest}" \
    "${pnpm_version}" "${pnpm_registry}" "${pnpm_network_concurrency}" \
    "${host_uid}" "${host_gid}" "${cache_platform}"
} | shasum -a 256 | cut -c 1-16)"
export AB_TOOLCHAIN_TAG="${toolchain_tag}"

phase="validate-compose-profile"
docker compose -p "${project}" -f "${compose_file}" config --format json \
  | python3 "${script_dir}/validate-compose-profile.py"

phase="capture-rust-source"
rust_source_before="$(rust_source_identity)"

phase="build-image-set"
builder_args=()
if [[ -n "${builder}" ]]; then
  builder_args=(--builder "${builder}")
fi
# Image identity is independent of run identity. Retain exact-source images so
# subsequent suites avoid even the BuildKit export/load step.
image_tag="$(printf '%s\n' "${rust_source_before}" "${toolchain_tag}" | shasum -a 256 | cut -c 1-32)"
ts_tag="$({ printf '%s\n' "${ts_revision}" "${toolchain_tag}"; shasum -a 256 "${script_dir}/Dockerfile.ts" "${script_dir}/prepare-ts-context.sh" "${script_dir}/docker-bake.hcl"; } | shasum -a 256 | cut -c 1-32)"
export AB_RUST_IMAGE_REF="rcoder-file-server-ab-rust:${image_tag}"
ts_image_ref="rcoder-file-server-ab-ts:${ts_tag}"
export AB_TS_IMAGE_REF="${ts_image_ref}"
missing_targets=()
for target in rust typescript; do
  image_ref="${AB_RUST_IMAGE_REF}"
  if [[ "${target}" == typescript ]]; then image_ref="${AB_TS_IMAGE_REF}"; fi
  if docker image inspect "${image_ref}" >/dev/null 2>&1; then
    echo "Reusing ${target} image ${image_ref}"
  else
    missing_targets+=("${target}")
  fi
done
if (( ${#missing_targets[@]} )); then
  docker buildx bake -f "${script_dir}/docker-bake.hcl" \
    "${builder_args[@]}" --provenance=false --progress plain --metadata-file "${report_dir}/build-metadata.json" "${missing_targets[@]}" 2>&1 | tee "${report_dir}/logs/build.log"
fi
# Pin runtime containers to immutable local IDs, including the driver.
rust_image_id="$(docker image inspect --format '{{.Id}}' "${AB_RUST_IMAGE_REF}")"
ts_image_id="$(docker image inspect --format '{{.Id}}' "${AB_TS_IMAGE_REF}")"
export AB_RUST_IMAGE_REF="${rust_image_id}"
export AB_TS_IMAGE_REF="${ts_image_id}"
printf 'rust=%s\ntypescript=%s\n' "${rust_image_id}" "${ts_image_id}" > "${report_dir}/images.txt"

phase="check-rust-source-stability"
rust_source_after="$(rust_source_identity)"
if [[ "${rust_source_before}" != "${rust_source_after}" ]]; then
  for target in "${missing_targets[@]}"; do
    if [[ "${target}" == rust ]]; then docker image rm "rcoder-file-server-ab-rust:${image_tag}" >/dev/null 2>&1 || true; fi
  done
  echo "RCoder source changed while the A/B image was building; no comparison was run." >&2
  exit 1
fi

if [[ "${AB_BUILD_ONLY:-0}" == "1" ]]; then
  phase="complete"
  echo "A/B images ready: ${report_dir}/images.txt"
  exit 0
fi

phase="prepare-pnpm-volumes"
for service in rust typescript; do
  docker compose -p "${project}" -f "${compose_file}" run --pull never --rm --no-deps -T \
  --user 0:0 --entrypoint /bin/sh "${service}" -ec '
    uid="$1"
    gid="$2"

    # Project workspace volumes are unique to this run and start empty, so
    # changing only their mount-point ownership is sufficient.
    chown "${uid}:${gid}" /data/project-workspace

    # PNPM cache volumes persist across runs and can contain hundreds of
    # thousands of files. Repair existing ownership once, then leave cache
    # files owned by the non-root service user and avoid a full-tree chown on
    # every A/B invocation.
    for cache_dir in /pnpm-cache /pnpm-metadata; do
      marker="${cache_dir}/.file-server-ab-owner-${uid}-${gid}"
      if [ ! -f "${marker}" ]; then
        chown -R "${uid}:${gid}" "${cache_dir}"
        : > "${marker}"
        chown "${uid}:${gid}" "${marker}"
        echo "Initialized ownership for ${cache_dir} (${uid}:${gid})"
      else
        echo "Reusing ownership for ${cache_dir} (${uid}:${gid})"
      fi
    done
  ' sh "${AB_UID}" "${AB_GID}"
done

phase="start-containers"
# The preceding build step is authoritative. Prevent `up` from rebuilding the
# Compose build graph (which would rebuild/export the shared toolchain again).
docker compose -p "${project}" -f "${compose_file}" up --no-build --pull never -d --wait rust typescript
phase="validate-pnpm-store"
for service in rust typescript; do
  configured_store="$(docker compose -p "${project}" -f "${compose_file}" exec -T "${service}" pnpm config get store-dir | tr -d '\r')"
  resolved_store="$(docker compose -p "${project}" -f "${compose_file}" exec -T "${service}" pnpm store path | tr -d '\r')"
  if [[ "${configured_store}" != "/pnpm-cache" || "${resolved_store}" != /pnpm-cache/* ]]; then
    echo "${service} pnpm is not using the mounted persistent store (configured=${configured_store}, resolved=${resolved_store})" >&2
    exit 1
  fi
done
phase="validate-pnpm-concurrency"
for service in rust typescript; do
  configured_concurrency="$(docker compose -p "${project}" -f "${compose_file}" exec -T "${service}" pnpm config get network-concurrency | tr -d '\r')"
  if [[ "${configured_concurrency}" != "${pnpm_network_concurrency}" ]]; then
    echo "${service} pnpm is not using configured network concurrency ${pnpm_network_concurrency} (got ${configured_concurrency})" >&2
    exit 1
  fi
done
phase="validate-pnpm-registry"
for service in rust typescript; do
  configured_registry="$(docker compose -p "${project}" -f "${compose_file}" exec -T "${service}" pnpm config get registry | tr -d '\r')"
  normalize_registry() { printf '%s' "$1" | sed 's:/*$::'; }
  if [[ "$(normalize_registry "${configured_registry}")" != "$(normalize_registry "${pnpm_registry}")" ]]; then
    echo "${service} pnpm is not using the configured A/B registry" >&2
    exit 1
  fi
done
phase="validate-runtime-home"
for service in rust typescript; do
  node_home="$(docker compose -p "${project}" -f "${compose_file}" exec -T "${service}" node -p 'require("node:os").homedir()' | tr -d '\r')"
  if [[ -z "${node_home}" ]] || ! docker compose -p "${project}" -f "${compose_file}" exec -T "${service}" node -e 'const fs = require("node:fs"); const os = require("node:os"); if (!fs.statSync(os.homedir()).isDirectory()) process.exit(1)'; then
    echo "${service} Node runtime has no resolvable home directory (homedir=${node_home})" >&2
    exit 1
  fi
done
rust_mapping="$(docker compose -p "${project}" -f "${compose_file}" port rust 60000)"
ts_mapping="$(docker compose -p "${project}" -f "${compose_file}" port typescript 60000)"
export AB_RUST_HOST_URL="http://${rust_mapping}"
export AB_TS_HOST_URL="http://${ts_mapping}"
echo "Rust API (host): ${AB_RUST_HOST_URL}"
echo "TypeScript API (host): ${AB_TS_HOST_URL}"

export AB_RUST_SOURCE="${rust_source_before}"
export AB_TS_SOURCE="${ts_revision}"
rust_image_id="$(docker image inspect --format '{{.Id}}' "rcoder-file-server-ab-rust:${image_tag}")"
ts_image_id="$(docker image inspect --format '{{.Id}}' "${ts_image_ref}")"
toolchain_image_id="recipe:${toolchain_tag}"
export AB_RUST_IMAGE="${rust_image_id} (toolchain=${toolchain_image_id}, rust=${rust_builder_digest:-${rust_builder_image}}, node=${node_digest})"
export AB_TS_IMAGE="${ts_image_id} (toolchain=${toolchain_image_id})"
export AB_RUST_NODE_VERSION="$(docker compose -p "${project}" -f "${compose_file}" exec -T rust node --version)"
export AB_TS_NODE_VERSION="$(docker compose -p "${project}" -f "${compose_file}" exec -T typescript node --version)"
export AB_RUST_NODE_ARCH="$(docker compose -p "${project}" -f "${compose_file}" exec -T rust node -p 'process.arch')"
export AB_TS_NODE_ARCH="$(docker compose -p "${project}" -f "${compose_file}" exec -T typescript node -p 'process.arch')"
export AB_RUST_PNPM_VERSION="$(docker compose -p "${project}" -f "${compose_file}" exec -T rust pnpm --version)"
export AB_TS_PNPM_VERSION="$(docker compose -p "${project}" -f "${compose_file}" exec -T typescript pnpm --version)"
export AB_RUST_GIT_VERSION="$(docker compose -p "${project}" -f "${compose_file}" exec -T rust git --version)"
export AB_TS_GIT_VERSION="$(docker compose -p "${project}" -f "${compose_file}" exec -T typescript git --version)"

phase="validate-runtime-parity"
for runtime_property in NODE_VERSION NODE_ARCH PNPM_VERSION GIT_VERSION; do
  rust_name="AB_RUST_${runtime_property}"
  ts_name="AB_TS_${runtime_property}"
  rust_value="${!rust_name}"
  ts_value="${!ts_name}"
  if [[ "${rust_value}" != "${ts_value}" ]]; then
    echo "Runtime mismatch for ${runtime_property}: Rust=${rust_value}, TypeScript=${ts_value}" >&2
    exit 1
  fi
done
phase="run-selected-suites"
docker compose -p "${project}" -f "${compose_file}" run --pull never --rm --no-deps -T \
	 -e "AB_DOCKER_BUILDER=${AB_DOCKER_BUILDER}" \
	 -e "AB_RUST_SOURCE=${rust_source_before}" \
	 -e "AB_TS_SOURCE=${ts_revision}" \
	 -e "AB_RUST_HOST_URL=${AB_RUST_HOST_URL}" \
	 -e "AB_TS_HOST_URL=${AB_TS_HOST_URL}" \
	 -e "AB_RUST_IMAGE=${rust_image_id} (toolchain=${toolchain_image_id}, rust=${rust_builder_digest:-${rust_builder_image}}, node=${node_digest})" \
	 -e "AB_TS_IMAGE=${ts_image_id} (toolchain=${toolchain_image_id})" \
	 -e "AB_RUST_NODE_VERSION=${AB_RUST_NODE_VERSION}" \
	 -e "AB_TS_NODE_VERSION=${AB_TS_NODE_VERSION}" \
	 -e "AB_RUST_NODE_ARCH=${AB_RUST_NODE_ARCH}" \
	 -e "AB_TS_NODE_ARCH=${AB_TS_NODE_ARCH}" \
	 -e "AB_RUST_PNPM_VERSION=${AB_RUST_PNPM_VERSION}" \
	 -e "AB_TS_PNPM_VERSION=${AB_TS_PNPM_VERSION}" \
	 -e "AB_RUST_GIT_VERSION=${AB_RUST_GIT_VERSION}" \
	 -e "AB_TS_GIT_VERSION=${AB_TS_GIT_VERSION}" \
	 driver run --suite "${suite}" --run-id "${run_id}" \
	 --rules "/ab-input/diff-rules.json" \
	 --route-coverage "/ab-input/route-coverage.json" \
	 --rust-url "http://rust:60000" \
	 --ts-url "http://typescript:60000" \
	 --rust-dev-url "http://rust" \
	 --ts-dev-url "http://typescript" \
	 --rust-root "/ab-runtime/rust" \
	 --ts-root "/ab-runtime/typescript" \
	 --fixtures "/ab-fixtures" \
	 --report-root "/ab-reports"

phase="complete"
echo "A/B report: ${report_dir}"
