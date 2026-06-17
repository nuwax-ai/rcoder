# 应用管理服务设计文档

## 1. 概述

### 1.1 背景

RCoder 需要为用户提供应用部署能力。支持两种部署模式：
- **Docker 模式**：开发/测试环境，单机部署
- **K8s 模式**：生产环境，集群部署

### 1.2 核心功能

- 应用生命周期管理（创建、启动、停止、重启、删除）
- 多语言支持（Java、Python、TypeScript、Go、Rust）
- 服务暴露（HTTP/TCP）
- 健康检查
- 日志查看

---

## 2. 部署模式

### 2.1 Docker 模式

适用于开发/测试环境，单机部署。

**架构：**
```
用户 → Pingora → 容器
```

**特点：**
- 容器直接管理
- Pingora 代理流量
- 目录挂载持久化

### 2.2 K8s 模式

适用于生产环境，集群部署。

**架构：**
```
用户 → Gateway API → Service → Pod
```

**特点：**
- Pod 作为最小单元
- Service 提供服务发现
- Gateway API 管理入口流量
- ConfigMap/Secret 管理配置
- PVC 管理持久化存储
- Liveness/Readiness Probe 健康检查

---

## 3. K8s 云原生设计

### 3.1 资源模型

K8s 模式下，每个应用对应以下资源：

```
┌─────────────────────────────────────────────────────────────────┐
│                    应用资源结构                                  │
├─────────────────────────────────────────────────────────────────┤
│                                                                 │
│  ┌─────────────────────────────────────────────────────────┐   │
│  │  Namespace: rcoder-apps                                 │   │
│  │                                                         │   │
│  │  ┌─────────────┐    ┌─────────────┐                    │   │
│  │  │ ConfigMap   │    │ Secret      │                    │   │
│  │  │ (配置)      │    │ (敏感信息)  │                    │   │
│  │  └──────┬──────┘    └──────┬──────┘                    │   │
│  │         │                  │                            │   │
│  │         └────────┬─────────┘                            │   │
│  │                  ▼                                      │   │
│  │         ┌─────────────────┐                            │   │
│  │         │  Deployment     │                            │   │
│  │         │  (Pod 模板)     │                            │   │
│  │         └────────┬────────┘                            │   │
│  │                  │                                      │   │
│  │                  ▼                                      │   │
│  │         ┌─────────────────┐                            │   │
│  │         │  Service        │                            │   │
│  │         │  (服务发现)     │                            │   │
│  │         └────────┬────────┘                            │   │
│  │                  │                                      │   │
│  │         ┌────────┴────────┐                            │   │
│  │         ▼                 ▼                            │   │
│  │  ┌─────────────┐  ┌─────────────┐                    │   │
│  │  │ HTTPRoute   │  │ NodePort    │                    │   │
│  │  │ (HTTP 路由) │  │ (TCP 端口)  │                    │   │
│  │  └─────────────┘  └─────────────┘                    │   │
│  │                                                         │   │
│  │  ┌─────────────────────────────────────────────────┐   │   │
│  │  │  PVC (持久化存储)                                │   │   │
│  │  │  - app-data: 应用数据                            │   │   │
│  │  │  - app-logs: 日志文件                            │   │   │
│  │  └─────────────────────────────────────────────────┘   │   │
│  └─────────────────────────────────────────────────────────┘   │
│                                                                 │
└─────────────────────────────────────────────────────────────────┘
```

### 3.2 资源定义

**Deployment：**
```yaml
apiVersion: apps/v1
kind: Deployment
metadata:
  name: app-123
  namespace: rcoder-apps
  labels:
    app: app-123
    managed-by: rcoder
spec:
  replicas: 1
  selector:
    matchLabels:
      app: app-123
  template:
    metadata:
      labels:
        app: app-123
    spec:
      containers:
      - name: app
        image: eclipse-temurin:17
        command: ["java", "-jar", "/app/code/app.jar"]
        ports:
        - containerPort: 8080
        envFrom:
        - configMapRef:
            name: app-123-config
        - secretRef:
            name: app-123-secret
        volumeMounts:
        - name: app-data
          mountPath: /app/data
        - name: app-logs
          mountPath: /app/logs
        resources:
          requests:
            cpu: "1"
            memory: "512Mi"
          limits:
            cpu: "2"
            memory: "1Gi"
        livenessProbe:
          httpGet:
            path: /health
            port: 8080
          initialDelaySeconds: 30
          periodSeconds: 10
        readinessProbe:
          httpGet:
            path: /health
            port: 8080
          initialDelaySeconds: 5
          periodSeconds: 5
      volumes:
      - name: app-data
        persistentVolumeClaim:
          claimName: app-123-data
      - name: app-logs
        persistentVolumeClaim:
          claimName: app-123-logs
```

**Service：**
```yaml
apiVersion: v1
kind: Service
metadata:
  name: app-123-svc
  namespace: rcoder-apps
spec:
  selector:
    app: app-123
  ports:
  - name: http
    port: 8080
    targetPort: 8080
  # TCP 端口（如数据库）
  - name: postgres
    port: 5432
    targetPort: 5432
```

**NodePort Service（TCP 外部访问）：**
```yaml
apiVersion: v1
kind: Service
metadata:
  name: app-123-nodeport
  namespace: rcoder-apps
spec:
  type: NodePort
  selector:
    app: app-123
  ports:
  - name: postgres
    port: 5432
    targetPort: 5432
    # nodePort 由 K8s 自动分配
```

**HTTPRoute：**
```yaml
apiVersion: gateway.networking.k8s.io/v1
kind: HTTPRoute
metadata:
  name: app-123-route
  namespace: rcoder-apps
spec:
  parentRefs:
  - name: nuwax-gateway
    namespace: default
  rules:
  - matches:
    - path:
        type: PathPrefix
        value: /apps/app-123
    backendRefs:
    - name: app-123-svc
      port: 8080
```

**ConfigMap：**
```yaml
apiVersion: v1
kind: ConfigMap
metadata:
  name: app-123-config
  namespace: rcoder-apps
data:
  APP_ENV: "production"
  LOG_LEVEL: "info"
  DB_HOST: "app-456-svc.rcoder-apps"
  DB_PORT: "5432"
```

**Secret：**
```yaml
apiVersion: v1
kind: Secret
metadata:
  name: app-123-secret
  namespace: rcoder-apps
type: Opaque
data:
  DB_PASSWORD: cGFzc3dvcmQ=  # base64 encoded
  API_KEY: c2VjcmV0LWtleQ==
```

