# Requirements Document

## Introduction

本文档定义了ACP代理管理系统的需求，该系统旨在解决AgentSideConnection和ClientSideConnection不实现Send trait的问题，通过代理管理器模式实现在Axum HTTP处理器中安全使用ACP协议的能力。系统需要支持动态创建和管理多个项目的Agent服务，每个项目对应一个独立的Agent实例。

## Requirements

### Requirement 1

**User Story:** 作为开发者，我希望能够在Axum HTTP处理器中安全地使用ACP协议，以便为不同项目提供AI代理服务

#### Acceptance Criteria

1. WHEN HTTP请求到达时 THEN 系统SHALL能够处理非Send的ACP连接而不阻塞Axum的多线程运行时
2. WHEN 使用ACP协议时 THEN 系统SHALL确保AgentSideConnection和ClientSideConnection在单线程环境中运行
3. IF ACP连接需要跨线程通信 THEN 系统SHALL使用MPSC通道进行消息传递

### Requirement 2

**User Story:** 作为系统管理员，我希望系统能够根据project_id动态创建和管理Agent服务，以便为每个项目提供独立的AI代理

#### Acceptance Criteria

1. WHEN 收到包含project_id的请求时 THEN 系统SHALL检查是否存在对应的Agent服务
2. IF Agent服务不存在 THEN 系统SHALL创建新的Agent服务实例
3. IF Agent服务已存在 THEN 系统SHALL复用现有的Agent服务
4. WHEN 创建Agent服务时 THEN 系统SHALL为每个project_id分配独立的工作目录
5. WHEN Agent服务空闲超过配置时间时 THEN 系统SHALL自动清理该服务

### Requirement 3

**User Story:** 作为用户，我希望在没有提供project_id时系统能够自动创建项目，以便快速开始使用AI代理服务

#### Acceptance Criteria

1. WHEN "/chat"请求没有提供project_id时 THEN 系统SHALL生成唯一的project_id
2. WHEN 生成project_id时 THEN 系统SHALL使用UUID去掉中划线的格式
3. WHEN 创建新项目时 THEN 系统SHALL在"./project_workspace/{project_id}"目录下创建工作空间
4. WHEN 项目目录不存在时 THEN 系统SHALL自动创建所需的目录结构

### Requirement 4

**User Story:** 作为开发者，我希望系统提供线程安全的代理管理器，以便在多线程环境中安全地管理Agent服务

#### Acceptance Criteria

1. WHEN 多个HTTP请求并发访问时 THEN 代理管理器SHALL保证线程安全
2. WHEN 管理Agent服务时 THEN 系统SHALL使用适当的同步原语防止竞态条件
3. WHEN 处理MPSC消息时 THEN 系统SHALL确保消息的顺序性和可靠性
4. IF 系统需要隔离非Send逻辑 THEN 系统SHALL使用LocalSet或类似机制

### Requirement 5

**User Story:** 作为系统架构师，我希望系统采用最佳实践的Rust异步编程模式，以便确保系统的性能和可维护性

#### Acceptance Criteria

1. WHEN 处理异步任务时 THEN 系统SHALL使用tokio::task::LocalSet处理非Send futures
2. WHEN 需要跨线程通信时 THEN 系统SHALL使用tokio::sync::mpsc通道
3. WHEN 管理共享状态时 THEN 系统SHALL使用Arc<Mutex<T>>或类似的线程安全容器
4. WHEN 处理错误时 THEN 系统SHALL提供适当的错误处理和恢复机制
5. IF 需要生命周期管理 THEN 系统SHALL实现适当的资源清理机制

### Requirement 6

**User Story:** 作为运维人员，我希望系统能够监控和管理Agent服务的生命周期，以便确保系统资源的有效利用

#### Acceptance Criteria

1. WHEN Agent服务创建时 THEN 系统SHALL记录服务的创建时间和状态
2. WHEN Agent服务处理请求时 THEN 系统SHALL更新服务的最后活动时间
3. WHEN 检查服务状态时 THEN 系统SHALL提供服务健康检查机制
4. IF 服务出现异常 THEN 系统SHALL能够重启或重新创建服务
5. WHEN 系统关闭时 THEN 系统SHALL优雅地关闭所有Agent服务

### Requirement 7

**User Story:** 作为API用户，我希望系统提供一致的接口来与不同项目的Agent服务交互，以便简化客户端的实现

#### Acceptance Criteria

1. WHEN 发送prompt请求时 THEN 系统SHALL将请求路由到正确的Agent服务
2. WHEN Agent处理完成时 THEN 系统SHALL返回统一格式的响应
3. IF 请求失败 THEN 系统SHALL返回明确的错误信息和错误代码
4. WHEN 处理长时间运行的任务时 THEN 系统SHALL支持异步响应机制
5. WHEN 需要实时更新时 THEN 系统SHALL支持SSE或WebSocket连接