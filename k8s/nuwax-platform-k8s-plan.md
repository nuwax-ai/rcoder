# Nuwax Platform K8s 部署方案

本文档记录 **`build-agent-docker` 项目** 从 docker-compose 迁移到 K8s 部署的完整方案。rcoder 项目的 Helm chart 是其中一个组件的**复制来源**,不做二次开发。

> 代码仓库:`/home/swufe/gitworkspace/build-agent-docker`
> 分支:`dev-k8s`
> 对应 rcoder Helm chart:`agents-a24a3ab5f6/k8s/helm/rcoder/`(模板复制基础)

---

## 1. 背景 / 为什么迁移

### 1.1 docker-compose 的两个痛点

1. **hostPath 限制扩容节点**
   - 所有数据卷 bind-mount 到宿主机目录
   - 新加节点时文件系统不共享,rcoder 动态创建的 agent-runner 容器只能跑在同一台机器
   - 单点故障、IO 瓶颈、无弹性

2. **政企客户需要 K8s 原生部署**
   - 很多客户已有自建 K8s 集群
   - 方案必须兼容**标准 K8s 1.25+**(k3s 只作 dev 便利)
   - 必须支持离线部署

### 1.2 目标

- **全栈 Helm chart** (`k8s/helm/nuwax-platform/`),支持 dev / test / prod / offline 四环境
- **JuiceFS + PostgreSQL + MinIO** K8s 原生文件服务栈(替代 hostPath)
- **保留现有 workflow**:`make dev` → `make push-test` → `make push-prod` 不变,新增 `make k8s-deploy-*` 对接
- **政企离线 bundle**(照抄 rcoder offline 模式,自包含所有镜像)
- **双栈并存**:docker-compose 继续可用,不 break 现有流程

---

## 2. 架构

```
┌──────────────────────────────────────────────────────────────┐
│  Ingress (可选, className 可配)                               │
│    └── / → frontend svc:80                                   │
│                                                               │
│  NodePort (dev 便利 / 未配域名时兜底):                         │
│    frontend :30080  MinIO API :30900  Console :30901         │
│                                                               │
│  集群内部 (ClusterIP):                                         │
│    backend:8080(主 REST API)                                 │
│         +18082(Netty 代理 /page/)                            │
│         +18085(computer 路由,noVNC/websockify/audio)         │
│         +18086(模型代理)                                      │
│         +18087(沙盒代理)                                      │
│    mcp-proxy:8089 / rcoder:8086                              │
│    mysql:3306 / redis:6379 / milvus:19530 / es:9200          │
│                                                               │
│  文件服务(K8s 原生,跨节点):                                    │
│    ├── JuiceFS StorageClass (juicefs-sc-<release>, RWX)      │
│    │     元数据: PostgreSQL StatefulSet (块存储 PVC)          │
│    │     数据块: MinIO StatefulSet (块存储 PVC)               │
│    │           bucket juicefs    (JuiceFS chunks)            │
│    │           bucket nuwax-app  (应用对象存储)               │
│    └── RWX PVC (挂 JuiceFS SC):                               │
│          backend-upload / rcoder-workspace /                 │
│          rcoder-computer-workspace                           │
│          动态 agent-runner Pod 也挂同一组                      │
│                                                               │
│  数据库自有块存储 (global.blockStorageClass / 集群默认):        │
│    mysql / redis / milvus / elasticsearch-ik StatefulSet     │
└──────────────────────────────────────────────────────────────┘
```

**分档原则**: 数据库类用**块存储**(RWO,低延迟),共享文件类用 **JuiceFS**(RWX,跨节点)。两档都可通过 values 切换到客户自有 SC。

---

## 3. 关键决策 + 理由

### 3.1 Helm chart 大一统(不用 Kustomize)

