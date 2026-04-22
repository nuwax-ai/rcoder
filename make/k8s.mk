# ============================================================================
# K8s 开发模式命令 (Kustomize + Helm)
# ============================================================================

IMAGE ?= rcoder:test-k8s
K8S_NAMESPACE := nuwax-rcoder
ROLLOUT_TIMEOUT ?= 180s
K8S_IMAGE_REGISTRY ?= nuwax-docker-images-registry.cn-hangzhou.cr.aliyuncs.com/dev/rcoder:latest
KUSTOMIZE_DIR ?= k8s/manifests
HELM_DIR ?= k8s/helm/rcoder
RELEASE_NAME ?= rcoder

dev-build-k8s: docker-build-master-base
	@echo "☸️  构建 K8s 镜像..."
	@echo "📍 镜像名称: $(IMAGE)"
	@echo "⏳ 这可能需要较长时间（包含 Rust 编译）..."
	@docker build -f docker/rcoder-master/Dockerfile -t $(IMAGE) --build-arg CARGO_FLAGS="--features kubernetes" .
	@echo "✅ K8s 镜像构建完成！"
	@echo "📤 推送镜像到阿里云仓库..."
	@docker tag $(IMAGE) $(K8S_IMAGE_REGISTRY)
	@skopeo copy docker-daemon:$(K8S_IMAGE_REGISTRY) docker://$(K8S_IMAGE_REGISTRY)
	@echo "✅ 镜像已推送到 $(K8S_IMAGE_REGISTRY)"
	@echo "💡 下一步: make dev-up-k8s 启动 K8s 开发模式"

# 启动 K8s 开发模式（使用 Kustomize）
dev-up-k8s:
	@echo "☸️  启动 K8s 开发模式 (JuiceFS + MinIO + PostgreSQL)..."
	@echo ""
	@echo "[1/4] 部署 JuiceFS CSI Driver..."
	@kubectl rollout status daemonset/juicefs-csi-driver-node -n kube-system --timeout=120s 2>/dev/null || \
		(echo "⚠️  JuiceFS CSI 未部署，请先执行: helm install juicefs-csi-driver juicefs/juicefs-csi-driver --namespace kube-system" && exit 1)

	@echo ""
	@echo "[2/4] 部署存储层 + 应用层 (使用 Kustomize)..."
	@kubectl apply -k $(KUSTOMIZE_DIR)/base

	@echo ""
	@echo "[3/4] 等待存储层就绪..."
	@kubectl wait --for=condition=ready pod -l app=postgresql -n $(K8S_NAMESPACE) --timeout=180s 2>/dev/null || echo "⚠️  PostgreSQL 等待超时，继续..."
	@kubectl wait --for=condition=ready pod -l app=minio -n $(K8S_NAMESPACE) --timeout=180s 2>/dev/null || echo "⚠️  MinIO 等待超时，继续..."
	@kubectl wait --for=condition=complete job/minio-init -n $(K8S_NAMESPACE) --timeout=120s 2>/dev/null || echo "⚠️  MinIO 初始化等待超时，继续..."

	@echo ""
	@echo "[4/4] 等待 RCoder 就绪..."
	@kubectl rollout status deploy/rcoder -n $(K8S_NAMESPACE) --timeout=$(ROLLOUT_TIMEOUT)

	@echo ""
	@echo "📋 K8s 部署状态:"
	@kubectl get pods -n $(K8S_NAMESPACE)
	@echo ""
	@kubectl get pvc -n $(K8S_NAMESPACE)
	@echo ""
	@kubectl get storageclass | grep juice
	@echo ""
	@echo "💡 查看日志: make dev-logs-k8s"

# 重启 K8s 开发模式（重新构建镜像+部署）
dev-restart-k8s: dev-build-k8s
	@echo "☸️  重启 K8s 开发模式..."
	# 使用 kustomize 重新部署 (保留 PVC)
	@kubectl apply -k $(KUSTOMIZE_DIR)/base --selector=!app.kubernetes.io/instance
	@kubectl delete pods -n $(K8S_NAMESPACE) -l app=rcoder --ignore-not-found
	@kubectl apply -k $(KUSTOMIZE_DIR)/base
	@kubectl rollout status deploy/rcoder -n $(K8S_NAMESPACE) --timeout=$(ROLLOUT_TIMEOUT)
	@echo "✅ K8s 部署已重启！"
	@echo "📋 查看状态: kubectl get pods -n $(K8S_NAMESPACE)"
	@kubectl get pods -n $(K8S_NAMESPACE)

# 停止 K8s 开发模式
dev-down-k8s:
	@echo "☸️  停止 K8s 开发模式..."
	# 使用 kustomize 删除
	@kubectl delete -k $(KUSTOMIZE_DIR)/base --ignore-not-found
	@echo "✅ K8s 开发栈已从集群移除"

# 查看 K8s 开发模式日志
dev-logs-k8s:
	@echo "☸️  查看 K8s 开发模式日志..."
	@kubectl logs -n $(K8S_NAMESPACE) -l app=rcoder -f

# ============================================================================
# 其他环境部署命令
# ============================================================================

