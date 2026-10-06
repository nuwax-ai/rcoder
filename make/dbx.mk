# DBX fork build — shared cache protocol v2 with build-agent-docker.
# A source commit, pinned base-image/toolchain digests, recipe and build inputs
# identify immutable generations. Legacy stage/fork.stamp remain untouched.
DBX_REPO_ROOT := $(abspath $(dir $(lastword $(MAKEFILE_LIST)))/..)
include $(DBX_REPO_ROOT)/make/dbx-versions.mk

USE_GITHUB_MIRROR ?= false
GITHUB_MIRROR_URL ?= https://gh-proxy.org/https://github.com/
wrap-github-url = $(if $(findstring github.com,$(1)),$(if $(findstring true,$(USE_GITHUB_MIRROR)),$(GITHUB_MIRROR_URL)$(subst https://github.com/,,$(1)),$(1)),$(1))

DBX_FORK_REPO ?= https://github.com/nuwax-ai/dbx.git
DBX_FORK_BRANCH ?= test
DBX_PERSIST_ROOT ?= $(abspath $(DBX_REPO_ROOT)/../.cache/nuwax-build/dbx)
DBX_FORK_PIP_INDEX ?= https://mirrors.aliyun.com/pypi/simple
DBX_FORK_REPO_WRAPPED := $(call wrap-github-url,$(DBX_FORK_REPO))
DBX_CONTEXTS ?= docker/rcoder-agent-runner/downloads docker/app-runtime-base/downloads
DBX_HELPER := $(abspath $(dir $(lastword $(MAKEFILE_LIST)))/../tools/build/dbx_cache.py)
ifeq ($(shell uname -s),Darwin)
  DBX_BUILDER ?= orbstack
else
  DBX_BUILDER ?= default
endif
DBX_ARGS = --root "$(DBX_PERSIST_ROOT)" --repo "$(DBX_FORK_REPO)" \
    --fetch-repo "$(DBX_FORK_REPO_WRAPPED)" --ref "$(DBX_FORK_BRANCH)" \
    --builder "$(DBX_BUILDER)" --pip-index "$(DBX_FORK_PIP_INDEX)" \
    --ziglang-version "$(DBX_ZIGLANG_VERSION)" --cargo-zigbuild-version "$(DBX_CARGO_ZIGBUILD_VERSION)"
DBX_DISTRIBUTION_ARGS = $(foreach ctx,$(DBX_CONTEXTS),--context "$(ctx)") \
    $(if $(DBX_OUTPUT_REF),--output-ref "$(DBX_OUTPUT_REF)")

.PHONY: update-dbx-fork build-dbx-fork build-dbx-fork-real distribute-dbx-stage clean-dbx-fork-cache
update-dbx-fork:
	@python3 "$(DBX_HELPER)" update $(DBX_ARGS)

# One helper captures one commit and uses it throughout. No recursive second
# fetch can change the source between guard, build, extraction and publication.
build-dbx-fork:
	@python3 "$(DBX_HELPER)" build $(DBX_ARGS) $(DBX_DISTRIBUTION_ARGS) $(if $(filter 1,$(FORCE_DBX_FORK)),--force)

build-dbx-fork-real:
	@python3 "$(DBX_HELPER)" build $(DBX_ARGS) $(DBX_DISTRIBUTION_ARGS) --force

# Explicit DBX_STAGE must be a verified v2 entry. Unverified old stage/stamp
# content cannot acquire a new cache identity merely through redistribution.
distribute-dbx-stage:
	@python3 "$(DBX_HELPER)" distribute $(DBX_ARGS) $(DBX_DISTRIBUTION_ARGS) $(if $(DBX_STAGE),--stage "$(DBX_STAGE)")

clean-dbx-fork-cache:
	@python3 "$(DBX_HELPER)" clean-source $(DBX_ARGS)