| 维度 | 选择 | 理由 |
|------|------|------|
| 部署方式 | **单一 Helm chart**,4 个 values 文件 | 8+ 组件 + 多环境用 Kustomize overlay 会很臃肿;Helm values 参数化更友好,对外交付好用 |
| 已有 K8s 资产 | 推倒重建(tekton / deploy / overlays / 旧 helm 全删) | 现有都是半成品,有硬编码密码/hostPath/和 GitHub Actions 重复的 Tekton |

### 3.2 rcoder:新增 `rcoder-k8s` 镜像(feature flag 编译)

rcoder 源码里 `kubernetes` feature 已经存在:

```toml
# crates/rcoder/Cargo.toml
[features]
default = []
kubernetes = ["docker_manager/kubernetes"]   # CONTAINER_RUNTIME=kubernetes 生效
```

**方式 A(采用)**: 新增独立 image tag `rcoder-k8s`
- `Dockerfile` 加 `ARG CARGO_FEATURES`,`make build-rcoder-k8s` 传 `CARGO_FEATURES=kubernetes`
- 不挂 docker.sock,通过 K8s API 创建动态 agent-runner Pod
- docker-compose 用的 `rcoder` 镜像不受影响

**方式 B(未采用)**: 让 `rcoder` 镜像始终含 kubernetes feature
- 镜像多几 MB,但逻辑简单
- docker-compose 老用户场景也会带上 K8s 代码(轻微浪费)

### 3.3 Elasticsearch IK 分词插件:定制镜像

docker-compose 通过挂载 `docker/config/elasticsearch/plugins/` 使用 IK 插件,K8s 不能挂主机目录,所以:

- 新建 `build_config/elasticsearch-ik/Dockerfile`:4 行 `FROM elasticsearch:9.2.1` + `COPY analysis-ik-9.2.1/`
- 复制 docker-compose 已用的 IK 发行包到 `build_config/elasticsearch-ik/`(维护时双份同步,IK 本身 ~1.5MB)
- K8s values `elasticsearch.image.repository: elasticsearch-ik`

### 3.4 文件服务:**JuiceFS + PG + MinIO,不用 Longhorn**

rcoder chart 有 Longhorn,但 nuwax-platform 不带:
- 数据库用集群默认块存储 SC 就够(用户自有 longhorn/ceph-rbd/local-path 都行)
- 共享文件用 JuiceFS 完全覆盖跨节点读写
- 少装一个 Longhorn 减少离线 bundle 体积(~5GB → ~3GB)

**MinIO 只起一个实例**,开**两个 bucket**:
- `juicefs` — JuiceFS 数据块
- `nuwax-app` — 应用对象存储(backend 的文件等)

### 3.5 兼容标准 K8s 1.25+

明确 **不依赖 k3s 特有机制**:
- StorageClass 空串 → 走集群默认,不强制 `local-path`
- Ingress Controller 由用户提供(`className` 参数化,默认 `nginx`,可改 `traefik` / `alb` / `istio`)
- 镜像拉取走公共或客户私有 registry,不依赖 k3s registries.yaml

k3s 只作为 dev 便利路径(开发者自己本地起一套测),生产一律走标准 K8s。

### 3.6 对外暴露:只 frontend + MinIO API

| 服务 | 是否对外 | 方式 |
|-----|--------|------|
| frontend | ✓ | Ingress(配域名时)或 NodePort(dev) |
| **MinIO API** | **✓** | NodePort 30900(应用要直连 S3 协议) |
| MinIO Console | (可选) | NodePort 30901(管理员) |
| backend / mcp-proxy / rcoder | ✗ | ClusterIP 内部 |
| mysql / redis / milvus / es | ✗ | ClusterIP 内部 |
| postgresql(JuiceFS 元数据) | ✗ | ClusterIP 内部 |

### 3.7 多环境同集群共存

同一 K8s 集群可同时跑 dev / test / prod 三个 release,天然隔离:
- **namespace 级资源** → 各自 namespace 隔离
- **集群级资源**(冲突风险):
  - `ClusterRole` 名字固定 `nuwax-rcoder-pods-clusterrole`(规则相同,多 release 共享,`helm.sh/resource-policy: keep`)
  - `ClusterRoleBinding` 名字 `{release}-rcoder-pods-crb`(Release 独立)
  - `StorageClass` 名字 `juicefs-sc-{release}`(Release 独立)

