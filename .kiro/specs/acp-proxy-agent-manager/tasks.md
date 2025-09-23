# Implementation Plan

- [ ] 1. 创建基础模块结构和类型定义

  - 创建 `proxy_agent_manager.rs` 模块文件
  - 定义核心数据结构：ProxyConfig, AgentServiceHandle, AgentServiceStatus
  - 定义消息类型：ProxyRequest, AgentRequest, AgentResponse
  - 定义错误类型：ProxyAgentError 和 Result 类型别名
  - _Requirements: 1.1, 4.2, 5.1_

- [ ] 2. 实现项目工作空间管理

  - [ ] 2.1 实现 ProjectWorkspace 结构体

    - 编写项目工作空间创建和管理逻辑
    - 实现目录结构自动创建功能
    - 添加工作空间路径验证和权限检查
    - _Requirements: 3.3, 3.4_

  - [ ] 2.2 实现项目 ID 生成和验证
    - 编写 UUID 生成函数，去掉中划线格式
    - 实现项目 ID 验证逻辑
    - 添加项目目录路径构建功能
    - _Requirements: 3.1, 3.2_

- [ ] 3. 实现 ProxyAgentManager 核心功能

  - [ ] 3.1 实现 ProxyAgentManager 结构体和构造函数

    - 编写 ProxyAgentManager::new() 方法
    - 初始化服务注册表 (Arc<DashMap>)
    - 设置 MPSC 通道和配置
    - 启动 LocalSet 运行时
    - _Requirements: 4.1, 4.3, 5.2_

  - [ ] 3.2 实现消息分发器

    - 编写请求分发循环逻辑
    - 实现 ProxyRequest 消息路由
    - 添加错误处理和重试机制
    - 实现优雅关闭逻辑
    - _Requirements: 4.3, 5.4_

  - [ ] 3.3 实现 send_prompt 方法
    - 编写 send_prompt 公共接口
    - 实现请求验证和预处理
    - 添加 oneshot 通道响应处理
    - 实现超时和错误处理
    - _Requirements: 1.1, 7.1, 7.2_

- [ ] 4. 实现 AgentServiceHandle 管理

  - [ ] 4.1 实现 AgentServiceHandle 结构体

    - 编写服务句柄创建逻辑
    - 实现服务状态管理
    - 添加会话信息存储 (active_sessions)
    - 实现最后活动时间跟踪
    - _Requirements: 2.1, 2.2, 6.1, 6.2_

  - [ ] 4.2 实现服务生命周期管理
    - 编写服务创建和初始化逻辑
    - 实现服务状态转换机制
    - 添加服务健康检查功能
    - 实现服务关闭和清理逻辑
    - _Requirements: 2.5, 6.3, 6.4, 6.6_

- [ ] 5. 实现 LocalSetAgentService

  - [ ] 5.1 创建 LocalSetAgentService 基础结构

    - 编写 LocalSetAgentService 结构体
    - 实现构造函数和初始化逻辑
    - 设置 ACP 连接占位符 (Non-Send)
    - 初始化消息处理通道
    - _Requirements: 1.2, 5.1_

  - [ ] 5.2 实现 ACP 连接管理

    - 编写 initialize_connections 方法
    - 实现 AgentSideConnection 和 ClientSideConnection 创建
    - 添加连接状态监控和重连逻辑
    - 实现连接错误处理
    - _Requirements: 1.1, 1.3_

  - [ ] 5.3 实现会话管理逻辑

    - 编写 ensure_session 方法，自动判断 new_session 或 load_session
    - 实现 create_new_session 调用 ACP new_session
    - 实现 load_existing_session 调用 ACP load_session
    - 添加会话状态跟踪和管理
    - _Requirements: 2.2, 2.3_

  - [ ] 5.4 实现 prompt 处理逻辑
    - 编写 handle_prompt 方法
    - 实现会话自动创建/加载逻辑
    - 添加 ACP prompt 请求处理
    - 实现响应收集和返回机制
    - _Requirements: 7.1, 7.2, 7.4_

- [ ] 6. 实现 LocalSet 运行时集成

  - [ ] 6.1 创建 LocalSet 任务管理器

    - 编写 LocalSet 启动和管理逻辑
    - 实现非 Send future 的任务调度
    - 添加任务生命周期管理
    - 实现任务错误处理和恢复
    - _Requirements: 1.2, 5.1_

  - [ ] 6.2 实现消息处理循环
    - 编写 LocalSetAgentService::run 方法
    - 实现 AgentRequest 消息处理循环
    - 添加消息优先级和队列管理
    - 实现优雅关闭机制
    - _Requirements: 4.3, 5.3_

- [ ] 7. 实现服务清理和监控

  - [ ] 7.1 实现空闲服务清理

    - 编写 cleanup_idle_agents 方法
    - 实现基于时间的服务清理逻辑
    - 添加清理策略配置
    - 实现资源释放和状态更新
    - _Requirements: 2.5, 6.5_

  - [ ] 7.2 实现服务监控和健康检查
    - 编写服务状态监控逻辑
    - 实现健康检查和故障检测
    - 添加服务重启和恢复机制
    - 实现监控数据收集和报告
    - _Requirements: 6.3, 6.4_

- [ ] 8. 集成到现有 Axum 应用

  - [ ] 8.1 修改 main.rs 集成 ProxyAgentManager

    - 在 AppState 中添加 ProxyAgentManager 实例
    - 修改应用启动逻辑，初始化代理管理器
    - 更新配置结构以支持代理管理器配置
    - 实现优雅关闭时的代理管理器清理
    - _Requirements: 4.1, 5.5_

  - [ ] 8.2 更新 handle_chat 函数使用代理管理器
    - 修改 handle_chat 函数调用 ProxyAgentManager
    - 更新项目 ID 生成和验证逻辑
    - 修改响应格式以包含 session_id
    - 添加错误处理和状态码映射
    - _Requirements: 3.1, 3.2, 7.1, 7.3_

- [ ] 9. 编写测试和文档

  - [ ] 9.1 编写基础单元测试

    - 为 ProxyAgentManager 核心功能编写单元测试
    - 为 LocalSetAgentService 基础功能编写单元测试
    - 为消息处理逻辑编写测试
    - 验证 "/chat" 接口的基本功能
    - _Requirements: 所有需求的验证_

  - [ ] 9.2 编写端到端集成测试

    - 编写完整的 "/chat" 接口流程测试
    - 测试项目 ID 自动生成和工作空间创建
    - 测试 session_id 的创建和复用逻辑
    - 验证 ACP 协议与 Codex 工具的集成
    - _Requirements: 1.1, 2.1, 3.1, 7.1_

  - [ ] 9.3 编写基础使用文档
    - 编写 "/chat" 接口使用说明
    - 创建基本配置示例
    - 编写 Codex 工具集成说明
    - 添加基本故障排除指南
    - _Requirements: 系统可用性_
