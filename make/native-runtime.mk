# Native CLI contract. Build explicitly once; the test target reuses binaries.
NATIVE_APP_CLI ?= crates/app-cli/target/debug/app-cli
NATIVE_PROXY ?= target/debug/file-server-proxy
NATIVE_PINGAP ?= pingap
NATIVE_REPORT_ROOT ?= tmp/native-runtime
NATIVE_PYTHON ?= python3

.PHONY: native-runtime-build test-native-runtime
native-runtime-build:
	cargo build --manifest-path crates/app-cli/Cargo.toml --bin app-cli
	cargo build -p file-server-proxy --bin file-server-proxy

test-native-runtime:
	$(NATIVE_PYTHON) tools/native_runtime_acceptance.py --app-cli '$(NATIVE_APP_CLI)' --proxy '$(NATIVE_PROXY)' --pingap '$(NATIVE_PINGAP)' --root '$(NATIVE_REPORT_ROOT)'