### 3.8 不在范围内的

| 组件 | 为什么不做 |
|-----|-----------|
| Doris | 未正式使用,后续用到再加 |
| log-platform | 废弃 |
| video-analysis | 废弃 |
| subapp-deployer | 容器里 npm install 安装,不需要独立部署 |
| agent-client | 不需要 K8s 部署 |

---

## 4. 实现(3 个 PR + 1 个 fix commit)

Git 提交历史(`build-agent-docker` 仓库 `dev-k8s` 分支):

```
1563052 refactor(build): remove dev-parallel target, rely on serial make dev
15ca818 fix(k8s): post-review fixes to make deployment actually work
77d33af feat(k8s): offline bundle tooling + production hardening (PR3)
138b313 feat(k8s): complete nuwax-platform Helm chart with K8s-native file service (PR2)
edf1f7d refactor(k8s): rebuild k8s/ directory with Helm chart skeleton (PR1)
```

### PR1 — 清理 + Helm 骨架 (`edf1f7d`)

- 删除 `k8s/{tekton,deploy,overlays,helm/nuwax-platform}`(195 文件)
- 新建 `k8s/helm/nuwax-platform/` 骨架:Chart.yaml + values + `_helpers.tpl` + NOTES + 空目录占位
- 重写 `makefiles/10-k8s.mk`:`k8s-lint` / `k8s-render-*`
- **验证**:helm lint 4 环境通过,render 空骨架

### PR2 — 全量模板 + 构建系统 (`138b313`)

**构建系统**:
- `build_config/rcoder/Dockerfile` 加 `ARG CARGO_FEATURES`
- 新建 `build_config/elasticsearch-ik/Dockerfile` + 复制 IK 插件
- `makefiles/02-build.mk` 加 `build-rcoder-k8s-{amd64,arm64}` / `build-elasticsearch-ik-{amd64,arm64}`,挂到现有 `build-{amd64,arm64}-only`(→ `make dev` 自动覆盖)
- `makefiles/05-push.mk` 加 `push-rcoder-k8s` / `push-elasticsearch-ik`,挂到 `push-custom-images`(→ `make push-test`/`push-prod` 自动覆盖)

**Helm 模板**(33 个):
- `rcoder/` 8 个(复制自 rcoder chart,去 docker.sock + 换 image 为 rcoder-k8s)
- `frontend/` 4 个(nginx.conf 里 `backend:` 通过 replace 自动变 `{release}-backend:`)
- `backend/` 4 个(5 个端口 + ConfigMap 化 application-external.yml + entrypoint.sh)
- `mcp-proxy/` 3 个
- `storage/` 15 个(MySQL/Redis/Milvus/ES/PG/MinIO + JuiceFS Secret/SC + credentials)
- `shared-pvcs/` 3 个 RWX PVC
- `ingress.yaml` 1 个(条件渲染)

**Make 部署目标**:
- `k8s-install-juicefs-csi`(集群级,一次性)
- `k8s-deploy-{dev,test,prod}` + preflight
- `k8s-undeploy-*` / `k8s-purge-*` / `k8s-status-*` / `k8s-logs-<env>-<component>`

### PR3 — 离线 bundle + 生产加固 (`77d33af`)

**离线**:
- `k8s/offline/images.txt` — ~20 个镜像 pin 版本
- `k8s/offline/install.sh` — direct / registry 双模式,direct 自动探测 k3s ctr / nerdctl / ctr
- `k8s/offline/rewrite-registry.sh` — 重 tag 推到客户 registry
- `k8s/offline/README.md` — 客户交付手册
- `makefiles/10-k8s.mk` 加 `k8s-offline-bundle` / `k8s-offline-import`

**加固**:
- `templates/common/network-policy.yaml` — prod 默认 deny-all + 显式放行
- values-prod 开启 NetworkPolicy + Ingress + TLS + PDB

