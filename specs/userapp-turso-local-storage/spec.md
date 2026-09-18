# UserApp 本地控制存储改用 Turso

## 决策与背景

2026-09-18 用户确认：SQLx SQLite 后端刚开发、尚未上线，没有存量兼容需求。直接以 Turso 替代，不维护双本地后端、不做旧 SQLite 数据导入。本文覆盖此前方案中“Compose 使用 SQLx SQLite”的目标；历史 verification 保留原事实。

## 目标

- Compose / Docker 模式的 UserApp 权威控制存储默认使用进程内 Turso，无云服务、账号或额外数据库容器。
- K8s 继续使用 SQLx PostgreSQL，保持多副本事务与生命周期保护。
- 移除 SQLx SQLite 实现、feature 和生效配置；保留 UserAppLifecycleStore、领域状态机和现有业务语义。
- 持久化覆盖操作、幂等请求、执行输入、资源绑定、租约、deadline、dev/prod/application 槽位等当前完整契约，不能只移植基本 CRUD。
- 数据库初始化、迁移、配置和读写失败明确传播，不退回 SQLite/JSON/内存，不吞错后返回成功。

## 行为要求

1. 同域操作互斥，dev/prod 独立操作可以受理；Application 操作与两域冲突。数据库短事务串行不等于把整个容器操作串行。
2. 操作记录和生命周期槽位在同一事务提交；旧 revision、旧 lifecycle、旧 executor 不得覆写当前状态。
3. 维持本地单进程目录独占。数据库锁只证明本地执行器排他，不证明远程容器写入已经结束。
4. 重启保留 Pending 和历史终态；按当前恢复规则隔离中断写入，未知结果保持 RecoveryRequired。不得因更换引擎清理不确定操作或租约。
5. 请求取消、排队超时、提交错误、关机分别处理，事务不得泄漏给下一请求。只有提交成功才报告持久化成功。
6. 整个数据目录持久化，业务应用 purge 不得删除控制库。目录/文件别名、链接和不支持的文件系统保持既有保护，并适配 Turso 的实际旁文件。
7. 无线上数据迁移不代表允许自动删除开发者的旧文件。使用新文件名；对检测到旧库而没有新库的既有目录明确拒绝并说明使用独立新目录，不静默初始化出另一份应用身份。

## 非目标

不更换 PostgreSQL、Agent 存储、应用容器内数据库或 app-cli journal；不引入 Turso Cloud、复制、MVCC、向量搜索；不扩大到 RCoder Windows 宿主适配；不做 npm 发布或集群升级。

## 完成标准

默认 Docker、仅 PostgreSQL、全 features 三种构建边界正确；存储契约、取消/崩溃/恢复反例、真实 Compose E2E 通过；真实 PostgreSQL 回归通过。共享生命周期或 K8s 装配变更还必须运行 remote-k8s 对应业务套件。报告区分实现、组件测试和部署验收。

## 2026-09-19 补充决定：存储抽象边界

本次保留 PostgreSQL 初始化受 kubernetes feature 控制、rcoder-pg 依赖 kubernetes 的现有限制，不新增 userapp-pg feature，不解耦、不扩展 Compose PostgreSQL 或多副本能力。此前“仅 PostgreSQL”验证指 `--no-default-features --features rcoder-pg`，仍包含 Kubernetes 编译能力。

业务层继续只依赖 UserAppLifecycleStore 及 shared_types 领域类型。两种数据库必须提供相同的原子操作和错误语义；不是仅提供同名 CRUD。具体接口规范见 [trait-design.md](trait-design.md)，该补充属于本次实施要求。
