.PHONY: help build docker-build docker-build-base docker-build-master docker-build-master-base docker-build-agent-runner docker-build-agent-base docker-build-agent-production docker-pre-download-libreoffice docker-clean-libreoffice-downloads install install-agent uninstall dev-build dev-up dev-restart dev-down dev-logs update-image-tag test test-unit test-integration test-all test-blocking test-ebpf-install test-ebpf-no-install test-ebpf-debug test-pyroscope-offcpu pyroscope-up pyroscope-down pyroscope-logs dev-build-k8s dev-up-k8s dev-restart-k8s dev-down-k8s dev-logs-k8s

# 默认目标：显示帮助信息
help:
	@echo "rcoder 开发模式 Makefile"
	@echo ""
	@echo "📦 编译和安装："
	@echo "  make build          - 本地编译 rcoder（仅编译）"
	@echo "  make install        - 安装所有二进制到 ~/.cargo/bin/"
	@echo "  make install-agent  - 仅安装 codex-acp-agent"
	@echo "  make uninstall      - 卸载所有二进制"
	@echo ""
	@echo "🐳 Docker 镜像构建："
	@echo "  make docker-build                 - 构建所有 Docker 镜像"
	@echo "  make docker-build-base            - 构建所有基础镜像（很少需要）"
	@echo "  make docker-build-master          - 仅构建 master-rcoder 镜像（需要基础镜像）"
	@echo "  make docker-build-master-base     - 仅构建 master-rcoder-base 基础镜像"
	@echo "  make docker-build-agent-runner    - 仅构建 rcoder-agent-runner 镜像（启用 eBPF 调试）"
	@echo "  make docker-build-agent-base      - 仅构建 rcoder-agent-base 基础镜像"
	@echo "  make docker-build-agent-production - 构建生产镜像（无 eBPF 工具，镜像更小）"
	@echo "  make docker-pre-download-libreoffice - 预下载 LibreOffice（避免每次构建重新下载）"
	@echo "  make docker-clean-libreoffice-downloads - 清理已下载的 LibreOffice 文件"
	@echo "  make dev-build                    - 本地编译 + 构建 Docker 镜像（一键完成）"
	@echo "  make update-image-tag             - 根据系统架构更新镜像标签 (arm64/amd64 -> latest)"
	@echo ""
	@echo "🔧 开发模式命令："
	@echo "  make dev-up         - 启动开发模式容器（使用镜像内编译的二进制）"
	@echo "  make dev-restart    - 重启开发模式容器（重新构建镜像并启动）"
	@echo "  make dev-down       - 停止开发模式容器"
	@echo "  make dev-logs       - 查看开发模式容器日志"
	@echo ""
	@echo "☸️  K8s 开发模式命令："
	@echo "  make dev-build-k8s  - 构建 K8s 镜像（启用 kubernetes feature）"
	@echo "  make dev-up-k8s     - 启动 K8s 开发模式（标准 kubectl apply）"
	@echo "  make dev-restart-k8s - 重启 K8s 开发模式（重新构建镜像+部署）"
	@echo "  make dev-down-k8s   - 停止 K8s 开发模式（Workload + RBAC + Namespace，与 dev-up 对称）"
	@echo "  make dev-logs-k8s   - 查看 K8s 开发模式日志"
	@echo "  变量: IMAGE=rcoder:test-k8s ROLLOUT_TIMEOUT=180s"
	@echo "  示例: make dev-restart-k8s IMAGE=rcoder:test-k8s"
	@echo ""
	@echo "📊 Pyroscope 持续剖析："
	@echo "  make pyroscope-up   - 启动 Pyroscope Server"
	@echo "  make pyroscope-down - 停止 Pyroscope Server"
	@echo "  make pyroscope-logs - 查看 Pyroscope 日志"
	@echo ""
	@echo "🧪 测试命令："
	@echo "  make test           - 运行所有测试"
	@echo "  make test-unit      - 运行单元测试"
	@echo "  make test-integration - 运行集成测试"
	@echo "  make test-blocking  - 运行极端场景测试（包含阻塞）"
	@echo "  make test-all       - 运行完整测试套件（所有 features）"
	@echo ""
	@echo "开发模式工作流程："
	@echo "  1. make dev-build    # 首次：构建所有 Docker 镜像（容器内编译）"
	@echo "  2. make dev-up       # 启动容器"
	@echo "  3. 修改代码后: make dev-restart  # 重新构建镜像+重启容器"
	@echo ""
	@echo "💡 提示："
	@echo "  - dev-build: 在 Docker 容器内编译，确保 Linux 兼容性"
	@echo "  - dev-restart: 每次代码修改都需要重新构建镜像（容器内重新编译）"
	@echo ""

