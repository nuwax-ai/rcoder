# Toasty 独立存储探针

此目录是独立 Cargo workspace。它提供 ORM/驱动实验，并通过 `#[path]` 引用部分 RCoder 生产存储源码；探针运行结果不代表整个 RCoder 已通过部署验收。依赖版本以本目录 Cargo.toml 和 Cargo.lock 为准。

## 运行边界

仅使用新建、空的临时数据库。探针会创建表，`push_schema` 仅用于实验；不重置数据库，不连接已有业务库。连接串通过 `RCODER_TOASTY_PROBE_URL` 传入，不把真实凭据写进命令示例、公开文档或报告。

```bash
probe_dir=$(mktemp -d)
RCODER_TOASTY_PROBE_DISPOSABLE=1 \
RCODER_TOASTY_PROBE_URL="turso:$probe_dir/control.db" \
cargo run --locked --manifest-path tools/toasty-probe/Cargo.toml
```

`RCODER_TOASTY_PROBE_DISPOSABLE=1` 是显式允许探针使用可丢弃数据库，不会替调用方判断目标是否安全。运行前必须核对 URL 指向新建测试库。

## 探针入口

| 环境变量 | 入口与范围 |
|---|---|
| 不设置下列模式变量 | ORM 读写、真实 FK/CHECK、CAS、rollback、连接退出；`managed.rs` 是独立实验驱动 |
| `RCODER_TOASTY_OWNER_PROBE=1` | 生产 owner 的独立运行验证 |
| `RCODER_TOASTY_SCHEMA_PROBE=1` | schema 初始化和存储结构验证 |
| `RCODER_TOASTY_COMMON_PG_PROBE=1` | UserApp 共用 PostgreSQL 合同，需要 `--all-features` |
| `RCODER_TOASTY_PREVIEW_PG_PROBE=1` | Preview PostgreSQL 合同，需要 `--all-features` |

每次仅选择一种模式，并同时提供 `RCODER_TOASTY_PROBE_DISPOSABLE` 和 `RCODER_TOASTY_PROBE_URL`。默认 `managed.rs` 的 PostgreSQL 传输实验要求测试专用 `sslmode=disable`；其他模式按实际连接器和源码约束配置。这不是生产 TLS 配置建议。

```bash
cargo nextest run --manifest-path tools/toasty-probe/Cargo.toml --no-fail-fast

# 先创建独立临时 PostgreSQL，并通过环境设置 RCODER_TOASTY_PROBE_URL。
RCODER_TOASTY_PROBE_DISPOSABLE=1 RCODER_TOASTY_OWNER_PROBE=1 \
cargo run --locked --manifest-path tools/toasty-probe/Cargo.toml

# 显式启用真实 PG 合同测试时，需要新的可丢弃数据库 DSN。
# RCODER_USERAPP_PG_TEST_DSN 通过环境注入；缺少配置应明确失败。
cargo nextest run --manifest-path tools/toasty-probe/Cargo.toml \
  --all-features --no-fail-fast --run-ignored all
```

共用 PG/Preview 探针通过 URL 查询参数隔离 schema，需按源码要求提供查询参数。使用不同数据库或 schema 隔离并发运行，避免多个 Cargo 任务共用同一 target 目录。

## 结果解释

生产基础层位于 [rcoder-storage/src/db](../../crates/rcoder-storage/src/db/)，现已参与存储装配；本探针部分模块直接编译同一源文件。独立探针仍不能替代生产调用链、TLS 实机、跨进程恢复、完整 Compose 或 K8s 验收。

本地 transport 退出与 PostgreSQL 已处理断连并非同一瞬间。锁释放应通过有界轮询真实锁结果判断，不能以本地计数或一次抢锁失败推断服务器状态。测试报告应记录实际模式、后端、版本与结果，不将源码存在或历史轮次结果算作当前通过。
