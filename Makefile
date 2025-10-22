.PHONY: help build dev install install-agent uninstall docker-build docker-up docker-down docker-logs docker-restart docker-clean docker-test

# 默认目标：显示帮助信息
help:
	@echo "rcoder 项目 Makefile"
	@echo ""
	@echo "可用命令："
	@echo "  make build          - 安装 codex-acp-agent + 编译项目（release）"
	@echo "  make dev            - 安装 codex-acp-agent + 编译项目（debug）"
	@echo "  make install        - 安装所有二进制到 ~/.cargo/bin/"
	@echo "  make install-agent  - 仅安装 codex-acp-agent"
	@echo "  make uninstall      - 卸载所有二进制"
	@echo ""
	@echo "Docker 相关命令："
	@echo "  make docker-build   - 构建 rcoder Docker 镜像"
	@echo "  make docker-up      - 启动 rcoder 容器服务"
	@echo "  make docker-down    - 停止 rcoder 容器服务"
	@echo "  make docker-logs    - 查看容器服务日志"
	@echo "  make docker-restart - 重启 rcoder 容器服务"
	@echo "  make docker-clean   - 清理 Docker 资源"
	@echo "  make docker-test    - 完整测试（构建+启动+健康检查）"
	@echo ""

# 完整构建（release）
build: install-agent
	@echo "🔨 编译 rcoder（release 模式）..."
	@cargo build --release --bin rcoder
	@echo ""
	@echo "✅ 构建完成！"
	@echo "运行: ./target/release/rcoder"

# 开发构建（debug）
dev: install-agent
	@echo "🔨 编译 rcoder（debug 模式）..."
	@cargo build --bin rcoder
	@echo ""
	@echo "✅ 开发构建完成！"
	@echo "运行: cargo run --bin rcoder"

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

# Docker 相关命令
docker-build:
	@echo "🐳 构建 rcoder Docker 镜像..."
	@if [ ! -f "docker/Dockerfile" ]; then \
		echo "❌ 错误: 未找到 docker/Dockerfile"; \
		exit 1; \
	fi
	@echo "📦 构建 rcoder 二进制..."
	@cargo build --release --bin rcoder
	@echo "🐳 构建 Docker 镜像..."
	@docker build -t rcoder:latest -f docker/Dockerfile .
	@echo "✅ Docker 镜像构建成功: rcoder:latest"

docker-up:
	@echo "🚀 启动 rcoder 容器服务..."
	@if [ ! -f "docker/docker-compose.yml" ]; then \
		echo "❌ 错误: 未找到 docker/docker-compose.yml"; \
		exit 1; \
	fi
	@docker-compose -f docker/docker-compose.yml up -d
	@echo "📋 服务状态:"
	@docker-compose -f docker/docker-compose.yml ps

docker-down:
	@echo "🛑 停止 rcoder 容器服务..."
	@if [ -f "docker/docker-compose.yml" ]; then \
		docker-compose -f docker/docker-compose.yml down; \
	else \
		echo "⚠️  docker-compose.yml 未找到，跳过停止操作"; \
	fi

docker-logs:
	@echo "📋 查看服务日志..."
	@if [ -f "docker/docker-compose.yml" ]; then \
		docker-compose -f docker/docker-compose.yml logs -f; \
	else \
		echo "❌ 错误: 未找到 docker/docker-compose.yml"; \
		exit 1; \
	fi

docker-restart:
	@echo "🔄 重启 rcoder 容器服务..."
	@if [ -f "docker/docker-compose.yml" ]; then \
		docker-compose -f docker/docker-compose.yml restart; \
		echo "✅ 服务重启完成"; \
		docker-compose -f docker/docker-compose.yml ps; \
	else \
		echo "❌ 错误: 未找到 docker/docker-compose.yml"; \
		exit 1; \
	fi

docker-clean:
	@echo "🧹 清理 Docker 资源..."
	@if [ -f "docker/docker-compose.yml" ]; then \
		docker-compose -f docker/docker-compose.yml down -v --remove-orphans; \
	fi
	@docker image prune -f
	@echo "✅ 清理完成"

docker-test: docker-build docker-up
	@echo "🧪 等待服务启动..."
	@sleep 5
	@echo "🔍 检查服务状态..."
	@curl -f http://localhost:8086/health || (echo "❌ 服务健康检查失败" && docker-compose -f docker/docker-compose.yml logs && exit 1)
	@echo "✅ 服务启动成功并正常运行"
	@echo "📋 管理命令:"
	@echo " 查看日志: make docker-logs"
	@echo " 重启服务: make docker-restart"
	@echo " 停止服务: make docker-down"
	@echo " 清理资源: make docker-clean"