**PVC：**
```yaml
apiVersion: v1
kind: PersistentVolumeClaim
metadata:
  name: app-123-data
  namespace: rcoder-apps
spec:
  accessModes:
    - ReadWriteOnce
  storageClassName: ceph-rbd
  resources:
    requests:
      storage: 10Gi
```

---

## 4. HTTP API 设计

### 4.1 应用管理接口

#### 生命周期管理

| 方法 | 路径 | 描述 | 说明 |
|------|------|------|------|
| POST | `/api/v1/apps` | 创建应用 | 创建 Deployment + Service + HTTPRoute |
| POST | `/api/v1/apps/query` | 查询应用列表 | 支持复杂过滤和分页 |
| GET | `/api/v1/apps/{app_id}` | 获取应用详情 | 包含状态、访问信息、健康状态 |
| PUT | `/api/v1/apps/{app_id}` | 更新应用配置 | 更新 ConfigMap/Secret |
| DELETE | `/api/v1/apps/{app_id}` | 删除应用 | 删除所有关联资源 |

#### 应用操作

| 方法 | 路径 | 描述 | 说明 |
|------|------|------|------|
| POST | `/api/v1/apps/{app_id}/start` | 启动应用 | 启动已停止的应用 |
| POST | `/api/v1/apps/{app_id}/stop` | 停止应用 | 停止运行中的应用 |
| POST | `/api/v1/apps/{app_id}/restart` | 重启应用 | 重启应用 |

#### 查询接口

| 方法 | 路径 | 描述 | 说明 |
|------|------|------|------|
| GET | `/api/v1/apps/{app_id}/logs` | 获取日志 | 支持 tail、follow |
| GET | `/api/v1/apps/{app_id}/health` | 获取健康状态 | Pod 状态 + Probe 结果 |
| GET | `/api/v1/apps/{app_id}/events` | 获取事件 | K8s Events |
| GET | `/api/v1/apps/{app_id}/stats` | 获取资源使用 | CPU/内存/网络 |

#### 文件管理接口

| 方法 | 路径 | 描述 | 说明 |
|------|------|------|------|
| POST | `/api/v1/apps/{app_id}/upload` | 上传文件 | multipart/form-data |
| GET | `/api/v1/apps/{app_id}/files` | 列出文件 | 列出应用目录文件 |
| DELETE | `/api/v1/apps/{app_id}/files/{path}` | 删除文件 | 删除指定文件 |

### 4.2 创建应用

**请求：**

```json
POST /api/v1/apps

{
  "name": "my-java-app",
  "image": "eclipse-temurin:17",
  "command": ["java", "-jar", "/app/code/app.jar"],
  "env": {
    "APP_ENV": "production",
    "DB_HOST": "app-456-svc.rcoder-apps"
  },
  "secrets": {
    "DB_PASSWORD": "password123"
  },
  "resources": {
    "cpu": "1",
    "memory": "512Mi",
    "storage": "10Gi"
  },
  "ports": [
    { "name": "http", "port": 8080, "expose_type": "Http" },
    { "name": "postgres", "port": 5432, "expose_type": "Tcp" }
  ],
  "health_check": {
    "type": "Http",
    "path": "/health",
    "port": 8080
  }
}
```

**请求参数说明：**

| 参数 | 类型 | 必填 | 说明 |
|------|------|------|------|
| name | String | 是 | 应用名称 |
| image | String | 是 | 容器镜像 |
| command | Vec\<String\> | 否 | 启动命令 |
| env | HashMap | 否 | 环境变量（存储到 ConfigMap） |
| secrets | HashMap | 否 | 敏感信息（存储到 Secret） |
| resources.cpu | String | 否 | CPU: "1", "500m", "0.5" |
| resources.memory | String | 否 | 内存: "512Mi", "1Gi" |
| resources.storage | String | 否 | 存储: "10Gi" |
| ports | Vec | 否 | 端口配置 |
| ports[].name | String | 是 | 端口名称: "http", "postgres" |
| ports[].port | u16 | 是 | 容器端口 |
| ports[].expose_type | Enum | 是 | 暴露类型: Http / Tcp |
| health_check.type | Enum | 否 | 健康检查类型: Http / Tcp / Exec |
| health_check.path | String | 否 | HTTP 检查路径 |
| health_check.port | u16 | 否 | 检查端口 |

**响应：**

```json
{
  "success": true,
  "data": {
    "app_id": "app-123",
    "name": "my-java-app",
    "status": "Running",
    "access": {
      "external": {
        "http": "http://192.168.11.216:30080/apps/app-123",
        "tcp": [
          {
            "name": "postgres",
            "node_port": 30432,
            "access_url": "tcp://192.168.11.216:30432"
          }
        ]
      },
      "internal": {
        "domain": "app-123-svc.rcoder-apps.svc.cluster.local",
        "short_domain": "app-123-svc.rcoder-apps",
        "ports": [
          { "name": "http", "port": 8080 },
          { "name": "postgres", "port": 5432 }
        ]
      }
    },
    "resources": {
      "cpu": "1",
      "memory": "512Mi",
      "storage": "10Gi"
    },
    "created_at": "2026-06-16T10:00:00Z"
  }
}
```

### 4.3 更新应用配置

更新应用配置，支持更新环境变量、镜像等。

**请求：**

```json
PUT /api/v1/apps/app-123

{
  "env": {
    "APP_ENV": "production",
    "LOG_LEVEL": "info"
  },
  "secrets": {
    "DB_PASSWORD": "new-password"
  }
}
```

**更新镜像（重启生效）：**

```json
PUT /api/v1/apps/app-123

{
  "image": "my-app:v2.0.0"
}
```

**响应：**

```json
{
  "success": true,
  "data": {
    "app_id": "app-123",
    "name": "my-java-app",
    "status": "Running",
    "message": "配置已更新，重启后生效"
  }
}
```

**说明：**

- 更新环境变量：更新 ConfigMap/Secret，需要重启生效
- 更新镜像：更新 Deployment，触发滚动更新
- 可以同时更新多个配置

### 4.4 获取应用详情

**请求：**

```json
GET /api/v1/apps/app-123
```

**响应：**

