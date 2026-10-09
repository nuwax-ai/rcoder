AGENT_BASE_IMAGE ?= dev-rcoder-agent-base:latest
# ============================================================================
# Docker 镜像构建
# ============================================================================

# 镜像推送开关：构建后是否自动推送到阿里云仓库
#   - false（默认）: 仅本地构建，不推送，适合 make dev-restart 快速构建
#   - true          : 构建完成后自动推送
# 用法: make dev-restart PUSH_IMAGE=true
PUSH_IMAGE ?= false

# Buildx 远程 builder（留空=本地 docker build；CI/远程构建时设为 nuwax-clusters 等）
# 用法: make dev-restart BUILDX_BUILDER=nuwax-clusters
BUILDX_BUILDER ?=

# 默认从本次 app-cli 源码契约读取，不重复维护镜像版本。
# 显式构建参数仍由版本门禁核验，不允许覆盖成不配对的二进制。
PINGAP_VERSION ?= $(shell python3 tools/build/pingap_identity.py --field version)
PINGAP_COMMIT ?= $(shell python3 tools/build/pingap_identity.py --field commit)

# Docker 镜像构建（仅构建镜像，不编译）
# 串行构建镜像，避免资源竞争
docker-build:
	@# Pingap 版本一致性门禁（batch8-followup §5）：镜像内 pingap 二进制与
	@# app-cli 链接的 pingap-config 序列化不一致 → config_hash 确认恒失败
	@#（2026-09-19 第八批根因）。四个构建入口对齐 devtool.rs 单一事实源。
	@python3 k8s/scripts/pingap_version_gate.py || exit 1
	@echo "🔨 依次构建 agent-runner 和主镜像..."
	@$(MAKE) docker-build-agent-runner
	@$(MAKE) docker-build-master
	@echo ""
	@echo "✅ 所有 Docker 镜像构建完成！"
	@echo "  ✓ dev-master-rcoder:latest"
	@echo "  ✓ dev-rcoder-agent-runner:latest"
	@echo ""
	@echo "🎯 使用方式："
	@echo "  docker run -d -p 8087:8087 dev-master-rcoder:latest"

# 构建所有基础镜像（很少需要，只有修改系统依赖时才需要）
# 串行构建基础镜像，避免资源竞争
docker-build-base: docker-build-master-base docker-build-agent-base
	@echo ""
	@echo "✅ 所有基础镜像构建完成！"
	@echo "  ✓ dev-master-rcoder-base:latest"
	@echo "  ✓ dev-rcoder-agent-base:latest"
	@echo ""
	@echo "💡 提示: 平时开发只需运行 make dev-restart，无需重新构建基础镜像"

