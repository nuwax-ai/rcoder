# Feature Specification: Kubernetes Runtime Support

**Feature Branch**: `dev-k8s`  
**Created**: 2026-04-16  
**Status**: Draft  
**Input**: User description: "修复 K8s 支持，问题是 global 模块未使用 RuntimeManager，K8s Runtime 中 user_id 未处理，使用 Pod IP 而非 Service DNS"

## User Scenarios & Testing

### Primary User Story
RCoder 在 Kubernetes 集群中运行时，需要动态为每个项目/用户创建和管理容器（Pod）。当前实现仅支持 Docker Socket 方式，不支持 K8s 环境。

### Acceptance Scenarios
1. **Given** RCoder 运行在 K8s 环境（设置 `CONTAINER_RUNTIME=kubernetes`），**When** 用户调用 `/chat` 接口，**Then** 系统应在 K8s 中创建 Pod 并正常通信
2. **Given** RCoder 运行在 K8s 环境，**When** 用户调用 `/computer/chat` 接口，**Then** 系统应使用 `user_id` 作为 Pod 标识创建容器
3. **Given** K8s Pod 发生重启，**When** gRPC 通信发生，**Then** 系统应能通过稳定的 Service DNS 找到新 Pod

### Edge Cases
- Pod 重启后 IP 变化如何处理？
- K8s API 访问失败时的降级策略？
- 容器清理逻辑在 K8s 中如何适配？

## Requirements

### Functional Requirements
- **FR-001**: 系统必须根据 `CONTAINER_RUNTIME` 环境变量选择 Docker 或 Kubernetes 运行时
- **FR-002**: Kubernetes Runtime 必须支持 `/chat` 接口（使用 project_id 作为 Pod 标识）
- **FR-003**: Kubernetes Runtime 必须支持 `/computer/chat` 接口（使用 user_id 作为 Pod 标识）
- **FR-004**: Kubernetes Runtime 必须使用稳定的 Service DNS 而非 Pod IP
- **FR-005**: 系统必须实现 `list_containers` 接口以支持 pod list 管理接口
- **FR-006**: Kubernetes Runtime 必须支持容器健康检查

### Key Entities
- **ContainerRuntime**: 容器运行时抽象接口
- **KubernetesRuntime**: K8s 运行时实现
- **DockerRuntime**: Docker 运行时实现（封装 DockerManager）
- **RuntimeManager**: 运行时管理器，负责选择和初始化正确的运行时

## Technical Context (for planning)

### Problem Analysis
| 问题 | 严重程度 | 位置 |
|------|----------|------|
| global 模块未使用 RuntimeManager，K8s 路径从未触发 | P0 | docker_manager/src/lib.rs |
| user_id 在 K8s Runtime 中被忽略 | P0 | kubernetes_runtime.rs |
| 使用 Pod IP 而非 Service DNS | P0 | kubernetes_runtime.rs |
| list_containers 未实现 | P1 | kubernetes_runtime.rs |
| 健康检查机制不兼容 K8s | P1 | health/ |

### Proposed Changes
1. 修改 `global` 模块使用 `RuntimeManager` 选择运行时
2. K8s Runtime 支持 `user_id` 参数
3. K8s Runtime 使用 Service DNS（`{service}-{id}.{namespace}.svc.cluster.local`）
4. 实现 `list_containers` 方法
5. 添加 K8s 健康检查支持（ Readiness Probe 概念）

## Review & Acceptance Checklist

- [x] User description parsed
- [x] Key concepts extracted  
- [x] Ambiguities marked (Pod DNS 格式、K8s API 权限)
- [x] User scenarios defined
- [x] Requirements generated
- [x] Entities identified
- [x] Review checklist passed