# 本地编译（仅编译，不构建镜像）
build:
	@echo "🔨 本地编译 rcoder..."
	@cargo build --release --bin rcoder
	@echo "✅ 编译完成！"
	@echo "可执行文件: ./target/release/rcoder"

# Docker 镜像构建（仅构建镜像，不编译）
# 串行构建镜像，避免资源竞争
docker-build: docker-build-agent-base
	@echo "🔨 开始构建主镜像..."
	@$(MAKE) docker-build-master & \
	$(MAKE) docker-build-agent-runner & \
	wait
	@echo ""
	@echo "✅ 所有 Docker 镜像构建完成！"
	@echo "  ✓ master-rcoder:latest"
	@echo "  ✓ rcoder-agent-runner:latest"
	@echo ""
	@echo "🎯 使用方式："
	@echo "  docker run -d -p 8087:8087 master-rcoder:latest"

# 构建所有基础镜像（很少需要，只有修改系统依赖时才需要）
# 串行构建基础镜像，避免资源竞争
docker-build-base: docker-build-master-base docker-build-agent-base
	@echo ""
	@echo "✅ 所有基础镜像构建完成！"
	@echo "  ✓ master-rcoder-base:latest"
	@echo "  ✓ rcoder-agent-base:latest"
	@echo ""
	@echo "💡 提示: 平时开发只需运行 make dev-restart，无需重新构建基础镜像"

# 构建主服务镜像（基于基础镜像，快速构建）
docker-build-master:
	@echo "🐳 构建 master-rcoder 镜像..."
	@echo "📍 镜像名称: master-rcoder:latest"
	@# 检查基础镜像是否存在
	@if ! docker image inspect master-rcoder-base:latest >/dev/null 2>&1; then \
		echo "⚠️  基础镜像 master-rcoder-base:latest 不存在，先构建基础镜像..."; \
		$(MAKE) docker-build-master-base; \
	else \
		echo "✓ 基础镜像 master-rcoder-base:latest 已存在"; \
	fi
	@echo "📦 使用 Dockerfile 多阶段构建（基于基础镜像）..."
	@# 🔧 根据 CARGO_FEATURES 决定是否启用 eBPF 调试
	@(if [ "$(CARGO_FEATURES)" != "" ]; then \
		MASTER_CARGO_FLAGS="$(CARGO_FEATURES)"; \
		echo "🔧 master-rcoder 将启用 eBPF 调试模式"; \
	else \
		MASTER_CARGO_FLAGS=""; \
		echo "🔒 master-rcoder 生产模式（无 eBPF 调试）"; \
	fi; \
	docker build \
		--build-arg CARGO_FLAGS="$$MASTER_CARGO_FLAGS" \
		--build-arg CACHEBUST=$$(date +%s) \
		-f docker/rcoder-master/Dockerfile -t master-rcoder:latest .;)
	@echo "✅ master-rcoder 镜像构建完成！"

# 构建 master-base 基础镜像（包含所有运行时依赖，很少需要重新构建）
docker-build-master-base:
	@echo "🐳 构建 master-rcoder-base 基础镜像..."
	@echo "📍 镜像名称: master-rcoder-base:latest"
	@echo "⏳ 这可能需要较长时间（包含所有运行时依赖安装）..."
	@docker build -f docker/rcoder-master/Dockerfile.base -t master-rcoder-base:latest .
	@echo "✅ master-rcoder-base 基础镜像构建完成！"
	@echo "💡 提示: 平时开发只需运行 make dev-restart，无需重新构建基础镜像"

