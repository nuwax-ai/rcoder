# RCoder 文档

本目录是 RCoder 的公开文档集。接口的完整定义以运行时自动生成的 OpenAPI 文档为准（启动服务后访问 `/api/docs`，Swagger UI 与 Scalar 双面）。

## 导航

### 架构

- [架构总览](architecture/overview.md) —— 主链路、核心组件、crate 地图与三种部署形态
- [gRPC 内部通信](architecture/grpc.md) —— rcoder 与 agent_runner 之间的 gRPC 设计：RPC 清单、oneof 事件系统、连接池

### 业务概念

- [UserApp 应用管理](concepts/userapp.md) —— dev/prod 双环境、构建发布链、闲置回收与流量唤醒、存储与删除语义

### 部署形态

- [宿主机单机形态（deploy-host）](deployment/host.md) —— 控制平面直接跑在宿主机，"有 Docker 就能跑"的桌面基座形态

### 运维与排障

- [可观测性指南](observability.md) —— 分布式追踪（Tempo）、日志链路（Loki）、事件级 Tokio tracing（dial9）、span 耗时指标

## 快速入口

| 目的 | 入口 |
|------|------|
| 快速开始 / 本地开发 | [README 快速开始](../README.md#-快速开始) |
| 全量 API（权威） | 启动后访问 `/api/docs`（Swagger UI / Scalar） |
| 测试体系 | [README 开发指南](../README.md#-开发指南)、[E2E 说明](../tests-e2e/tools/README.md) |
| 贡献指引 | [CONTRIBUTING](../CONTRIBUTING.md) |