**文档**:`k8s/README.md` 覆盖 dev/test/prod/offline 四场景

### Fix commit — 部署前自查发现的 5 处 bug (`15ca818`)

| # | 问题 | 修法 |
|---|-----|-----|
| 1 | backend 等 Redis 时 `redis-cli ping` 未带 `-a $REDIS_PASSWORD` → NOAUTH,entrypoint exit 1 | `configmap.yaml` replace 加专项规则 |
| 2 | `manage_jwt_secret()` 覆盖 env,Pod 重启换 JWT → session 失效 | Secret 投影为 `/app/config/jwt/jwt_secret_key.txt`,脚本走"读文件"分支 |
| 3 | rcoder 缺 nginx 子应用 conf + 404/502 + 3 个 workspace 子目录 + 脚手架模板 + agent-runner 覆盖目录 | 加 2 个 ConfigMap(nginx conf / binaryData 模板 zip)+ subPath 切分 workspace PVC + emptyDir 给 agent-runner |
| 4 | ES readinessProbe 硬编码 Basic base64 → 改密码后 401 | 改 exec curl -u `elastic:$ELASTIC_PASSWORD`,从 Secret 动态注入 |
| 5 | values-prod.yaml 6 处 CHANGE-ME 需人工替换 | 改成和 `docker/.env` 一致的默认密码(`root` / `admin123` / `123456` / `elastic123` / `minioadmin`)开箱即用 |
| 6 | 文档补漏 | prod namespace PSS ≤ baseline(Milvus 需要 seccomp Unconfined)+ CNI 支持矩阵 |

### 精简 commit — 移除 `dev-parallel` (`1563052`)

- 并行 docker buildx 会偶发 BuildKit cache 竞态,生产/复现难
- `make dev` 串行路径已覆盖所有 K8s 镜像,移除 `dev-parallel` 及其子目标
- 保留 `setup-parallel`(git clone/pull 无竞态)

---

## 5. 使用流程

### 5.1 本地 dev(k3s / kind)

```bash
cd build-agent-docker

# 1. 构建全部镜像 + 推到测试仓库(含 rcoder-k8s + elasticsearch-ik)
make dev

# 2. 装 JuiceFS CSI Driver(集群级,一次性)
make k8s-install-juicefs-csi

# 3. 部署到 nuwax-dev namespace
make k8s-deploy-dev
make k8s-status-dev

# 4. 访问
curl http://<node-ip>:30080/          # frontend
curl http://<node-ip>:30080/api/health # 经 nginx 到 backend

# 验证 JuiceFS 跨 Pod 共享
kubectl exec -n nuwax-dev deploy/nuwax-dev-backend -- sh -c 'echo test > /app/upload/x'
kubectl exec -n nuwax-dev deploy/nuwax-dev-rcoder  -- cat /app/upload/x   # test
```

### 5.2 test / prod

```bash
# 改 values-test.yaml / values-prod.yaml(blockStorageClass / ingress hosts / 域名)
kubectl config use-context <test-cluster>
make k8s-install-juicefs-csi
make k8s-deploy-test

# prod(镜像切生产仓库)
make push-prod
kubectl config use-context <prod-cluster>
make k8s-install-juicefs-csi
make k8s-deploy-prod
```

### 5.3 政企离线

```bash
# 构建机(有网)
make push-prod                           # 先把镜像推到 /nuwax
make k8s-offline-bundle                  # dist/nuwax-offline-<ver>-<arch>.tar.gz (~3GB)

# 客户机器(断网)
scp dist/nuwax-offline-*.tar.gz offline-node:/tmp/
ssh offline-node
cd /tmp && tar xzf nuwax-offline-*.tar.gz -C nuwax-offline && cd nuwax-offline
bash install.sh --mode=direct --env=prod        # 单节点
# 或多节点:
bash install.sh --mode=registry --registry=harbor.internal/nuwax --env=prod
```

---