```json
{
  "success": true,
  "data": {
    "app_id": "app-123",
    "name": "my-java-app",
    "status": "Running",
    "image": "eclipse-temurin:17",
    "command": ["java", "-jar", "/app/code/app.jar"],
    "replicas": 1,
    "access": {
      "external": {
        "http": "http://192.168.11.216:30080/apps/app-123",
        "tcp": [
          {
            "name": "postgres",
            "node_port": 30432,
            "access_url": "tcp://192.168.11.216:30432"
          }
        ]
      },
      "internal": {
        "domain": "app-123-svc.rcoder-apps.svc.cluster.local",
        "short_domain": "app-123-svc.rcoder-apps",
        "ports": [
          { "name": "http", "port": 8080 },
          { "name": "postgres", "port": 5432 }
        ]
      }
    },
    "health": {
      "status": "Healthy",
      "instance": {
        "name": "app-123-7d8b9c6f5-x2z4k",
        "phase": "Running",
        "ready": true,
        "restart_count": 0,
        "node": "worker-1",
        "ip": "10.244.1.15",
        "started_at": "2026-06-16T10:00:00Z"
      },
      "probes": {
        "liveness": { "status": "Passed", "last_checked": "2026-06-16T10:30:00Z" },
        "readiness": { "status": "Passed", "last_checked": "2026-06-16T10:30:00Z" }
      }
    },
    "resources": {
      "cpu": "1",
      "memory": "512Mi",
      "storage": "10Gi"
    },
    "env": {
      "APP_ENV": "production",
      "DB_HOST": "app-456-svc.rcoder-apps"
    },
    "created_at": "2026-06-16T10:00:00Z",
    "updated_at": "2026-06-16T10:00:00Z"
  }
}
```

**响应字段说明：**

| 字段 | 类型 | 说明 |
|------|------|------|
| app_id | String | 应用 ID |
| name | String | 应用名称 |
| status | Enum | 状态：Created/Starting/Running/Stopping/Stopped/Error |
| image | String | 容器镜像 |
| command | Vec\<String\> | 启动命令 |
| replicas | u32 | 副本数 |
| access.external.http | String | 外部 HTTP 访问地址 |
| access.external.tcp | Vec | 外部 TCP 端口列表 |
| access.internal.domain | String | 集群内部完整域名 |
| access.internal.short_domain | String | 集群内部简写域名 |
| access.internal.ports | Vec | 内部端口列表 |
| health.status | String | 健康状态 |
| health.instance | Object | 实例信息（K8s 为 Pod） |
| health.probes | Object | Probe 结果 |
| resources | Object | 资源配置 |
| env | HashMap | 环境变量 |

### 4.5 查询应用列表

**请求：**

```json
POST /api/v1/apps/query

{
  "page": 1,
  "page_size": 20,
  "filters": {
    "status": ["Running", "Created"],
    "name": "my-app",
    "app_ids": ["app-123", "app-456"],
    "created_at": {
      "start": "2026-06-01T00:00:00Z",
      "end": "2026-06-16T23:59:59Z"
    }
  },
  "sort_by": "created_at",
  "sort_order": "desc"
}
```

**请求参数：**

| 参数 | 类型 | 必填 | 说明 |
|------|------|------|------|
| page | u32 | 否 | 页码，默认 1 |
| page_size | u32 | 否 | 每页数量，默认 20，最大 100 |
| filters.status | Vec\<String\> | 否 | 按状态过滤 |
| filters.name | String | 否 | 按名称模糊搜索 |
| filters.app_ids | Vec\<String\> | 否 | 按应用 ID 过滤 |
| filters.created_at.start | String | 否 | 创建时间起始（RFC3339） |
| filters.created_at.end | String | 否 | 创建时间结束（RFC3339） |
| sort_by | String | 否 | 排序字段：name/created_at/updated_at |
| sort_order | String | 否 | 排序方式：asc/desc |

**响应：**

```json
{
  "success": true,
  "data": {
    "items": [
      {
        "app_id": "app-123",
        "name": "my-java-app",
        "status": "Running",
        "image": "eclipse-temurin:17",
        "access": {
          "external": {
            "http": "http://192.168.11.216:30080/apps/app-123"
          }
        },
        "health": { "status": "Healthy" },
        "created_at": "2026-06-16T10:00:00Z"
      }
    ],
    "pagination": {
      "page": 1,
      "page_size": 20,
      "total": 45,
      "total_pages": 3
    }
  }
}
```

### 4.6 健康检查

**K8s 原生健康检查：**

K8s 通过 Liveness/Readiness Probe 自动执行健康检查，无需额外实现。

**接口实现：**

查询 K8s Pod 状态和 Probe 结果：

```json
GET /api/v1/apps/app-123/health
```

**响应：**

```json
{
  "success": true,
  "data": {
    "status": "Healthy",
    "instance": {
      "name": "app-123-7d8b9c6f5-x2z4k",
      "phase": "Running",
      "ready": true,
      "restart_count": 0,
      "node": "worker-1",
      "ip": "10.244.1.15",
      "started_at": "2026-06-16T10:00:00Z"
    },
    "probes": {
      "liveness": {
        "status": "Passed",
        "last_checked": "2026-06-16T10:30:00Z"
      },
      "readiness": {
        "status": "Passed",
        "last_checked": "2026-06-16T10:30:00Z"
      }
    },
    "events": [
      {
        "type": "Normal",
        "reason": "Started",
        "message": "Container started",
        "timestamp": "2026-06-16T10:00:00Z"
      }
    ]
  }
}
```

**响应字段说明：**

| 字段 | 类型 | 说明 |
|------|------|------|
| status | String | 整体状态：Healthy/Unhealthy/Starting |
| instance.name | String | 实例名称（Pod 名称） |
| instance.phase | String | Pod 阶段：Running/Pending/Succeeded/Failed |
| instance.ready | bool | 是否就绪 |
| instance.restart_count | u32 | 重启次数 |
| instance.node | String | 所在节点 |
| instance.ip | String | Pod IP |
| instance.started_at | String | 启动时间 |
| probes.liveness.status | String | Liveness 状态：Passed/Failed |
| probes.readiness.status | String | Readiness 状态：Passed/Failed |
| events | Vec | 最近事件 |

### 4.7 日志查看

**请求：**

```json
GET /api/v1/apps/app-123/logs?tail=1000&follow=true
```

**请求参数：**

| 参数 | 类型 | 必填 | 说明 |
|------|------|------|------|
| tail | u32 | 否 | 返回最后 N 行，默认 1000 |
| follow | bool | 否 | 是否持续输出，默认 false |
| timestamps | bool | 否 | 是否显示时间戳，默认 true |
| since | String | 否 | 起始时间（RFC3339） |

**响应：**

```json
{
  "success": true,
  "data": {
    "logs": [
      {
        "timestamp": "2026-06-16T10:30:00.123Z",
        "stream": "stdout",
        "message": "Starting application..."
      },
      {
        "timestamp": "2026-06-16T10:30:01.456Z",
        "stream": "stdout",
        "message": "Application started on port 8080"
      }
    ],
    "has_more": true,
    "total": 5000
  }
}
```

**实时日志流（WebSocket）：**

```
WS /api/v1/apps/app-123/logs/stream?tail=1000
```

