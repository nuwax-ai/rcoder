# RCoder 离线部署指南

本文档面向政企/内网客户：**目标机器无公网访问，所有镜像与依赖必须随部署包交付**。

---

## 目录内容（解压 `rcoder-offline-*.tar.gz` 后）

```
.
├── install.sh                       # 主安装脚本
├── rewrite-registry.sh              # registry 模式: re-tag + push 辅助脚本
├── README.md                        # 本文件
├── values-dev.yaml                  # RCoder dev 环境 values
├── values-prod.yaml                 # RCoder prod 环境 values
├── values-offline.yaml              # 离线 registry 覆盖模板
├── images/
│   ├── all-images.tar               # 所有镜像的 docker save 产物
│   └── images.txt                   # 镜像清单
├── charts/
│   └── rcoder-*.tgz                 # RCoder Helm chart
├── longhorn/
│   └── longhorn-v*.yaml             # Longhorn 全量 manifest
├── juicefs-csi/
│   └── juicefs-csi-v*.yaml          # JuiceFS CSI Driver 官方 manifest
└── manifests/                       # 原始 Kustomize 资产 (参考, 不必使用)
```

---

## 前置条件

在离线机器上，提前准备好：

| 组件 | 版本 | 说明 |
|------|------|------|
| K8s | 1.19+ | 已部署, kubeconfig 已配置 |
| kubectl | 与集群匹配 | `kubectl cluster-info` 能通 |
| helm | 3.x | 用于 `helm upgrade --install` |
| docker | 最新 | **仅 registry 模式需要** |
| k3s / nerdctl / ctr | 最新 | **仅 direct 模式需要** |
| open-iscsi | 任意 | Longhorn 依赖 (Ubuntu: `apt install open-iscsi`) |

---

## 两种部署模式

### 模式 A: direct（推荐单节点 / 小集群）

直接把镜像 import 到节点 containerd，**无需私有 registry**。适合 k3s 单节点或 3-5 节点小集群。

多节点集群需要**在每个节点上依次执行**镜像导入。

```bash
# 1. 解压 bundle (可通过 make k8s-offline-import 一步完成)
tar xzf rcoder-offline-<version>-<arch>.tar.gz -C /opt/rcoder-offline
cd /opt/rcoder-offline

# 2. 一键安装
sudo ./install.sh --mode=direct --env=dev

# 生产环境
sudo ./install.sh --mode=direct --env=prod \
    --set credentials.postgresql.password='<prod-pg-pass>' \
    --set credentials.minio.rootPassword='<prod-minio-pass>'
```

> **多节点注意**: 每个节点都要执行 `sudo k3s ctr -n k8s.io image import images/all-images.tar`
> 或把 bundle scp 到每个节点各跑一次 `--skip-juicefs-csi --skip-longhorn` 只做镜像导入。

### 模式 B: registry（推荐多节点 / 生产）

把镜像重新打 tag 推送到客户的私有 registry（Harbor / Nexus / 阿里云 ACR 内网实例），K8s 按常规流程拉取。

**前置: 集群节点能访问私有 registry，并配置好拉取凭据（如需要）。**

```bash
# 1. 解压
tar xzf rcoder-offline-<version>-<arch>.tar.gz -C /opt/rcoder-offline
cd /opt/rcoder-offline

# 2. 如果私有 registry 需要认证, 先登录 docker daemon
docker login harbor.internal

# 3. 安装 (一条命令完成推送+部署)
./install.sh \
    --mode=registry \
    --registry=harbor.internal/rcoder \
    --thirdparty-registry=harbor.internal/thirdparty \
    --env=prod
```

推送完成后，镜像会以如下命名出现在 registry：

| 原镜像 | 推送目标 |
|--------|---------|
| `nuwax-docker-images-registry.cn-hangzhou.cr.aliyuncs.com/dev/rcoder:latest` | `harbor.internal/rcoder/rcoder:latest` |
| `postgres:16-alpine` | `harbor.internal/thirdparty/postgres:16-alpine` |
| `longhornio/longhorn-manager:v1.7.2` | `harbor.internal/thirdparty/longhornio/longhorn-manager:v1.7.2` |
| `registry.k8s.io/sig-storage/csi-provisioner:v5.1.0` | `harbor.internal/thirdparty/sig-storage/csi-provisioner:v5.1.0` |