# ============================================================================
# 🔧 Cargo feature 配置
# ============================================================================
# 开发模式：启用所有调试、监控和追踪功能
# ⚠️  注意：添加新的调试 feature 时，必须同步更新此列表！
#
# 当前启用的调试 features：
#   - ebpf-debug    (docker_manager, rcoder): eBPF 诊断工具
#   - pyroscope     (agent_runner):         性能分析 (CPU/Memory)
#   - otel          (agent_runner):         OpenTelemetry 追踪
#   - debug         (rcoder):               调试路由
#   - proxy         (agent_runner):         Pingora 反向代理（Linux 容器默认开启）
#   - kubernetes    (rcoder, docker_manager): Kubernetes 运行时支持
#
# 本地开发调试默认开启所有功能
CARGO_FEATURES ?= --features ebpf-debug,pyroscope,otel,debug,proxy,kubernetes
#
# 生产模式：禁用 eBPF 工具（通过 make docker-build-agent-production）
# CARGO_FEATURES ?=

# 构建 agent-runner 镜像（基于基础镜像，快速构建）
docker-build-agent-runner:
	@echo "🐳 构建 rcoder-agent-runner 镜像..."
	@echo "📍 镜像名称: rcoder-agent-runner:latest"
	@# 检查基础镜像是否存在
	@if ! docker image inspect rcoder-agent-base:latest >/dev/null 2>&1; then \
		echo "⚠️  基础镜像 rcoder-agent-base:latest 不存在，先构建基础镜像..."; \
		$(MAKE) docker-build-agent-base; \
	else \
		echo "✓ 基础镜像 rcoder-agent-base:latest 已存在"; \
	fi
	@echo "📦 步骤1: 在 debian:12 环境中构建 agent_runner 二进制（确保 GLIBC 版本兼容）..."
	@# 🔧 调试模式：默认启用 ebpf-debug feature，允许使用 eBPF 诊断工具
	@echo "🔧 Cargo features: $(CARGO_FEATURES)"
	@# 计算业务代码哈希，只有代码变化时才重新编译（系统依赖和 Rust 安装保持缓存）
	$(eval CRATES_HASH := $(shell find crates Cargo.toml Cargo.lock -name "*.rs" -o -name "Cargo.toml" -o -name "Cargo.lock" 2>/dev/null | sort | xargs cat 2>/dev/null | md5sum | cut -d' ' -f1))
	@echo "🔑 业务代码哈希: $(CRATES_HASH)"
	@# 🔥 关键修改：通过 CARGO_FEATURES 变量控制
	@docker build --build-arg CRATES_HASH=$(CRATES_HASH) \
		--build-arg CARGO_FLAGS="$(CARGO_FEATURES)" \
		-f docker/rcoder-agent-runner/Dockerfile.build -t rcoder-agent-runner-build .
	@echo "📦 步骤2: 复制二进制文件到 agent-runner 目录..."
	@# 创建容器并复制 agent_runner 二进制文件
	@mkdir -p docker/rcoder-agent-runner/bin
	@docker create --name build-container rcoder-agent-runner-build
	@docker cp build-container:/build/target/release/agent_runner docker/rcoder-agent-runner/bin/
	@docker rm build-container
	@docker rmi rcoder-agent-runner-build
	@echo "📦 步骤3: 构建最终的 agent-runner 镜像（基于基础镜像，快速）..."
	@# 🔧 根据 CARGO_FEATURES 决定是否安装 eBPF 工具
	@(if [ "$(CARGO_FEATURES)" != "" ]; then \
		INSTALL_EBPF="true"; \
		echo "🔧 将安装 eBPF 诊断工具"; \
	else \
		INSTALL_EBPF="false"; \
		echo "🔒 跳过 eBPF 工具安装（生产模式）"; \
	fi; \
	cd docker/rcoder-agent-runner && \
		docker buildx build --platform linux/$(DOCKER_HOST_ARCH) --load \
			--build-arg CACHEBUST=$$(date +%s) \
			--build-arg INSTALL_EBPF_TOOLS="$${INSTALL_EBPF}" \
			--build-arg INSTALL_PYROSCOPE="$${INSTALL_EBPF}" \
			--build-arg INSTALL_ALLOY="$${INSTALL_EBPF}" \
			-f Dockerfile -t rcoder-agent-runner:latest .;)
	@echo "✅ rcoder-agent-runner 镜像构建完成！"
	@if [ "$(CARGO_FEATURES)" != "" ]; then \
		echo "🔧 eBPF 调试模式已启用，容器将以特权模式运行"; \
	else \
		echo "🔒 生产模式，容器权限受限"; \
	fi
	@echo "📤 推送镜像到阿里云仓库..."
	@docker tag rcoder-agent-runner:latest $(K8S_IMAGE_REGISTRY)
	@docker tag rcoder-agent-runner:latest nuwax-docker-images-registry.cn-hangzhou.cr.aliyuncs.com/dev/rcoder-agent-runner:latest
	@skopeo copy docker-daemon:nuwax-docker-images-registry.cn-hangzhou.cr.aliyuncs.com/dev/rcoder-agent-runner:latest docker://nuwax-docker-images-registry.cn-hangzhou.cr.aliyuncs.com/dev/rcoder-agent-runner:latest
	@echo "✅ 镜像已推送: nuwax-docker-images-registry.cn-hangzhou.cr.aliyuncs.com/dev/rcoder-agent-runner:latest"

