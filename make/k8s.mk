# ============================================================================
# K8s 开发/部署 Makefile (Kustomize)
# ============================================================================

IMAGE ?= rcoder:test-k8s
K8S_IMAGE_REGISTRY ?= nuwax-docker-images-registry.cn-hangzhou.cr.aliyuncs.com/dev/rcoder:latest
KUSTOMIZE_DIR ?= k8s/manifests
ROLLOUT_TIMEOUT ?= 180s

# Namespace 按 overlay 推导
K8S_NAMESPACE_DEV  := nuwax-rcoder-dev
K8S_NAMESPACE_PROD := nuwax-rcoder-prod

# 本地 k3s 同步配置（make dev-build-k8s 构建后自动 import 到本地 k3s）
SUDO ?= sudo
K3S_CTR ?= $(SUDO) k3s ctr
K3S_NAMESPACE ?= k8s.io
# 设为 1 跳过本地 k3s import（例如机器没装 k3s 或不想 import）
SKIP_K3S_IMPORT ?= 0
# 设为 1 跳过 build 后自动 rollout restart
SKIP_ROLLOUT_RESTART ?= 0

# ============================================================================
# 镜像构建
# ============================================================================

dev-build-k8s: docker-build-master-base
	@echo "☸️  构建 K8s 镜像..."
	@echo "📍 镜像名称: $(IMAGE)"
	@echo "⏳ 这可能需要较长时间（包含 Rust 编译）..."
	@docker build -f docker/rcoder-master/Dockerfile -t $(IMAGE) --build-arg CARGO_FLAGS="--features kubernetes" .
	@echo "✅ K8s 镜像构建完成！"
	@echo ""
	@echo "📤 推送镜像到阿里云仓库..."
	@docker tag $(IMAGE) $(K8S_IMAGE_REGISTRY)
	@skopeo copy docker-daemon:$(K8S_IMAGE_REGISTRY) docker://$(K8S_IMAGE_REGISTRY)
	@echo "✅ 镜像已推送到 $(K8S_IMAGE_REGISTRY)"
	@$(MAKE) -s --no-print-directory _k8s-import-image
	@$(MAKE) -s --no-print-directory _k8s-rollout-restart
	@echo ""
	@echo "💡 下一步: make dev-up-k8s 启动 K8s 开发模式 (镜像已就位)"

# 内部 target: 把刚构建的镜像直接导入本地 k3s containerd, 跳过 registry 网络往返
# 使用 docker save | k3s ctr image import, 完全离线
_k8s-import-image:
	@if [ "$(SKIP_K3S_IMPORT)" = "1" ]; then \
		echo ""; echo "⏭  跳过 k3s 本地 import (SKIP_K3S_IMPORT=1)"; exit 0; \
	fi
	@if ! command -v k3s >/dev/null 2>&1; then \
		echo ""; echo "⚠️  未检测到 k3s (本机非 k3s 节点), 跳过本地 import"; \
		echo "   远程 registry 已更新, 其他集群仍可正常拉取"; exit 0; \
	fi
	@echo ""
	@echo "📥 导入镜像到本地 k3s containerd (跳过 registry 网络往返)..."
	@echo "   方式: docker save | k3s ctr image import"
	@docker save $(K8S_IMAGE_REGISTRY) | $(K3S_CTR) -n $(K3S_NAMESPACE) image import - >/dev/null
	@echo "✅ 镜像已 import 到本地 k3s 节点 ($(K3S_NAMESPACE) namespace)"

# 内部 target: 重启已部署的 rcoder Deployment 使其拉取/使用新 image
# imagePullPolicy 是 IfNotPresent, 节点上已经是新 image, restart 后即生效
_k8s-rollout-restart:
	@if [ "$(SKIP_ROLLOUT_RESTART)" = "1" ]; then \
		echo "⏭  跳过 rollout restart (SKIP_ROLLOUT_RESTART=1)"; exit 0; \
	fi
	@if ! command -v kubectl >/dev/null 2>&1; then exit 0; fi
	@echo ""
	@echo "🔄 重启已部署的 rcoder Deployment (使新镜像生效)..."
	@for ns in $(K8S_NAMESPACE_DEV) $(K8S_NAMESPACE_PROD); do \
		if kubectl get deploy rcoder -n $$ns >/dev/null 2>&1; then \
			kubectl rollout restart deploy/rcoder -n $$ns >/dev/null && \
				echo "  ✓ $$ns: rollout restart 已触发"; \
		else \
			echo "  - $$ns: deploy 不存在, 跳过"; \
		fi; \
	done

