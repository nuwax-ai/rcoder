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
1. 重构 `global` 模块使用 `RuntimeManager` 选择运行时 **（P0 - 关键路径）**
2. K8s Runtime 支持 `user_id` 参数 **（P0 - ComputerAgent 支持）**
3. 修复 `stop_container` 方法正确处理不同 ServiceType **（P0）**
4. 更新 Makefile K8s 命令避免重复操作 **（P1）**

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

## Problem Analysis (Updated 2026-04-20)

### P0 Issues (Critical - Blocking K8s)

| Issue | Location | Description |
|-------|----------|-------------|
| `RuntimeManager::init()` never called | `main.rs:181-186` | K8s mode calls `init_global_docker_manager_with_config()` which does NOT initialize `RuntimeManager::RUNTIME_INSTANCE` |
| `stop_container` ignores service_type | `kubernetes_runtime.rs:437` | Always uses `ServiceType::RCoder`, cannot stop ComputerAgentRunner pods |
| K8s path not properly initialized | `lib.rs:201-223` | `init_global_docker_manager_with_config()` has K8s code but never reaches it properly |

### Root Cause

```rust
// main.rs calls this:
docker_manager::global::init_global_docker_manager_with_config(config).await

// But this function does NOT call RuntimeManager::init() for K8s mode!
// It only sets GLOBAL_DOCKER_MANAGER (for Docker mode)
```

### Required Changes

1. **main.rs**: Initialize using `RuntimeManager::init()` for K8s, or modify `init_global_docker_manager_with_config()` to properly call `RuntimeManager::init()` when K8s mode detected

2. **kubernetes_runtime.rs**: Fix `stop_container` to handle different service types

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

### Actual Code Changes Required

#### 1. Fix `lib.rs` - global module initialization

**Current (broken)**:
```rust
pub async fn init_global_docker_manager_with_config(config: DockerManagerConfig) -> DockerResult<()> {
    let runtime_type = RuntimeType::from_env();
    crate::runtime::RuntimeManager::init(config.clone()).await  // <- Already calls RuntimeManager::init()!

    if runtime_type == RuntimeType::Docker {
        let manager = Arc::new(DockerManager::new(config).await?);
        GLOBAL_DOCKER_MANAGER.set(manager)...;
    }
    // Problem: Sets RUNTIME_INSTANCE in RuntimeManager::init() but returns DockerResult
}
```

**Fix**: Ensure proper error handling and that K8s path doesn't try to set GLOBAL_DOCKER_MANAGER

#### 2. Fix `main.rs` - runtime initialization

**Current**: Calls `init_global_docker_manager_with_config()` which should work, but error handling may be wrong

**Fix**: Verify RuntimeManager is properly initialized before using `RuntimeManager::get()`

#### 3. Fix `kubernetes_runtime.rs` - stop_container

**Current**:
```rust
async fn stop_container(&self, project_id: &str) -> ContainerRuntimeResult<()> {
    let pod_name = self.pod_name(project_id, &ServiceType::RCoder);  // Always RCoder!
    // ...
}
```

**Fix**: Need to track service_type per container or change the interface

#### 4. Fix Makefile k8s commands

**Current**:
```makefile
dev-up-k8s:
    kubectl apply -f manifests/rcoder-deployment.yaml
    kubectl set image deployment/rcoder rcoder=$(IMAGE)  # Redundant after apply

dev-restart-k8s: dev-build-k8s
    kubectl apply -f manifests/rcoder-deployment.yaml
    kubectl set image deployment/rcoder rcoder=$(IMAGE)
    kubectl rollout restart deploy/rcoder  # rollout restart uses current deployment image, not new one!
```

**Fix**: Use `kubectl delete pods` or fix the image update flow

## Phase 2: Task Planning Approach
*This section describes what the /tasks command will do - DO NOT execute during /plan*

**Task Generation Strategy**:
- P0 优先级：先解决阻止 K8s 运行的 critical 问题
- P1 优先级：修复已知的 bug 和不完整实现
- P2 优先级：改进和优化

**Task Order (Dependency-Based)**:
1. **[P0] Fix global module initialization** - 确保 K8s 模式下 RuntimeManager 正确初始化
2. **[P0] Fix rcoder main.rs** - 调用正确的初始化函数
3. **[P0] Fix stop_container in KubernetesRuntime** - 正确处理不同 service_type
4. **[P1] Fix Makefile k8s commands** - 消除重复操作，简化逻辑
5. **[P1] Test K8s mode end-to-end** - 验证修复有效
6. **[P2] Improve K8s health check** - 如有时间，优化健康检查

**Estimated Output**: 8-10 focused tasks

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
