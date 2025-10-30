# Implementation Plan

## 任务概述

将分散在多个文件中的容器停止逻辑统一到 docker_manager crate 的新模块中，提供两种清理策略：启动时清理和运行时清理。

## 任务列表

- [x] 1. 创建 container_stop 模块基础结构

  - 在 `crates/docker_manager/src/` 创建 `container_stop.rs` 文件
  - 定义模块常量（超时时间等）
  - 在 `lib.rs` 中导出新模块
  - _Requirements: 3.6, 5.1_

- [x] 2. 实现启动时清理功能
- [x] 2.1 实现 startup_cleanup_containers 函数

  - 实现容器模式匹配查找
  - 实现 5 秒超时停止逻辑
  - 实现 409 错误过滤
  - 返回 CleanupResult 统计信息
  - _Requirements: 1.1, 1.2, 1.3, 1.4_

- [x] 2.2 实现 stop_container_startup_mode 辅助函数

  - 调用 DockerManager::stop_container_by_id_with_timeout
  - 使用 STARTUP_CLEANUP_TIMEOUT_SECONDS 常量
  - 实现错误处理和日志记录
  - _Requirements: 1.2, 4.1, 4.2_

- [x] 2.3 实现 is_409_conflict_error 辅助函数

  - 检查 DockerError 是否包含 "409" 和 "already in progress"
  - 返回布尔值
  - _Requirements: 1.3_

- [x] 3. 实现运行时清理功能
- [x] 3.1 实现 runtime_cleanup_container 函数

  - 实现单个容器停止逻辑
  - 使用 3 秒优雅停止超时
  - 超时后立即强制停止
  - _Requirements: 2.1, 2.2, 2.3_

- [x] 3.2 实现 runtime_cleanup_containers 批量清理函数

  - 批量处理多个容器 ID
  - 返回 CleanupResult 统计信息
  - _Requirements: 2.4, 2.5_

- [x] 3.3 实现 stop_container_runtime_mode 辅助函数

  - 调用 DockerManager::stop_container_by_id_with_timeout
  - 使用 RUNTIME_CLEANUP_TIMEOUT_SECONDS 常量
  - 实现错误处理和日志记录
  - _Requirements: 2.2, 4.1, 4.4_

- [x] 4. 更新 main.rs 使用新的启动清理接口
- [x] 4.1 导入 container_stop 模块

  - 添加 `use docker_manager::container_stop;`
  - _Requirements: 3.3, 5.2_

- [x] 4.2 替换 startup_cleanup_orphaned_containers 函数

  - 调用 `container_stop::startup_cleanup_containers`
  - 更新错误处理逻辑
  - 更新日志输出
  - _Requirements: 3.3, 4.2, 4.3_

- [x] 4.3 删除旧的 startup_cleanup_orphaned_containers 实现

  - 删除 `startup_cleanup_orphaned_containers` 函数
  - 删除 `find_and_cleanup_orphaned_containers` 函数（如果不再使用）
  - _Requirements: 5.5_

- [x] 5. 更新 cleanup_task.rs 使用新的运行时清理接口
- [x] 5.1 导入 container_stop 模块

  - 在 `destroy_docker_container` 方法中添加导入
  - _Requirements: 3.4, 5.3_

- [x] 5.2 替换容器停止逻辑

  - 将 `stop_container_by_id_with_timeout` 调用替换为 `container_stop::runtime_cleanup_container`
  - 简化错误处理逻辑
  - 更新日志输出
  - _Requirements: 3.4, 4.4_

- [x] 5.3 清理 cleanup_single_orphaned_container 方法

  - 使用新的 runtime_cleanup_container 接口
  - 删除重复的停止逻辑
  - _Requirements: 5.5_

- [x] 6. 更新 container_manager.rs（如需要）
- [x] 6.1 检查是否有容器停止逻辑

  - 搜索 `stop_container` 相关调用
  - 确定是否需要更新
  - _Requirements: 3.5, 5.4_

- [x] 6.2 如有需要，替换为新接口

  - 导入 container_stop 模块
  - 替换停止逻辑
  - _Requirements: 3.5, 5.4_

- [x] 7. 添加文档和注释
- [x] 7.1 为 container_stop.rs 添加模块级文档

  - 说明两种清理策略的区别
  - 提供使用示例
  - _Requirements: 5.5_

- [x] 7.2 为公共函数添加详细文档注释

  - 包含参数说明
  - 包含返回值说明
  - 包含使用示例
  - _Requirements: 4.5_

- [x] 8. 验证和测试
- [x] 8.1 编译检查

  - 运行 `cargo check` 确保无编译错误
  - 运行 `cargo clippy` 检查代码质量
  - _Requirements: 5.5_

- [x] 8.2 功能测试
  - 测试启动时清理（模拟 409 错误场景）
  - 测试运行时清理（验证 3 秒超时）
  - 验证日志输出正确
  - _Requirements: 1.5, 2.5, 4.1-4.5_