# 部署开发环境
deploy-dev:
	@echo "☸️  部署开发环境..."
	@kubectl apply -k $(KUSTOMIZE_DIR)/overlays/dev
	@kubectl rollout status deploy/rcoder -n $(K8S_NAMESPACE)-dev --timeout=$(ROLLOUT_TIMEOUT)
	@echo "✅ 开发环境部署完成！"

# 部署生产环境
deploy-prod:
	@echo "☸️  部署生产环境..."
	@kubectl apply -k $(KUSTOMIZE_DIR)/overlays/prod
	@kubectl rollout status deploy/rcoder -n nuwax-rcoder-prod --timeout=$(ROLLOUT_TIMEOUT)
	@echo "✅ 生产环境部署完成！"

# 清理指定环境
undeploy-dev:
	@echo "☸️  清理开发环境..."
	@kubectl delete -k $(KUSTOMIZE_DIR)/overlays/dev --ignore-not-found

undeploy-prod:
	@echo "☸️  清理生产环境..."
	@kubectl delete -k $(KUSTOMIZE_DIR)/overlays/prod --ignore-not-found

# 验证 Kustomize 配置
kustomize-build:
	@echo "☸️  验证 Kustomize 配置..."
	@echo "=== Base ==="
	@kustomize build $(KUSTOMIZE_DIR)/base
	@echo ""
	@echo "=== Dev Overlay ==="
	@kustomize build $(KUSTOMIZE_DIR)/overlays/dev
	@echo ""
	@echo "=== Prod Overlay ==="
	@kustomize build $(KUSTOMIZE_DIR)/overlays/prod

# ============================================================================
# Helm 部署命令
# ============================================================================

# 检查 Helm 是否安装
helm-check:
	@if ! command -v helm &> /dev/null; then \
		echo "Error: Helm is not installed"; \
		echo "Install: curl https://raw.githubusercontent.com/helm/helm/master/scripts/get-helm-3 | bash"; \
		exit 1; \
	fi

# Helm 部署 (默认开发配置)
helm-up: helm-check
	@echo "☸️  Helm 部署 RCoder..."
	@echo ""
	@echo "[1/3] 检查 JuiceFS CSI Driver..."
	@kubectl get daemonset juicefs-csi-driver-node -n kube-system &> /dev/null || \
		(echo "⚠️  JuiceFS CSI 未部署，请先执行:" && \
		 echo "  helm repo add juicefs https://juicefs.github.io/charts" && \
		 echo "  helm install juicefs-csi-driver juicefs/juicefs-csi-driver --namespace kube-system --set webhook.enabled=false" && \
		 exit 1)

	@echo ""
	@echo "[2/3] Helm 部署..."
	@helm upgrade --install $(RELEASE_NAME) $(HELM_DIR) \
		--namespace $(K8S_NAMESPACE) \
		--create-namespace \
		--values $(HELM_DIR)/values-dev.yaml \
		--wait --timeout 5m

	@echo ""
	@echo "[3/3] 部署状态:"
	helm-status

# Helm 部署生产环境
helm-up-prod: helm-check
	@echo "☸️  Helm 部署 RCoder (生产环境)..."
	@helm upgrade --install $(RELEASE_NAME) $(HELM_DIR) \
		--namespace $(K8S_NAMESPACE) \
		--create-namespace \
		--values $(HELM_DIR)/values-production.yaml \
		--wait --timeout 5m
	helm-status

# Helm 升级
helm-upgrade: helm-check
	@echo "☸️  Helm 升级 RCoder..."
	@helm upgrade $(RELEASE_NAME) $(HELM_DIR) \
		--namespace $(K8S_NAMESPACE) \
		--reuse-values \
		--wait --timeout 5m
	helm-status

# Helm 回滚
helm-rollback:
	@echo "☸️  Helm 回滚 RCoder..."
	@helm rollback $(RELEASE_NAME) -n $(K8S_NAMESPACE) --wait --timeout 5m
	helm-status

# Helm 卸载
helm-down:
	@echo "☸️  Helm 卸载 RCoder..."
	@helm uninstall $(RELEASE_NAME) -n $(K8S_NAMESPACE) --wait --timeout 3m
	@echo "✅ RCoder 已卸载"

# Helm 状态
helm-status:
	@echo ""
	@echo "📋 Pods:"
	@kubectl get pods -n $(K8S_NAMESPACE) 2>/dev/null || echo "  (namespace 不存在)"
	@echo ""
	@echo "📋 PVC:"
	@kubectl get pvc -n $(K8S_NAMESPACE) 2>/dev/null || echo "  (namespace 不存在)"
	@echo ""
	@echo "📋 Helm Release:"
	@helm list -n $(K8S_NAMESPACE) 2>/dev/null || echo "  (无 release)"

# Helm 模板渲染 (查看将部署的内容)
helm-template:
	@echo "☸️  Helm 模板渲染..."
	@helm template $(RELEASE_NAME) $(HELM_DIR) \
		--namespace $(K8S_NAMESPACE) \
		--values $(HELM_DIR)/values-dev.yaml

# Helm 打包
helm-package:
	@echo "☸️  Helm 打包..."
	@helm package $(HELM_DIR) --destination $(HELM_DIR)/charts
	@echo "✅ Chart 已打包到 $(HELM_DIR)/charts/"
