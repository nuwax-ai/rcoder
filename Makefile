.PHONY: help build docker-build install install-agent uninstall dev-build dev-up dev-restart dev-down dev-logs

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
	@echo "  make docker-build   - 仅构建 Docker 镜像"
	@echo "  make dev-build      - 本地编译 + 构建 Docker 镜像（一键完成）"
	@echo ""
	@echo "🔧 开发模式命令："
	@echo "  make dev-up         - 启动开发模式容器（挂载本地编译的可执行文件）"
	@echo "  make dev-restart    - 重启开发模式容器（重新编译并启动）"
	@echo "  make dev-restart-full - 完整重启（重新构建镜像+启动，确保代码更改生效）"
	@echo "  make dev-down       - 停止开发模式容器"
	@echo "  make dev-logs       - 查看开发模式容器日志"
	@echo ""
	@echo "开发模式工作流程："
	@echo "  1. make dev-build    # 首次：编译+构建镜像"
	@echo "  2. make dev-up       # 启动容器"
	@echo "  3. 修改代码后: make dev-restart  # 快速：仅编译+重启容器"
	@echo ""
	@echo "💡 提示："
	@echo "  - dev-build: 首次使用，会构建 Docker 镜像（较慢）"
	@echo "  - dev-restart: 日常开发，只编译+重启（快速迭代）"
	@echo ""

# 本地编译（仅编译，不构建镜像）
build:
	@echo "🔨 本地编译 rcoder..."
	@cargo build --release --bin rcoder
	@echo "✅ 编译完成！"
	@echo "可执行文件: ./target/release/rcoder"

# Docker 镜像构建（仅构建镜像，不编译）
docker-build:
	@echo "🐳 构建 Docker 镜像..."
	@echo "📍 镜像名称: master-rcoder:latest"
	@echo "📦 使用 Dockerfile 多阶段构建（会在镜像内编译）..."
	@docker build -f docker/Dockerfile -t master-rcoder:latest .
	@echo "✅ Docker 镜像构建完成！"
	@echo ""
	@echo "🎯 使用方式："
	@echo "  docker run -d -p 8087:8087 master-rcoder:latest"

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

# 开发模式：本地编译 + Docker 镜像构建（一键完成）
dev-build:
	@echo "🔨 [1/2] 本地编译 rcoder..."
	@cargo build --release --bin rcoder
	@echo "📁 复制可执行文件到 docker 目录..."
	@cp ./target/release/rcoder ./docker/rcoder
	@chmod +x ./docker/rcoder
	@echo "✅ 本地编译完成！"
	@echo ""
	@echo "🐳 [2/2] 构建 Docker 镜像..."
	@docker build -f docker/Dockerfile -t master-rcoder:latest .
	@echo ""
	@echo "🎉 全部完成！"
	@echo "  ✓ 本地可执行文件: ./docker/rcoder"
	@echo "  ✓ Docker 镜像: master-rcoder:latest"
	@echo ""
	@echo "💡 下一步: make dev-up 启动容器"



dev-up:
	@echo "🚀 启动开发模式容器服务..."
	@if [ ! -f "docker/docker-compose.yml" ]; then \
		echo "❌ 错误: 未找到 docker/docker-compose.yml"; \
		exit 1; \
	fi
	@if [ ! -f "docker/rcoder" ]; then \
		echo "⚠️  警告: 未找到 ./docker/rcoder，请先运行 'make dev-build'"; \
	fi
	@echo "🔧 使用开发模式配置："
	@echo "  - 可执行文件: ./docker/rcoder (本地编译)"
	@echo "  - 启动命令: 直接执行 /app/rcoder"
	@RCODER_IMAGE=master-rcoder:latest \
	RCODER_MODE=dev \
	RCODER_DEV_VOLUME="" \
	RCODER_COMMAND='["/app/rcoder"]' \
	docker-compose -f docker/docker-compose.yml up -d
	@echo "📋 开发模式服务状态:"
	@RCODER_MODE=dev docker-compose -f docker/docker-compose.yml ps
	@echo "📝 开发模式特点："
	@echo "  - 挂载本地编译的 rcoder 可执行文件"
	@echo "  - 代码修改后只需运行 'make dev-restart'"
	@echo "  - dev-restart 只编译+重启，无需重新构建镜像（快速）"

dev-down:
	@echo "🛑 停止开发模式容器服务..."
	@if [ -f "docker/docker-compose.yml" ]; then \
		RCODER_MODE=dev docker-compose -f docker/docker-compose.yml down; \
	else \
		echo "⚠️  docker-compose.yml 未找到，跳过停止操作"; \
	fi

dev-logs:
	@echo "📋 查看开发模式服务日志..."
	@if [ -f "docker/docker-compose.yml" ]; then \
		RCODER_MODE=dev docker-compose -f docker/docker-compose.yml logs -f; \
	else \
		echo "❌ 错误: 未找到 docker/docker-compose.yml"; \
		exit 1; \
	fi


# 快速重启：依赖 dev-build 确保代码更改生效
dev-restart: dev-build
	@echo "🔄 重启容器服务（使用最新构建的镜像）..."
	@if [ -f "docker/docker-compose.yml" ]; then \
		RCODER_MODE=dev docker-compose -f docker/docker-compose.yml down; \
		RCODER_MODE=dev docker-compose -f docker/docker-compose.yml up -d; \
		echo "✅ 容器已重启！"; \
	else \
		echo "❌ 错误: 未找到 docker-compose.yml"; \
		exit 1; \
	fi
	@echo ""
	@echo "🎉 完整重启完成！"
	@echo "💡 代码更改已生效，因为重新构建了镜像！"