# 构建生产版本（禁用 eBPF 工具，减小镜像大小）
docker-build-agent-production:
	@echo "🐳 构建 rcoder-agent-runner 生产镜像（无 eBPF 工具）..."
	@$(MAKE) docker-build-agent-runner CARGO_FEATURES=""
	@echo "✅ 生产镜像构建完成（无 eBPF 工具，镜像更小）"

# ============================================================================
# LibreOffice 预下载配置（避免每次构建都重新下载）
# ============================================================================
# LibreOffice 版本（必须与 Dockerfile.base 保持一致）
LIBREOFFICE_VERSION := 25.8.5

# 下载目录（相对于项目根目录）
LIBREOFFICE_DOWNLOAD_DIR := docker/rcoder-agent-runner/downloads

# x86_64 下载信息
LIBREOFFICE_X86_FILE := LibreOffice_$(LIBREOFFICE_VERSION)_Linux_x86-64_deb.tar.gz
LIBREOFFICE_X86_URL := https://mirror.csclub.uwaterloo.ca/tdf/libreoffice/stable/$(LIBREOFFICE_VERSION)/deb/x86_64/LibreOffice_$(LIBREOFFICE_VERSION)_Linux_x86-64_deb.tar.gz
LIBREOFFICE_X86_FALLBACK := https://download.documentfoundation.org/libreoffice/stable/$(LIBREOFFICE_VERSION)/deb/x86_64/LibreOffice_$(LIBREOFFICE_VERSION)_Linux_x86-64_deb.tar.gz
LIBREOFFICE_X86_PATH := $(LIBREOFFICE_DOWNLOAD_DIR)/$(LIBREOFFICE_X86_FILE)

# aarch64 下载信息
LIBREOFFICE_ARM_FILE := LibreOffice_$(LIBREOFFICE_VERSION)_Linux_aarch64_deb.tar.gz
LIBREOFFICE_ARM_URL := https://mirror.csclub.uwaterloo.ca/tdf/libreoffice/stable/$(LIBREOFFICE_VERSION)/deb/aarch64/LibreOffice_$(LIBREOFFICE_VERSION)_Linux_aarch64_deb.tar.gz
LIBREOFFICE_ARM_FALLBACK := https://download.documentfoundation.org/libreoffice/stable/$(LIBREOFFICE_VERSION)/deb/aarch64/LibreOffice_$(LIBREOFFICE_VERSION)_Linux_aarch64_deb.tar.gz
LIBREOFFICE_ARM_PATH := $(LIBREOFFICE_DOWNLOAD_DIR)/$(LIBREOFFICE_ARM_FILE)

# docker-build-agent-base 构建时使用的架构（检测宿主 Docker 架构）
# OrbStack on Apple Silicon = linux/arm64, 普通 Linux = linux/amd64
# 注意：docker version --format '{{.Server.Arch}}' 在 OrbStack 上返回 "arm64"，不是 "aarch64"
DOCKER_HOST_ARCH := $(shell docker version --format '{{.Server.Arch}}' 2>/dev/null || echo "amd64")
ifeq ($(DOCKER_HOST_ARCH),arm64)
	LIBREOFFICE_ARCH := arm64
	LIBREOFFICE_FILE := $(LIBREOFFICE_ARM_FILE)
else ifeq ($(DOCKER_HOST_ARCH),aarch64)
	LIBREOFFICE_ARCH := arm64
	LIBREOFFICE_FILE := $(LIBREOFFICE_ARM_FILE)
