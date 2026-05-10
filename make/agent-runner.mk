# ============================================================================
# agent_runner 本地开发测试服务
# ============================================================================

AGENT_RUNNER_PORT ?= 8286
AGENT_RUNNER_PROJECTS_DIR ?= ./project_workspace
AGENT_RUNNER_GRPC ?= no
AGENT_RUNNER_LOG_DIR ?= ./logs

ifeq ($(AGENT_RUNNER_GRPC),yes)
    AGENT_RUNNER_FEATURES := http-server,grpc-server
else
    AGENT_RUNNER_FEATURES := http-server
endif

agent-runner-up:
	@mkdir -p $(AGENT_RUNNER_LOG_DIR)
	@echo "🚀 启动 agent_runner (端口: $(AGENT_RUNNER_PORT), gRPC: $(AGENT_RUNNER_GRPC))..."
	@echo "📝 日志文件: $(AGENT_RUNNER_LOG_DIR)/agent-runner.log"
	@echo "💡 按 Ctrl+C 停止服务"
	@mkdir -p $(AGENT_RUNNER_PROJECTS_DIR)
	cargo run -p agent_runner \
		--no-default-features --features "$(AGENT_RUNNER_FEATURES)" \
		-- --port $(AGENT_RUNNER_PORT) --projects-dir $(AGENT_RUNNER_PROJECTS_DIR) \
		2>&1 | tee $(AGENT_RUNNER_LOG_DIR)/agent-runner.log

agent-runner-logs:
	@tail -f $(AGENT_RUNNER_LOG_DIR)/agent-runner.log

agent-runner-down:
	@echo "🛑 前台模式无需单独停止，按 Ctrl+C 即可"