如果集群需要 imagePullSecret:
```bash
kubectl -n nuwax-rcoder-prod create secret docker-registry harbor-pull \
    --docker-server=harbor.internal \
    --docker-username=<user> \
    --docker-password=<pass>
# 重新安装时:
./install.sh ... --set 'global.imagePullSecrets[0].name=harbor-pull'
```

---

## 常见操作

### 查看部署状态
```bash
kubectl get pods -n nuwax-rcoder-dev
kubectl get svc  -n nuwax-rcoder-dev
kubectl rollout status deploy/rcoder -n nuwax-rcoder-dev
```

### 访问服务
```bash
# NodePort
NODE_IP=$(kubectl get nodes -o jsonpath='{.items[0].status.addresses[?(@.type=="InternalIP")].address}')
curl http://${NODE_IP}:30080/health    # dev
curl http://${NODE_IP}:30081/health    # prod

# 或 port-forward
kubectl port-forward -n nuwax-rcoder-dev svc/rcoder 8087:8087
curl http://localhost:8087/health
```

### 卸载
```bash
helm uninstall rcoder-dev --namespace nuwax-rcoder-dev

# 彻底删除 (包含 PVC)
kubectl delete pvc --all -n nuwax-rcoder-dev
kubectl delete namespace nuwax-rcoder-dev
```

### 升级
收到新 bundle 后直接重新跑 `install.sh`（`helm upgrade --install` 会自动升级）：
```bash
./install.sh --mode=registry --registry=harbor.internal/rcoder --env=prod
```

---

## 跳过选项

| 参数 | 用途 |
|------|------|
| `--skip-longhorn` | 集群已有其他 CSI (Ceph/NFS/CSI-local-path)，不装 Longhorn |
| `--skip-juicefs-csi` | 集群已有 JuiceFS CSI 或用其他 RWX 方案 |
| `--skip-image-import` | 镜像已手工导入，调试用 |

---

## 多节点镜像批量导入（direct 模式）

`install.sh` 只在执行节点导入镜像。多节点集群建议写一个辅助脚本循环每个节点：

```bash
NODES=(node1 node2 node3)
for n in "${NODES[@]}"; do
    scp images/all-images.tar root@$n:/tmp/
    ssh root@$n 'sudo k3s ctr -n k8s.io image import /tmp/all-images.tar && rm /tmp/all-images.tar'
done
# 然后在控制平面节点跑:
./install.sh --mode=direct --env=prod --skip-image-import
```

---

## 故障排查

### Pod 长时间 Pending
```bash
kubectl describe pod -n nuwax-rcoder-dev <pod-name>
# 常见原因:
# - ImagePullBackOff -> 节点 containerd 没有镜像 (direct 模式漏了某节点)
# - 长时间 ContainerCreating -> Longhorn / JuiceFS CSI 未就绪
```

### JuiceFS 挂载失败
```bash
kubectl logs -n kube-system -l app=juicefs-csi-driver-node --tail=50
# 检查 postgresql / minio 是否就绪
kubectl get pods -n nuwax-rcoder-dev -l app=postgresql
kubectl get pods -n nuwax-rcoder-dev -l app=minio
```

### Longhorn 无法启动
```bash
# 常见原因: open-iscsi 未安装
ssh <node> "sudo apt install -y open-iscsi && sudo systemctl enable --now iscsid"
kubectl delete pod -n longhorn-system -l app=longhorn-manager --force
```

---

## 版本信息

本 bundle 固定了以下版本：
- RCoder: (由构建时的 git tag 决定, 见 `images/images.txt`)
- PostgreSQL: 16-alpine
- MinIO: 固定 release tag
- JuiceFS Mount: ce-v1.3.1
- JuiceFS CSI: v0.31.3 (官方 deploy/k8s.yaml; helm 仓库已停更)
- Longhorn: v1.7.2

升级任意组件需要**重新构建 bundle**（`make k8s-offline-bundle`），本离线包不能在线更新。
