# ============================================================================
# Docker Compose 开发模式
# ============================================================================

dev-build: docker-build
	@echo ""
	@echo "🎉 构建完成！"
	@echo "  ✓ Docker 镜像: dev-master-rcoder:latest"
	@echo "  ✓ Docker 镜像: dev-computer-agent-runner:latest"
	@echo ""
	@echo "💡 下一步: make dev-up 启动容器"

dev-up:
	@echo "🚀 启动开发模式容器服务..."
	@if [ ! -f "docker/docker-compose.yml" ]; then \
		echo "❌ 错误: 未找到 docker/docker-compose.yml"; \
		exit 1; \
	fi
	@echo "🔧 使用开发模式配置："
	@echo "  - 镜像: nuwax-docker-images-registry.cn-hangzhou.cr.aliyuncs.com/nuwax-test/dev-master-rcoder:latest"
	@echo "  - 启动命令: 直接执行 /app/rcoder"
	@docker-compose -f docker/docker-compose.yml up -d
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

# ============================================================================
# 容器内热编译（改 Rust 源码后秒级生效，替代 dev-restart）
# ============================================================================
# 前提：docker-compose.yml 已挂载源码到 /app/src（首次需 make dev-restart 应用）。
# 流程：容器内 cargo build --release --bin rcoder（增量）→ 替换 /app/bin/rcoder
#       → docker restart 拉起新 binary。
# tracing 火焰图模式（本地 cargo run）：flame feature；RCODER_FLAME 控制输出路径
# 用法: make run-flame RCODER_FLAME=logs/tracing.folded
# 事后: inferno-flamegraph < logs/tracing.folded > flame.svg（cargo install inferno）
run-flame:
	@echo "🔥 本地运行 rcoder（tracing 火焰图模式）..."
	@export RCODER_FLAME=${RCODER_FLAME:-logs/tracing.folded}; 	cargo run -p rcoder --features flame; 	echo "📊 folded: $"

# tokio-console 观测模式（本地 cargo run）：feature+RUSTFLAGS+独立 target 三件套
# 连接: cargo install tokio-console && tokio-console localhost:6669
run-console:
	@echo "🖥️  本地运行 rcoder（tokio-console 观测模式，端口 6669）..."
	@RUSTFLAGS="--cfg tokio_unstable" CARGO_TARGET_DIR=target-console \
	cargo run -p rcoder --features console

dev-hot:
	@echo "🔥 容器内热编译 rcoder（DEV_FLAME 默认开启，DEV_FLAME=0 关闭）..."
	@DEV_CID=$$(docker-compose -f docker/docker-compose.yml ps -q rcoder); \
	if [ -z "$$DEV_CID" ]; then \
		echo "❌ rcoder 容器未运行，请先 make dev-up"; exit 1; \
	fi; \
	docker exec -e DEV_CONSOLE="$${DEV_CONSOLE:-0}" -e DEV_FLAME="$${DEV_FLAME:-1}" $$DEV_CID bash /app/src/docker/dev-hot-build.sh && \
	echo "🔄 重启 rcoder 进程（拉起新 binary）..." && \
	docker restart $$DEV_CID >/dev/null && \
	echo "✅ 热编译完成（日志: docker logs -f $$DEV_CID）"
	@echo "🔥 火焰图启用: RCODER_FLAME=/app/logs/flame.folded docker compose -f docker/docker-compose.yml up -d rcoder"
