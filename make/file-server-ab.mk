# Isolated black-box comparison of the embedded Rust file-server and TS baseline.
AB_TS_SOURCE ?= /Users/soddy/Documents/git-workspace/nuwax-file-server
AB_TS_REF ?= HEAD
AB_REPORT_ROOT ?= $(CURDIR)/tests-e2e/reports/file-server-ab
AB_PNPM_CACHE_VOLUME_PREFIX ?= rcoder-file-server-ab-pnpm
AB_PNPM_VERSION ?= 10.34.5
AB_PNPM_REGISTRY ?= https://registry.npmmirror.com
AB_PNPM_NETWORK_CONCURRENCY ?= 32
AB_SUITE ?= core
AB_RUST_PORT ?=
AB_TS_PORT ?=
AB_KEEP ?= 0
AB_BUILDER ?=
AB_DOCKER_MIRROR ?= $(DOCKER_MIRROR)
ifeq ($(strip $(AB_DOCKER_MIRROR)),)
AB_DOCKER_MIRROR := $(shell sed -n 's/^DOCKER_MIRROR=//p' .env.local 2>/dev/null | head -n 1)
endif

.PHONY: file-server-ab-build file-server-ab file-server-ab-doctor file-server-ab-help

file-server-ab-build: export AB_BUILD_ONLY=1
file-server-ab-build: file-server-ab

file-server-ab:
	@AB_TS_SOURCE="$(AB_TS_SOURCE)" AB_TS_REF="$(AB_TS_REF)" \
	 AB_REPORT_ROOT="$(AB_REPORT_ROOT)" AB_PNPM_CACHE_VOLUME_PREFIX="$(AB_PNPM_CACHE_VOLUME_PREFIX)" \
	 AB_RUST_PORT="$(AB_RUST_PORT)" \
	 AB_TS_PORT="$(AB_TS_PORT)" AB_PNPM_VERSION="$(AB_PNPM_VERSION)" \
	 AB_PNPM_REGISTRY="$(AB_PNPM_REGISTRY)" \
	 AB_PNPM_NETWORK_CONCURRENCY="$(AB_PNPM_NETWORK_CONCURRENCY)" \
	 AB_DOCKER_MIRROR="$(AB_DOCKER_MIRROR)" \
	 AB_BUILDER="$(AB_BUILDER)" \
	 AB_SUITE="$(AB_SUITE)" \
	 AB_KEEP="$(AB_KEEP)" \
	 tests/file-server-ab/run.sh

file-server-ab-doctor:
	@test -n "$(RUST_URL)" -a -n "$(TS_URL)" || (echo "usage: make file-server-ab-doctor RUST_URL=http://... TS_URL=http://..." >&2; exit 2)
	@cargo run -p file-server-ab -- doctor \
	 --rust-url "$(RUST_URL)" \
	 --ts-url "$(TS_URL)"

file-server-ab-help:
	@echo "make file-server-ab [AB_SUITE=core|git|build|all] [AB_TS_SOURCE=/path/to/nuwax-file-server] [AB_TS_REF=HEAD] [AB_PNPM_VERSION=10.34.5] [AB_PNPM_REGISTRY=https://registry.npmmirror.com] [AB_PNPM_NETWORK_CONCURRENCY=32] [AB_PNPM_CACHE_VOLUME_PREFIX=rcoder-file-server-ab-pnpm] [AB_BUILDER=buildx-builder] [DOCKER_MIRROR=registry-prefix/]"
	@echo "  DOCKER_MIRROR can also be set in .env.local; it only selects local image pull/build sources."
	@echo "  core: offline API/file behavior; git: gix-vs-system-Git API; build: template install/build/dev lifecycle; all: all three."
	@echo "  Prefer the active local Docker builder; set AB_BUILDER only when another build occupies it (docker-container builders may export large OCI images)."
	@echo "  The Rust and TypeScript APIs independently install template dependencies from the configured registry; no template install/prefetch runs before the comparison."
	@echo "  Each implementation has its own persistent pnpm content and metadata Docker volumes, isolated by pnpm version/platform/host uid/gid; set AB_PNPM_CACHE_VOLUME_PREFIX to choose the volume-name prefix."
	@echo "  Project workspaces and pnpm stores use Docker volumes to avoid slow macOS host-bind small-file I/O; template node_modules are installed only by the tested APIs."
	@echo "  Builds one shared Rust+Node+pnpm toolchain image, then isolated Rust/TS services and evidence under tests-e2e/reports/file-server-ab/."
	@echo "  AB_KEEP=1 retains containers and workspaces after the run; AB_RUST_PORT/AB_TS_PORT override local ports."