## 6. 镜像清单 + 用途

| 镜像 | docker-compose | K8s | 说明 |
|-----|:---:|:---:|------|
| agent-platform-front | ✓ | ✓ | 前端 nginx |
| agent-platform-backend | ✓ | ✓ | Spring Boot 后端 |
| mcp-proxy | ✓ | ✓ | Rust MCP 代理 |
| rcoder | ✓ | ✗ | docker-compose 用,挂 docker.sock |
| **rcoder-k8s** | ✗ | ✓ | **K8s 用,feature=kubernetes 编译,K8s API 起 Pod** |
| rcoder-computer-agent-runner | ✓ | ✓ | 动态 agent-runner 基础镜像 |
| agent-client | ✓ | ✗ | 不纳入 K8s |
| elasticsearch(官方) | ✓ | ✗ | docker-compose 用 |
| **elasticsearch-ik** | ✗ | ✓ | **K8s 用,含 IK 中文分词插件** |
| mysql:8.0 | ✓ | ✓ | 第三方 |
| redis:7.0 | ✓ | ✓ | 第三方 |
| milvusdb/milvus:v2.5.8 | ✓ | ✓ | 第三方 |
| minio:RELEASE-xxx | ✓ | ✓ | JuiceFS 后端 + 应用对象存储(2 bucket) |
| postgres:16-alpine | ✗ | ✓ | JuiceFS 元数据(K8s 专用) |
| busybox:1.36 | ✓ | ✓ | init container |
| juicedata/juicefs-csi-driver:v0.31.3 | ✗ | ✓ | K8s 专用,集群级组件 |
| juicedata/mount:ce-v1.3.1 | ✗ | ✓ | JuiceFS CSI 运行时 |
| registry.k8s.io/sig-storage/csi-* 4 个 | ✗ | ✓ | JuiceFS CSI sidecars |

---

## 7. 文件结构

```
build-agent-docker/
├── build_config/
│   ├── rcoder/
│   │   └── Dockerfile                      # 加 ARG CARGO_FEATURES,rcoder-k8s 传 kubernetes
│   └── elasticsearch-ik/                   # 【新增】
│       ├── Dockerfile                      # 4 行 FROM + COPY
│       └── elasticsearch-analysis-ik-9.2.1/
│
├── makefiles/
│   ├── 00-vars.mk                          # + RCODER_K8S_IMAGE / ELASTICSEARCH_IK_IMAGE
│   ├── 02-build.mk                         # + build-rcoder-k8s-* / build-elasticsearch-ik-*
│   ├── 05-push.mk                          # + push-rcoder-k8s / push-elasticsearch-ik
│   └── 10-k8s.mk                           # 整体重写: k8s-lint/render/deploy/offline
│
└── k8s/                                    # 【整体重建】
    ├── README.md                           # 四场景操作指南 + 故障排查
    ├── helm/
    │   └── nuwax-platform/
    │       ├── Chart.yaml
    │       ├── values.yaml                 # 默认
    │       ├── values-dev.yaml             # 本地 k3s / NodePort / 小资源
    │       ├── values-test.yaml            # 自建 K8s / 中等规模
    │       ├── values-prod.yaml            # 副本 2+ / Ingress+TLS / NetworkPolicy
    │       ├── values-offline.yaml         # 私有 registry 占位
    │       ├── configs/                    # .Files.Get 读的配置文件
    │       │   ├── nginx.conf              # (backend: 主机自动替换)
    │       │   ├── application-external.yml
    │       │   ├── docker-entrypoint.sh
    │       │   ├── mcp_config.yml
    │       │   ├── mysql.cnf / redis.conf / milvus/*.yaml / elasticsearch/*
    │       │   ├── sub_app_multi_apps.conf # rcoder 内置 nginx 子应用路由
    │       │   ├── init_mysql{,_data}.sql
    │       │   ├── nginx/custom/{404,502}.html
    │       │   └── rcoder-templates/       # react-vite-template.zip, vue3-vite-template.zip
    │       └── templates/
    │           ├── _helpers.tpl
    │           ├── NOTES.txt
    │           ├── ingress.yaml
    │           ├── frontend/ (4)           # Deploy + Service + nginx CM + 404/502 CM
    │           ├── backend/ (4)            # Deploy + Service + config CM + pdb
    │           ├── mcp-proxy/ (3)
    │           ├── rcoder/ (10)            # 8 复制自 rcoder chart + nginx CM + 模板 CM
    │           ├── storage/ (17)           # 6 组中间件 + PG + MinIO + JuiceFS
    │           ├── shared-pvcs/ (3)        # backend-upload / rcoder-workspace ×2
    │           └── common/ (1)             # network-policy (prod)
    │
    └── offline/                            # 离线 bundle 源素材
        ├── images.txt                      # 20+ 镜像清单
        ├── install.sh                      # direct / registry 双模式
        ├── rewrite-registry.sh
        └── README.md                       # 客户交付手册
```

