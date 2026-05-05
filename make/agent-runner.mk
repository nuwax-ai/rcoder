# ============================================================================
# agent_runner 本地开发测试服务
# ============================================================================

AGENT_RUNNER_PORT ?= 8086
AGENT_RUNNER_PROJECTS_DIR ?= ./project_workspace
AGENT_RUNNER_GRPC ?= yes

ifeq ($(AGENT_RUNNER_GRPC),yes)
    AGENT_RUNNER_FEATURES := http-server,grpc-server
else
    AGENT_RUNNER_FEATURES := http-server
endif

agent-runner-up:
	@echo "🚀 启动 agent_runner (端口: $(AGENT_RUNNER_PORT), gRPC: $(AGENT_RUNNER_GRPC))..."
	@echo "💡 按 Ctrl+C 停止服务"
	@mkdir -p $(AGENT_RUNNER_PROJECTS_DIR)
	cargo run -p agent_runner \
		--features "$(AGENT_RUNNER_FEATURES)" \
		-- --port $(AGENT_RUNNER_PORT) --projects-dir $(AGENT_RUNNER_PROJECTS_DIR)

agent-runner-logs:
	@echo "⚠️  前台模式无单独日志，请查看上方输出"

agent-runner-down:
	@echo "🛑 前台模式无需单独停止，按 Ctrl+C 即可"