else
	LIBREOFFICE_ARCH := amd64
	LIBREOFFICE_FILE := $(LIBREOFFICE_X86_FILE)
endif

# ============================================================================
# LibreOffice 预下载目标
# ============================================================================

# 预下载 LibreOffice（下载两个架构的文件，确保 Docker 多平台构建可用）
docker-pre-download-libreoffice:
	@echo "📦 检查并下载 LibreOffice $(LIBREOFFICE_VERSION)..."
	@mkdir -p $(LIBREOFFICE_DOWNLOAD_DIR)
	@# ========== 下载 x86_64 ==========
	@echo ""; echo "=== 检查 x86_64 版本 ==="; \
	if [ -f "$(LIBREOFFICE_X86_PATH)" ] && [ -f "$(LIBREOFFICE_X86_PATH).sha256" ]; then \
		SAVED_HASH=$$(cat "$(LIBREOFFICE_X86_PATH).sha256"); \
		CALC_HASH=$$(sha256sum "$(LIBREOFFICE_X86_PATH)" 2>/dev/null | cut -d' ' -f1); \
		if [ "$$SAVED_HASH" = "$$CALC_HASH" ]; then \
			echo "✅ x86_64 已存在且有效，跳过下载"; \
		else \
			echo "❌ x86_64 Hash 不匹配，重新下载..."; \
			rm -f "$(LIBREOFFICE_X86_PATH)" "$(LIBREOFFICE_X86_PATH).sha256"; \
			echo "📥 下载 x86_64 版本..."; \
			curl -fSL -o "$(LIBREOFFICE_X86_PATH)" "$(LIBREOFFICE_X86_URL)" 2>/dev/null || \
			curl -fSL -L -o "$(LIBREOFFICE_X86_PATH)" "$(LIBREOFFICE_X86_FALLBACK)" 2>/dev/null || { \
				rm -f "$(LIBREOFFICE_X86_PATH)"; echo "❌ x86_64 下载失败"; exit 1; }; \
			CALC_HASH=$$(sha256sum "$(LIBREOFFICE_X86_PATH)" 2>/dev/null | cut -d' ' -f1); \
			echo "$$CALC_HASH" > "$(LIBREOFFICE_X86_PATH).sha256"; \
			echo "✅ x86_64 下载完成: $$CALC_HASH"; \
		fi; \
	else \
		echo "📥 下载 x86_64 版本..."; \
		curl -fSL -o "$(LIBREOFFICE_X86_PATH)" "$(LIBREOFFICE_X86_URL)" 2>/dev/null || \
		curl -fSL -L -o "$(LIBREOFFICE_X86_PATH)" "$(LIBREOFFICE_X86_FALLBACK)" 2>/dev/null || { \
			rm -f "$(LIBREOFFICE_X86_PATH)"; echo "❌ x86_64 下载失败"; exit 1; }; \
		CALC_HASH=$$(sha256sum "$(LIBREOFFICE_X86_PATH)" 2>/dev/null | cut -d' ' -f1); \
		echo "$$CALC_HASH" > "$(LIBREOFFICE_X86_PATH).sha256"; \
		echo "✅ x86_64 下载完成: $$CALC_HASH"; \
	fi
	@# ========== 下载 aarch64 ==========
	@echo ""; echo "=== 检查 aarch64 版本 ==="; \
	if [ -f "$(LIBREOFFICE_ARM_PATH)" ] && [ -f "$(LIBREOFFICE_ARM_PATH).sha256" ]; then \
		SAVED_HASH=$$(cat "$(LIBREOFFICE_ARM_PATH).sha256"); \
		CALC_HASH=$$(sha256sum "$(LIBREOFFICE_ARM_PATH)" 2>/dev/null | cut -d' ' -f1); \
		if [ "$$SAVED_HASH" = "$$CALC_HASH" ]; then \
			echo "✅ aarch64 已存在且有效，跳过下载"; \
		else \
			echo "❌ aarch64 Hash 不匹配，重新下载..."; \
			rm -f "$(LIBREOFFICE_ARM_PATH)" "$(LIBREOFFICE_ARM_PATH).sha256"; \
			echo "📥 下载 aarch64 版本..."; \
			curl -fSL -o "$(LIBREOFFICE_ARM_PATH)" "$(LIBREOFFICE_ARM_URL)" 2>/dev/null || \
			curl -fSL -L -o "$(LIBREOFFICE_ARM_PATH)" "$(LIBREOFFICE_ARM_FALLBACK)" 2>/dev/null || { \
				rm -f "$(LIBREOFFICE_ARM_PATH)"; echo "❌ aarch64 下载失败"; exit 1; }; \
			CALC_HASH=$$(sha256sum "$(LIBREOFFICE_ARM_PATH)" 2>/dev/null | cut -d' ' -f1); \
			echo "$$CALC_HASH" > "$(LIBREOFFICE_ARM_PATH).sha256"; \
			echo "✅ aarch64 下载完成: $$CALC_HASH"; \
		fi; \
	else \
		echo "📥 下载 aarch64 版本..."; \
		curl -fSL -o "$(LIBREOFFICE_ARM_PATH)" "$(LIBREOFFICE_ARM_URL)" 2>/dev/null || \
		curl -fSL -L -o "$(LIBREOFFICE_ARM_PATH)" "$(LIBREOFFICE_ARM_FALLBACK)" 2>/dev/null || { \
			rm -f "$(LIBREOFFICE_ARM_PATH)"; echo "❌ aarch64 下载失败"; exit 1; }; \
		CALC_HASH=$$(sha256sum "$(LIBREOFFICE_ARM_PATH)" 2>/dev/null | cut -d' ' -f1); \
		echo "$$CALC_HASH" > "$(LIBREOFFICE_ARM_PATH).sha256"; \
		echo "✅ aarch64 下载完成: $$CALC_HASH"; \
	fi
	@echo ""; echo "✅ LibreOffice $(LIBREOFFICE_VERSION) 下载检查完成"

