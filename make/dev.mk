# ============================================================================
# Docker Compose 开发模式
# ============================================================================

# 统一叠加 turso-volume overlay：/app/data 切为 named volume，SQLite/Turso WAL
# 不再逐笔写穿 virtiofs（每次 fsync 都放大为宿主 FSEvents）。
# base compose 保持 bind 形态不变；契约校验见 tests-e2e/tools/turso_compose_contract.py。
RCODER_COMPOSE = docker-compose -f docker/docker-compose.yml -f docker/docker-compose.turso-volume.yml

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
	@$(RCODER_COMPOSE) up -d
	@echo "📋 开发模式服务状态:"
	@$(RCODER_COMPOSE) ps

dev-down:
	@echo "🛑 停止开发模式容器服务..."
	@if [ -f "docker/docker-compose.yml" ]; then \
		$(RCODER_COMPOSE) down; \
	else \
		echo "⚠️  docker-compose.yml 未找到，跳过停止操作"; \
	fi

# 快速重启：依赖 dev-build 确保代码更改生效
dev-restart: dev-build
	@echo "🔄 重启容器服务（使用最新构建的镜像）..."
	@if [ -f "docker/docker-compose.yml" ]; then \
		$(RCODER_COMPOSE) down || exit $$?; \
		$(RCODER_COMPOSE) up -d || exit $$?; \
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
	@DEV_CID=$$($(RCODER_COMPOSE) ps -q rcoder); \
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
	@DIAL9_ENABLED=1 $(RCODER_COMPOSE) up -d rcoder && \
	echo "🔬 dial9 已启用：trace 落 docker/logs/dial9（60s 轮转分段）；make dial9-view 打开 viewer"

## 关闭 dial9 记录（重建容器 DIAL9_ENABLED=0；recorder 纯 passthrough 零开销）
dial9-off:
	@DIAL9_ENABLED=0 $(RCODER_COMPOSE) up -d rcoder && \
	echo "✅ dial9 已关闭（纯 passthrough，零开销）"

## 启动 dial9 单二进制 viewer 离线查看本地 trace（需本机 `cargo binstall dial9`）
dial9-view:
	@dial9 serve --local-dir ./docker/logs/dial9

## 查看开发模式容器日志（rcoder + 全部关联服务，跟随输出）
dev-logs:
	@echo "📋 开发模式容器日志（Ctrl+C 退出）:"
	@$(RCODER_COMPOSE) logs -f

## 清理本地 dev compose 的历史大文件（可重复执行）：
## 1) docker/logs/rcoder.<日期> 按天日志，保留最新一份（活跃文件在被追加，
##    删除也不释放空间且影响滚动）；
## 2) 已迁入 named volume 的一次性旧目录：docker/data/rcoder、docker/computer-cache。
##    若 rcoder 容器正以 bind 形态挂载它们（未叠加 turso-volume overlay 的
##    base compose 直跑形态）则拒绝执行，防止误删活跃数据。
dev-clean:
	@echo "🧹 [1/2] 清理 docker/logs 按天历史日志（保留最新一份）..."
	@if [ -d docker/logs ]; then \
		files=$$(ls -t docker/logs/rcoder.20* 2>/dev/null | tail -n +2); \
		if [ -n "$$files" ]; then \
			echo "$$files" | xargs rm -v; \
		else \
			echo "  无历史日志可清理"; \
		fi; \
	else \
		echo "  docker/logs 不存在，跳过"; \
	fi
	@echo "🧹 [2/2] 清理已迁入 volume 的旧目录（docker/data/rcoder、docker/computer-cache）..."
	@if docker inspect rcoder-rcoder-1 >/dev/null 2>&1 && \
		docker inspect rcoder-rcoder-1 --format '{{range .Mounts}}{{.Source}}{{"\n"}}{{end}}' 2>/dev/null \
			| grep -q -e 'docker/data/rcoder' -e 'docker/computer-cache'; then \
		echo "❌ rcoder 容器正以 bind 挂载待清理目录（未叠加 turso-volume overlay），拒绝清理"; \
		exit 1; \
	fi; \
	for d in docker/data/rcoder docker/computer-cache; do \
		if [ -e "$$d" ]; then \
			du -sh "$$d"; rm -rf "$$d"; \
		else \
			echo "  $$d 不存在，跳过"; \
		fi; \
	done; \
	echo "✅ 清理完成"