---

## 8. 已知限制 / 运维注意

| 项 | 详情 |
|----|------|
| K3s flannel 不支持 NetworkPolicy | prod 启用 NetworkPolicy 需用 Calico / Cilium / Antrea / kube-router |
| prod namespace PSS 级别 | 必须 ≤ baseline(Milvus 需要 seccompProfile: Unconfined,restricted 会拒) |
| rcoder chart 双份维护 | `build-agent-docker/.../templates/rcoder/` 和 rcoder 仓库 `k8s/helm/rcoder/` 同步更新 |
| ES IK 插件目录双份 | `docker/config/elasticsearch/plugins/` 和 `build_config/elasticsearch-ik/` 同步 |
| JuiceFS 上的 DB 性能 | MySQL/Redis/Milvus/ES 严禁用 JuiceFS,模板里已强制块存储 PVC |
| 密码管理 | dev/prod values 里是明文默认值(开箱即用);生产环境建议 External Secrets / sealed-secrets |
| direct 模式镜像导入 | 多节点集群每个节点都要运行 install.sh,或推荐 registry 模式一次推送全集群可用 |

---

## 9. 验证矩阵

| 检查项 | 命令 | 预期 |
|-------|------|------|
| Helm chart 语法 | `make k8s-lint` | 4 环境都 pass |
| 模板渲染 | `make k8s-render-all` | 4 环境输出有效 YAML |
| dev 资源数 | `make k8s-render-dev \| grep -c "^kind:"` | 46 |
| prod 资源数 | `make k8s-render-prod \| grep -c "^kind:"` | 54(含 5 NetworkPolicy + Ingress + 2 PDB) |
| 镜像构建全量 | `make -n dev \| grep -c "docker buildx build"` | 20(9 镜像 × 2 架构 + 基础镜像) |
| K8s 集群端到端 | `make k8s-deploy-dev && curl http://<ip>:30080/` | HTTP 200 |
| JuiceFS 跨 Pod 共享 | 见 §5.1 最后两条 kubectl exec | `test` |
| 离线 bundle 构建 | `make k8s-offline-bundle` | `dist/nuwax-offline-*.tar.gz` 产出 |
| 离线 bundle 内容 | `tar tzf dist/nuwax-offline-*.tar.gz \| head` | install.sh + images + charts + values |

---

## 10. 术语

- **release name**: Helm 安装时的名字,和 namespace 同名,形如 `nuwax-dev` / `nuwax-test` / `nuwax-prod`
- **block storage**: 块存储(RWO, 单 Pod 读写),用于数据库
- **shared storage**: 共享文件存储(RWX, 多 Pod 读写),用于上传文件 / workspace
- **JuiceFS CSI**: 集群级 CSI driver,不在 nuwax-platform chart 里,单独 `make k8s-install-juicefs-csi`
- **direct 模式**: 离线部署时把镜像直接导入每个节点 containerd,单节点 / 小集群用
- **registry 模式**: 离线部署时把镜像推到客户私有 registry,多节点推荐
