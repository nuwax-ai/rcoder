# 隔离类型 (Isolation Type) 需求规范

## 1. 概述

### 1.1 背景与目标

RCoder 项目目前通过 `ServiceType` (RCoder / ComputerAgentRunner) 来区分不同业务类型的容器，实现基础的容器隔离。但在多租户场景下，需要更细粒度的数据隔离能力，以提升资源使用率。

本需求旨在增加**数据隔离维度**，支持：
- **租户维度隔离**：同一租户下的所有用户共享容器
- **空间维度隔离**：同一租户同一空间下的用户共享容器
- **项目维度隔离**：每个项目独立容器（当前逻辑）

### 1.2 核心变更

新增 `pod_id`、`tenant_id`、`space_id`、`isolation_type` 四个字段，通过 `pod_id` 唯一映射容器，通过 `isolation_type` 控制数据目录结构。

---

## 2. 新增字段定义

### 2.1 请求字段

| 字段名 | 类型 | 必填 | 说明 |
|--------|------|------|------|
| `pod_id` | String | 可选 | 容器唯一标识。若传值，则 `isolation_type`、`tenant_id`、`space_id` 必须有值 |
| `tenant_id` | String | 可选 | 租户 ID |
| `space_id` | String | 可选 | 空间 ID |
| `isolation_type` | String | 可选 | 隔离类型枚举值：`tenant` / `space` / `project` |

### 2.2 字段约束

```
IF pod_id IS NOT NULL THEN
    isolation_type IS NOT NULL
    tenant_id IS NOT NULL
    space_id IS NOT NULL
END IF
```

### 2.3 枚举值

| isolation_type 值 | 含义 | 容器共享粒度 | 数据目录结构 |
|-------------------|------|-------------|-------------|
| `tenant` | 租户隔离 | 同一租户共用一个容器 | `/app/project_workspace/{tenant_id}/{space_id}/{project_id}` 或 `/app/computer-project-workspace/{tenant_id}/{space_id}/{project_id}` |
| `space` | 空间隔离 | 同一租户同一空间共用一个容器 | 同上 |
| `project` | 项目隔离 | 每个项目独立容器（当前逻辑） | `/app/project_workspace/{project_id}` 或 `/app/computer-project-workspace/{user_id}/{project_id}` |

---

## 3. 接口变更

### 3.1 `/chat` 接口

**变更前**：
- 工作目录：`/app/project_workspace/{project_id}`
- 容器标识：基于 `project_id`

**变更后**：
- 若 `pod_id` 为空：保持原有逻辑
- 若 `pod_id` 有值：
  - 工作目录：`/app/project_workspace/{tenant_id}/{space_id}/{project_id}`
  - 容器标识：基于 `pod_id`
  - 容器前缀：根据 `isolation_type` 动态生成

### 3.2 `/computer/chat` 接口

**变更前**：
- 工作目录：`/app/computer-project-workspace/{user_id}/{project_id}`
- 容器标识：基于 `user_id`

**变更后**：
- 若 `pod_id` 为空：保持原有逻辑
- 若 `pod_id` 有值：
  - 工作目录：`/app/computer-project-workspace/{tenant_id}/{space_id}/{project_id}`
  - 容器标识：基于 `pod_id`
  - 容器前缀：根据 `isolation_type` 动态生成

---

## 4. 数据目录结构

### 4.1 Docker Compose 环境

宿主机目录结构（通过 volume 挂载到容器内）：

```
# 原有挂载（docker-compose.yml）
./project_workspace:/app/project_workspace
./computer-project-workspace:/app/computer-project-workspace

# 变更后，目录结构保持不变，通过路径区分
/app/project_workspace/
├── {project_id}/                    # isolation_type=project（默认）
├── {tenant_id}/                     # isolation_type=tenant/space
│   └── {space_id}/
│       └── {project_id}/
```

### 4.2 Kubernetes 环境

K8s 环境使用 JuiceFS CSI 挂载，基础路径通过 PVC 定义。容器内路径结构与 Docker Compose 保持一致。

### 4.3 路径模板

| 接口 | isolation_type | 容器内路径模板 |
|------|---------------|----------------|
| `/chat` | `project` (默认) | `/app/project_workspace/{project_id}` |
| `/chat` | `tenant` / `space` | `/app/project_workspace/{tenant_id}/{space_id}/{project_id}` |
| `/computer/chat` | `project` (默认) | `/app/computer-project-workspace/{user_id}/{project_id}` |
| `/computer/chat` | `tenant` / `space` | `/app/computer-project-workspace/{tenant_id}/{space_id}/{project_id}` |