# 清理 LibreOffice 下载文件
docker-clean-libreoffice-downloads:
	@echo "🗑️  清理 LibreOffice 下载文件..."
	@rm -rf $(LIBREOFFICE_DOWNLOAD_DIR)
	@echo "✅ LibreOffice 下载文件已清理"

# 构建 agent-base 基础镜像（包含所有系统依赖，很少需要重新构建）
docker-build-agent-base: docker-pre-download-libreoffice
	@echo "🐳 构建 rcoder-agent-base 基础镜像..."
	@echo "📍 镜像名称: rcoder-agent-base:latest"
	@echo "📦 LibreOffice: $(LIBREOFFICE_VERSION) ($(LIBREOFFICE_ARCH))"
	@echo "⏳ 这可能需要较长时间（包含所有系统依赖安装）..."
	@# CACHEBUST_NOVNC: 传入时间戳强制每次重新克隆 noVNC
	@# 使用 --platform 确保 TARGETARCH 正确传递给 Dockerfile.base
	@cd docker/rcoder-agent-runner && \
		docker buildx build --platform linux/$(LIBREOFFICE_ARCH) --load \
		--build-arg LIBREOFFICE_FILE=$(LIBREOFFICE_FILE) \
		--build-arg CACHEBUST_NOVNC=$$(date +%s) \
		-f Dockerfile.base -t rcoder-agent-base:latest .
	@echo "✅ rcoder-agent-base 基础镜像构建完成！"
	@echo "💡 提示: 平时开发只需运行 make dev-restart，无需重新构建基础镜像"

# 安装 codex-acp-agent
install-agent:
	@echo "📦 安装 codex-acp-agent..."
	@cargo install --path crates/codex-acp-agent --bin codex-acp-agent --force
	@echo "✅ codex-acp-agent 已安装到: ~/.cargo/bin/codex-acp-agent"

# 安装所有二进制到系统
install: build
	@echo "📥 安装 rcoder 到系统..."
	@cargo install --path crates/rcoder --bin rcoder --force
	@echo ""
	@echo "✅ 已安装："
	@echo "  - codex-acp-agent -> ~/.cargo/bin/codex-acp-agent"
	@echo "  - rcoder -> ~/.cargo/bin/rcoder"

# 卸载所有二进制
uninstall:
	@echo "🗑️  卸载二进制..."
	@cargo uninstall rcoder 2>/dev/null || echo "rcoder 未安装"
	@cargo uninstall codex-acp-agent 2>/dev/null || echo "codex-acp-agent 未安装"
	@echo "✅ 卸载完成"

