# ============================================================================
# Docker Compose 开发模式
# ============================================================================

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
	@echo "  - 镜像: nuwax-docker-images-registry.cn-hangzhou.cr.aliyuncs.com/dev/master-rcoder:latest"
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
