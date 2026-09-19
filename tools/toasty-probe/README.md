# Toasty T0 独立探针

此目录是独立 Cargo workspace，不接入 RCoder 的运行时。生产依赖没有因此切换。
锁定 Toasty 0.10.0，Cargo.lock 解析 Turso 0.7.2。

## 运行边界

只使用新建、空的临时数据库。探针会创建表，`push_schema` 仅限此探针；不重置数据库，也不连接既有测试/生产业务库。
连接串通过 `RCODER_TOASTY_PROBE_URL` 环境变量传入，不把真实凭据写进命令示例或报告。

```bash
# 真实文件 Turso；路径必须指向新文件
probe_dir=$(mktemp -d)
RCODER_TOASTY_PROBE_DISPOSABLE=1 \
RCODER_TOASTY_PROBE_URL="turso:$probe_dir/control.db" \
cargo run --manifest-path tools/toasty-probe/Cargo.toml

# PG：先在独立临时 PG 中创建空数据库，再设置上述两个环境变量运行。
# 这个传输关闭探针要求 sslmode=disable，不能拿它替换生产 TLS 连接工厂。
```

## 已覆盖

- ORM 模型创建与读取。
- 真实数据库 FK / CHECK 拒绝非法写入。
- 参数绑定、CAS 影响行数 1 与 0。
- 显式 rollback 后无残留记录。
- DB / connection 所有权退出通知。
- PG 独占 advisory lock：持有期间另一连接不能取得；释放时等待本地 transport 任务退出，竞争者必须再次从 PG 实际取得锁，不能靠本地计数授权。

## 仍未覆盖，T0 不得整体打勾

生产 TLS/URL 配置兼容、完整新 schema 映射、双 store 并发 CAS、完整业务事务取消、BEGIN/COMMIT/ROLLBACK 故障隔离、迁移并发/checksum、断网与旧库拒绝、Turso owner runtime 与目录锁的完整关闭顺序。

`managed.rs` 是验证公开 `Connection::new(client)` 接口的实验实现。生产实现仍须处理：连接创建取消、关闭失败共享结果、事务污染隔离、配置/密码脱敏、连接池预算、TLS 语义、独占 leader 工厂。不得直接把实验代码搬进生产。

## 发现的两项接口边界

1. 官方 PostgreSQL 驱动内部 spawn `tokio_postgres::Connection`，不公开其 JoinHandle。只包装外层 Drop 无法证明后台传输结束。探针用公开 `Connection::new(client)` 装配自己持有的 transport；这条路径已用真实 PG 验证，但生产 TLS 尚未覆盖。
2. 0.10.0 对 raw SQL 推断 PostgreSQL `void` 返回值会 panic。`SELECT pg_advisory_lock(...)` 必须走不解码结果的 `sql::statement`，不能用 `sql::query`。锁是否获取仍用有 bool 返回的 `pg_try_advisory_lock` 判断。

本地 transport 退出与 PG 已处理断连不是同一瞬间。因此“drop 后立即一次 try_lock 必须成功”是过强的测试断言，不构成服务器锁泄漏证明；有界轮询真实锁结果才是选主证据。

## 第二轮：直接验证生产基础层源文件

`owner.rs`、`driver.rs`、`models.rs`、`schema.rs` 通过 `#[path]` 引用
`crates/rcoder-storage/src/db/`，探针与待接入存储层测试的是同一份实现。
当前尚未从 `rcoder-storage/src/lib.rs` 启用该模块，SQLx 业务迁移还未完成。

```bash
cargo nextest run --manifest-path tools/toasty-probe/Cargo.toml --no-fail-fast
# 使用只属于本探针的临时 PostgreSQL；URL 从环境传入
RCODER_TOASTY_PROBE_DISPOSABLE=1 RCODER_TOASTY_OWNER_PROBE=1 \
cargo run --manifest-path tools/toasty-probe/Cargo.toml
RCODER_TOASTY_PROBE_DISPOSABLE=1 RCODER_TOASTY_SCHEMA_PROBE=1 \
cargo run --manifest-path tools/toasty-probe/Cargo.toml
```

- 新 owner 采用专用线程及其 current-thread runtime，完整事务入队；关闭排空后销毁 runtime 并 join。
  官方 PG 驱动自己的传输任务也归属该 runtime，因此无需在生产复制 `NoTls` 连接工厂。
- 新 PG 路径使用官方连接器，真实 PG 已覆盖并发 CAS、关机后 advisory lock 释放、两个独立 owner 并发安装三个组件。
  本轮临时 PG 未配置 TLS，不能把源码保留官方 TLS 解释成 TLS 实机验收。
- Turso 覆盖取消、关闭、初始化失败、WAL/FULL/FK 回读、规范化模型读写、非法 FK/CHECK、旧开发表保护、checksum/未来版本/缺表/缺索引拒绝。
- 四个 DDL 在 `crates/rcoder-storage/schema/`；初始化 runner 记录 checksum 和实际 catalog 指纹。
  旧迁移尚未移除，新 runner 尚未成为生产初始化入口。
- BEGIN/COMMIT/ROLLBACK/savepoint 错误隔离测试使用受控驱动故障。
  它证明连接隔离策略，尚不证明完整业务链能正确记录未知提交结果并保留租约。

仍需补全：真实文件跨进程恢复、完整 UserApp 事务及双后端公共契约、真实目录锁、连接中断矩阵、
独占选主连接全部退出路径、TLS 实机、生产后端装配和完整组件验证。

## 2026-09-19 共用后端与 Preview

`--all-features` 现在直接编译 UserApp 共用事务、CR10 配置存储及 Preview 生产源文件。
`RCODER_USERAPP_PG_TEST_DSN` 指向新建可丢弃 PostgreSQL 后，可以用
`cargo nextest run --manifest-path tools/toasty-probe/Cargo.toml --all-features --no-fail-fast --run-ignored all`
同时执行真实 PG 契约；缺 DSN 会使显式启用的 PG 测试失败，不计作通过。

额外两副本/端口竞争使用 `RCODER_TOASTY_COMMON_PG_PROBE=1` 或
`RCODER_TOASTY_PREVIEW_PG_PROBE=1`，并设置既有的 DISPOSABLE/URL 门禁后运行探针。
当前探针 URL 需包含查询参数（例如测试专用的 `?sslmode=disable`），因隔离 schema
通过追加 search_path 参数实现。生产连接器没有强制关闭 TLS。

最新有效性与未完成范围见 `specs/toasty-storage-unification/verification.md`。
UserApp/Preview 已接入导出，但 Project/Activity、旧测试夹具及完整 CR10 执行链仍待迁移；
不能把本探针通过当成整个 RCoder 已可部署。
