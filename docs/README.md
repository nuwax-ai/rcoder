# RCoder 文档

本目录维护公开的使用指南、接口契约和开发说明。完整 API 定义以运行时 OpenAPI 为准：`/api/docs` 为 Swagger UI，`/api/docs/scalar` 为 Scalar，均提供主服务与 file-server 文档。

## 开始与部署

- [项目与快速开始](../README.md#quick-start)
- [配置与 Agent 准备](configuration.md)：配置优先级、模型输入、API Key、Agent 与 UserApp 存储
- [Docker Compose](deployment/docker.md)：本地开发、镜像、持久化与停止
- [宿主机部署](deployment/host.md)：deploy-host 目录、端口与 Docker/K8s 前置
- [Kubernetes](deployment/kubernetes.md)：资源模型、部署与验证入口

## 功能与接口

- [Agent 会话与 SSE](concepts/agent-sessions.md)：对话、事件、续传和并行会话
- [权限审批](concepts/permissions.md)：规则、审批事件与回执
- [文件服务](concepts/file-services.md)：工作区、Git、技能与开发服务
- [Computer Agent](concepts/computer-agent.md)：桌面、VNC、音频与输入法
- [UserApp 应用管理](concepts/userapp.md)：开发/生产环境、构建发布与生命周期
- [UserApp 子路径预览](concepts/userapp-subpath-routing.md)：路径与静态资源策略
- [项目与任务诊断](userapp-project-diagnostics.md)：预检查失败、诊断任务与日志接入
- [运行时错误与安全重试](runtime-error-contract.md)：错误码、操作身份和结果未知

## 运行与恢复

- [app-cli 使用指南](development/app-cli.md)：源码启动、构建制品、常驻 owner 与控制命令
- [UserApp dev owner 恢复](userapp-dev-owner-recovery.md)：重复 run、工作区定位与自动恢复
- [本地监督与恢复](deployment/native-supervision.md)：app-cli / file-server-proxy 的独立监督通道
- [重复构建与等待](userapp-build-queue.md)：替换、取消与构建执行范围
- [启动探针](userapp-startup-probes.md)：worker 与服务启动检查
- [容器状态与业务就绪](userapp-readiness-container.md)：容器控制状态与实际 ready
- [迁移与凭据策略](userapp-migration-policy.md)：迁移输出、失败处理与运行凭据
- [停服日志](userapp-stopped-logs.md)：只读日志查询与边界
- [可观测性](observability.md)：追踪、日志、指标与性能诊断

## 开发与测试

- [贡献指南](../CONTRIBUTING.md)：工具链、格式化、Clippy 与测试要求
- [E2E 说明](../tests-e2e/tools/README.md)：Compose/K8s 前置、运行方式与验收边界
- [app-cli 配置校验](development/app-cli-validate.md)：只读检查与分类退出码
- [本机 K8s 核心回归](development/local-k8s-core-validation.md)：隔离输入、真实资源与保卷验收
- [K8s 租户入站隔离验证](deployment/k8s-tenant-isolation-validation.md)：策略加载与真实业务请求
- [迁移失败的隔离 Docker 验收](development/userapp-migration-advisory-verification.md)：真实进程、任务日志与业务 HTTP
- [Builder 注册恢复](development/builder-registration-recovery.md)：完成回执与资源身份核验
- [DBX 构建缓存](development/dbx-build-cache.md)：构建身份、缓存复用与诊断
- [Pingap 受控配置切换](development/resident-pingap.md)：配置预检、切换与观察
- [npm 分发](../npm/README.md)：CLI 包结构、平台二进制与发布机制

## 架构

- [架构总览](architecture/overview.md)：主链、crate 地图、存储边界与部署形态
- [gRPC 内部通信](architecture/grpc.md)：协议、连接池与 ACP 事件透传

## 模块参考与测试工具

- [Docker/K8s 运行时](../crates/docker_manager/README.md)：后端装配、资源身份与数据保护
- [PostgreSQL 生命周期合同](../crates/rcoder-storage/PG_LIFECYCLE_CONTRACT.md)：持久身份、事务、schema ledger 与升级边界
- [文件服务配置](../crates/file-server/docs/configuration.md)、[Electron 集成](../crates/file-server/docs/electron-integration.md)
- [pnpm 构建脚本审批](../crates/file-server/docs/pnpm-build-approval.md)、[上传与技能安全](../crates/file-server/docs/upload-and-skill-security.md)
- [file-server-proxy 分发与使用](../crates/file-server-proxy/npm/README.md)、[聚焦真实入口验证](../crates/file-server-proxy/tests/e2e/README.md)
- [文件服务 A/B 对照](../tests/file-server-ab/README.md)、[差分工具](../tools/file-server-differential/README.md)
- [SSE 专项](../tests/sse_e2e/README.md)、[E2E 架构](../tests-e2e/architecture.md)、[独立存储探针](../tools/toasty-probe/README.md)
- [Docker 资源与手动工具](../docker/README.md)：桌面测试页、文件服务请求集与实验项目

## 项目协作

- [安全策略](../SECURITY.md)：漏洞私下报告与部署入口保护
- [许可证](../LICENSE)
