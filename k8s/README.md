# RCoder K8s 部署配置

本目录提供 **三种部署方式**，按场景选择：

| 场景 | 方式 | 入口 |
|------|------|------|
| 日常开发 / 在线环境 | **Kustomize** | `deploy-dev.sh` / `deploy-prod.sh` |
| 参数化部署 / 对外交付 | **Helm** (双栈并存) | `helm install ... k8s/helm/rcoder` |
| 政企客户 / 完全断网 | **离线 Bundle** | `make k8s-offline-bundle` → `install.sh` |

另外还有可选的 **本地镜像加速**（`register2/`），面向多节点 k3s / 频繁构建场景，详见下文"本地镜像加速"一节。

## 目录结构

```
k8s/
├── manifests/                    # Kustomize manifests (日常开发 + 在线部署)
│   ├── base/                    # 基础配置 (不可直接部署, 需走 overlay)
│   │   ├── storage/             # postgresql / minio / juicefs-init / juicefs-pvc
│   │   └── rcoder/              # rcoder Deployment/Service/NetworkPolicy/PDB/SA+ClusterRole
│   └── overlays/
│       ├── dev/                 # 开发环境 (nuwax-rcoder-dev, NodePort 30080)
│       └── prod/                # 生产环境 (nuwax-rcoder-prod, NodePort 30081)
│
├── helm/                         # Helm chart (对外交付 + 离线部署源)
│   └── rcoder/
│       ├── Chart.yaml
│       ├── values.yaml           # 默认值 (共享)
│       ├── values-dev.yaml       # dev 覆盖
│       ├── values-prod.yaml      # prod 覆盖
│       ├── values-offline.yaml   # 离线 registry 覆盖
│       └── templates/            # K8s 模板
│
├── offline/                      # 离线 bundle 素材
│   ├── images.txt                # 镜像清单 (单一数据源)
│   ├── install.sh                # 离线安装脚本 (direct / registry 双模式)
│   ├── rewrite-registry.sh       # 重打 tag + 推送私有 registry
│   └── README.md                 # 交付给政企客户的部署手册
│
├── register2/                    # 本地私有镜像仓库 (docker compose)
│   ├── docker-compose.yml        # registry:2 容器 + 数据卷
│   └── README.md                 # 作 k3s mirror / 离线 push 目标的用法
│
├── deploy-dev.sh                 # [在线] Kustomize 开发环境一键部署
├── deploy-prod.sh                # [在线] Kustomize 生产环境一键部署 (含占位符密码守卫)
├── undeploy.sh                   # [在线] 清理部署 (ENV=dev|prod)
│
├── scripts/                      # 辅助脚本
│   ├── deploy-juicefs.sh         # 仅部署存储层 (ENV=dev|prod)
│   ├── test-chat.sh              # 功能冒烟测试 (NAMESPACE 可覆盖)
│   ├── install-k3s-registry-mirrors-cn.sh  # K3s 节点镜像加速配置 (支持 REGISTRY_HOST)
│   └── k3s-registries-cn.yaml    # 镜像加速配置模板
│
└── README.md
```

dev 和 prod 可以并存于同一个集群：
- 集群级资源（StorageClass / ClusterRoleBinding）命名独立：`juicefs-sc-dev` / `juicefs-sc-prod`、`rcoder-pods-crb-dev` / `rcoder-pods-crb-prod`
- NodePort 错开：dev=30080 / prod=30081
- ClusterRole `rcoder-pods-clusterrole` 是唯一共享的集群级资源（两个 overlay 写入完全一致，幂等）

---

## 快速开始

### 开发环境

```bash
# 首次完整部署（含 Longhorn、JuiceFS CSI、open-iscsi 检查）
./k8s/deploy-dev.sh

# 常用
kubectl apply -k k8s/manifests/overlays/dev     # 部署/更新
kubectl delete -k k8s/manifests/overlays/dev    # 删除
kubectl get all -n nuwax-rcoder-dev             # 查看状态
k8s/scripts/test-chat.sh                        # 冒烟测试
```

### 生产环境