---

## 5. 容器标识与命名

### 5.1 容器名称生成规则

**默认逻辑（pod_id 为空）**：
- RCoder：`{prefix}-agent-{project_id}`
- ComputerAgentRunner：`{prefix}-{user_id}`

**新增逻辑（pod_id 有值）**：
- 容器名称：`{prefix}-{pod_id}`
- 前缀根据 `isolation_type` 确定：
  - `tenant`：`{service_type}-tenant`
  - `space`：`{service_type}-space`
  - `project`：`{service_type}-project`

### 5.2 容器标识查询

当 `pod_id` 有值时：
1. 使用 `pod_id` 作为容器标识进行查询
2. 容器创建时使用 `pod_id` 生成容器名称
3. 容器复用时通过 `pod_id` 匹配

---

## 6. 配置变更

### 6.1 ServiceImageConfig 扩展

`container_path_template` 支持新的变量占位符：

```rust
// 支持的变量
- {project_id}: 项目ID
- {user_id}: 用户ID (仅 computer/chat)
- {tenant_id}: 租户ID (新增)
- {space_id}: 空间ID (新增)
- {isolation_type}: 隔离类型 (新增)
```

### 6.2 默认路径模板

```yaml
# RCoder 服务
container_path_template: "/app/project_workspace/{project_id}"

# ComputerAgentRunner 服务
container_path_template: "/app/computer-project-workspace/{user_id}/{project_id}"

# 新增隔离类型路径（通过代码动态拼接，非配置）
# tenant/space: "/app/project_workspace/{tenant_id}/{space_id}/{project_id}"
# tenant/space: "/app/computer-project-workspace/{tenant_id}/{space_id}/{project_id}"
```

---

## 7. 业务流程变更

### 7.1 `/chat` 处理流程

```
POST /chat { prompt, project_id?, pod_id?, tenant_id?, space_id?, isolation_type?, ... }
    ↓
1. 参数校验：
   - IF pod_id IS NOT NULL THEN
       - isolation_type NOT NULL (tenant|space|project)
       - tenant_id NOT NULL
       - space_id NOT NULL
     END IF
    ↓
2. 确定容器标识：
   - pod_id 有值 → 使用 pod_id
   - pod_id 为空 → 使用 project_id (保持原逻辑)
    ↓
3. 确定数据目录：
   - isolation_type=project → /app/project_workspace/{project_id}
   - isolation_type=tenant/space → /app/project_workspace/{tenant_id}/{space_id}/{project_id}
    ↓
4. 获取/创建容器 (使用确定的标识)
    ↓
5. gRPC 调用 → agent_runner
    ↓
返回 ChatResponse
```

### 7.2 `/computer/chat` 处理流程

```
POST /computer/chat { user_id, project_id?, pod_id?, tenant_id?, space_id?, isolation_type?, ... }
    ↓
1. 参数校验（同上）
    ↓
2. 确定容器标识：
   - pod_id 有值 → 使用 pod_id
   - pod_id 为空 → 使用 user_id (保持原逻辑)
    ↓
3. 确定数据目录：
   - isolation_type=project → /app/computer-project-workspace/{user_id}/{project_id}
   - isolation_type=tenant/space → /app/computer-project-workspace/{tenant_id}/{space_id}/{project_id}
    ↓
4. 获取/创建容器 (使用确定的标识)
    ↓
5. 创建工作目录（使用确定的路径）
    ↓
6. gRPC 调用 → agent_runner
    ↓
返回 ChatResponse
```

---

## 8. 错误处理

### 8.1 参数校验错误

| 场景 | 错误码 | 错误信息 |
|------|--------|----------|
| pod_id 有值但 isolation_type 为空 | ERR_VALIDATION | `isolation_type is required when pod_id is provided` |
| pod_id 有值但 tenant_id 为空 | ERR_VALIDATION | `tenant_id is required when pod_id is provided` |
| pod_id 有值但 space_id 为空 | ERR_VALIDATION | `space_id is required when pod_id is provided` |
| isolation_type 值无效 | ERR_VALIDATION | `invalid isolation_type: {value}, expected tenant|space|project` |

### 8.2 业务错误

保持原有错误码定义不变。

---

## 9. 向后兼容性

### 9.1 现有客户端

