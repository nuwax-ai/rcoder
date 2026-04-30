# ============================================================================
# Docker 镜像构建
# ============================================================================

# Docker 镜像构建（仅构建镜像，不编译）
# 串行构建镜像，避免资源竞争
docker-build: docker-build-agent-base
	@echo "🔨 开始构建主镜像..."
	@$(MAKE) docker-build-master & \
	$(MAKE) docker-build-agent-runner & \
	wait
	@echo ""
	@echo "✅ 所有 Docker 镜像构建完成！"
	@echo "  ✓ master-rcoder:latest"
	@echo "  ✓ rcoder-agent-runner:latest"
	@echo ""
	@echo "🎯 使用方式："
	@echo "  docker run -d -p 8087:8087 master-rcoder:latest"

# 构建所有基础镜像（很少需要，只有修改系统依赖时才需要）
# 串行构建基础镜像，避免资源竞争
docker-build-base: docker-build-master-base docker-build-agent-base
	@echo ""
	@echo "✅ 所有基础镜像构建完成！"
	@echo "  ✓ master-rcoder-base:latest"
	@echo "  ✓ rcoder-agent-base:latest"
	@echo ""
	@echo "💡 提示: 平时开发只需运行 make dev-restart，无需重新构建基础镜像"

# 构建主服务镜像（基于基础镜像，快速构建）
docker-build-master:
	@echo "🐳 构建 master-rcoder 镜像..."
	@echo "📍 镜像名称: master-rcoder:latest"
	@# 检查基础镜像是否存在
	@if ! docker image inspect master-rcoder-base:latest >/dev/null 2>&1; then \
		echo "⚠️  基础镜像 master-rcoder-base:latest 不存在，先构建基础镜像..."; \
		$(MAKE) docker-build-master-base; \
	else \
		echo "✓ 基础镜像 master-rcoder-base:latest 已存在"; \
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
		--build-arg CARGO_FLAGS="$$MASTER_CARGO_FLAGS" \
		--build-arg CACHEBUST=$$(date +%s) \
		-f docker/rcoder-master/Dockerfile -t master-rcoder:latest .;)
	@echo "✅ master-rcoder 镜像构建完成！"
	@echo "📤 推送镜像到阿里云仓库..."
	@docker tag master-rcoder:latest nuwax-docker-images-registry.cn-hangzhou.cr.aliyuncs.com/dev/master-rcoder:latest
	@skopeo copy docker-daemon:nuwax-docker-images-registry.cn-hangzhou.cr.aliyuncs.com/dev/master-rcoder:latest docker://nuwax-docker-images-registry.cn-hangzhou.cr.aliyuncs.com/dev/master-rcoder:latest
	@echo "✅ 镜像已推送: nuwax-docker-images-registry.cn-hangzhou.cr.aliyuncs.com/dev/master-rcoder:latest"

# 构建 master-base 基础镜像（包含所有运行时依赖，很少需要重新构建）
docker-build-master-base:
	@echo "🐳 构建 master-rcoder-base 基础镜像..."
	@echo "📍 镜像名称: master-rcoder-base:latest"
	@echo "⏳ 这可能需要较长时间（包含所有运行时依赖安装）..."
	@docker build -f docker/rcoder-master/Dockerfile.base -t master-rcoder-base:latest .
	@echo "✅ master-rcoder-base 基础镜像构建完成！"
	@echo "📤 推送基础镜像到阿里云仓库..."
	@docker tag master-rcoder-base:latest nuwax-docker-images-registry.cn-hangzhou.cr.aliyuncs.com/dev/master-rcoder-base:latest
	@skopeo copy docker-daemon:nuwax-docker-images-registry.cn-hangzhou.cr.aliyuncs.com/dev/master-rcoder-base:latest docker://nuwax-docker-images-registry.cn-hangzhou.cr.aliyuncs.com/dev/master-rcoder-base:latest
	@echo "✅ 基础镜像已推送: nuwax-docker-images-registry.cn-hangzhou.cr.aliyuncs.com/dev/master-rcoder-base:latest"
	@echo "💡 提示: 平时开发只需运行 make dev-restart，无需重新构建基础镜像"

# ============================================================================
# Cargo feature 配置
# ============================================================================
# 开发模式：启用所有调试、监控和追踪功能
# ⚠️  注意：添加新的调试 feature 时，必须同步更新此列表！
#
# 当前启用的调试 features：
#   - ebpf-debug    (docker_manager, rcoder): eBPF 诊断工具
#   - pyroscope     (agent_runner):         性能分析 (CPU/Memory)
#   - otel          (agent_runner):         OpenTelemetry 追踪
#   - debug         (rcoder):               调试路由
#   - proxy         (agent_runner):         Pingora 反向代理（Linux 容器默认开启）
#   - kubernetes    (rcoder, docker_manager): Kubernetes 运行时支持
#   - http-server   (agent_runner):         HTTP REST API 服务（默认启用）
#   - grpc-server   (agent_runner):         gRPC 服务（默认启用）
#
# 本地开发调试默认开启所有功能（http-server 和 grpc-server 默认启用）
CARGO_FEATURES ?= --features ebpf-debug,pyroscope,otel,debug,proxy,kubernetes