# 构建主服务镜像（基于基础镜像，快速构建）
docker-build-master:
	@echo "🐳 构建 master-rcoder 镜像..."
	@echo "📍 镜像名称: dev-master-rcoder:latest"
	@# 检查基础镜像是否存在
	@if ! docker image inspect dev-master-rcoder-base:latest >/dev/null 2>&1; then \
		echo "⚠️  基础镜像 dev-master-rcoder-base:latest 不存在，先构建基础镜像..."; \
		$(MAKE) docker-build-master-base; \
	else \
		echo "✓ 基础镜像 dev-master-rcoder-base:latest 已存在"; \
	fi
	@python3 docker/master-base-contract.py check dev-master-rcoder-base:latest
	@echo "📦 使用 Dockerfile 多阶段构建（基于基础镜像）..."
	@# 🔧 根据 CARGO_FEATURES 决定调试 feature 集；dial9 在列时附带 tokio_unstable
	@(if [ "$(CARGO_FEATURES)" != "" ]; then \
		MASTER_CARGO_FLAGS="$(CARGO_FEATURES)"; \
		echo "🔧 master-rcoder 将启用调试 features"; \
	else \
		MASTER_CARGO_FLAGS=""; \
		echo "🔒 master-rcoder 生产模式（无调试 features）"; \
	fi; \
	if echo "$(CARGO_FEATURES)" | grep -q "dial9"; then \
		DIAL9_RUSTFLAGS="--cfg tokio_unstable"; \
	else \
		DIAL9_RUSTFLAGS=""; \
	fi; \
	docker build \
		--build-arg BASE_IMAGE=dev-master-rcoder-base:latest \
		--build-arg CARGO_FLAGS="$$MASTER_CARGO_FLAGS" \
		--build-arg RUSTFLAGS="$$DIAL9_RUSTFLAGS" \
		--build-arg CACHEBUST=$(AGENT_TOOLS_CACHE_KEY) \
		-f docker/rcoder-master/Dockerfile -t dev-master-rcoder:latest .;)
	@echo "✅ master-rcoder 镜像构建完成！"
	@if [ "$(PUSH_IMAGE)" = "true" ]; then \
		echo "📤 推送镜像到阿里云仓库..."; \
		skopeo copy docker-daemon:dev-master-rcoder:latest docker://nuwax-docker-images-registry.cn-hangzhou.cr.aliyuncs.com/nuwax-test/dev-master-rcoder:latest; \
		echo "✅ 镜像已推送: nuwax-docker-images-registry.cn-hangzhou.cr.aliyuncs.com/nuwax-test/dev-master-rcoder:latest"; \
	else \
		echo "⏭️  跳过镜像推送（PUSH_IMAGE != true）。如需推送：make ... PUSH_IMAGE=true"; \
	fi

# 构建 master-base 基础镜像（包含所有运行时依赖，很少需要重新构建）
docker-build-master-base:
	@echo "🐳 构建 master-rcoder-base 基础镜像..."
	@echo "📍 镜像名称: dev-master-rcoder-base:latest"
	@echo "⏳ 这可能需要较长时间（包含所有运行时依赖安装）..."
	@docker build --build-arg RCODER_MASTER_BASE_SOURCE_SHA=$$(python3 docker/master-base-contract.py fingerprint) \
		-f docker/rcoder-master/Dockerfile.base -t dev-master-rcoder-base:latest .
	@echo "✅ master-rcoder-base 基础镜像构建完成！"
	@if [ "$(PUSH_IMAGE)" = "true" ]; then \
		echo "📤 推送基础镜像到阿里云仓库..."; \
		skopeo copy docker-daemon:dev-master-rcoder-base:latest docker://nuwax-docker-images-registry.cn-hangzhou.cr.aliyuncs.com/nuwax-test/dev-master-rcoder-base:latest; \
		echo "✅ 基础镜像已推送: nuwax-docker-images-registry.cn-hangzhou.cr.aliyuncs.com/nuwax-test/dev-master-rcoder-base:latest"; \
	else \
		echo "⏭️  跳过基础镜像推送（PUSH_IMAGE != true）。如需推送：make ... PUSH_IMAGE=true"; \
	fi
	@echo "💡 提示: 平时开发只需运行 make dev-restart，无需重新构建基础镜像"

