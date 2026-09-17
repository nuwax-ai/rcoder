# ============================================================================
# Docker Compose 开发模式
# ============================================================================

dev-build: docker-build
	@echo ""
	@echo "🎉 构建完成！"
	@echo "  ✓ Docker 镜像: dev-master-rcoder:latest"
	@echo "  ✓ Docker 镜像: dev-rcoder-agent-runner:latest"
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
		docker-compose -f docker/docker-compose.yml down || exit $$?; \
		docker-compose -f docker/docker-compose.yml up -d || exit $$?; \
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
# dial9 恒编入（feature hotpath,dial9 + tokio_unstable，见 dev-hot-build.sh）。
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
	echo "🔬 dial9 恒编入（运行期默认关）：make dial9-on 启用 / make dial9-off 关闭 / make dial9-view 离线查看 trace"

## 启用 dial9 事件级 Tokio tracing（重建 rcoder 容器注入 DIAL9_ENABLED=1；
## binary 恒编入 dial9 feature，复用 target-unstable volume 编译产物，不触发
## 重编。trace 落宿主 docker/logs/dial9；agent 容器同步透传（仅新建容器生效，
## 已有 agent 容器需重建）。
dial9-on:
	@DIAL9_ENABLED=1 docker-compose -f docker/docker-compose.yml up -d rcoder && \
	echo "🔬 dial9 已启用：trace 落 docker/logs/dial9（60s 轮转分段）；make dial9-view 打开 viewer"

## 关闭 dial9 记录（重建容器 DIAL9_ENABLED=0；recorder 纯 passthrough 零开销）
dial9-off:
	@DIAL9_ENABLED=0 docker-compose -f docker/docker-compose.yml up -d rcoder && \
	echo "✅ dial9 已关闭（纯 passthrough，零开销）"

## 启动 dial9 单二进制 viewer 离线查看本地 trace（需本机 `cargo binstall dial9`）
dial9-view:
	@dial9 serve --local-dir ./docker/logs/dial9

## 查看开发模式容器日志（rcoder + 全部关联服务，跟随输出）
dev-logs:
	@echo "📋 开发模式容器日志（Ctrl+C 退出）:"
	@docker-compose -f docker/docker-compose.yml logs -f