- 所有现有字段保持可选
- `pod_id`、`tenant_id`、`space_id`、`isolation_type` 均为新增字段
- 现有客户端不传这些字段时，系统行为与变更前完全一致

### 9.2 现有数据

- 已有的项目数据不受影响
- 容器复用逻辑基于 `pod_id` 或原有标识（`project_id`/`user_id`）

---

## 10. 多集群支持

### 10.1 Docker Compose 集群

- 挂载点保持不变：`./project_workspace:/app/project_workspace`
- 目录结构在容器内通过路径自动区分

### 10.2 Kubernetes 集群

- 使用 JuiceFS CSI 挂载（参考 `values.yaml` 配置）
- PVC 大小根据租户/空间规模调整
- 路径模板保持一致

---

## 11. 影响范围

### 11.1 涉及模块

| 模块 | 变更内容 |
|------|----------|
| `shared_types` | 新增 `IsolationType` 枚举、请求/响应结构体扩展 |
| `rcoder` (handler) | `/chat`、`/computer/chat` 参数解析和校验 |
| `rcoder` (service) | 容器管理器路径拼接逻辑 |
| `docker_manager` | 容器创建、查询逻辑支持 `pod_id` |
| `rcoder` (config) | 配置模板扩展，支持新变量 |

### 11.2 涉及文件

```
crates/shared_types/src/
├── model.rs              # 请求/响应结构体
├── service_type.rs       # 可能需要扩展
└── service_config.rs    # 路径模板变量扩展

crates/rcoder/src/
├── handler/
│   ├── chat_handler.rs
│   └── computer_chat_handler.rs
├── service/
│   ├── container_manager.rs
│   └── computer_container_manager.rs
└── handler/utils/paths.rs

crates/docker_manager/src/
├── manager.rs            # 容器创建/查询逻辑
└── types.rs              # DockerContainerConfig 扩展
```

---

## 12. 测试用例

### 12.1 参数校验测试

| 场景 | 输入 | 预期结果 |
|------|------|----------|
| pod_id 有值，isolation_type 为空 | `pod_id="abc"` | 返回错误 |
| pod_id 有值，tenant_id 为空 | `pod_id="abc", isolation_type="tenant"` | 返回错误 |
| pod_id 有值，space_id 为空 | `pod_id="abc", isolation_type="space"` | 返回错误 |
| isolation_type 无效值 | `isolation_type="invalid"` | 返回错误 |
| pod_id 为空，其他字段为空 | 无 | 正常流程 |

### 12.2 路径拼接测试

| 接口 | isolation_type | 输入参数 | 预期路径 |
|------|---------------|----------|----------|
| /chat | project | project_id="p1" | `/app/project_workspace/p1` |
| /chat | tenant | tenant_id="t1", space_id="s1", project_id="p1" | `/app/project_workspace/t1/s1/p1` |
| /computer/chat | project | user_id="u1", project_id="p1" | `/app/computer-project-workspace/u1/p1` |
| /computer/chat | space | tenant_id="t1", space_id="s1", project_id="p1" | `/app/computer-project-workspace/t1/s1/p1` |

### 12.3 容器复用测试

| 场景 | 输入 | 预期结果 |
|------|------|----------|
| 相同 pod_id 二次请求 | pod_id="abc" 两次请求 | 复用同一容器 |
| 不同 pod_id 请求 | pod_id="abc", pod_id="def" | 创建不同容器 |
| pod_id 与 project_id 混用 | 第一次用 project_id="p1"，第二次用 pod_id="p1" | 两个不同容器 |

---

## 13. 附录

### 13.1 术语表

| 术语 | 定义 |
|------|------|
| Tenant (租户) | 最高级别的业务隔离单元，通常对应一个组织或企业 |
| Space (空间) | 租户下的逻辑分区，用于区分不同业务线或环境 |
| Project (项目) | 具体的 AI 开发项目，对应一个工作空间 |
| Pod ID | 上游系统传递的容器唯一标识符，用于容器复用 |
| Isolation Type | 隔离类型，决定容器共享的粒度 |

### 13.2 参考文档

- [ServiceType 枚举定义](../../crates/shared_types/src/service_type.rs)
- [容器路径模板配置](../../crates/shared_types/src/service_config.rs)
- [Docker Compose 配置](../../docker/docker-compose.yml)
- [Kubernetes Helm Values](../../k8s/helm/rcoder/values.yaml)
