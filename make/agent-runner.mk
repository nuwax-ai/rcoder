# ============================================================================
# agent_runner 本地开发测试服务
# ============================================================================

AGENT_RUNNER_PORT ?= 8286
AGENT_RUNNER_PROJECTS_DIR ?= ./project_workspace
AGENT_RUNNER_GRPC ?= no
AGENT_RUNNER_LOG_DIR ?= ./logs
# 代理端口（可通过环境变量 RCODER_PROXY_PORT 覆盖）
AGENT_RUNNER_PROXY_PORT ?= 8088

# 默认启用 proxy feature，使 API Key 和 Base URL 通过 Pingora 代理注入真实值
AGENT_RUNNER_PROXY ?= yes

ifeq ($(AGENT_RUNNER_GRPC),yes)
    AGENT_RUNNER_FEATURES := http-server,grpc-server
else
    AGENT_RUNNER_FEATURES := http-server
endif

# 根据 AGENT_RUNNER_PROXY 决定是否启用 proxy feature
ifeq ($(AGENT_RUNNER_PROXY),yes)
    AGENT_RUNNER_FEATURES := $(AGENT_RUNNER_FEATURES),proxy
endif

# 根据 AGENT_RUNNER_PROXY 决定是否启用 --enable-proxy
# 端口可通过环境变量 RCODER_PROXY_PORT 覆盖（默认 8088）
AGENT_RUNNER_PROXY_FLAG := $(if $(filter yes,$(AGENT_RUNNER_PROXY)),--enable-proxy --proxy-port $(AGENT_RUNNER_PROXY_PORT),)

agent-runner-up:
	@mkdir -p $(AGENT_RUNNER_LOG_DIR)
	@echo "🚀 启动 agent_runner (端口: $(AGENT_RUNNER_PORT), gRPC: $(AGENT_RUNNER_GRPC), proxy: $(AGENT_RUNNER_PROXY))..."
	@echo "📝 日志文件: $(AGENT_RUNNER_LOG_DIR)/agent-runner.log"
	@echo "💡 按 Ctrl+C 停止服务"
	@mkdir -p $(AGENT_RUNNER_PROJECTS_DIR)
	cargo run -p agent_runner \
		--no-default-features --features "$(AGENT_RUNNER_FEATURES)" \
		-- --port $(AGENT_RUNNER_PORT) --projects-dir $(AGENT_RUNNER_PROJECTS_DIR) $(AGENT_RUNNER_PROXY_FLAG) \
		2>&1 | tee $(AGENT_RUNNER_LOG_DIR)/agent-runner.log

agent-runner-logs:
	@tail -f $(AGENT_RUNNER_LOG_DIR)/agent-runner.log

agent-runner-down:
	@echo "🛑 前台模式无需单独停止，按 Ctrl+C 即可"