> **重要**：首次部署前必须替换 `k8s/manifests/overlays/prod/credentials.yaml` 和 `juicefs-secret.yaml` 里的 `CHANGE-ME-BEFORE-DEPLOY` 占位符；`deploy-prod.sh` 有守卫检查，发现占位符会拒绝部署。若使用 SealedSecret / ExternalSecret 在集群外注入，设置 `FORCE_PROD_DEPLOY=1` 跳过。

```bash
# 首次完整部署
./k8s/deploy-prod.sh

# 跳过占位符检查（仅当通过外部密钥注入方案时）
FORCE_PROD_DEPLOY=1 ./k8s/deploy-prod.sh

# 常用
kubectl apply -k k8s/manifests/overlays/prod
kubectl get all -n nuwax-rcoder-prod
NAMESPACE=nuwax-rcoder-prod k8s/scripts/test-chat.sh
```

### 清理

```bash
ENV=dev  ./k8s/undeploy.sh      # 清理 dev
ENV=prod ./k8s/undeploy.sh      # 清理 prod
```

---

## Makefile 快捷命令

在项目根目录执行：

```bash
# 镜像
make dev-build-k8s             # 构建并推送 K8s 镜像 (含 k3s 本地 import + rollout restart)

# 部署/清理（默认走 overlay）
make deploy-dev                # kubectl apply -k overlays/dev
make deploy-prod               # kubectl apply -k overlays/prod
make undeploy-dev              # 清理 dev
make undeploy-prod             # 清理 prod

# 开发闭环
make dev-up-k8s                # 部署 dev
make dev-restart-k8s           # 重新构建镜像 + 重部署 dev
make dev-down-k8s              # 清理 dev
make dev-logs-k8s              # 跟随 rcoder 日志

# 本地验证（不部署）
make kustomize-build           # 分别 build base / dev / prod

# 离线部署 (政企内网)
make k8s-offline-bundle        # 打包完整离线包 (images + helm + manifests)
make k8s-offline-import BUNDLE=<tgz> INSTALL_ARGS="--mode=direct --env=dev"
make k8s-offline-images-list   # 打印所有离线依赖镜像清单
make k8s-offline-clean         # 清理构建产物
```

---

## Helm 部署 (参数化 / 对外交付)

Helm chart 与 Kustomize **双栈并存、长期维护**。修改 `manifests/base/*.yaml` 时请同步更新 `helm/rcoder/templates/*` 对应模板。

```bash
# dev (本地或在线环境)
helm install rcoder-dev k8s/helm/rcoder \
    --namespace nuwax-rcoder-dev --create-namespace \
    -f k8s/helm/rcoder/values-dev.yaml

# 同集群已有 Kustomize 部署的话, 加 --set rcoder.clusterRole.create=false
# (避免 Helm 尝试接管 Kustomize 已创建的 ClusterRole)
helm install rcoder-helm-test k8s/helm/rcoder \
    --namespace nuwax-rcoder-helm-test --create-namespace \
    -f k8s/helm/rcoder/values-dev.yaml \
    --set rcoder.service.nodePort=30082 \
    --set juicefs.storageClass.name=juicefs-sc-helm-test \
    --set rcoder.clusterRole.create=false

# prod (密码通过 --set 或 external secret 注入)
helm install rcoder-prod k8s/helm/rcoder \
    --namespace nuwax-rcoder-prod --create-namespace \
    -f k8s/helm/rcoder/values-prod.yaml \
    --set credentials.postgresql.password=<real> \
    --set credentials.minio.rootPassword=<real>

# 升级
helm upgrade rcoder-dev k8s/helm/rcoder -f k8s/helm/rcoder/values-dev.yaml

# 卸载
helm uninstall rcoder-dev --namespace nuwax-rcoder-dev
```

### 渲染对比 (开发调试)

```bash
helm template rcoder-dev k8s/helm/rcoder -f k8s/helm/rcoder/values-dev.yaml > /tmp/helm-dev.yaml
kubectl kustomize k8s/manifests/overlays/dev > /tmp/kustomize-dev.yaml
diff <(grep -E 'image:|nodePort:|storageClassName:' /tmp/helm-dev.yaml | sort -u) \
     <(grep -E 'image:|nodePort:|storageClassName:' /tmp/kustomize-dev.yaml | sort -u)
```

### 集群级资源命名

Helm 为每个 release 自动生成独立的集群级资源名：

