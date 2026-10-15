# PostgreSQL 生命周期写入合同

项目和会话使用持久化的不透明身份。同一存续项目的更新保留身份；删除后重新创建获得新身份，并关联前一身份。删除为已受理的身份保存 tombstone，不能作用于后来重建的对象。会话清空绑定捕获的会话身份；容器删除同时核验原项目身份及当前容器 ID。

## 写入、取消与关闭

写事务在执行语句前按固定顺序取得 advisory lock，避免锁序相反。进程内登记串行化镜像更新和不可变操作捕获，同步锁不跨数据库 await。

持久操作在提交或移交写队列前保持登记；请求取消会将其尚需完成的操作排队。关闭先停止受理，等待已登记的直接写入，再排空 writer。并发或重复 flush 共享终态 `FlushOutcome`，调用方观察超时不会丢弃完成观察器。未知提交结果保留原操作和保护，不能重新生成身份重放。

## Schema ledger 与升级边界

当前初始化通过 Toasty 共用数据库层执行显式 DDL，使用 `rcoder_schema_migrations` 记录组件、版本、迁移名、DDL 校验和及实际 catalog 指纹。初始化与 PostgreSQL schema 锁处于同一事务；已有记录必须与二进制支持的 DDL 和实际表、列、约束、索引一致。

| 组件 | 基线 | 当前追加迁移 |
|---|---|---|
| Project | [project-pg-v1.sql](schema/project-pg-v1.sql) | [agent bindings v2](schema/project-agent-bindings-v2.sql) |
| UserApp | [userapp-pg-v1.sql](schema/userapp-pg-v1.sql) | [compute control v2](schema/userapp-compute-control-v2.sql)、[recovery witness v3](schema/userapp-recovery-witness-v3.sql) |
| Preview | [preview-pg-v1.sql](schema/preview-pg-v1.sql) | 无 |

版本支持与校验以 [schema.rs](src/db/schema.rs) 为准。仅空的新 schema 可以建立初始 ledger；存在未版本化旧表时明确拒绝，不自动认领、回填或清库。未知组件/未来版本、校验和冲突、迁移历史断档、缺表、非预期表或 catalog 不匹配也会拒绝初始化，并保留原数据。离线观察器要求 schema 已完整安装，不执行升级。

已安装的基线保持不变，新增能力使用显式追加迁移。需要改变现有表或导入旧数据时，必须提供独立、经过旧数据验证的升级方案，不能将初始化成功当作旧库已兼容。升级到不兼容的身份或写入协议前，先停止并成功排空旧 writer；不要在旧操作仍可能重放时删除 tombstone。

## 操作作用域

UserApp 操作的 `scope` 由服务端从操作类型推导：`Dev` 管理 builder，`Prod` 管理生产运行时，`Application` 覆盖两种环境。生命周期的 `active_operations {dev, prod, application}` 分别记录占用者。

受理和推进核验原操作自己的槽位；终态只清除该操作占用的槽位。Dev 与 Prod 操作可以独立协调，Application 操作要求全部槽位空闲，并在同一事务中占用应用槽、按操作语义推进生命周期。槽位及租约不能用最新请求替换原执行身份。

物理租约遵守同一作用域：Dev 使用 builder-family 的 `acquire_builder_family_operation`，Prod/Application 使用 app-family 运行时回执。作用域合同与转换校验位于 [shared_types/userapp](../shared_types/src/userapp/mod.rs)。

## 隔离回归

使用独占、可丢弃的 PostgreSQL 17 测试库，从环境传入 DSN，不在命令或报告中写真实凭据。从仓库根执行 ProjectStore 生命周期聚焦回归：

```bash
RCODER_PG_TEST_STRICT=1 cargo nextest run -p rcoder-storage \
  --features pg --lib --no-fail-fast -E 'test(lifecycle_contract)'
```

运行前设置 `RCODER_PG_TEST_DSN`；strict 模式缺 DSN 会失败。用例覆盖延迟删除、tombstone、会话复用、容器重新关联、重载/跨副本同步、未版本化旧 schema 拒绝、并发关闭，以及 advisory lock 阻塞真实写入时的取消。

普通 workspace 测试缺少数据库时可能由环境门控不执行，不能据此宣称真实 PG 验收通过。完整存储入口及 UserApp 契约见 [持久化回归映射](../../tests-e2e/tools/storage-acceptance.md) 和 [严格 E2E 说明](../../tests-e2e/tools/README.md)。测试仅清理本轮自有数据库/容器，不使用已有业务库；每轮结果另外记录源码、后端、命令、退出码和实际执行范围。