### 4.8 获取资源使用

**前提条件：**

| 模式 | 前提条件 | 说明 |
|------|---------|------|
| K8s | 安装 Metrics Server | 收集 Pod 资源指标 |
| Docker | 无 | Docker 原生支持 |

#### Metrics Server 原理

```
┌─────────────────────────────────────────────────────────────────┐
│                          K8s 集群                                │
├─────────────────────────────────────────────────────────────────┤
│                                                                 │
│  ┌─────────────────────────────────────────────────────────┐   │
│  │  kubelet (每个节点)                                      │   │
│  │  ┌─────────────────────────────────────────────────┐   │   │
│  │  │  cAdvisor (容器资源监控)                          │   │   │
│  │  │  - 收集 CPU、内存、网络使用数据                   │   │   │
│  │  │  - 内置在 kubelet 中                             │   │   │
│  │  └─────────────────────────────────────────────────┘   │   │
│  └─────────────────────────────────────────────────────────┘   │
│                            │                                    │
│                            ▼ 定期采集                           │
│  ┌─────────────────────────────────────────────────────────┐   │
│  │  Metrics Server                                         │   │
│  │  - 从所有 kubelet 收集指标                              │   │
│  │  - 聚合数据                                             │   │
│  │  - 暴露 metrics.k8s.io API                             │   │
│  └─────────────────────────────────────────────────────────┘   │
│                            │                                    │
│                            ▼ API 调用                           │
│  ┌─────────────────────────────────────────────────────────┐   │
│  │  kubectl top pod / RCoder API                           │   │
│  │  - 查询 Pod 资源使用                                    │   │
│  └─────────────────────────────────────────────────────────┘   │
│                                                                 │
└─────────────────────────────────────────────────────────────────┘
```

**数据流：**

```
kubelet (cAdvisor) → Metrics Server → metrics.k8s.io API → RCoder
```

#### 安装 Metrics Server

**部署方式：** 使用专门脚本部署（与 Rook/Ceph 同级）

```
k8s/
├── scripts/
│   ├── deploy-rook.sh          # Ceph 存储
│   ├── deploy-metrics.sh       # Metrics Server（新增）
│   └── ...
```

**部署脚本：**

```bash
#!/bin/bash
# scripts/deploy-metrics.sh

set -e

METRICS_VERSION="v0.7.1"

echo "Installing Metrics Server ${METRICS_VERSION}..."

# 下载 YAML
curl -fsSL "https://github.com/kubernetes-sigs/metrics-server/releases/download/${METRICS_VERSION}/components.yaml" -o /tmp/metrics-server.yaml

# 应用
kubectl apply -f /tmp/metrics-server.yaml

# 等待就绪
echo "Waiting for Metrics Server to be ready..."
kubectl rollout status deployment/metrics-server -n kube-system --timeout=120s

# 验证
echo "Verifying installation..."
kubectl top nodes

echo "Metrics Server installed successfully!"
```

**执行部署：**

```bash
chmod +x k8s/scripts/deploy-metrics.sh
./k8s/scripts/deploy-metrics.sh
```

**验证安装：**

```bash
# 检查 Metrics Server 是否运行
kubectl get pods -n kube-system | grep metrics-server

# 测试资源查询
kubectl top pods
kubectl top nodes
```

**离线环境：** 需要提前下载镜像并导入私有仓库。

```bash
# 下载镜像
docker pull registry.k8s.io/metrics-server/metrics-server:v0.7.1

# 导入私有仓库
docker tag registry.k8s.io/metrics-server/metrics-server:v0.7.1 your-registry/metrics-server:v0.7.1
docker push your-registry/metrics-server:v0.7.1
```

**请求：**

```json
GET /api/v1/apps/app-123/stats
```

**响应：**

```json
{
  "success": true,
  "data": {
    "cpu": {
      "usage_percent": 25.5,
      "usage_cores": 0.255,
      "limit_cores": 1.0
    },
    "memory": {
      "usage_bytes": 268435456,
      "usage_percent": 50.0,
      "limit_bytes": 536870912
    },
    "network": {
      "rx_bytes": 1048576,
      "tx_bytes": 524288
    },
    "restart_count": 0
  }
}
```

**响应字段说明：**

| 字段 | 类型 | 说明 |
|------|------|------|
| cpu.usage_percent | f64 | CPU 使用率 (0-100) |
| cpu.usage_cores | f64 | CPU 使用核数 |
| cpu.limit_cores | f64 | CPU 限制核数 |
| memory.usage_bytes | u64 | 内存使用（字节） |
| memory.usage_percent | f64 | 内存使用率 (0-100) |
| memory.limit_bytes | u64 | 内存限制（字节） |
| network.rx_bytes | u64 | 网络接收字节数 |
| network.tx_bytes | u64 | 网络发送字节数 |
| restart_count | u64 | 重启次数 |

**实现方式：**

- **K8s**：调用 Metrics Server API（`metrics.k8s.io`）
- **Docker**：调用 Docker API（`/containers/{id}/stats`）

---

## 5. 流量路由

### 5.1 HTTP 流量

```
用户请求
    │
    ▼
┌─────────────────────────────────────────────────────────┐
│  Gateway: nuwax-gateway                                  │
│  匹配路径: /apps/app-123                                │
└─────────────────────────────────────────────────────────┘
    │
    │ HTTPRoute
    ▼
┌─────────────────────────────────────────────────────────┐
│  Service: app-123-svc                                    │
│  ClusterIP: 10.96.0.100                                 │
└─────────────────────────────────────────────────────────┘
    │
    ▼
┌─────────────────────────────────────────────────────────┐
│  Pod: app-123-xxx                                        │
│  IP: 10.244.1.15                                        │
└─────────────────────────────────────────────────────────┘
```

### 5.2 TCP 流量

```
用户请求 (数据库连接)
    │
    ▼
┌─────────────────────────────────────────────────────────┐
│  NodePort: 30432                                         │
└─────────────────────────────────────────────────────────┘
    │
    ▼
┌─────────────────────────────────────────────────────────┐
│  Service: app-123-nodeport                               │
└─────────────────────────────────────────────────────────┘
    │
    ▼
┌─────────────────────────────────────────────────────────┐
│  Pod: app-123-xxx:5432                                   │
└─────────────────────────────────────────────────────────┘
```

### 5.3 集群内部访问