# ============================================================================
# 开发模式快捷命令 (默认指向 dev overlay)
# ============================================================================

# 启动 K8s 开发模式（使用 Kustomize dev overlay）
dev-up-k8s:
	@echo "☸️  启动 K8s 开发模式 (overlays/dev -> $(K8S_NAMESPACE_DEV))..."
	@echo ""
	@echo "[1/4] 检查 JuiceFS CSI Driver..."
	@kubectl rollout status daemonset/juicefs-csi-driver-node -n kube-system --timeout=120s 2>/dev/null || \
		(echo "⚠️  JuiceFS CSI 未部署，请先执行: helm install juicefs-csi-driver juicefs/juicefs-csi-driver --namespace kube-system" && exit 1)

	@echo ""
	@echo "[2/4] 部署 dev overlay..."
	@kubectl apply -k $(KUSTOMIZE_DIR)/overlays/dev

	@echo ""
	@echo "[3/4] 等待存储层就绪..."
	@kubectl wait --for=condition=ready pod -l app=postgresql -n $(K8S_NAMESPACE_DEV) --timeout=180s 2>/dev/null || echo "⚠️  PostgreSQL 等待超时，继续..."
	@kubectl wait --for=condition=ready pod -l app=minio -n $(K8S_NAMESPACE_DEV) --timeout=180s 2>/dev/null || echo "⚠️  MinIO 等待超时，继续..."
	@kubectl wait --for=condition=complete job/minio-init -n $(K8S_NAMESPACE_DEV) --timeout=120s 2>/dev/null || echo "⚠️  MinIO 初始化等待超时，继续..."

	@echo ""
	@echo "[4/4] 等待 RCoder 就绪..."
	@kubectl rollout status deploy/rcoder -n $(K8S_NAMESPACE_DEV) --timeout=$(ROLLOUT_TIMEOUT)

	@echo ""
	@echo "📋 K8s 部署状态:"
	@kubectl get pods -n $(K8S_NAMESPACE_DEV)
	@echo ""
	@kubectl get pvc -n $(K8S_NAMESPACE_DEV)
	@echo ""
	@kubectl get storageclass | grep juice || true
	@echo ""
	@echo "💡 查看日志: make dev-logs-k8s"

# 重启 K8s 开发模式（重新构建镜像+部署）
dev-restart-k8s: dev-build-k8s
	@echo "☸️  重启 K8s 开发模式..."
	@kubectl apply -k $(KUSTOMIZE_DIR)/overlays/dev
	@kubectl delete pods -n $(K8S_NAMESPACE_DEV) -l app=rcoder --ignore-not-found
	@kubectl rollout status deploy/rcoder -n $(K8S_NAMESPACE_DEV) --timeout=$(ROLLOUT_TIMEOUT)
	@echo "✅ K8s 部署已重启！"
	@kubectl get pods -n $(K8S_NAMESPACE_DEV)

# 停止 K8s 开发模式
dev-down-k8s:
	@echo "☸️  停止 K8s 开发模式..."
	@kubectl delete -k $(KUSTOMIZE_DIR)/overlays/dev --ignore-not-found
	@echo "✅ K8s 开发栈已从集群移除"

# 查看 K8s 开发模式日志
dev-logs-k8s:
	@echo "☸️  查看 K8s 开发模式日志..."
	@kubectl logs -n $(K8S_NAMESPACE_DEV) -l app=rcoder -f

# ============================================================================
# 多环境部署命令
# ============================================================================

# 部署开发环境
deploy-dev:
	@echo "☸️  部署开发环境..."
	@kubectl apply -k $(KUSTOMIZE_DIR)/overlays/dev
	@kubectl rollout status deploy/rcoder -n $(K8S_NAMESPACE_DEV) --timeout=$(ROLLOUT_TIMEOUT)
	@echo "✅ 开发环境部署完成！"

# 部署生产环境
deploy-prod:
	@echo "☸️  部署生产环境..."
	@kubectl apply -k $(KUSTOMIZE_DIR)/overlays/prod
	@kubectl rollout status deploy/rcoder -n $(K8S_NAMESPACE_PROD) --timeout=$(ROLLOUT_TIMEOUT)
	@echo "✅ 生产环境部署完成！"

# 清理指定环境
undeploy-dev:
	@echo "☸️  清理开发环境..."
	@kubectl delete -k $(KUSTOMIZE_DIR)/overlays/dev --ignore-not-found