# ============================================================================
# Cargo feature 配置
# ============================================================================
# 开发模式：启用调试、监控和追踪（默认不含 agent_runner 的 proxy，见下文）
# ⚠️  注意：添加新的调试 feature 时，必须同步更新此列表！
#
# 当前启用的调试 features：
#   - otel          (agent_runner):         OpenTelemetry 追踪
#   - debug         (rcoder):               调试路由
#   - hotpath       (rcoder, agent_runner): 本地性能剖析（本地 dev 默认开启；见下方说明与 AGENTS.md）
#   - dial9         (rcoder, agent_runner): 事件级 Tokio tracing（运行期 DIAL9_ENABLED 开关，
#                                           见 docs/observability.md dial9 节）
#   - proxy         (agent_runner):         Pingora + 模型密钥代理（可选；见下方说明）
#   - kubernetes    (rcoder, docker_manager): Kubernetes 运行时支持
#   - http-server   (agent_runner):         HTTP REST API 服务（默认启用）
#   - grpc-server   (agent_runner):         gRPC 服务（默认启用）
#
# dial9 feature 在 features 列表中时，下方构建步骤自动附带 RUSTFLAGS="--cfg
# tokio_unstable"（全量 task 覆盖必需；生产 CARGO_FEATURES 为空则不传，零影响）。
# ebpf-debug/pyroscope 已随 Pyroscope/eBPF 链下线移除（2026-09 批次1）。
#
# proxy 默认关闭：子进程会收到真实 MODEL_PROVIDER API key/base_url（如 nuwax-codex-acp 本地鉴权）。
# 需要密钥经 Pingora 注入时，构建前设置例如：
#   make dev-restart CARGO_FEATURES='--features otel,debug,proxy'
#
# hotpath 默认开启基础档（函数耗时/路由剖析/runtime 指标；容器内 6770/6771 绑 127.0.0.1，
# 观测方式见 AGENTS.md「AI 调试路由」）。按需叠加 MCP 档，构建前设置例如：
#   make dev-restart CARGO_FEATURES='--features otel,debug,hotpath,hotpath-mcp'
# 生产构建（build-agent-docker）不含 hotpath/dial9，零影响。
#
# 本地开发调试默认开启上述功能（http-server / grpc-server 仍由 agent_runner 默认 features 提供）
# 注意：kubernetes feature 仅用于 K8s 环境，Docker Compose 模式不要启用
CARGO_FEATURES ?= --features otel,debug,hotpath,dial9
# Explicitly bump to refresh external agent tools; routine Rust builds reuse them.
AGENT_TOOLS_CACHE_KEY ?= 1

# ============================================================================
# pingap release 预下载（对齐生产仓 16-app-runtime.mk 的 download-pingap-cache）
# 下载双架构 full tarball 到 .cache/pingap（已 gitignore），分发到 agent-runner
# downloads/ 与 app-runtime-base cache/。⚠️ 分发前清同架构旧版 + prune 缓存内
# 非当前版本：消费方 Dockerfile 通配 COPY (pingap-v*-linux-gnu-*-full.tar.gz)
# 会把旧包拖进构建上下文/镜像层。
# ============================================================================
PINGAP_DL_VERSION ?= $(PINGAP_VERSION)
PINGAP_CACHE_DIR := .cache/pingap
.PHONY: download-pingap-cache
download-pingap-cache:
	@python3 tools/build/runtime_assets.py pingap --version "$(PINGAP_DL_VERSION)" --cache "$(RUNTIME_ASSET_CACHE)" --context docker/app-runtime-base --download-context docker/rcoder-agent-runner --output-ref "$(ASSET_REF_DIR)/pingap.ref"

.PHONY: asset-preflight docker-build-agent-assets docker-build-runtime-assets check-build-contracts
# 门禁读取实际展开值；只打印身份，不准备资产或构建镜像。
.PHONY: print-pingap-build-identity
print-pingap-build-identity:
	@printf '%s %s\n' "$(PINGAP_VERSION)" "$(PINGAP_COMMIT)"

asset-preflight:
	@python3 k8s/scripts/pingap_version_gate.py --pingap-version "$(PINGAP_VERSION)" --pingap-commit "$(PINGAP_COMMIT)" --download-version "$(PINGAP_DL_VERSION)" --node-version "$(NODE_RUNTIME_VERSION)"

check-build-contracts:
	@python3 k8s/scripts/pingap_version_gate.py --cross-repo

docker-build-agent-assets: asset-preflight
	@$(MAKE) build-dbx-fork download-pingap-cache

docker-build-runtime-assets: asset-preflight
	@$(MAKE) build-dbx-fork download-pingap-cache download-ttyd download-node download-go-cache download-deno