```
其他服务调用
    │
    ▼
┌─────────────────────────────────────────────────────────┐
│  DNS: app-123-svc.rcoder-apps.svc.cluster.local         │
└─────────────────────────────────────────────────────────┘
    │
    ▼
┌─────────────────────────────────────────────────────────┐
│  Service: app-123-svc:8080                               │
└─────────────────────────────────────────────────────────┘
    │
    ▼
┌─────────────────────────────────────────────────────────┐
│  Pod: app-123-xxx:8080                                   │
└─────────────────────────────────────────────────────────┘
```

---

## 6. 数据模型

### 6.1 应用配置

```rust
/// 创建应用请求
pub struct CreateAppRequest {
    pub name: String,                          // 应用名称
    pub image: String,                         // 容器镜像
    pub command: Option<Vec<String>>,          // 启动命令
    pub env: Option<HashMap<String, String>>,  // 环境变量
    pub secret_env: Option<HashMap<String, String>>, // 敏感环境变量
    pub resources: Option<ResourceLimits>,     // 资源限制
    pub ports: Option<Vec<PortConfig>>,        // 端口配置
    pub health_check: Option<HealthCheckConfig>, // 健康检查
}

/// 资源限制
pub struct ResourceLimits {
    pub cpu: Option<String>,        // CPU: "1", "500m", "0.5"
    pub memory: Option<String>,     // 内存: "512Mi", "1Gi"
    pub storage: Option<String>,    // 存储: "10Gi", "100Mi"
}

/// 端口配置
pub struct PortConfig {
    pub name: String,               // 端口名称: "http", "postgres"
    pub port: u16,                  // 容器端口
    pub expose: ExposeType,         // 暴露类型: http / tcp
}

/// 暴露类型
pub enum ExposeType {
    Http,  // 通过 Gateway HTTPRoute
    Tcp,   // 通过 NodePort
}
```

### 6.2 应用信息

```rust
/// 应用信息
pub struct AppInfo {
    pub app_id: String,
    pub name: String,
    pub status: AppStatus,
    pub image: String,
    pub command: Vec<String>,
    pub access: AccessInfo,
    pub health: HealthInfo,
    pub resources: ResourceLimits,
    pub env: HashMap<String, String>,
    pub created_at: String,
    pub updated_at: String,
}

/// 访问信息
pub struct AccessInfo {
    pub external: ExternalAccess,
    pub internal: InternalAccess,
}

/// 外部访问
pub struct ExternalAccess {
    pub http: Option<String>,              // HTTP 地址
    pub tcp: Vec<TcpPortMapping>,          // TCP 端口列表
}

/// TCP 端口映射
pub struct TcpPortMapping {
    pub name: String,                      // 端口名称
    pub node_port: u16,                    // NodePort
    pub access_url: String,                // 访问地址
}

/// 内部访问
pub struct InternalAccess {
    pub domain: String,                    // 完整域名
    pub short_domain: String,              // 简写域名
    pub ports: Vec<InternalPort>,          // 端口列表
}

/// 内部端口
pub struct InternalPort {
    pub name: String,                      // 端口名称
    pub port: u16,                         // 端口号
}
```

---

## 7. 实现说明

### 7.1 K8s 模式实现状态

| 功能 | 实现方式 | 状态 | 说明 |
|------|---------|------|------|
| 创建应用 | Deployment + Service + HTTPRoute | ✅ | 完整实现 |
| 删除应用 | 删除所有 K8s 资源 | ✅ | 完整实现 |
| 启动应用 | scale replicas=1 | ✅ | 完整实现 |
| 停止应用 | scale replicas=0 | ✅ | 完整实现 |
| 重启应用 | 滚动重启（更新 annotation） | ✅ | 完整实现 |
| 查询列表 | 内存查询 | ✅ | 完整实现 |
| 获取详情 | 查询 Pod 状态 | ✅ | 完整实现 |
| 日志查询 | kube-rs logs API | ✅ | 完整实现 |
| 事件查询 | K8s Events API | ✅ | 完整实现 |
| 文件管理 | PVC 挂载目录读写 | ✅ | 完整实现 |
| 资源使用 | Pod spec + Metrics Server | ⚠️ | 需要 Metrics Server |

### 7.2 K8s 资源创建流程

```
用户请求创建应用
        │
        ▼
┌─────────────────────────────────────────┐
│ 1. 创建 ConfigMap + Secret              │
│    (环境变量和敏感信息)                  │
└─────────────────────────────────────────┘
        │
        ▼
┌─────────────────────────────────────────┐
│ 2. 创建 PVC                             │
│    (持久化存储)                          │
└─────────────────────────────────────────┘
        │
        ▼
┌─────────────────────────────────────────┐
│ 3. 创建 Deployment                      │
│    (Pod 模板，包含健康检查配置)          │
└─────────────────────────────────────────┘
        │
        ▼
┌─────────────────────────────────────────┐
│ 4. 创建 Service                         │
│    (ClusterIP，服务发现)                 │
└─────────────────────────────────────────┘
        │
        ▼
┌─────────────────────────────────────────┐
│ 5. 创建 HTTPRoute (如果有 HTTP 端口)    │
│    (绑定到 Gateway)                      │
└─────────────────────────────────────────┘
        │
        ▼
┌─────────────────────────────────────────┐
│ 6. 创建 NodePort Service (如果有 TCP 端口)│
│    (K8s 自动分配 NodePort)               │
└─────────────────────────────────────────┘
        │
        ▼
┌─────────────────────────────────────────┐
│ 7. 查询实际分配的 NodePort              │
│    记录映射关系                          │
└─────────────────────────────────────────┘
        │
        ▼
┌─────────────────────────────────────────┐
│ 8. 返回访问信息                         │
│    - 外部地址 (Gateway/NodePort)        │
│    - 内部域名 (Service DNS)             │
└─────────────────────────────────────────┘
```

### 7.2 健康检查实现

K8s 原生支持，无需额外实现：

```yaml
# Deployment 中配置
livenessProbe:
  httpGet:
    path: /health
    port: 8080
  initialDelaySeconds: 30
  periodSeconds: 10

readinessProbe:
  httpGet:
    path: /health
    port: 8080
  initialDelaySeconds: 5
  periodSeconds: 5
```

查询健康状态：
```rust
// 查询 Pod 状态
let pod = pods.get(&pod_name).await?;
let ready = pod.status.conditions
    .find(|c| c.type == "Ready")
    .map(|c| c.status == "True");
```

### 7.3 日志查看实现

K8s 原生支持：

```rust
// 查询 Pod 日志
let logs = pods.log_stream(&pod_name, &LogParams {
    tail_lines: Some(1000),
    follow: true,
    ..Default::default()
}).await?;
```

---

## 8. 应用代码与目录管理

### 8.1 目录约定规则

#### 宿主机目录结构

