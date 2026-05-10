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