# 开发模式：Docker 镜像构建（在容器内编译，避免 glibc 版本不匹配）
dev-build: docker-build
	@echo ""
	@echo "🎉 构建完成！"
	@echo "  ✓ Docker 镜像: master-rcoder:latest"
	@echo "  ✓ Docker 镜像: rcoder-agent-runner:latest"
	@echo ""
	@echo "💡 下一步: make dev-up 启动容器"



dev-up:
	@echo "🚀 启动开发模式容器服务..."
	@if [ ! -f "docker/docker-compose.yml" ]; then \
		echo "❌ 错误: 未找到 docker/docker-compose.yml"; \
		exit 1; \
	fi
	@echo "🔧 使用开发模式配置："
	@echo "  - 镜像: master-rcoder:latest (容器内编译的 Linux 二进制)"
	@echo "  - 启动命令: 直接执行 /app/rcoder"
	@RCODER_IMAGE=master-rcoder:latest \
	docker-compose -f docker/docker-compose.yml up -d
	@echo "📋 开发模式服务状态:"
	@docker-compose -f docker/docker-compose.yml ps

dev-down:
	@echo "🛑 停止开发模式容器服务..."
	@if [ -f "docker/docker-compose.yml" ]; then \
		docker-compose -f docker/docker-compose.yml down; \
	else \
		echo "⚠️  docker-compose.yml 未找到，跳过停止操作"; \
	fi

# 快速重启：依赖 dev-build 确保代码更改生效
dev-restart: dev-build
	@echo "🔄 重启容器服务（使用最新构建的镜像）..."
	@if [ -f "docker/docker-compose.yml" ]; then \
		docker-compose -f docker/docker-compose.yml down; \
		docker-compose -f docker/docker-compose.yml up -d; \
		echo "✅ 容器已重启！"; \
	else \
		echo "❌ 错误: 未找到 docker-compose.yml"; \
		exit 1; \
	fi
	@echo ""
	@echo "🎉 完整重启完成！"
	@echo "🎉 如需构建基础镜像,可以执行: make docker-build-base"
	@echo "💡 代码更改已生效，因为重新构建了镜像！"

# ==================== K8s 开发模式命令 ====================

IMAGE ?= rcoder:test-k8s
K8S_NAMESPACE := rcoder
ROLLOUT_TIMEOUT ?= 180s

# 构建 K8s 镜像（启用 kubernetes feature）
# 依赖 docker-build-master-base 确保基础镜像存在
K8S_IMAGE_REGISTRY ?= nuwax-docker-images-registry.cn-hangzhou.cr.aliyuncs.com/dev/rcoder:latest

dev-build-k8s: docker-build-master-base
	@echo "☸️  构建 K8s 镜像..."
	@echo "📍 镜像名称: $(IMAGE)"
	@echo "⏳ 这可能需要较长时间（包含 Rust 编译）..."
	@docker build -f docker/rcoder-master/Dockerfile -t $(IMAGE) --build-arg CARGO_FLAGS="--features kubernetes" .
	@echo "✅ K8s 镜像构建完成！"
	@echo "📤 推送镜像到阿里云仓库..."
	@docker tag $(IMAGE) $(K8S_IMAGE_REGISTRY)
	@skopeo copy docker-daemon:$(K8S_IMAGE_REGISTRY) docker://$(K8S_IMAGE_REGISTRY)
	@echo "✅ 镜像已推送到 $(K8S_IMAGE_REGISTRY)"
	@echo "💡 下一步: make dev-up-k8s 启动 K8s 开发模式"

# 启动 K8s 开发模式（部署到已有 K8s 集群）
dev-up-k8s:
	@echo "☸️  启动 K8s 开发模式..."
	@kubectl apply -f k8s/manifests/namespace.yaml
	@kubectl apply -f k8s/manifests/serviceaccount.yaml
	@kubectl apply -f k8s/manifests/rcoder-configmap.yaml
	@sed "s|image: rcoder:test|image: $(IMAGE)|" k8s/manifests/rcoder-deployment.yaml | kubectl apply -f -
	@kubectl apply -f k8s/manifests/rcoder-service.yaml
	@kubectl rollout status deploy/rcoder -n $(K8S_NAMESPACE) --timeout=$(ROLLOUT_TIMEOUT)
	@echo "📋 K8s 部署状态:"
	@kubectl get pods -n $(K8S_NAMESPACE)
	@echo ""
	@echo "💡 查看日志: make dev-logs-k8s"

