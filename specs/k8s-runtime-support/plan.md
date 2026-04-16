# Implementation Plan: Kubernetes Runtime Support

**Branch**: `dev-k8s` | **Date**: 2026-04-16 | **Spec**: [spec.md](./spec.md)
**Input**: Feature specification from `/specs/k8s-runtime-support/spec.md`

## Execution Flow (/plan command scope)
```
1. Load feature spec from Input path
2. Fill Technical Context
3. Constitution Check (empty constitution - skip)
4. Execute Phase 0 → research.md
5. Execute Phase 1 → contracts, data-model.md, quickstart.md
6. Re-evaluate Constitution Check
7. Plan Phase 2 → tasks.md (via /tasks command)
8. STOP - Ready for /tasks command
```

## Summary
修复 RCoder 的 Kubernetes 运行时支持，使系统能够在 K8s 环境中动态创建和管理容器（Pod），而不依赖 Docker Socket。

主要改动：
1. 重构 `global` 模块使用 `RuntimeManager` 选择运行时
2. K8s Runtime 支持 `user_id` 参数
3. 使用 Pod IP 通信（与 Docker 保持一致，无需 Service DNS）
4. 实现 `list_containers`

## Technical Context
**Language/Version**: Rust 1.75+ (2024 Edition)  
**Primary Dependencies**: kube-rs 0.98, bollard, tonic, tower  
**Storage**: N/A (状态在 K8s API 中)  
**Testing**: cargo test, integration tests  
**Target Platform**: Linux (Docker + Kubernetes)  
**Performance Goals**: Pod 启动 < 120s, gRPC 延迟 < 100ms  
**Constraints**: 
- 必须兼容现有 Docker 模式
- K8s Service Account 需要预配置
- 需要 RBAC 权限创建/删除 Pod

## Constitution Check
*Note: Constitution file is empty template - no gates to check*

## Project Structure

### Documentation (this feature)
```
specs/k8s-runtime-support/
├── plan.md              # This file
├── spec.md              # Feature specification
├── research.md          # Phase 0 output
├── data-model.md        # Phase 1 output (interfaces)
└── tasks.md             # Phase 2 output (/tasks command)
```

### Source Code (repository root)
```
crates/
├── docker_manager/
│   ├── src/
│   │   ├── lib.rs                    # MODIFY: global module
│   │   └── runtime/
│   │       ├── mod.rs               # MODIFY: export changes
│   │       ├── manager.rs           # KEEP: RuntimeManager
│   │       ├── kubernetes_runtime.rs # MODIFY: fix issues
│   │       └── docker_runtime.rs    # KEEP: unchanged
├── container-runtime-api/
│   └── src/
│       └── runtime_trait.rs         # KEEP: trait definition
└── rcoder/
    └── src/
        └── main.rs                   # MODIFY: runtime init
```

**Structure Decision**: 修改现有 `docker_manager` crate，重构 `global` 模块使其根据 `CONTAINER_RUNTIME` 环境变量选择 `RuntimeManager` 或直接使用 `DockerManager`

### K8s Local Development Environment

新增 `k8s/` 目录，用于本地 K8s 测试环境（类比现有 `docker/` 目录）：

```
k8s/
├── README.md                    # K8s 环境使用说明
├── kind-config.yaml            # Kind (Kubernetes IN Docker) 配置
├── start-kind.sh               # 启动本地 K8s 集群脚本
├── stop-kind.sh                # 停止本地 K8s 集群脚本
├── manifests/
│   ├── namespace.yaml           # Namespace 定义
│   ├── serviceaccount.yaml     # ServiceAccount + RBAC
│   ├── rcoder-deployment.yaml  # RCoder 主服务 Deployment
│   └── rcoder-service.yaml     # RCoder 主服务 Service
└── test-chat.sh               # 测试脚本
```

**功能**：
- 使用 [Kind](https://kind.sigs.k8s.io/) 在本地运行 K8s
- 一键启动本地 K8s 集群
- 部署 RCoder 到本地 K8s
- 测试 `/chat` 和 `/computer/chat` 接口

## Phase 0: Outline & Research

### Research Tasks
1. **K8s Pod IP 通信**: Pod 之间直接用 IP 通信，同 Docker 方式
2. **kube-rs 最佳实践**: 社区推荐的 K8s Runtime 实现模式
3. **K8s 健康检查**: HTTP Health Check 方式（与 Docker 一致）
4. **K8s 存储**: workspace 存储问题（标记为后续优化项）

### Output
**research.md**:
- Decision: 使用 Pod IP 直接通信（与 Docker 方式一致）
- Rationale: Pod IP 在同一集群内可直接通信，无需额外 Service；Pod 重启后 cleanup_task 会重建并更新 IP
- Alternatives considered: Service DNS（需要额外创建 Service，增加复杂度，当前不需要）

## Phase 1: Design & Contracts

### Interface Changes

1. **`ContainerRuntime` trait** (新增方法):
```rust
// 新增方法
async fn list_containers_by_label(&self, label: &str) -> ContainerRuntimeResult<Vec<RuntimeContainerInfo>>;
```

2. **`KubernetesRuntime` 实现**:
```rust
// 支持 user_id
async fn create_container(&self, project_id: Option<&str>, user_id: Option<&str>, ...) -> Result<...>

// 使用 Pod IP 通信（与 Docker 保持一致）
// service_url: "http://{pod_ip}:{port}"
```

3. **`global` 模块重构**:
```rust
// 修改 init
pub async fn init_global_runtime(config: DockerManagerConfig) -> DockerResult<()> {
    match RuntimeType::from_env() {
        RuntimeType::Kubernetes => RuntimeManager::init(config).await,
        RuntimeType::Docker => {
            // 直接初始化 DockerManager（保持向后兼容）
            init_docker_manager_direct(config).await
        }
    }
}
```

4. **Health Check 抽象**:
```rust
// 新增 K8s 健康检查
async fn wait_for_pod_ready(&self, pod_name: &str) -> ContainerRuntimeResult<()>
```

### Data Model
See `data-model.md`

## Phase 2: Task Planning Approach
*This section describes what the /tasks command will do - DO NOT execute during /plan*

**Task Generation Strategy**:
- 依赖分析：首先解决 global 模块重构（P0）
- 然后修复 K8s Runtime 的 user_id 支持（P0）
- 实现 list_containers（P1）
- 最后更新健康检查（P1）

**Ordering Strategy**:
1. 重构 global 模块使其使用 RuntimeManager
2. 修改 rcoder main.rs 适配新接口
3. 修复 KubernetesRuntime user_id 支持
4. 实现 list_containers
5. 更新 K8s 健康检查
6. 测试和文档

**Estimated Output**: ~10-15 tasks

## Complexity Tracking
| Violation | Why Needed | Simpler Alternative Rejected Because |
|-----------|------------|-------------------------------------|
| 重构 global 模块 | 需要统一运行时选择逻辑 | 直接在 rcoder 中判断，但会导致代码重复 |

## Progress Tracking

**Phase Status**:
- [x] Phase 0: Research complete - research.md created
- [x] Phase 1: Design complete - data-model.md created
- [x] Phase 2: Task planning approach defined (above)
- [ ] Phase 3: Tasks generated (/tasks command)
- [ ] Phase 4: Implementation complete
- [ ] Phase 5: Validation passed

**Gate Status**:
- [x] Initial Constitution Check: N/A (empty constitution)
- [x] Post-Design Constitution Check: N/A
- [x] All NEEDS CLARIFICATION resolved
- [ ] Complexity deviations documented

---
*Based on Constitution v2.1.1 - See `/memory/constitution.md`*
