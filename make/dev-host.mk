# ============================================================================
# deploy-host 宿主机运行形态（Phase 5；cargo feature，见 docs/deployment/host.md）
# ============================================================================
# 形态定位：rcoder 直接跑在宿主机（Docker 必需 / K8s 可选），控制平面经
# published-port 注册表触达 agent 容器/Pod；K8s NodePort 可指定节点 IP。
# 默认目录 ~/.rcoder。

.PHONY: dev-host dev-host-published dev-host-direct dev-host-k8s

# Docker 宿主机形态：本地 Docker（OrbStack/Docker Desktop）
# - 默认落 ~/.rcoder/{config.yml,data,workspace,logs,agent-cache}（首启自动生成）
# - agent 容器接入独立网络 rcoder-agent-network（自动创建）
# - HTTP 默认仅监听 127.0.0.1（RCODER_BIND_HOST 可覆盖）；60000 dispatcher 保留
# - Reach 默认 auto（OrbStack/Linux→direct 零端口发布；Docker Desktop→published）
# - 本地 cargo run 开发想用仓库目录时：
#   RCODER_CONFIG_FILE=config.yml RCODER_DEPLOY_HOST_PATH_MAP='/app/project_workspace=./project_workspace,/app/computer-project-workspace=./computer-project-workspace,/app/userapp-workspace=./docker/userapp-workspace,/app/logs=./logs,/app/data=./data,/app/agent-cache=./agent-cache' make dev-host
dev-host:
	@echo "🚀 deploy-host 宿主机形态（Docker 运行时）..."
	cargo run -p rcoder --bin rcoder --features deploy-host

# Published 专项：Docker Desktop 或需要验证宿主端口重建时使用。
# OrbStack 的 auto 默认是 Direct，测试 host 组前须显式使用此入口。
dev-host-published:
	@echo "🚀 deploy-host 宿主机形态（Docker Published）..."
	RCODER_DEPLOY_HOST_REACH=published cargo run -p rcoder --bin rcoder --features deploy-host

# Direct 直拨形态：容器零端口发布，注册表登记容器 IPv4 直拨
# （e2e host_direct 套件的前置；详见 docs/deployment/host.md「端口行为」）
dev-host-direct:
	@echo "🚀 deploy-host 宿主机形态（Docker 运行时，Reach=direct 零端口发布）..."
	RCODER_DEPLOY_HOST_REACH=direct cargo run -p rcoder --bin rcoder --features deploy-host

# K8s 宿主机形态：本地 kubeconfig（OrbStack k8s 等）直连集群
# - storage class 默认 local-path / RWO（env 可覆盖）
# - agent Service 自动 NodePort 化 + nodePort 读回注册表
# - NodePort 未转发到宿主机 loopback 时设置 RCODER_K8S_NODE_IP，例如
#   RCODER_K8S_NODE_IP=$(kubectl get node -o jsonpath='{.items[0].status.addresses[?(@.type=="InternalIP")].address}') make dev-host-k8s
# - 三前置（缺一 fail-fast，详见 docs/deployment/host.md「K8s 形态三前置」）：
#   ① userApp 控制面 PG（docker run postgres + RCODER_USERAPP_STORAGE_BACKEND=
#      postgres RCODER_USERAPP_PG_URL=postgres://...@127.0.0.1:55432/userapp）
#   ② ~/.rcoder/config.yml 的 kubernetes_config.services 配 resource_limits
#   ③ 共享 computer workspace PVC 预建（kubectl apply local-path/RWO 10Gi）
# - 若要在 RCoder 进程重启后保留容器登记，设置 RCODER_STORAGE_BACKEND=postgres
#   和 RCODER_PG_URL；memory 后端按现有语义在启动时清理旧计算资源。
dev-host-k8s:
	@echo "🚀 deploy-host 宿主机形态（K8s 运行时）..."
	CONTAINER_RUNTIME=kubernetes cargo run -p rcoder --bin rcoder --features kubernetes,deploy-host,rcoder-pg