```
docker/                              # docker-compose.yml 所在目录
├── docker-compose.yml
├── app-workspace/                   # 应用工作空间
│   ├── {app_id}/
│   │   ├── code/                  # 应用代码（只读）
│   │   │   ├── app.jar
│   │   │   ├── app
│   │   │   └── dist/index.js
│   │   ├── data/                  # 应用数据（读写）
│   │   │   └── db/
│   │   └── logs/                  # 应用日志（读写）
│   │       └── app.log
```

#### 容器内路径

```
/app/
├── code/              ← 挂载自宿主机 app-workspace/{id}/code
│   └── app.jar
├── data/              ← 挂载自宿主机 app-workspace/{id}/data
│   └── db/
└── logs/              ← 挂载自宿主机 app-workspace/{id}/logs
    └── app.log
```

#### 目录用途

| 目录 | 容器内路径 | 用途 | 权限 | 持久化 |
|------|-----------|------|------|--------|
| code/ | /app/code/ | 应用代码 | 只读 | 是 |
| data/ | /app/data/ | 应用数据 | 读写 | 是 |
| logs/ | /app/logs/ | 应用日志 | 读写 | 是 |

### 8.2 Docker 模式挂载

**docker-compose.yml：**

```yaml
services:
  app-123:
    image: eclipse-temurin:17
    volumes:
      # 整个应用目录挂载到 /app
      - ./app-workspace/app-123:/app
    command: ["java", "-jar", "/app/code/app.jar"]
```

**目录映射：**

```
宿主机                              容器
./app-workspace/app-123/      →    /app/
├── code/                     →    /app/code/
│   └── app.jar                     └── app.jar
├── data/                     →    /app/data/
│   └── db/                         └── db/
└── logs/                     →    /app/logs/
    └── app.log                     └── app.log
```

### 8.3 K8s 模式挂载

**PVC 命名规则：** `{app_id}-{type}`

```yaml
# PVC 定义
apiVersion: v1
kind: PersistentVolumeClaim
metadata:
  name: app-123-code
spec:
  accessModes: [ReadOnlyMany]
  storageClassName: cephfs
  resources:
    requests:
      storage: 1Gi
---
apiVersion: v1
kind: PersistentVolumeClaim
metadata:
  name: app-123-data
spec:
  accessModes: [ReadWriteOnce]
  storageClassName: ceph-rbd
  resources:
    requests:
      storage: 10Gi
```

**Deployment 挂载：**

```yaml
volumeMounts:
- name: app-code
  mountPath: /app/code
  readOnly: true
- name: app-data
  mountPath: /app/data
- name: app-logs
  mountPath: /app/logs
volumes:
- name: app-code
  persistentVolumeClaim:
    claimName: app-123-code
- name: app-data
  persistentVolumeClaim:
    claimName: app-123-data
- name: app-logs
  persistentVolumeClaim:
    claimName: app-123-logs
```

### 8.4 上传接口

**请求：**

```bash
POST /api/v1/apps/app-123/upload
Content-Type: multipart/form-data

file: app.jar
target: code/  # 目标子目录（可选，默认 code/）
```

**响应：**

```json
{
  "success": true,
  "data": {
    "file_path": "code/app.jar",
    "file_size": 10485760,
    "uploaded_at": "2026-06-16T10:00:00Z"
  }
}
```

**实际存储路径：**
- Docker：`./app-workspace/app-123/code/app.jar`
- K8s PVC：`app-123-code:/app/code/app.jar`

### 8.5 完整部署流程

```
┌─────────────────────────────────────────────────────────────────┐
│                    应用部署流程                                  │
├─────────────────────────────────────────────────────────────────┤
│                                                                 │
│  1. 用户编译应用                                                 │
│     $ mvn package -f pom.xml                                    │
│     → target/app.jar                                            │
│                                                                 │
│  2. 上传代码                                                     │
│     $ curl -X POST /api/v1/apps/app-123/upload                  │
│         -F "file=@target/app.jar"                               │
│     → 存储到 app-workspace/app-123/code/app.jar                 │
│                                                                 │
│  3. 创建应用                                                     │
│     POST /api/v1/apps                                           │
│     {                                                           │
│       "name": "my-java-app",                                    │
│       "image": "eclipse-temurin:17",                            │
│       "command": ["java", "-jar", "/app/code/app.jar"]          │
│     }                                                           │
│     → 创建容器（挂载目录）                                        │
│                                                                 │
│  4. 容器启动                                                     │
│     → /app/code/app.jar 可访问                                  │
│     → 执行 command: java -jar /app/code/app.jar                 │
│                                                                 │
└─────────────────────────────────────────────────────────────────┘
```

### 8.6 文件管理接口

| 方法 | 路径 | 描述 |
|------|------|------|
| POST | `/api/v1/apps/{id}/upload` | 上传文件 |
| GET | `/api/v1/apps/{id}/files` | 列出文件 |
| DELETE | `/api/v1/apps/{id}/files/{path}` | 删除文件 |

---

## 9. 实现细节

### 9.1 RBAC 权限配置

RCoder 需要扩展 ClusterRole 以管理应用资源：

```yaml
apiVersion: rbac.authorization.k8s.io/v1
kind: ClusterRole
metadata:
  name: rcoder-app-manager
rules:
  # Pod 管理
  - apiGroups: [""]
    resources: ["pods"]
    verbs: ["create", "delete", "get", "list", "watch", "patch", "update"]
  - apiGroups: [""]
    resources: ["pods/log"]
    verbs: ["get", "list"]
  - apiGroups: [""]
    resources: ["pods/status"]
    verbs: ["get"]

  # Deployment 管理
  - apiGroups: ["apps"]
    resources: ["deployments"]
    verbs: ["create", "delete", "get", "list", "watch", "patch", "update"]

  # Service 管理
  - apiGroups: [""]
    resources: ["services"]
    verbs: ["create", "delete", "get", "list", "watch"]

  # ConfigMap/Secret 管理
  - apiGroups: [""]
    resources: ["configmaps", "secrets"]
    verbs: ["create", "delete", "get", "list", "watch"]

  # PVC 管理
  - apiGroups: [""]
    resources: ["persistentvolumeclaims"]
    verbs: ["get", "list", "watch", "create", "delete"]

  # HTTPRoute 管理（Gateway API）
  - apiGroups: ["gateway.networking.k8s.io"]
    resources: ["httproutes"]
    verbs: ["create", "delete", "get", "list", "watch"]

  # Events 查询
  - apiGroups: [""]
    resources: ["events"]
    verbs: ["get", "list"]
```

### 9.2 创建应用流程

