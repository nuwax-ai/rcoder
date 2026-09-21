# ============================================================================
# deploy-host 宿主机运行形态（Phase 5；cargo feature，见 docs/deploy-host.md）
# ============================================================================
# 形态定位：rcoder 直接跑在宿主机（Docker 必需 / K8s 可选），控制平面经
# published-port 注册表拨 127.0.0.1 触达 agent 容器/Pod。默认目录 ~/.rcoder。

.PHONY: dev-host dev-host-k8s

# Docker 宿主机形态：本地 Docker（OrbStack/Docker Desktop）
# - 默认落 ~/.rcoder/{config.yml,data,workspace,logs,agent-cache}（首启自动生成）
# - agent 容器接入独立网络 rcoder-agent-network（自动创建）
# - HTTP 默认仅监听 127.0.0.1（RCODER_BIND_HOST 可覆盖）；60000 dispatcher 保留
# - 本地 cargo run 开发想用仓库目录时：
#   RCODER_CONFIG_FILE=config.yml RCODER_DEPLOY_HOST_PATH_MAP='/app/project_workspace=./project_workspace,/app/computer-project-workspace=./computer-project-workspace,/app/userapp-workspace=./docker/userapp-workspace,/app/logs=./logs,/app/data=./data,/app/agent-cache=./agent-cache' make dev-host
dev-host:
	@echo "🚀 deploy-host 宿主机形态（Docker 运行时）..."
	cargo run -p rcoder --bin rcoder --features deploy-host

# K8s 宿主机形态：本地 kubeconfig（OrbStack k8s 等）直连集群
# - storage class 默认 local-path / RWO（env 可覆盖）
# - agent Service 自动 NodePort 化 + nodePort 读回注册表
dev-host-k8s:
	@echo "🚀 deploy-host 宿主机形态（K8s 运行时）..."
	CONTAINER_RUNTIME=kubernetes cargo run -p rcoder --bin rcoder --features kubernetes,deploy-host
