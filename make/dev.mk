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
# tokio-console 观测模式（本地 cargo run）：feature+RUSTFLAGS+独立 target 三件套
# 连接: cargo install tokio-console && tokio-console localhost:6669
run-console:
	@echo "🖥️  本地运行 rcoder（tokio-console 观测模式，端口 6669）..."
	@RUSTFLAGS="--cfg tokio_unstable" CARGO_TARGET_DIR=target-console \
	cargo run -p rcoder --features console

dev-hot:
	@echo "🔥 容器内热编译 rcoder..."
	@DEV_CID=$$(docker-compose -f docker/docker-compose.yml ps -q rcoder); \
	if [ -z "$$DEV_CID" ]; then \
		echo "❌ rcoder 容器未运行，请先 make dev-up"; exit 1; \
	fi; \
	docker exec $$DEV_CID bash /app/src/docker/dev-hot-build.sh && \
	echo "🔄 重启 rcoder 进程（拉起新 binary）..." && \
	docker restart $$DEV_CID >/dev/null && \
	echo "✅ 热编译完成（日志: docker logs -f $$DEV_CID）" && \
	echo "🖥️  tokio-console 恒编入（运行期默认关）：make console-on 启用 / make console-off 关闭 / make console 连接面板"

## 启用 tokio-console（重建 rcoder 容器注入 DEV_CONSOLE=1；binary 复用
## target-console volume 编译产物，不触发重编。console-subscriber 无背压
## 记账，rcoder 后台事件量下 RSS 会持续爬升——观测完记得 console-off）
console-on:
	@DEV_CONSOLE=1 docker-compose -f docker/docker-compose.yml up -d rcoder && \
	echo "🖥️  tokio-console 已启用：make console 连接面板（localhost:6669）"

## 关闭 tokio-console（重建容器 DEV_CONSOLE=0；EnvFilter 拦截 tokio/runtime
## trace 事件不进 Registry，内存回到常态水位）
console-off:
	@DEV_CONSOLE=0 docker-compose -f docker/docker-compose.yml up -d rcoder && \
	echo "✅ tokio-console 已关闭（内存回落常态水位）"

## 连接本地 dev 容器的 tokio-console TUI 面板（6669 已随 compose 映射宿主）
console:
	@tokio-console localhost:6669