```rust
// 创建应用容器
async fn create_app(request: CreateAppRequest) -> Result<AppInfo> {
    let client = kube::Client::try_default().await?;
    let namespace = "rcoder-apps";
    let app_id = generate_app_id();

    // 1. 创建 ConfigMap
    if let Some(env) = &request.env {
        let configmap = build_configmap(&app_id, env);
        create_configmap(&client, namespace, &configmap).await?;
    }

    // 2. 创建 Secret
    if let Some(secret_env) = &request.secret_env {
        let secret = build_secret(&app_id, secret_env);
        create_secret(&client, namespace, &secret).await?;
    }

    // 3. 创建 PVC
    if let Some(resources) = &request.resources {
        if let Some(storage) = &resources.storage {
            let pvc = build_pvc(&app_id, storage);
            create_pvc(&client, namespace, &pvc).await?;
        }
    }

    // 4. 创建 Deployment
    let deployment = build_deployment(&app_id, &request);
    create_deployment(&client, namespace, &deployment).await?;

    // 5. 创建 Service
    let service = build_service(&app_id, &request.ports);
    create_service(&client, namespace, &service).await?;

    // 6. 创建 HTTPRoute（HTTP 端口）
    if has_http_port(&request.ports) {
        let httproute = build_httproute(&app_id);
        create_httproute(&client, namespace, &httproute).await?;
    }

    // 7. 创建 NodePort Service（TCP 端口）
    let node_ports = if has_tcp_port(&request.ports) {
        let nodeport_svc = build_nodeport_service(&app_id, &request.ports);
        create_nodeport_service(&client, namespace, &nodeport_svc).await?
    } else {
        vec![]
    };

    // 8. 等待 Pod 就绪
    wait_for_pod_ready(&client, namespace, &app_id).await?;

    // 9. 构建访问信息
    let access = build_access_info(&app_id, &node_ports);

    Ok(AppInfo {
        app_id,
        name: request.name,
        status: AppStatus::Running,
        access,
        ..
    })
}
```

### 9.3 HTTPRoute 创建

```rust
async fn create_httproute(
    client: &kube::Client,
    namespace: &str,
    app_id: &str,
) -> Result<()> {
    let httproutes: kube::Api<HTTPRoute> = kube::Api::namespaced(client.clone(), namespace);

    let httproute = serde_json::json!({
        "apiVersion": "gateway.networking.k8s.io/v1",
        "kind": "HTTPRoute",
        "metadata": {
            "name": format!("{}-route", app_id),
            "namespace": namespace
        },
        "spec": {
            "parentRefs": [{
                "name": "nuwax-gateway",
                "namespace": "default"
            }],
            "rules": [{
                "matches": [{
                    "path": {
                        "type": "PathPrefix",
                        "value": format!("/apps/{}", app_id)
                    }
                }],
                "backendRefs": [{
                    "name": format!("{}-svc", app_id),
                    "port": 8080
                }]
            }]
        }
    });

    httproutes.create(&PostParams::default(), &serde_json::from_value(httproute)?).await?;
    Ok(())
}
```

### 9.4 NodePort Service 创建

```rust
async fn create_nodeport_service(
    client: &kube::Client,
    namespace: &str,
    app_id: &str,
    ports: &[PortConfig],
) -> Result<Vec<TcpPortMapping>> {
    let services: kube::Api<Service> = kube::Api::namespaced(client.clone(), namespace);

    // 只为 TCP 端口创建 NodePort
    let tcp_ports: Vec<_> = ports.iter()
        .filter(|p| p.expose == ExposeType::Tcp)
        .collect();

    if tcp_ports.is_empty() {
        return Ok(vec![]);
    }

    let service = serde_json::json!({
        "apiVersion": "v1",
        "kind": "Service",
        "metadata": {
            "name": format!("{}-nodeport", app_id),
            "namespace": namespace
        },
        "spec": {
            "type": "NodePort",
            "selector": { "app": app_id },
            "ports": tcp_ports.iter().map(|p| {
                serde_json::json!({
                    "name": p.name,
                    "port": p.port,
                    "targetPort": p.port
                })
            }).collect::<Vec<_>>()
        }
    });

    let created = services.create(&PostParams::default(), &serde_json::from_value(service)?).await?;

    // 获取分配的 NodePort
    let mut node_ports = vec![];
    if let Some(spec) = created.spec {
        if let Some(ports) = spec.ports {
            for port in ports {
                if let Some(node_port) = port.node_port {
                    node_ports.push(TcpPortMapping {
                        name: port.name.unwrap_or_default(),
                        node_port: node_port as u16,
                        access_url: format!("tcp://{}:{}", NODE_IP, node_port),
                    });
                }
            }
        }
    }

    Ok(node_ports)
}
```

### 9.5 健康检查查询

```rust
async fn get_app_health(
    client: &kube::Client,
    app_id: &str,
) -> Result<HealthInfo> {
    let pods: kube::Api<Pod> = kube::Api::namespaced(client.clone(), "rcoder-apps");

    // 查询 Pod
    let pod_list = pods.list(&ListParams::default().labels(&format!("app={}", app_id))).await?;

    if let Some(pod) = pod_list.items.first() {
        let status = pod.status.as_ref();

        // 获取 Ready 状态
        let ready = status
            .and_then(|s| s.conditions.as_ref())
            .and_then(|conditions| {
                conditions.iter()
                    .find(|c| c.type_ == "Ready")
                    .map(|c| c.status == "True")
            })
            .unwrap_or(false);

        // 获取容器状态
        let container_status = status
            .and_then(|s| s.container_statuses.as_ref())
            .and_then(|statuses| statuses.first());

        Ok(HealthInfo {
            status: if ready { "Healthy" } else { "Unhealthy" }.to_string(),
            pod: PodInfo {
                name: pod.metadata.name.unwrap_or_default(),
                phase: status.map(|s| s.phase.clone().unwrap_or_default()).unwrap_or_default(),
                ready,
                restart_count: container_status.map(|cs| cs.restart_count).unwrap_or(0),
                node: pod.spec.as_ref().and_then(|s| s.node_name.clone()).unwrap_or_default(),
                ip: status.and_then(|s| s.pod_ip.clone()).unwrap_or_default(),
            },
            ..
        })
    } else {
        Err(anyhow::anyhow!("未找到 Pod"))
    }
}
```

### 9.6 日志查询

