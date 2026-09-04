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

# Docker 镜像构建（仅构建镜像，不编译）
# 串行构建镜像，避免资源竞争
docker-build:
	@echo "🔨 开始构建主镜像..."
	@task_build_status=0; \
	$(MAKE) docker-build-master & master_pid=$$!; \
	$(MAKE) docker-build-agent-runner & agent_pid=$$!; \
	wait $$master_pid || task_build_status=$$?; \
	wait $$agent_pid || task_build_status=$$?; \
	exit $$task_build_status
	@echo ""
	@echo "✅ 所有 Docker 镜像构建完成！"
	@echo "  ✓ dev-master-rcoder:latest"
	@echo "  ✓ dev-computer-agent-runner:latest"
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
	@echo "📦 使用 Dockerfile 多阶段构建（基于基础镜像）..."
	@# 🔧 根据 CARGO_FEATURES 决定是否启用 eBPF 调试
	@(if [ "$(CARGO_FEATURES)" != "" ]; then \
		MASTER_CARGO_FLAGS="$(CARGO_FEATURES)"; \
		echo "🔧 master-rcoder 将启用 eBPF 调试模式"; \
	else \
		MASTER_CARGO_FLAGS=""; \
		echo "🔒 master-rcoder 生产模式（无 eBPF 调试）"; \
	fi; \
	docker build \
		--build-arg BASE_IMAGE=dev-master-rcoder-base:latest \
		--build-arg CARGO_FLAGS="$$MASTER_CARGO_FLAGS" \
		--build-arg CACHEBUST=$$(date +%s) \
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
	@docker build -f docker/rcoder-master/Dockerfile.base -t dev-master-rcoder-base:latest .
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
#   - ebpf-debug    (docker_manager, rcoder): eBPF 诊断工具
#   - pyroscope     (agent_runner):         性能分析 (CPU/Memory)
#   - otel          (agent_runner):         OpenTelemetry 追踪
#   - debug         (rcoder):               调试路由
#   - proxy         (agent_runner):         Pingora + 模型密钥代理（可选；见下方说明）
#   - kubernetes    (rcoder, docker_manager): Kubernetes 运行时支持
#   - http-server   (agent_runner):         HTTP REST API 服务（默认启用）
#   - grpc-server   (agent_runner):         gRPC 服务（默认启用）
#
# proxy 默认关闭：子进程会收到真实 MODEL_PROVIDER API key/base_url（如 nuwax-codex-acp 本地鉴权）。
# 需要密钥经 Pingora 注入时，构建前设置例如：
#   make dev-restart CARGO_FEATURES='--features ebpf-debug,pyroscope,otel,debug,proxy'
#
# 本地开发调试默认开启上述功能（http-server / grpc-server 仍由 agent_runner 默认 features 提供）
# 注意：kubernetes feature 仅用于 K8s 环境，Docker Compose 模式不要启用
CARGO_FEATURES ?= --features ebpf-debug,pyroscope,otel,debug

# 构建 agent-runner 镜像（基于基础镜像，快速构建）
# pingap 版本说明（构建注入，单一来源 = app-cli devtool.rs DEFAULT_PINGAP_VERSION/COMMIT，
# 与生产 build_config 16-app-runtime.mk 同值；三处同步改）
docker-build-agent-runner:
	@echo "🐳 构建 rcoder-agent-runner 镜像（本地开发用 dev-computer-agent-runner）..."
	@echo "📍 镜像名称: dev-computer-agent-runner:latest"
	@# 生产源头同步：start-up*.sh 以 build-agent-docker 仓库为单一事实源，
	@# 构建前自动拉取防止本地/生产启动行为漂移（ime_server.py 等本地维护文件不在此清单）
	@BUILD_CONFIG_DIR=~/Documents/git-workspace/build-agent-docker/build_config/rcoder-agent-runner; \
	if [ -d "$$BUILD_CONFIG_DIR" ]; then \
		for f in start-up.sh start-up-common.sh start-up-docker-extra.sh start-up-k8s-extra.sh; do \
			if [ -f "$$BUILD_CONFIG_DIR/$$f" ] && ! cmp -s "docker/rcoder-agent-runner/$$f" "$$BUILD_CONFIG_DIR/$$f"; then \
				cp -p "$$BUILD_CONFIG_DIR/$$f" "docker/rcoder-agent-runner/$$f"; \
				echo "🔄 已同步生产源头: $$f"; \
			fi; \
		done; \
	fi
	@# 检查基础镜像是否存在
	@if ! docker image inspect dev-rcoder-agent-base:latest >/dev/null 2>&1; then \
		echo "⚠️  基础镜像 dev-rcoder-agent-base:latest 不存在，先构建基础镜像..."; \
		$(MAKE) docker-build-agent-base; \
	else \
		echo "✓ 基础镜像 dev-rcoder-agent-base:latest 已存在"; \
	fi
	@echo "📦 步骤1: 在 debian:12 环境中构建 agent_runner 二进制（确保 GLIBC 版本兼容）..."
	@# 🔧 调试模式：默认启用 ebpf-debug feature，允许使用 eBPF 诊断工具
	@echo "🔧 Cargo features: $(CARGO_FEATURES)"
	@# 计算业务代码哈希，只有代码变化时才重新编译（系统依赖和 Rust 安装保持缓存）
	$(eval CRATES_HASH := $(shell find crates Cargo.toml Cargo.lock -name "*.rs" -o -name "Cargo.toml" -o -name "Cargo.lock" 2>/dev/null | sort | xargs cat 2>/dev/null | md5sum | cut -d' ' -f1))
	@echo "🔑 业务代码哈希: $(CRATES_HASH)"
	@# 🔥 关键修改：通过 CARGO_FEATURES 变量控制
	@# tokio-console 观测模式：AGENT_CONSOLE=1 时传 tokio_unstable RUSTFLAGS +
	@# console feature（见 docs/console.md；普通构建零开销不传）
	@if [ "$(AGENT_CONSOLE)" = "1" ]; then \
		CONSOLE_RUSTFLAGS="--cfg tokio_unstable"; CONSOLE_FEATURES="console"; \
	else \
		CONSOLE_RUSTFLAGS=""; CONSOLE_FEATURES=""; \
	fi; \
	docker build --build-arg CRATES_HASH=$(CRATES_HASH) \
		--build-arg CARGO_FLAGS="$(CARGO_FEATURES) $$CONSOLE_FEATURES" \
		--build-arg RUSTFLAGS="$$CONSOLE_RUSTFLAGS" \
		-f docker/rcoder-agent-runner/Dockerfile.build -t dev-rcoder-agent-runner-build .
	@echo "📦 步骤2: 复制二进制文件到 agent-runner 目录..."
	@# 创建容器并复制 agent_runner 二进制文件
	@mkdir -p docker/rcoder-agent-runner/bin
	@docker create --name build-container dev-rcoder-agent-runner-build
	@docker cp build-container:/build/target/release/agent_runner docker/rcoder-agent-runner/bin/
	@docker cp build-container:/build/crates/app-cli/target/release/app-cli docker/rcoder-agent-runner/bin/
	@docker rm build-container
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
	PINGAP_VERSION=0.13.9 PINGAP_COMMIT=f7f9eddb029a5b07438bead2e0fd3df763086567; \
	cd docker/rcoder-agent-runner && \
		if [ -n "$(BUILDX_BUILDER)" ]; then \
			docker buildx build --builder $(BUILDX_BUILDER) --platform linux/$(DOCKER_HOST_ARCH) --load \
				--build-arg BASE_IMAGE=dev-rcoder-agent-base:latest \
				--build-arg PINGAP_VERSION=$$PINGAP_VERSION \
				--build-arg PINGAP_COMMIT=$$PINGAP_COMMIT \
				--build-arg CACHEBUST=$$(date +%s) \
				--build-arg INSTALL_EBPF_TOOLS="$${INSTALL_EBPF}" \
				--build-arg INSTALL_PYROSCOPE="$${INSTALL_EBPF}" \
				--build-arg INSTALL_ALLOY="$${INSTALL_EBPF}" \
				-f Dockerfile -t dev-rcoder-agent-runner:latest . ; \
		else \
			docker build \
				--build-arg BASE_IMAGE=dev-rcoder-agent-base:latest \
				--build-arg PINGAP_VERSION=$$PINGAP_VERSION \
				--build-arg PINGAP_COMMIT=$$PINGAP_COMMIT \
				--build-arg CACHEBUST=$$(date +%s) \
				--build-arg INSTALL_EBPF_TOOLS="$${INSTALL_EBPF}" \
				--build-arg INSTALL_PYROSCOPE="$${INSTALL_EBPF}" \
				--build-arg INSTALL_ALLOY="$${INSTALL_EBPF}" \
				-f Dockerfile -t dev-rcoder-agent-runner:latest . ; \
		fi;)
	@echo "✅ dev-computer-agent-runner 镜像构建完成！"
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
# UserApp 容器运行时。app-runtime-base/Dockerfile 用仓库根作构建上下文 (COPY . . + COPY docker/app-runtime-base/...,
# 同 rcoder-master 模式)，根 .dockerignore 已排除 target/.git/project_workspace 等，无需 rsync / code/rcoder 中转。
# 产物: dev-app-runtime-base:latest（基础设施+Rust app-cli）+ dev-app-runtime:latest（多语言运行时）
APP_RUNTIME_DIR := docker/app-runtime-base

# 构建 dev-app-runtime-base（基础设施层: PG/dbx/ttyd/supervisor + Rust app-cli）
docker-build-app-runtime-base:
	@echo "🐳 构建 dev-app-runtime-base:latest ..."
	@docker build --build-context rcoder=$(PWD) \
		--build-arg PINGAP_VERSION=0.13.7 \
		--build-arg PINGAP_COMMIT=f7f9eddb029a5b07438bead2e0fd3df763086567 \
		-t dev-app-runtime-base:latest -f $(APP_RUNTIME_DIR)/Dockerfile $(APP_RUNTIME_DIR)
	@echo "✅ dev-app-runtime-base:latest 构建完成"

# 构建 dev-app-runtime（多语言运行时: base + Node/Python/Java/Go），UserApp 部署用此镜像
docker-build-app-runtime: docker-build-app-runtime-base
	@echo "🐳 构建 dev-app-runtime:latest（基于 dev-app-runtime-base）..."
	@docker build --build-arg BASE_IMAGE=dev-app-runtime-base:latest \
		-t dev-app-runtime:latest -f $(APP_RUNTIME_DIR)/Dockerfile.runtime $(APP_RUNTIME_DIR)
	@echo "✅ dev-app-runtime:latest 构建完成"