# 重启 K8s 开发模式（重新构建镜像+部署）
dev-restart-k8s: dev-build-k8s
	@echo "☸️  重启 K8s 开发模式..."
	@kubectl apply -f k8s/manifests/namespace.yaml
	@kubectl apply -f k8s/manifests/serviceaccount.yaml
	@kubectl apply -f k8s/manifests/rcoder-configmap.yaml
	@kubectl delete pods -n $(K8S_NAMESPACE) -l app=rcoder --ignore-not-found
	@sed "s|image: rcoder:test|image: $(IMAGE)|" k8s/manifests/rcoder-deployment.yaml | kubectl apply -f -
	@kubectl rollout status deploy/rcoder -n $(K8S_NAMESPACE) --timeout=$(ROLLOUT_TIMEOUT)
	@echo "✅ K8s 部署已重启！"
	@echo "📋 查看状态: kubectl get pods -n $(K8S_NAMESPACE)"

# 停止 K8s 开发模式（与 dev-up-k8s / k8s/undeploy.sh 对称：Workload → 运行时 Pod → RBAC → Namespace）
dev-down-k8s:
	@echo "☸️  停止 K8s 开发模式..."
	@kubectl delete -f k8s/manifests/rcoder-deployment.yaml --ignore-not-found
	@kubectl delete -f k8s/manifests/rcoder-service.yaml --ignore-not-found
	@kubectl delete -f k8s/manifests/rcoder-configmap.yaml --ignore-not-found
	@echo "☸️  清理由 RCoder 运行时创建的 Pod（managed-by=rcoder-runtime）..."
	@kubectl get namespace $(K8S_NAMESPACE) >/dev/null 2>&1 && \
		kubectl delete pods -n $(K8S_NAMESPACE) -l managed-by=rcoder-runtime --ignore-not-found || true
	@echo "☸️  移除 ServiceAccount 与集群级 RBAC（ClusterRole / ClusterRoleBinding）..."
	@kubectl delete -f k8s/manifests/serviceaccount.yaml --ignore-not-found
	@echo "☸️  移除 Namespace $(K8S_NAMESPACE)..."
	@kubectl delete -f k8s/manifests/namespace.yaml --ignore-not-found
	@echo "✅ K8s 开发栈已从集群移除（含 $(K8S_NAMESPACE) 命名空间）"

# 查看 K8s 开发模式日志
dev-logs-k8s:
	@echo "☸️  查看 K8s 开发模式日志..."
	@kubectl logs -n $(K8S_NAMESPACE) -l app=rcoder -f

# ==================== 测试命令 ====================

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

# ============================================================================
# 📊 Pyroscope 持续剖析服务管理
# ============================================================================

# 启动 Pyroscope Server
pyroscope-up:
	@echo "🚀 启动 Pyroscope Server..."
	@if [ ! -f "docker/docker-compose.yml" ]; then \
		echo "❌ 错误: 未找到 docker/docker-compose.yml"; \
		exit 1; \
	fi
	@docker-compose -f docker/docker-compose.yml up -d pyroscope
	@echo ""
	@echo "✅ Pyroscope Server 已启动！"
	@echo "📊 Web UI: http://localhost:4040"
	@echo "💡 提示: 等待 agent_runner 容器启动并连接到 Pyroscope"

# 停止 Pyroscope Server
pyroscope-down:
	@echo "🛑 停止 Pyroscope Server..."
	@if [ -f "docker/docker-compose.yml" ]; then \
		docker-compose -f docker/docker-compose.yml stop pyroscope || true; \
	else \
		echo "⚠️  docker-compose.yml 未找到"; \
	fi
	@echo "✅ Pyroscope Server 已停止"

# 查看 Pyroscope 日志
pyroscope-logs:
	@echo "📋 Pyroscope Server 日志:"
	@docker logs -f rcoder-pyroscope 2>/dev/null || echo "❌ Pyroscope 容器未运行，请先执行 make pyroscope-up"
