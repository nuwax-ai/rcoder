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

# compose 环境（本地 docker compose，RCODER_URL 默认 http://127.0.0.1:8090）
# 前置: make dev-up / make dev-restart 且 .env.local 含 LLM 配置
test-e2e-compose:
	@echo "🧪 Rust e2e（compose 环境；串行；报告在 tests-e2e/reports/）..."
	@status=0; \
	cargo test -p rcoder-e2e --test compose_sse --test compose_session --test compose_userapp -- --test-threads=1 || status=$$?; \
	latest=$$(ls -t tests-e2e/reports/ 2>/dev/null | head -1); \
	echo ""; \
	if [ -n "$$latest" ]; then \
		echo "📋 报告目录: tests-e2e/reports/$$latest"; \
		python3 -c "import json; s=json.load(open('tests-e2e/reports/$$latest/summary.json')); [print(f\"  {e['verdict']:>7}  {e['scenario']}__{e['backend']}\") for e in s['scenarios']]" 2>/dev/null || true; \
	fi; \
	exit $$status

# K8s 专项（目标: 个人开发测试 K8s——20/229 单节点；19 机有生产环境禁用）。
# 配置在 .env.local（参照 .env.local.example）或环境变量：TEST_K8S_SSH、
# LB_ENTRY_HOSTS（单节点一个 IP 即可，场景退化同入口）。
# ⚠️ 负载均衡场景默认 ignore（此前已验证），确认后 RUN_LB=1 显式开启。
# 例: make test-e2e-k8s  /  make test-e2e-k8s RUN_LB=1
test-e2e-k8s:
	@echo "🧪 Rust e2e（K8s；串行；报告在 tests-e2e/reports/）..."
	@echo "   ⚠️ 仅个人测试 K8s（20/229）；19 机有生产环境，未经指示禁用"
	@if [ -n "$(RUN_LB)" ]; then \
		echo "   负载均衡场景开启（--ignored）"; \
		extra="--ignored"; \
	else \
		echo "   负载均衡场景默认关闭（RUN_LB=1 开启）——仅跑 gate 冒烟"; \
		extra=""; \
	fi; \
	status=0; \
	cargo test -p rcoder-e2e --test k8s_lb -- --test-threads=1 $$extra || status=$$?; \
	latest=$$(ls -t tests-e2e/reports/ 2>/dev/null | head -1); \
	echo ""; \
	if [ -n "$$latest" ]; then \
		echo "📋 报告目录: tests-e2e/reports/$$latest"; \
	fi; \
	exit $$status

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
