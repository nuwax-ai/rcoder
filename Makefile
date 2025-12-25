.PHONY: help build docker-build docker-build-master docker-build-agent-runner install install-agent uninstall dev-build dev-up dev-restart dev-down dev-logs update-image-tag

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
	@echo "  make docker-build-master          - 仅构建 master-rcoder 镜像"
	@echo "  make docker-build-agent-runner    - 仅构建 rcoder-agent-runner 镜像"
	@echo "  make dev-build                    - 本地编译 + 构建 Docker 镜像（一键完成）"
	@echo "  make update-image-tag - 根据系统架构更新镜像标签 (arm64/amd64 -> latest)"
	@echo ""
	@echo "🔧 开发模式命令："
	@echo "  make dev-up         - 启动开发模式容器（使用镜像内编译的二进制）"
	@echo "  make dev-restart    - 重启开发模式容器（重新构建镜像并启动）"
	@echo "  make dev-down       - 停止开发模式容器"
	@echo "  make dev-logs       - 查看开发模式容器日志"
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
docker-build: docker-build-master docker-build-agent-runner
	@echo "✅ 所有 Docker 镜像构建完成！"
	@echo "  ✓ master-rcoder:latest"
	@echo "  ✓ rcoder-agent-runner:latest"
	@echo ""
	@echo "🎯 使用方式："
	@echo "  docker run -d -p 8087:8087 master-rcoder:latest"

# 构建主服务镜像
docker-build-master:
	@echo "🐳 构建 master-rcoder 镜像..."
	@echo "📍 镜像名称: master-rcoder:latest"
	@echo "📦 使用 Dockerfile 多阶段构建（会在镜像内编译）..."
	@docker build -f docker/rcoder-master/Dockerfile -t master-rcoder:latest .
	@echo "✅ master-rcoder 镜像构建完成！"

# 构建 agent-runner 镜像
docker-build-agent-runner:
	@echo "🐳 构建 rcoder-agent-runner 镜像..."
	@echo "📍 镜像名称: rcoder-agent-runner:latest"
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
	@echo "📦 步骤3: 构建最终的 agent-runner 镜像..."
	@# 使用原本的 Dockerfile 和复制过来的二进制文件构建最终镜像
	@# CACHEBUST_NOVNC: 传入时间戳强制每次重新克隆 noVNC
	@cd docker/rcoder-agent-runner && \
		docker build --build-arg CACHEBUST_NOVNC=$$(date +%s) -f Dockerfile -t rcoder-agent-runner:latest .
	@echo "✅ rcoder-agent-runner 镜像构建完成！（GLIBC 2.36 兼容）"

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
	@echo "💡 代码更改已生效，因为重新构建了镜像！"
