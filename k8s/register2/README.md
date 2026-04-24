## registry2（本地私有镜像仓库）

本目录用 docker compose 起一个 `registry:2` 容器，作为局域网内的镜像中转站——既供 docker push 测试用，也可配到 k3s 节点作首选 mirror。

### 地址
- **私有仓库**：`192.168.32.228:5000`（本机启动后局域网可达）

### 启动/停止

```bash
cd /home/swufe/gitworkspace/rcoder/k8s/register2
docker compose up -d
docker compose ps
docker compose logs -f
docker compose down
```

数据落地在同目录下的 `./data/` （compose 已声明 volume）。

---

## 作为 k3s 的镜像 mirror

### 架构

```
  k3s/containerd (两个节点)
        │
        ├──(1st) http://192.168.32.228:5000  ← registry2 (本目录, 私有推送)
        ├──(2nd) http://192.168.32.228:5002  ← registry-cache (pull-through, 可选)
        ├──(3rd) https://docker.m.daocloud.io / ...  ← 公网 cn mirror 回退
        └──(4th) https://*.docker.io / registry.k8s.io (默认兜底)
```

containerd 按顺序尝试 endpoint，404 自动回退到下一个。

### 一键配置两个节点

```bash
# 1. 启动 registry2
cd k8s/register2 && docker compose up -d

# 2. 给每个 k3s 节点配 mirror (用 REGISTRY_HOST 环境变量)
sudo REGISTRY_HOST=192.168.32.228:5000 \
    k8s/scripts/install-k3s-registry-mirrors-cn.sh

# 脚本会:
#   - 备份已有 /etc/rancher/k3s/registries.yaml (如有)
#   - 写入新配置 (本地 registry 作首选, 公网 cn mirror 兜底)
#   - 自动 restart k3s 或 k3s-agent
#   - 用 crictl pull 验证 mirror 是否生效

# 3. 配置本机 docker daemon 允许 push 到 HTTP registry
# 编辑 /etc/docker/daemon.json:
#   {
#     "insecure-registries": ["192.168.32.228:5000"]
#   }
sudo systemctl restart docker
```

### 多个 registry 串联

如果同时有 registry2 (5000, 私有存储) 和 registry-cache (5002, pull-through 缓存)：

```bash
sudo REGISTRY_HOST=192.168.32.228:5000,192.168.32.228:5002 \
    k8s/scripts/install-k3s-registry-mirrors-cn.sh
```

### 验证

```bash
# 1) 推个镜像进去看 5000 是否可用
docker tag busybox:1.36 192.168.32.228:5000/busybox:test
docker push 192.168.32.228:5000/busybox:test
curl -s http://192.168.32.228:5000/v2/_catalog

# 2) k3s 节点上: 用 crictl 拉 (与 kubelet 一致的链路)
sudo crictl --runtime-endpoint unix:///run/k3s/containerd/containerd.sock \
    pull 192.168.32.228:5000/busybox:test

# 3) 验证 mirror 路由 (通过 docker.io 间接拉)
sudo crictl --runtime-endpoint unix:///run/k3s/containerd/containerd.sock \
    pull docker.io/rancher/mirrored-pause:3.6
```

---

## 与离线 bundle 的联动

配好 registry mirror 后，bundle 的 `--mode=registry` 可以直接推到 5000：

```bash
tar xzf dist/rcoder-offline-*.tar.gz -C /tmp/offline-test
cd /tmp/offline-test
docker load -i images/all-images.tar
bash rewrite-registry.sh \
    --registry 192.168.32.228:5000/rcoder \
    --thirdparty-registry 192.168.32.228:5000/thirdparty \
    --images-file images/images.txt

# 之后 helm install 时 k3s 会自动从本地 registry 拉, 走内网千兆
helm install rcoder-offline k8s/helm/rcoder \
    -f k8s/helm/rcoder/values-dev.yaml \
    -f k8s/helm/rcoder/values-offline.yaml \
    --set global.imageRegistry=192.168.32.228:5000/rcoder \
    --set global.thirdPartyRegistry=192.168.32.228:5000/thirdparty \
    --namespace nuwax-rcoder-offline-test --create-namespace
```
