# Isolated black-box comparison of the embedded Rust file-server and TS baseline.
AB_TS_SOURCE ?= /Users/soddy/Documents/git-workspace/nuwax-file-server
AB_TS_REF ?= HEAD
AB_REPORT_ROOT ?= $(CURDIR)/tests-e2e/reports/file-server-ab
AB_PNPM_VERSION ?= 10.34.5
AB_SUITE ?= core
AB_RUST_PORT ?=
AB_TS_PORT ?=
AB_KEEP ?= 0
AB_BUILDER ?=
AB_DOCKER_MIRROR ?= $(DOCKER_MIRROR)
ifeq ($(strip $(AB_DOCKER_MIRROR)),)
AB_DOCKER_MIRROR := $(shell sed -n 's/^DOCKER_MIRROR=//p' .env.local 2>/dev/null | head -n 1)
endif

.PHONY: file-server-ab file-server-ab-doctor file-server-ab-help

file-server-ab:
	@AB_TS_SOURCE="$(AB_TS_SOURCE)" AB_TS_REF="$(AB_TS_REF)" \
	 AB_REPORT_ROOT="$(AB_REPORT_ROOT)" AB_RUST_PORT="$(AB_RUST_PORT)" \
	 AB_TS_PORT="$(AB_TS_PORT)" AB_PNPM_VERSION="$(AB_PNPM_VERSION)" \
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
	@echo "make file-server-ab [AB_SUITE=core|git|build|all] [AB_TS_SOURCE=/path/to/nuwax-file-server] [AB_TS_REF=HEAD] [AB_PNPM_VERSION=10.34.5] [AB_BUILDER=buildx-builder] [DOCKER_MIRROR=registry-prefix/]"
	@echo "  DOCKER_MIRROR can also be set in .env.local; it only selects local image pull/build sources."
	@echo "  core: offline API/file behavior; git: gix-vs-system-Git API; build: template install/build/dev lifecycle; all: all three."
	@echo "  AB_BUILDER optionally selects a Docker Buildx builder when another local build is using the default builder."
	@echo "  Builds isolated Rust/TS Docker services and writes evidence under tests-e2e/reports/file-server-ab/."
	@echo "  AB_KEEP=1 retains containers and workspaces after the run; AB_RUST_PORT/AB_TS_PORT override local ports."
