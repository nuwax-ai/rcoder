.PHONY: help build docker-build docker-build-base docker-build-master docker-build-master-base docker-build-agent-runner docker-build-agent-base install install-agent uninstall dev-build dev-up dev-restart dev-down dev-logs update-image-tag test test-unit test-integration test-all test-blocking

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
	@echo "  make docker-build-agent-runner    - 仅构建 rcoder-agent-runner 镜像（需要基础镜像）"
	@echo "  make docker-build-agent-base      - 仅构建 rcoder-agent-base 基础镜像"
	@echo "  make dev-build                    - 本地编译 + 构建 Docker 镜像（一键完成）"
	@echo "  make update-image-tag - 根据系统架构更新镜像标签 (arm64/amd64 -> latest)"
	@echo ""
	@echo "🔧 开发模式命令："
	@echo "  make dev-up         - 启动开发模式容器（使用镜像内编译的二进制）"
	@echo "  make dev-restart    - 重启开发模式容器（重新构建镜像并启动）"
	@echo "  make dev-down       - 停止开发模式容器"
	@echo "  make dev-logs       - 查看开发模式容器日志"
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
# 使用 & 并行构建独立的镜像，提升构建速度
docker-build:
	@echo "🚀 开始并行构建 Docker 镜像..."
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
# 使用 & 并行构建独立的基础镜像
docker-build-base:
	@echo "🚀 开始并行构建基础镜像..."
	@$(MAKE) docker-build-master-base & \
	$(MAKE) docker-build-agent-base & \
	wait
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
	@docker build -f docker/rcoder-master/Dockerfile -t master-rcoder:latest .
	@echo "✅ master-rcoder 镜像构建完成！"

# 构建 master-base 基础镜像（包含所有运行时依赖，很少需要重新构建）
docker-build-master-base:
	@echo "🐳 构建 master-rcoder-base 基础镜像..."
	@echo "📍 镜像名称: master-rcoder-base:latest"
	@echo "⏳ 这可能需要较长时间（包含所有运行时依赖安装）..."
	@docker build -f docker/rcoder-master/Dockerfile.base -t master-rcoder-base:latest .
	@echo "✅ master-rcoder-base 基础镜像构建完成！"
	@echo "💡 提示: 平时开发只需运行 make dev-restart，无需重新构建基础镜像"

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
	@# 使用 debian:12 + Rust 1.90 构建，GLIBC 版本与运行环境一致
	@docker build -f docker/rcoder-agent-runner/Dockerfile.build -t rcoder-agent-runner-build .
	@echo "📦 步骤2: 复制二进制文件到 agent-runner 目录..."
	@# 创建容器并复制 agent_runner 二进制文件
	@mkdir -p docker/rcoder-agent-runner/bin
	@docker create --name build-container rcoder-agent-runner-build
	@docker cp build-container:/build/target/release/agent_runner docker/rcoder-agent-runner/bin/
	@docker rm build-container
	@docker rmi rcoder-agent-runner-build
	@echo "📦 步骤3: 构建最终的 agent-runner 镜像（基于基础镜像，快速）..."
	@cd docker/rcoder-agent-runner && \
		docker build -f Dockerfile -t rcoder-agent-runner:latest .
	@echo "✅ rcoder-agent-runner 镜像构建完成！"

# 构建 agent-base 基础镜像（包含所有系统依赖，很少需要重新构建）
docker-build-agent-base:
	@echo "🐳 构建 rcoder-agent-base 基础镜像..."
	@echo "📍 镜像名称: rcoder-agent-base:latest"
	@echo "⏳ 这可能需要较长时间（包含所有系统依赖安装）..."
	@# CACHEBUST_NOVNC: 传入时间戳强制每次重新克隆 noVNC
	@cd docker/rcoder-agent-runner && \
		docker build --build-arg CACHEBUST_NOVNC=$$(date +%s) -f Dockerfile.base -t rcoder-agent-base:latest .
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
