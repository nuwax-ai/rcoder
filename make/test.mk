# ============================================================================
# Rust 测试：nextest 为默认入口；严格业务 E2E 使用下方独立启动器。
# TEST_FEATURES= 可验证默认 features；NEXTEST_ARGS 可传 -p / -E 等筛选。
# 不要并发执行共享 Cargo target 的多个 Make 测试目标。
# ============================================================================
TEST_FEATURES ?= --all-features
NEXTEST_ARGS ?=

.PHONY: test test-default test-unit test-integration test-blocking test-app-cli test-doc test-all

test:
	cargo nextest run --workspace --no-fail-fast $(TEST_FEATURES) $(NEXTEST_ARGS)

test-default:
	cargo nextest run --workspace --no-fail-fast $(NEXTEST_ARGS)

test-unit:
	cargo nextest run --workspace --lib --no-fail-fast $(TEST_FEATURES) $(NEXTEST_ARGS)

# Rust integration test targets；不是 Compose 核心业务 E2E。
test-integration:
	cargo nextest run --workspace --test '*' --no-fail-fast $(TEST_FEATURES) $(NEXTEST_ARGS)

test-blocking:
	cargo nextest run --workspace --features testing --test '*_blocking*' --no-fail-fast --test-threads=1 $(NEXTEST_ARGS)

# app-cli 被根 workspace 排除，必须单独运行。
test-app-cli:
	cargo nextest run --manifest-path crates/app-cli/Cargo.toml --no-fail-fast $(TEST_FEATURES) $(NEXTEST_ARGS)

# nextest 不覆盖 doctest；两部分串行执行，任一失败整体失败。
test-doc:
	@status=0; \
	cargo test --workspace --doc --no-fail-fast $(TEST_FEATURES) || status=1; \
	cargo test --manifest-path crates/app-cli/Cargo.toml --doc --no-fail-fast $(TEST_FEATURES) || status=1; \
	exit $$status

# Rust 完整检查：即便某部分失败仍收集其他部分结果；不隐式部署或运行 E2E。
# 显式 recipe 保证 make -j 时各子步骤仍串行。
test-all:
	@status=0; \
	$(MAKE) test || status=1; \
	$(MAKE) test-app-cli || status=1; \
	$(MAKE) test-doc || status=1; \
	exit $$status

# ============================================================================
# Rust e2e 黑盒集成测试（tests-e2e crate；JSONL 报告供 agent 追溯）
# ============================================================================

# 显式入口严格验收：缺少前置、skip、aborted、空筛选和报告遗漏均失败。
# E2E_SUITE=compose_userapp_dev E2E_FILTER=case_name 可聚焦；无环境 workspace 测试仍可 skip。
.PHONY: test-e2e test-e2e-compose test-e2e-compose-deploy test-e2e-k8s

test-e2e:
	python3 tests-e2e/tools/run.py --group userapp

test-e2e-compose:
	python3 tests-e2e/tools/run.py --group compose

test-e2e-compose-deploy:
	python3 tests-e2e/tools/run.py --group deploy

# 仅显式选择测试集群。
test-e2e-k8s:
	python3 tests-e2e/tools/run.py --group k8s $(if $(RUN_LB),--ignored,)

# ============================================================================
# 🧪 eBPF 工具安装测试（快速验证 Makefile 变量传递）
# ============================================================================

# 测试 1: 模拟 Makefile 变量传递（启用 eBPF）
test-ebpf-install:
	@echo "🧪 测试 1: 启用 eBPF 工具安装..."
	@(set -e; if [ "$(CARGO_FEATURES)" != "" ]; then \
		INSTALL_EBPF="true"; \
		echo "✅ CARGO_FEATURES=[$(CARGO_FEATURES)], INSTALL_EBPF=$${INSTALL_EBPF}"; \
	else \
		INSTALL_EBPF="false"; \
		echo "⚠️  CARGO_FEATURES=[$(CARGO_FEATURES)], INSTALL_EBPF=$${INSTALL_EBPF}"; \
	fi; \
	cd docker/rcoder-agent-runner; \
		docker build --build-arg INSTALL_EBPF_TOOLS="$${INSTALL_EBPF}" \
			-f Dockerfile.test -t test-ebpf-install .; \
	docker run --rm test-ebpf-install which bpftrace; \
	echo "✅ 测试通过: bpftrace 已安装")

# 测试 2: 模拟生产模式（禁用 eBPF）
test-ebpf-no-install:
	@echo "🧪 测试 2: 禁用 eBPF 工具安装（生产模式）..."
	@(set -e; INSTALL_EBPF="false"; \
		echo "🔒 INSTALL_EBPF=$${INSTALL_EBPF}"; \
		cd docker/rcoder-agent-runner; \
		docker build --build-arg INSTALL_EBPF_TOOLS="$${INSTALL_EBPF}" \
			-f Dockerfile.test -t test-ebpf-no-install .; \
		docker run --rm test-ebpf-no-install /bin/sh -c 'if command -v bpftrace >/dev/null 2>&1; then echo "错误: 生产模式不应安装 bpftrace"; exit 1; fi'; \
	echo "✅ 测试通过: 生产模式正确跳过安装")

# 测试 3: 直接测试变量传递（调试用）
test-ebpf-debug:
	@echo "🧪 测试 3: 变量传递调试..."
	@echo "CARGO_FEATURES=[$(CARGO_FEATURES)]"
	@(set -e; if [ "$(CARGO_FEATURES)" != "" ]; then \
		INSTALL_EBPF="true"; \
		echo "Shell: INSTALL_EBPF=$${INSTALL_EBPF}"; \
		echo "Docker: INSTALL_EBPF_TOOLS=\"$${INSTALL_EBPF}\""; \
	else \
		INSTALL_EBPF="false"; \
		echo "Shell: INSTALL_EBPF=$${INSTALL_EBPF}"; \
		echo "Docker: INSTALL_EBPF_TOOLS=\"$${INSTALL_EBPF}\""; \
	fi)

# 测试 4: 完整测试 Pyroscope + Off-CPU 工具
test-pyroscope-offcpu:
	@echo "🧪 测试 4: Pyroscope Agent + Off-CPU 工具完整测试..."
	@(set -e; if [ "$(CARGO_FEATURES)" != "" ]; then \
		INSTALL_EBPF="true"; \
		echo "✅ CARGO_FEATURES=[$(CARGO_FEATURES)], INSTALL_EBPF=$${INSTALL_EBPF}"; \
	else \
		INSTALL_EBPF="false"; \
		echo "⚠️  CARGO_FEATURES=[$(CARGO_FEATURES)], INSTALL_EBPF=$${INSTALL_EBPF}"; \
	fi; \
	cd docker/rcoder-agent-runner; \
		docker build --build-arg INSTALL_EBPF_TOOLS="$${INSTALL_EBPF}" \
			--build-arg INSTALL_PYROSCOPE="$${INSTALL_EBPF}" \
			-f Dockerfile.test-full -t test-pyroscope-offcpu .; \
	echo "=== 验证 pyroscope ===" && \
	docker run --rm test-pyroscope-offcpu which pyroscope; \
	echo "=== 验证 offcputime-bpfcc ===" && \
	docker run --rm test-pyroscope-offcpu which offcputime-bpfcc)

# Real K8s userApp acceptance; explicit NodePort URLs and SSH test target required.
.PHONY: test-e2e-k8s-userapp
test-e2e-k8s-userapp:
	python3 tests-e2e/tools/k8s_userapp.py --ssh "$(TEST_K8S_SSH)" --url "$(RCODER_URL)" --proxy-url "$(E2E_PINGORA_URL)"