undeploy-prod:
	@echo "☸️  清理生产环境..."
	@kubectl delete -k $(KUSTOMIZE_DIR)/overlays/prod --ignore-not-found

# 验证 Kustomize 配置（构建不部署）
kustomize-build:
	@echo "☸️  验证 Kustomize 配置..."
	@echo "=== Base ==="
	@kubectl kustomize $(KUSTOMIZE_DIR)/base >/dev/null && echo "  ✓"
	@echo "=== Dev Overlay ==="
	@kubectl kustomize $(KUSTOMIZE_DIR)/overlays/dev >/dev/null && echo "  ✓"
	@echo "=== Prod Overlay ==="
	@kubectl kustomize $(KUSTOMIZE_DIR)/overlays/prod >/dev/null && echo "  ✓"

# ============================================================================
# 本地 K8s 测试 (OrbStack / k3d / kind)
# ============================================================================

K8S_LOCAL_NAMESPACE := rcoder-local
K8S_LOCAL_IMAGE ?= rcoder-local:latest
K8S_LOCAL_AGENT_IMAGE ?= rcoder-agent-runner-local:latest

# 构建本地 K8s 测试镜像（不推送到远程仓库）
dev-build-k8s-local:
	@echo "🔨 构建本地 K8s 测试镜像..."
	@echo "📦 [1/2] 构建 rcoder (features=kubernetes)..."
	@docker build -f docker/rcoder-master/Dockerfile -t $(K8S_LOCAL_IMAGE) --build-arg CARGO_FLAGS="--features kubernetes" .
	@echo "📦 [2/2] 构建 agent-runner..."
	@echo "  步骤 1: 在 Docker 中编译 agent_runner 二进制..."
	@docker build -f docker/rcoder-agent-runner/Dockerfile.build -t rcoder-agent-runner-build-local .
	@mkdir -p docker/rcoder-agent-runner/bin
	@docker create --name build-container-local rcoder-agent-runner-build-local 2>/dev/null || true
	@docker cp build-container-local:/build/target/release/agent_runner docker/rcoder-agent-runner/bin/agent_runner
	@docker rm build-container-local
	@docker rmi rcoder-agent-runner-build-local
	@echo "  步骤 2: 构建最终 agent-runner 镜像..."
	@cd docker/rcoder-agent-runner && docker build -f Dockerfile --build-arg INSTALL_EBPF_TOOLS=false -t $(K8S_LOCAL_AGENT_IMAGE) .
	@echo "✅ 本地镜像构建完成！"
	@docker images | grep -E "rcoder-local|rcoder-agent-runner-local"

# 启动本地 K8s 测试（OrbStack 自动共享 Docker 镜像，无需手动 import）
dev-up-k8s-local:
	@echo "☸️  启动本地 K8s 测试 ($(K8S_LOCAL_NAMESPACE))..."
	@kubectl apply -k $(KUSTOMIZE_DIR)/overlays/local
	@echo ""
	@echo "⏳ 等待 RCoder 就绪..."
	@kubectl rollout status deploy/rcoder -n $(K8S_LOCAL_NAMESPACE) --timeout=$(ROLLOUT_TIMEOUT)
	@echo ""
	@echo "📋 部署状态:"
	@kubectl get pods -n $(K8S_LOCAL_NAMESPACE)
	@kubectl get pvc -n $(K8S_LOCAL_NAMESPACE)
	@echo ""
	@echo "💡 访问: http://localhost:30087"
	@echo "💡 日志: make dev-logs-k8s-local"

# 重启本地 K8s 测试
dev-restart-k8s-local:
	@echo "☸️  重启本地 K8s 测试..."
	@kubectl delete pods -n $(K8S_LOCAL_NAMESPACE) -l app=rcoder --ignore-not-found
	@kubectl rollout status deploy/rcoder -n $(K8S_LOCAL_NAMESPACE) --timeout=$(ROLLOUT_TIMEOUT)
	@echo "✅ 已重启！"

# 停止本地 K8s 测试
dev-down-k8s-local:
	@echo "☸️  停止本地 K8s 测试..."
	@kubectl delete -k $(KUSTOMIZE_DIR)/overlays/local --ignore-not-found
	@echo "✅ 已清理"

# 查看本地 K8s 测试日志
dev-logs-k8s-local:
	@kubectl logs -n $(K8S_LOCAL_NAMESPACE) -l app=rcoder -f --tail=100

# 一键构建+部署（本地 K8s 测试）
dev-local-k8s: dev-build-k8s-local dev-up-k8s-local
	@echo "🎉 本地 K8s 测试环境已就绪！"