| 资源 | dev release (`rcoder-dev`) | prod release (`rcoder-prod`) |
|------|---------------------------|------------------------------|
| StorageClass | `juicefs-sc-dev` (显式 override) | `juicefs-sc-prod` (显式 override) |
| ClusterRoleBinding | `rcoder-dev-pods-crb` (自动) | `rcoder-prod-pods-crb` (自动) |
| ClusterRole | `rcoder-pods-clusterrole` (共享, `resource-policy: keep`) |

---

## 离线部署 (政企内网, 完全断网)

当客户内网无公网访问时，使用 **Bundle 模式**：在有网的构建机打包所有镜像、helm charts、manifests，产出单个 `tar.gz`；离线机器上解压运行 `install.sh` 完成部署。

### 构建 bundle (有网构建机)

```bash
# 拉取所有镜像 + 打包 helm charts + 下载 Longhorn/JuiceFS CSI manifest + 压缩
make k8s-offline-bundle

# 输出: dist/rcoder-offline-<version>-<arch>.tar.gz (约 1.7-2 GB, 共 27 个镜像)
ls -lh dist/rcoder-offline-*.tar.gz

# 查看镜像清单
make k8s-offline-images-list
```

### 离线机器部署

```bash
# 方式 1: 用 make 一键跑完
make k8s-offline-import BUNDLE=/path/to/rcoder-offline-xxx.tar.gz \
    INSTALL_ARGS="--mode=direct --env=dev"

# 方式 2: 手工解压 + 跑 install.sh
tar xzf rcoder-offline-xxx.tar.gz -C /opt/rcoder-offline
cd /opt/rcoder-offline
sudo bash install.sh --mode=direct --env=dev
```

### 两种导入模式

| 模式 | 镜像落盘方式 | 适用场景 |
|------|------------|----------|
| `--mode=direct` | `ctr image import` 到节点 containerd | 单节点 / 小集群 / 无私有 registry |
| `--mode=registry` | `docker load` + re-tag + push | 多节点 / 已有 Harbor/Nexus / 客户自建 registry |

详见 [`offline/README.md`](./offline/README.md)（客户交付文档）。

---

## 本地镜像加速（多节点 / 频繁构建场景）

当本地 k3s 有多个节点，或者你频繁 push 测试镜像又不想每次走公网时，启用 `register2/` 作本地 registry mirror，所有节点直接拉局域网镜像（千兆）。

### 架构

```
  k3s/containerd (所有节点)
        │
        ├── (1) http://<你的机器>:5000  ← register2 (本目录, 私有推送)
        ├── (2) http://<你的机器>:5002  ← registry-cache (可选 pull-through)
        ├── (3) https://docker.m.daocloud.io / ...  ← 公网 cn mirror 回退
        └── (4) 默认源 (docker.io / registry.k8s.io) 兜底
```

containerd 按顺序尝试 endpoint，命中（200）即返回，失败（404/超时）自动回退下一个。

### 启动本地 registry

```bash
cd k8s/register2
docker compose up -d
# 验证:
curl -s http://<你的机器>:5000/v2/_catalog
```

### 一键配置 k3s 节点

```bash
# 在每个 k3s 节点上执行 (REGISTRY_HOST 按实际填)
sudo REGISTRY_HOST=192.168.32.228:5000 \
    k8s/scripts/install-k3s-registry-mirrors-cn.sh

# 多个 registry 串联 (比如同时有 registry2 和 pull-through cache)
sudo REGISTRY_HOST=192.168.32.228:5000,192.168.32.228:5002 \
    k8s/scripts/install-k3s-registry-mirrors-cn.sh
```

脚本会：
- 备份现有 `/etc/rancher/k3s/registries.yaml`
- 写入包含本地 registry + 公网 cn mirror 回退的完整配置（含 `registry.k8s.io`/`ghcr.io`/`quay.io` 全套）
- 自动加 "自指 mirror" + `insecure_skip_verify` 允许 `crictl pull <host>/foo` 走 plain HTTP
- 自动 `systemctl restart k3s` 或 `k3s-agent`
- 用 `crictl pull docker.io/rancher/mirrored-pause:3.6` 验证

### 本机 docker daemon 配置（允许 push 到 HTTP registry）

```bash
sudo tee /etc/docker/daemon.json <<EOF
{
  "insecure-registries": ["192.168.32.228:5000"]
}
EOF
sudo systemctl restart docker
```

