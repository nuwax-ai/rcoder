# Versioned, checked generations. Legacy directories are compatibility outputs;
# Docker reads immutable references through asset_context.py.
TTYD_VERSION ?= 1.7.7
NODE_RUNTIME_VERSION ?= 22.23.2
DENO_VERSION ?= 2.9.7
GO_VERSION ?= 1.26.4
RUNTIME_ASSET_CACHE ?= .cache/runtime-assets
APP_RUNTIME_CONTEXT ?= docker/app-runtime-base
RUNTIME_ASSET_TOOL := $(abspath $(dir $(lastword $(MAKEFILE_LIST)))/../tools/build/runtime_assets.py)

.PHONY: download-ttyd download-ttyd-amd64 download-ttyd-arm64 download-node download-deno download-go-cache

download-ttyd: download-ttyd-amd64 download-ttyd-arm64

download-ttyd-amd64:
	@python3 "$(RUNTIME_ASSET_TOOL)" ttyd --version "$(TTYD_VERSION)" --arch amd64 --cache "$(RUNTIME_ASSET_CACHE)" --context "$(APP_RUNTIME_CONTEXT)" --output-ref "$(ASSET_REF_DIR)/ttyd-amd64.ref"

download-ttyd-arm64:
	@python3 "$(RUNTIME_ASSET_TOOL)" ttyd --version "$(TTYD_VERSION)" --arch arm64 --cache "$(RUNTIME_ASSET_CACHE)" --context "$(APP_RUNTIME_CONTEXT)" --output-ref "$(ASSET_REF_DIR)/ttyd-arm64.ref"

download-node:
	@python3 "$(RUNTIME_ASSET_TOOL)" node --version "$(NODE_RUNTIME_VERSION)" --cache "$(RUNTIME_ASSET_CACHE)" --context "$(APP_RUNTIME_CONTEXT)" --output-ref "$(ASSET_REF_DIR)/node.ref"

download-deno:
	@python3 "$(RUNTIME_ASSET_TOOL)" deno --version "$(DENO_VERSION)" --cache "$(RUNTIME_ASSET_CACHE)" --context "$(APP_RUNTIME_CONTEXT)" --output-ref "$(ASSET_REF_DIR)/deno.ref"

download-go-cache:
	@python3 "$(RUNTIME_ASSET_TOOL)" go --version "$(GO_VERSION)" --cache "$(RUNTIME_ASSET_CACHE)" --context "$(APP_RUNTIME_CONTEXT)" --output-ref "$(ASSET_REF_DIR)/go.ref"