# 构建 agent-runner 镜像（基于基础镜像，快速构建）
docker-build-agent-runner:
	@echo "🐳 构建 rcoder-agent-runner 镜像..."
	@echo "📍 镜像名称: rcoder-agent-runner:latest"
	@# 检查基础镜像是否存在
	@if ! docker image inspect rcoder-agent-base:latest >/dev/null 2>&1; then \
		echo "⚠️  基础镜像 rcoder-agent-base:latest 不存在，先构建基础镜像..."; \
		$(MAKE) docker-build-agent-base; \
	else \
		echo "✓ 基础镜像 rcoder-agent-base:latest 已存在"; \
	fi
	@echo "📦 步骤1: 在 debian:12 环境中构建 agent_runner 二进制（确保 GLIBC 版本兼容）..."
	@# 🔧 调试模式：默认启用 ebpf-debug feature，允许使用 eBPF 诊断工具
	@echo "🔧 Cargo features: $(CARGO_FEATURES)"
	@# 计算业务代码哈希，只有代码变化时才重新编译（系统依赖和 Rust 安装保持缓存）
	$(eval CRATES_HASH := $(shell find crates Cargo.toml Cargo.lock -name "*.rs" -o -name "Cargo.toml" -o -name "Cargo.lock" 2>/dev/null | sort | xargs cat 2>/dev/null | md5sum | cut -d' ' -f1))
	@echo "🔑 业务代码哈希: $(CRATES_HASH)"
	@# 🔥 关键修改：通过 CARGO_FEATURES 变量控制
	@docker build --build-arg CRATES_HASH=$(CRATES_HASH) \
		--build-arg CARGO_FLAGS="$(CARGO_FEATURES)" \
		-f docker/rcoder-agent-runner/Dockerfile.build -t rcoder-agent-runner-build .
	@echo "📦 步骤2: 复制二进制文件到 agent-runner 目录..."
	@# 创建容器并复制 agent_runner 二进制文件
	@mkdir -p docker/rcoder-agent-runner/bin
	@docker create --name build-container rcoder-agent-runner-build
	@docker cp build-container:/build/target/release/agent_runner docker/rcoder-agent-runner/bin/
	@docker rm build-container
	@docker rmi rcoder-agent-runner-build
	@echo "📦 步骤3: 构建最终的 agent-runner 镜像（基于基础镜像，快速）..."
	@# 🔧 根据 CARGO_FEATURES 决定是否安装 eBPF 工具
	@(if [ "$(CARGO_FEATURES)" != "" ]; then \
		INSTALL_EBPF="true"; \
		echo "🔧 将安装 eBPF 诊断工具"; \
	else \
		INSTALL_EBPF="false"; \
		echo "🔒 跳过 eBPF 工具安装（生产模式）"; \
	fi; \
	cd docker/rcoder-agent-runner && \
		docker buildx build --platform linux/$(DOCKER_HOST_ARCH) --load \
			--build-arg CACHEBUST=$$(date +%s) \
			--build-arg INSTALL_EBPF_TOOLS="$${INSTALL_EBPF}" \
			--build-arg INSTALL_PYROSCOPE="$${INSTALL_EBPF}" \
			--build-arg INSTALL_ALLOY="$${INSTALL_EBPF}" \
			-f Dockerfile -t rcoder-agent-runner:latest .;)
	@echo "✅ rcoder-agent-runner 镜像构建完成！"
	@if [ "$(CARGO_FEATURES)" != "" ]; then \
		echo "🔧 eBPF 调试模式已启用，容器将以特权模式运行"; \
	else \
		echo "🔒 生产模式，容器权限受限"; \
	fi
	@echo "📤 推送镜像到阿里云仓库..."
	@docker tag rcoder-agent-runner:latest $(K8S_IMAGE_REGISTRY)
	@docker tag rcoder-agent-runner:latest nuwax-docker-images-registry.cn-hangzhou.cr.aliyuncs.com/dev/rcoder-agent-runner:latest
	@skopeo copy docker-daemon:nuwax-docker-images-registry.cn-hangzhou.cr.aliyuncs.com/dev/rcoder-agent-runner:latest docker://nuwax-docker-images-registry.cn-hangzhou.cr.aliyuncs.com/dev/rcoder-agent-runner:latest
	@echo "✅ 镜像已推送: nuwax-docker-images-registry.cn-hangzhou.cr.aliyuncs.com/dev/rcoder-agent-runner:latest"

# 构建生产版本（禁用 eBPF 工具，减小镜像大小）
docker-build-agent-production:
	@echo "🐳 构建 rcoder-agent-runner 生产镜像（无 eBPF 工具）..."
	@$(MAKE) docker-build-agent-runner CARGO_FEATURES=""
	@echo "✅ 生产镜像构建完成（无 eBPF 工具，镜像更小）"