# 构建 agent-runner 镜像（基于基础镜像，快速构建）
# pingap 构建参数从 app-cli 源码契约派生，并在准备资产前核验。
docker-build-agent-runner: docker-build-agent-assets
	@echo "🐳 构建 rcoder-agent-runner 镜像（本地开发用 dev-rcoder-agent-runner）..."
	@echo "📍 镜像名称: dev-rcoder-agent-runner:latest"
	@# 检查基础镜像是否存在
	@if ! docker image inspect "$(AGENT_BASE_IMAGE)" >/dev/null 2>&1; then \
		if [ "$(AGENT_BASE_IMAGE)" != "dev-rcoder-agent-base:latest" ]; then echo "Missing AGENT_BASE_IMAGE=$(AGENT_BASE_IMAGE)"; exit 1; fi; \
		echo "⚠️  基础镜像 dev-rcoder-agent-base:latest 不存在，先构建基础镜像..."; \
		$(MAKE) docker-build-agent-base; \
	else \
		echo "✓ 基础镜像 $(AGENT_BASE_IMAGE) 已存在"; \
	fi
	@if docker image inspect dev-app-runtime:latest >/dev/null 2>&1; then \
		python3 docker/verify-userapp-toolchains.py --builder "$(AGENT_BASE_IMAGE)" || { \
			echo "Builder/runtime toolchain verification failed; inspect the diagnostic above. Rebuild or select compatible images only if a version mismatch is reported"; exit 1; }; \
	fi
	@echo "📦 步骤1: 在 debian:12 环境中构建 agent_runner 二进制（确保 GLIBC 版本兼容）..."
	@echo "🔧 Cargo features: $(CARGO_FEATURES)"
	@# 计算业务代码哈希，只有代码变化时才重新编译（系统依赖和 Rust 安装保持缓存）
	$(eval CRATES_HASH := $(shell python3 docker/cargo-source-hash.py))
	@echo "🔑 业务代码哈希: $(CRATES_HASH)"
	@# dial9 在 CARGO_FEATURES 中时传 tokio_unstable RUSTFLAGS（全量 task 覆盖必需；
	@# feature 本身已在 CARGO_FEATURES 里；普通/生产构建零开销不传）
	@if echo "$(CARGO_FEATURES)" | grep -q "dial9"; then \
		DIAL9_RUSTFLAGS="--cfg tokio_unstable"; \
	else \
		DIAL9_RUSTFLAGS=""; \
	fi; \
	docker build --pull --build-arg CRATES_HASH=$(CRATES_HASH) \
		--build-arg CARGO_FLAGS="$(CARGO_FEATURES)" \
		--build-arg RUSTFLAGS="$$DIAL9_RUSTFLAGS" \
		-f docker/rcoder-agent-runner/Dockerfile.build -t dev-rcoder-agent-runner-build .
	@echo "📦 步骤2: 复制二进制文件到 agent-runner 目录..."
	@# 创建容器并复制 agent_runner 二进制文件
	@mkdir -p docker/rcoder-agent-runner/bin
	@set -eu; build_id=$$(docker create dev-rcoder-agent-runner-build); \
	trap 'docker rm -f "$$build_id" >/dev/null' EXIT; \
	docker cp "$$build_id":/build/target/release/agent_runner docker/rcoder-agent-runner/bin/; \
	docker cp "$$build_id":/build/crates/app-cli/target/release/app-cli docker/rcoder-agent-runner/bin/
	@docker rmi dev-rcoder-agent-runner-build
	@echo "📦 步骤3: 构建最终的 agent-runner 镜像（基于基础镜像，快速）..."
	@# 🔧 根据 CARGO_FEATURES 决定是否安装 eBPF 工具
	@(if [ "$(CARGO_FEATURES)" != "" ]; then \
		INSTALL_EBPF="true"; \
		echo "🔧 将安装 eBPF 诊断工具"; \
	else \
		INSTALL_EBPF="false"; \
		echo "🔒 跳过 eBPF 工具安装（生产模式）"; \
	fi; \
	PINGAP_VERSION=$(PINGAP_VERSION) PINGAP_COMMIT=$(PINGAP_COMMIT); \
			if [ -n "$(BUILDX_BUILDER)" ]; then \
			python3 tools/build/asset_context.py --source docker/rcoder-agent-runner --kind agent -- docker buildx build --builder $(BUILDX_BUILDER) --platform linux/$(DOCKER_HOST_ARCH) --load \
				--build-arg BASE_IMAGE="$(AGENT_BASE_IMAGE)" \
				--build-arg PINGAP_VERSION=$$PINGAP_VERSION \
				--build-arg PINGAP_COMMIT=$$PINGAP_COMMIT \
				--build-arg CACHEBUST=$(AGENT_TOOLS_CACHE_KEY) \
				--build-arg INSTALL_EBPF_TOOLS="$${INSTALL_EBPF}" \
				-f "{dockerfile}" -t dev-rcoder-agent-runner:latest "{context}" ; \
		else \
			python3 tools/build/asset_context.py --source docker/rcoder-agent-runner --kind agent -- docker build \
				--build-arg BASE_IMAGE="$(AGENT_BASE_IMAGE)" \
				--build-arg PINGAP_VERSION=$$PINGAP_VERSION \
				--build-arg PINGAP_COMMIT=$$PINGAP_COMMIT \
				--build-arg CACHEBUST=$(AGENT_TOOLS_CACHE_KEY) \
				--build-arg INSTALL_EBPF_TOOLS="$${INSTALL_EBPF}" \
				-f "{dockerfile}" -t dev-rcoder-agent-runner:latest "{context}" ; \
		fi;)
	@echo "✅ dev-rcoder-agent-runner 镜像构建完成！"
	@if [ "$(CARGO_FEATURES)" != "" ]; then \
		echo "🔧 eBPF 调试模式已启用，容器将以特权模式运行"; \
	else \
		echo "🔒 生产模式，容器权限受限"; \
	fi