详见 [`register2/README.md`](./register2/README.md)。

### 与离线 bundle 的联动

配好本地 registry 后，在本机就能测通 bundle 的 `registry 模式`：

```bash
tar xzf dist/rcoder-offline-*.tar.gz -C /tmp/offline-test
cd /tmp/offline-test
docker load -i images/all-images.tar
bash rewrite-registry.sh \
    --registry 192.168.32.228:5000/rcoder \
    --thirdparty-registry 192.168.32.228:5000/thirdparty \
    --images-file images/images.txt
# 之后 helm install 时 k3s 自动从 192.168.32.228:5000 拉
```

---

## 存储架构

```
┌─────────────────────────────────────────────────────────────┐
│                     Kubernetes 集群                          │
│  ┌──────────────────────────────────────────────────────┐  │
│  │            JuiceFS CSI Driver (kube-system)           │  │
│  └──────────────────────────────────────────────────────┘  │
│                            ↓                                │
│  ┌────────────────┐    ┌────────────────┐                   │
│  │ rcoder 主服务   │    │ Agent Runner   │                   │
│  │ (Deployment)   │    │ (动态创建 Pod) │                    │
│  └────────┬───────┘    └────────┬───────┘                   │
│           └──────────┬───────────┘                          │
│                      ↓                                      │
│              ┌──────────────┐                               │
│              │ JuiceFS 卷   │  ← ReadWriteMany 共享         │
│              └───────┬──────┘                               │
└──────────────────────┼──────────────────────────────────────┘
                       ↓
┌───────────────────────────────────────────────────────────────┐
│  MinIO (S3)    ← JuiceFS 数据存储（per-namespace）             │
│  PostgreSQL    ← JuiceFS 元数据存储（per-namespace）           │
└───────────────────────────────────────────────────────────────┘
```

每个 namespace 独立拥有 PostgreSQL 和 MinIO 实例，metadata 和 object 存储互不干扰。

---

## 环境要求

- Kubernetes 1.14+（本仓库在 K3s 1.34.x 上测试）
- `kubectl` 已配置 kubeconfig
- `helm` 3.x（用于首次安装 JuiceFS CSI Driver + Helm 部署路径）
- Longhorn 存储（为 PostgreSQL / MinIO 提供 PV；`deploy-*.sh` 会自动安装）
- `open-iscsi`（Longhorn 依赖；`deploy-*.sh` 会自动安装）
- `docker`（仅在有网构建机 `make k8s-offline-bundle` 或本地 push 到 register2 时需要）

国内节点建议先配置 k3s 镜像加速（纯公网 cn mirror 版）：
```bash
sudo k8s/scripts/install-k3s-registry-mirrors-cn.sh
```

有本地 registry 时用 `REGISTRY_HOST=...`（见 "本地镜像加速" 一节）。

---

## 故障排查

```bash
# Pod 状态
kubectl get pods -n nuwax-rcoder-dev   # 或 -prod / -helm-test / <your-ns>

# 事件
kubectl get events -n nuwax-rcoder-dev --sort-by='.lastTimestamp'

# RCoder 日志
kubectl logs -n nuwax-rcoder-dev -l app=rcoder --tail=200 -f

# PVC 绑定情况
kubectl get pvc -n nuwax-rcoder-dev
kubectl describe pvc rcoder-workspace -n nuwax-rcoder-dev

# StorageClass
kubectl get sc

# 镜像加速是否生效 (与 kubelet 同链路验证, 不要用 k3s ctr images pull)
sudo crictl --runtime-endpoint unix:///run/k3s/containerd/containerd.sock \
    pull docker.io/rancher/mirrored-pause:3.6

# 本地 registry 命中情况
curl -s http://192.168.32.228:5000/v2/_catalog
curl -s http://192.168.32.228:5002/v2/_catalog   # pull-through 缓存
```

---

## 历史说明

此前版本曾提供 `k8s/helm/rcoder/` Helm chart 作为另一条部署路径；**2025 年后改为 Kustomize + Helm 双栈并存**：
- Kustomize 用于开发和已有部署，保持轻量；
- Helm 用于参数化对外交付和离线 bundle；
- 两者共享同一份 K8s 资源定义（修改时需同步）。
