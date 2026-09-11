# ============================================================================
# 测试命令
# ============================================================================

# 运行所有测试
test:
	@echo "🧪 运行所有测试..."
	@cargo test --workspace

# 运行单元测试
test-unit:
	@echo "🧪 运行单元测试..."
	@cargo test --workspace --lib

# 运行集成测试
test-integration:
	@echo "🧪 运行集成测试..."
	@cargo test --workspace --test '*'

# 运行极端场景测试（包含阻塞）
test-blocking:
	@echo "🧪 运行极端场景测试（包含阻塞）..."
	@cargo test --workspace --features testing --test '*_blocking*' -- --test-threads=1

# 运行完整测试套件
test-all:
	@echo "🧪 运行完整测试套件..."
	@cargo test --workspace --all-features

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

# 仅显式选择测试集群；本轮不执行真实集群测试。
test-e2e-k8s:
	python3 tests-e2e/tools/run.py --group k8s $(if $(RUN_LB),--ignored,)

# ============================================================================
# 🧪 eBPF 工具安装测试（快速验证 Makefile 变量传递）
# ============================================================================

# 测试 1: 模拟 Makefile 变量传递（启用 eBPF）
test-ebpf-install:
	@echo "🧪 测试 1: 启用 eBPF 工具安装..."
	@(if [ "$(CARGO_FEATURES)" != "" ]; then \
		INSTALL_EBPF="true"; \
		echo "✅ CARGO_FEATURES=[$(CARGO_FEATURES)], INSTALL_EBPF=$${INSTALL_EBPF}"; \
	else \
		INSTALL_EBPF="false"; \
		echo "⚠️  CARGO_FEATURES=[$(CARGO_FEATURES)], INSTALL_EBPF=$${INSTALL_EBPF}"; \
	fi; \
	cd docker/rcoder-agent-runner && \
		docker build --build-arg INSTALL_EBPF_TOOLS="$${INSTALL_EBPF}" \
			-f Dockerfile.test -t test-ebpf-install . 2>&1 | tail -20; \
	docker run --rm test-ebpf-install which bpftrace && echo "✅ 测试通过: bpftrace 已安装" || echo "❌ 测试失败: bpftrace 未安装")

# 测试 2: 模拟生产模式（禁用 eBPF）
test-ebpf-no-install:
	@echo "🧪 测试 2: 禁用 eBPF 工具安装（生产模式）..."
	@(INSTALL_EBPF="false"; \
		echo "🔒 INSTALL_EBPF=$${INSTALL_EBPF}"; \
		cd docker/rcoder-agent-runner && \
		docker build --build-arg INSTALL_EBPF_TOOLS="$${INSTALL_EBPF}" \
			-f Dockerfile.test -t test-ebpf-no-install . 2>&1 | tail -20; \
		docker run --rm test-ebpf-no-install which bpftrace && echo "❌ 测试失败: 生产模式不应安装 bpftrace" || echo "✅ 测试通过: 生产模式正确跳过安装")

# 测试 3: 直接测试变量传递（调试用）
test-ebpf-debug:
	@echo "🧪 测试 3: 变量传递调试..."
	@echo "CARGO_FEATURES=[$(CARGO_FEATURES)]"
	@(if [ "$(CARGO_FEATURES)" != "" ]; then \
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
	@(if [ "$(CARGO_FEATURES)" != "" ]; then \
		INSTALL_EBPF="true"; \
		echo "✅ CARGO_FEATURES=[$(CARGO_FEATURES)], INSTALL_EBPF=$${INSTALL_EBPF}"; \
	else \
		INSTALL_EBPF="false"; \
		echo "⚠️  CARGO_FEATURES=[$(CARGO_FEATURES)], INSTALL_EBPF=$${INSTALL_EBPF}"; \
	fi; \
	cd docker/rcoder-agent-runner && \
		docker build --build-arg INSTALL_EBPF_TOOLS="$${INSTALL_EBPF}" \
			--build-arg INSTALL_PYROSCOPE="$${INSTALL_EBPF}" \
			-f Dockerfile.test-full -t test-pyroscope-offcpu . 2>&1 | tail -30; \
	echo "=== 验证 pyroscope ===" && \
	docker run --rm test-pyroscope-offcpu which pyroscope && echo "✅ pyroscope 已安装" || echo "❌ pyroscope 未安装"; \
	echo "=== 验证 offcputime-bpfcc ===" && \
	docker run --rm test-pyroscope-offcpu which offcputime-bpfcc && echo "✅ offcputime-bpfcc 已安装" || echo "❌ offcputime-bpfcc 未安装")