```rust
async fn get_app_logs(
    client: &kube::Client,
    app_id: &str,
    params: &LogParams,
) -> Result<Vec<LogEntry>> {
    let pods: kube::Api<Pod> = kube::Api::namespaced(client.clone(), "rcoder-apps");

    // 查询 Pod
    let pod_list = pods.list(&ListParams::default().labels(&format!("app={}", app_id))).await?;
    let pod_name = pod_list.items.first()
        .and_then(|p| p.metadata.name.clone())
        .ok_or_else(|| anyhow::anyhow!("未找到 Pod"))?;

    // 查询日志
    let mut log_params = LogParams::default();
    log_params.tail_lines = Some(params.tail as i64);
    log_params.follow = Some(params.follow);
    log_params.timestamps = Some(true);

    let logs = pods.logs(&pod_name, &log_params).await?;

    // 解析日志
    let entries = logs.lines()
        .map(|line| parse_log_line(line))
        .collect();

    Ok(entries)
}
```

---

## 10. 依赖库

### 10.1 核心依赖

| 库 | 用途 | 说明 |
|---|------|------|
| **kube-rs** | K8s API 客户端 | 管理 Deployment、Service、Pod 等 |
| **bollard** | Docker API 客户端 | 管理 Docker 容器 |
| **axum** | Web 框架 | HTTP API 服务 |
| **utoipa** | OpenAPI 文档 | 自动生成 API 文档 |

### 10.2 依赖配置

```toml
[dependencies]
# Web 框架
axum = "0.7"
tokio = { version = "1", features = ["full"] }
tower = "0.4"
tower-http = { version = "0.5", features = ["cors", "trace"] }

# K8s 客户端
kube = { version = "0.90", features = ["runtime", "derive"] }
k8s-openapi = { version = "0.21", features = ["v1_29"] }

# Docker 客户端
bollard = "0.16"

# OpenAPI
utoipa = { version = "4", features = ["axum_extras"] }
utoipa-swagger-ui = { version = "6", features = ["axum"] }

# 序列化
serde = { version = "1", features = ["derive"] }
serde_json = "1"

# 错误处理
anyhow = "1"
thiserror = "1"

# 日志
tracing = "0.1"
tracing-subscriber = "0.3"

# 时间
chrono = { version = "0.4", features = ["serde"] }

# ID
uuid = { version = "1", features = ["v4"] }
```

### 10.3 使用示例

**kube-rs（K8s 客户端）：**

```rust
use kube::{Api, Client};
use k8s_openapi::api::apps::v1::Deployment;
use k8s_openapi::api::core::v1::Service;

// 创建客户端
let client = Client::try_default().await?;

// 管理 Deployment
let deployments: Api<Deployment> = Api::namespaced(client.clone(), "rcoder-apps");
let deploy = deployments.get("app-123").await?;

// 管理 Service
let services: Api<Service> = Api::namespaced(client.clone(), "rcoder-apps");
let svc = services.get("app-123-svc").await?;

// 查询 Pod 日志
let pods: Api<Pod> = Api::namespaced(client.clone(), "rcoder-apps");
let logs = pods.logs("app-123-xxx", &LogParams::default()).await?;
```

**bollard（Docker 客户端）：**

```rust
use bollard::Docker;
use bollard::container::{CreateContainerOptions, Config};

// 连接 Docker
let docker = Docker::connect_with_local_defaults()?;

// 创建容器
let config = Config {
    image: Some("eclipse-temurin:17"),
    cmd: Some(vec!["java", "-jar", "/app/code/app.jar"]),
    ..Default::default()
};
let container = docker.create_container(None, config).await?;

// 启动容器
docker.start_container(&container.id, None).await?;

// 查询日志
let logs = docker.logs(&container.id, Some(LogsOptions {
    tail: "1000",
    follow: true,
    ..Default::default()
})).await?;
```

**utoipa（OpenAPI 文档）：**

```rust
use utoipa::ToSchema;

#[derive(ToSchema, Serialize, Deserialize)]
struct AppInfo {
    app_id: String,
    name: String,
    status: String,
}

#[utoipa::path(
    get,
    path = "/api/v1/apps/{app_id}",
    params(
        ("app_id" = String, Path, description = "应用 ID")
    ),
    responses(
        (status = 200, description = "查询成功", body = AppInfo),
        (status = 404, description = "应用不存在")
    ),
    tag = "应用管理"
)]
async fn get_app() -> Json<AppInfo> { .. }
```

---

## 11. 配置项

```yaml
# config.yml
app_manager:
  runtime: k8s  # docker / k8s

  k8s:
    namespace: rcoder-apps
    gateway:
      name: nuwax-gateway
      namespace: default
    storage:
      block_storage_class: ceph-rbd
      shared_storage_class: cephfs
    node_ip: 192.168.11.216
    node_port: 30080
```

---

## 12. 错误处理

### 12.1 统一错误响应

```json
{
  "success": false,
  "code": "APP_NOT_FOUND",
  "message": "应用不存在: app-123",
  "details": {
    "app_id": "app-123"
  },
  "trace_id": "abc-123-xyz"
}
```

### 12.2 错误码

| 错误码 | HTTP 状态码 | 说明 |
|--------|------------|------|
| APP_NOT_FOUND | 404 | 应用不存在 |
| APP_ALREADY_EXISTS | 409 | 应用已存在 |
| INVALID_STATE | 409 | 状态不允许操作 |
| INVALID_REQUEST | 400 | 请求参数错误 |
| INTERNAL_ERROR | 500 | 内部错误 |
| K8S_API_ERROR | 500 | K8s API 调用失败 |
| DOCKER_API_ERROR | 500 | Docker API 调用失败 |
| RESOURCE_EXHAUSTED | 503 | 资源不足 |

---

## 13. 实施路线图

### Phase 1: 基础框架（1 周）

- [ ] 创建 `app_manager` 模块结构
- [ ] 实现数据模型（AppConfig、AppInfo 等）
- [ ] 集成 kube-rs 和 bollard
- [ ] 实现基础 CRUD 接口

### Phase 2: K8s 资源管理（1 周）

- [ ] 实现 Deployment 创建/管理
- [ ] 实现 Service 创建
- [ ] 实现 ConfigMap/Secret 管理
- [ ] 实现 PVC 管理

### Phase 3: 流量路由（1 周）

- [ ] 实现 HTTPRoute 创建
- [ ] 实现 NodePort Service 创建
- [ ] 实现端口映射记录
- [ ] 返回内外部访问地址

### Phase 4: 监控与日志（1 周）

- [ ] 实现健康检查查询
- [ ] 实现日志查询
- [ ] 实现资源使用查询（需安装 Metrics Server）
- [ ] 实现事件查询

### Phase 5: Docker 模式（1 周）

- [ ] 实现 Docker 容器管理
- [ ] 实现 Pingora 代理配置
- [ ] 实现端口池管理

### Phase 6: 测试与优化（1 周）

- [ ] 集成测试
- [ ] 文档完善
- [ ] 性能优化
