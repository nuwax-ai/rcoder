# Requirements Document

## Introduction

This document defines the requirements for unifying container stopping logic across the rcoder codebase. Currently, container stopping logic is scattered across multiple files with inconsistent implementations. We need to consolidate this into two well-defined scenarios with reusable functions.

## Glossary

- **Container**: Docker容器实例
- **Startup Cleanup**: 服务启动时的容器清理流程
- **Runtime Cleanup**: 运行时的容器清理流程  
- **409 Conflict Error**: Docker API返回的容器已在删除中的错误
- **Graceful Stop**: 给容器优雅退出时间的停止方式
- **Force Stop**: 立即强制停止容器的方式
- **docker_manager**: 负责Docker容器管理的crate模块

## Requirements

### Requirement 1: 启动时容器清理场景

**User Story:** As a system administrator, I want the rcoder service to clean up orphaned containers during startup without blocking the service initialization, so that the system can start reliably even when containers are already being deleted.

#### Acceptance Criteria

1. WHEN the rcoder service starts, THE Container Stop Module SHALL identify all rcoder-agent-* containers
2. WHEN a container is found during startup cleanup, THE Container Stop Module SHALL attempt to stop it with a 5-second timeout
3. IF a 409 conflict error occurs indicating the container is already being deleted, THEN THE Container Stop Module SHALL log this as informational and continue without error
4. WHEN all containers are processed, THE Container Stop Module SHALL return the count of successfully cleaned containers
5. THE Container Stop Module SHALL NOT block service startup if container cleanup encounters non-409 errors

### Requirement 2: 运行时容器清理场景

**User Story:** As a system operator, I want containers to be stopped quickly during runtime cleanup operations, so that resources are freed promptly without waiting for long graceful shutdown periods.

#### Acceptance Criteria

1. WHEN a container needs to be stopped during runtime, THE Container Stop Module SHALL give the container 3 seconds for graceful shutdown
2. WHEN the 3-second grace period expires, THE Container Stop Module SHALL immediately force-stop the container
3. THE Container Stop Module SHALL remove the container after stopping it
4. THE Container Stop Module SHALL log all stop operations with appropriate detail levels
5. THE Container Stop Module SHALL handle errors gracefully and continue cleanup operations

### Requirement 3: 统一的容器停止接口

**User Story:** As a developer, I want a unified API for stopping containers in different scenarios, so that I can easily invoke the appropriate stopping strategy without duplicating code.

#### Acceptance Criteria

1. THE Container Stop Module SHALL provide a function for startup cleanup scenario
2. THE Container Stop Module SHALL provide a function for runtime cleanup scenario  
3. WHEN called from main.rs startup logic, THE Container Stop Module SHALL use the startup cleanup strategy
4. WHEN called from cleanup_task.rs, THE Container Stop Module SHALL use the runtime cleanup strategy
5. WHEN called from container_manager.rs, THE Container Stop Module SHALL use the runtime cleanup strategy
6. THE Container Stop Module SHALL be located in the docker_manager crate for reusability

### Requirement 4: 错误处理和日志记录

**User Story:** As a system operator, I want clear logging of container stop operations, so that I can diagnose issues and understand what happened during cleanup.

#### Acceptance Criteria

1. THE Container Stop Module SHALL log container stop attempts at INFO level
2. THE Container Stop Module SHALL log 409 conflict errors at INFO level during startup cleanup
3. THE Container Stop Module SHALL log other errors at WARN level
4. THE Container Stop Module SHALL log successful stops at INFO level
5. THE Container Stop Module SHALL include container_id and project_id in all log messages

### Requirement 5: 代码复用和维护性

**User Story:** As a developer, I want the container stopping logic centralized in one module, so that future changes only need to be made in one place.

#### Acceptance Criteria

1. THE Container Stop Module SHALL be implemented in a new file under crates/docker_manager/src/
2. WHEN container stopping is needed in main.rs, THE code SHALL call the Container Stop Module
3. WHEN container stopping is needed in cleanup_task.rs, THE code SHALL call the Container Stop Module
4. WHEN container stopping is needed in container_manager.rs, THE code SHALL call the Container Stop Module
5. THE Container Stop Module SHALL eliminate all duplicated container stopping logic across the codebase
