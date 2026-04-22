# ============================================================================
# K8s 开发模式命令
# ============================================================================

IMAGE ?= rcoder:test-k8s
K8S_NAMESPACE := rcoder
ROLLOUT_TIMEOUT ?= 180s
K8S_IMAGE_REGISTRY ?= nuwax-docker-images-registry.cn-hangzhou.cr.aliyuncs.com/dev/rcoder:latest

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

# 启动 K8s 开发模式（部署到已有 K8s 集群）
# NFS 存储层: nfs-subdir-provisioner.yaml (连接外部 NFS Server)
# 如需内建 NFS Server，额外部署 nfs-server.yaml
dev-up-k8s:
	@echo "☸️  启动 K8s 开发模式..."
	# Step 1: 创建 nfs-storage namespace (provisioner 需要)
	@kubectl apply -f k8s/manifests/namespace.yaml
	# Step 2: 部署 NFS Subdir Provisioner + StorageClass
	# ⚠️ 使用外部 NFS Server 时，先修改 nfs-subdir-provisioner.yaml 中的 NFS_SERVER 为实际地址
	@kubectl apply -f k8s/manifests/nfs-subdir-provisioner.yaml
	@echo "⏳ 等待 NFS Provisioner 就绪..."
	@kubectl wait --for=condition=available deployment/nfs-client-provisioner -n nfs-storage --timeout=120s 2>/dev/null || echo "⚠️  NFS Provisioner 等待超时，继续部署..."
	# Step 3: 部署 RCoder 应用层
	@kubectl apply -f k8s/manifests/serviceaccount.yaml
	@kubectl apply -f k8s/manifests/rcoder-configmap.yaml
	@kubectl apply -f k8s/manifests/rcoder-pvc.yaml
	@kubectl apply -f k8s/manifests/rcoder-deployment.yaml
	@kubectl apply -f k8s/manifests/rcoder-service.yaml
	@kubectl apply -f k8s/manifests/rcoder-networkpolicy.yaml
	@kubectl apply -f k8s/manifests/rcoder-pdb.yaml
	@kubectl rollout status deploy/rcoder -n $(K8S_NAMESPACE) --timeout=$(ROLLOUT_TIMEOUT)
	@echo "📋 K8s 部署状态:"
	@kubectl get pods -n $(K8S_NAMESPACE)
	@kubectl get pods -n nfs-storage
	@kubectl get storageclass | grep rcoder-nfs
	@echo ""
	@echo "💡 查看日志: make dev-logs-k8s"

# 重启 K8s 开发模式（重新构建镜像+部署）
dev-restart-k8s: dev-build-k8s
	@echo "☸️  重启 K8s 开发模式..."
	# 重建 NFS Provisioner 层
	@kubectl apply -f k8s/manifests/nfs-subdir-provisioner.yaml
	@echo "⏳ 等待 NFS Provisioner 就绪..."
	@kubectl wait --for=condition=available deployment/nfs-client-provisioner -n nfs-storage --timeout=120s 2>/dev/null || echo "⚠️  NFS Provisioner 等待超时，继续部署..."
	# 重建 RCoder 应用层
	@kubectl apply -f k8s/manifests/serviceaccount.yaml
	@kubectl apply -f k8s/manifests/rcoder-configmap.yaml
	@kubectl apply -f k8s/manifests/rcoder-pvc.yaml
	@kubectl delete pods -n $(K8S_NAMESPACE) -l app=rcoder --ignore-not-found
	@kubectl apply -f k8s/manifests/rcoder-deployment.yaml
	@kubectl apply -f k8s/manifests/rcoder-networkpolicy.yaml
	@kubectl apply -f k8s/manifests/rcoder-pdb.yaml
	@kubectl rollout status deploy/rcoder -n $(K8S_NAMESPACE) --timeout=$(ROLLOUT_TIMEOUT)
	@echo "✅ K8s 部署已重启！"
	@echo "📋 查看状态: kubectl get pods -n $(K8S_NAMESPACE)"
	@kubectl get pods -n nfs-storage

# 停止 K8s 开发模式（与 dev-up-k8s 对称：Workload → RBAC → NFS Storage → Namespace）
dev-down-k8s:
	@echo "☸️  停止 K8s 开发模式..."
	# Step 1: 清理 RCoder 应用层
	@kubectl delete -f k8s/manifests/rcoder-deployment.yaml --ignore-not-found
	@kubectl delete -f k8s/manifests/rcoder-service.yaml --ignore-not-found
	@kubectl delete -f k8s/manifests/rcoder-pdb.yaml --ignore-not-found
	@kubectl delete -f k8s/manifests/rcoder-networkpolicy.yaml --ignore-not-found
	@kubectl delete -f k8s/manifests/rcoder-pvc.yaml --ignore-not-found
	@kubectl delete -f k8s/manifests/rcoder-configmap.yaml --ignore-not-found
	@echo "☸️  清理由 RCoder 运行时创建的 Pod 和 PVC..."
	@kubectl get namespace $(K8S_NAMESPACE) >/dev/null 2>&1 && \
		kubectl delete pods,pvc -n $(K8S_NAMESPACE) -l managed-by=rcoder-runtime --ignore-not-found || true
	@echo "☸️  移除 ServiceAccount 与集群级 RBAC..."
	@kubectl delete -f k8s/manifests/serviceaccount.yaml --ignore-not-found
	@echo "☸️  移除 Namespace $(K8S_NAMESPACE)..."
	@kubectl delete -f k8s/manifests/namespace.yaml --ignore-not-found
	# Step 2: 清理 NFS Provisioner 层
	@kubectl delete -f k8s/manifests/nfs-subdir-provisioner.yaml --ignore-not-found
	@echo "✅ K8s 开发栈已从集群移除"

# 查看 K8s 开发模式日志
dev-logs-k8s:
	@echo "☸️  查看 K8s 开发模式日志..."
	@kubectl logs -n $(K8S_NAMESPACE) -l app=rcoder -f