# 构建生产版本（禁用 eBPF 工具，减小镜像大小）
docker-build-agent-production:
	@echo "🐳 构建 rcoder-agent-runner 生产镜像（无 eBPF 工具）..."
	@$(MAKE) docker-build-agent-runner CARGO_FEATURES=""
	@echo "✅ 生产镜像构建完成（无 eBPF 工具，镜像更小）"

# ============================================================================
# app-runtime 镜像构建（本地开发/测试，dev 前缀，不推 registry）
# ============================================================================
# UserApp 容器运行时。分层与 build-agent-docker 65a1cb8/878c3f6 同步：
# - base（Dockerfile）：基础设施 + 语言运行时本体（node/python/java/go/deno），不含 rcoder 源码。
# - runtime（Dockerfile.runtime）：npm 全局工具 + app-cli/file-server-proxy（rcoder 源码产物末层）。
# rcoder 源码经 docker/build-app-runtime.py 以命名构建上下文注入（本仓源）。
# 产物: dev-app-runtime-base:latest + dev-app-runtime:latest
APP_RUNTIME_DIR := docker/app-runtime-base

# 构建 dev-app-runtime-base（基础设施 + 语言运行时层: Rust/PG/dbx/ttyd/supervisor + Node/Python/Java/Go/Deno）
docker-build-app-runtime-base: docker-build-runtime-assets
	@echo "🐳 构建 dev-app-runtime-base:latest ..."
	@python3 docker/build-app-runtime.py $(APP_RUNTIME_DIR)
	@echo "✅ dev-app-runtime-base:latest 构建完成"

# 构建 dev-app-runtime（FROM base；npm 工具 + 本仓源码编译的 app-cli/file-server-proxy）
docker-build-app-runtime: docker-build-app-runtime-base
	@python3 docker/build-app-runtime.py $(APP_RUNTIME_DIR) --also-runtime
	@echo "✅ dev-app-runtime:latest 构建完成"
