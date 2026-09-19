# Toasty PostgreSQL TLS 实际契约

## 依据与边界

本机锁定的 toasty-driver-postgresql 0.10.0 发布源码 Cargo.toml 默认 features 包含 tls；src/lib.rs 的 configure_tls 解析 sslmode/sslrootcert 等 URL 参数。项目 PostgresConfig::to_dsn 在显式 URL 非空时原样返回；db/postgres.rs 使用该 DSN 创建官方连接器。本次不修改生产连接行为、不向全局信任库安装 CA。

## 独立执行

```bash
python3 tools/test_pg_storage_tls.py --image postgres:17
```

要求 Docker、OpenSSL、Cargo nextest 及本地已有官方 PostgreSQL 17 镜像。脚本不自动拉取镜像；使用唯一容器名/归属标签、127.0.0.1 随机映射端口与全新匿名数据卷。临时目录 0700，证书与私钥文件 0600，容器删除前核对 ID/名称/归属标签；清理仅覆盖本轮容器与匿名卷。不会读取现有 PG 凭据、数据库、Compose/K8s 或安装 CA。

fixture 使用临时自签 CA 签发仅包含 DNS localhost 的服务器证书，另生成不可信 CA。短命 loopback fixture 故意允许 trust 认证及明文连接，不需要密码；这是用于证明 verify-full 不会在验证失败时降级明文的负控，不是部署配置。

真实 db::postgres::open → DatabaseOwner 验证：

1. verify-full + 正确 CA + localhost 成功，查询自身 backend 的 pg_stat_ssl 确认 ssl=true 且协议版本存在；核对 PG 主版本为 17。
2. 同地址错误 CA 拒绝连接。
3. 正确 CA 但使用无 IP SAN 的 127.0.0.1 拒绝连接。
4. sslmode=disable 负控可以连接，查询同一 backend 确认未加密。因此错误身份不能以明文回退获得成功。
5. 每组输入显式断言 PostgresConfig URL 没有被改写，成功 owner 显式 shutdown。

该 ignored 测试通过专用脚本执行，不加入每个默认 E2E 场景，避免重复创建数据库。它只证明 PG TLS 和连接策略，不等于断网未知提交、advisory lock 故障恢复或业务 K8s 验收。

## 当前状态

已完成源码与脚本；定向 rustfmt、Python AST 语法检查、git diff --check 通过。尚未运行 Cargo 或启动 TLS fixture；需集中执行后记录实际 exit code/测试数，不能提前标记 T0 TLS 通过。脚本不保存私钥、密码或 DSN 日志。


## 真实验证结果（2026-09-20）

集中执行隔离 PG17 TLS fixture 已完成，脚本退出 0。Nextest run ID：`8d01c0aa-71e7-458e-a5e4-498df7b01b30`；选中 1 项，通过 1 项，另外 86 项为筛选排除，不计入已验证数量。日志：`/tmp/rcoder-toasty-pg-tls.log`。

该项实际执行了正确 CA/主机名的 verify-full 加密连接、错误 CA/主机名拒绝，以及 sslmode=disable 可达负控。明文仅限本轮独立 loopback fixture，用来证明身份校验失败没有通过明文回退变为成功；生产连接配置未改动。结果不扩展为全部 T0、断网恢复或部署验收通过。

PG-only 构建同时暴露三个 Turso 专属项的 dead_code 告警。已按 userapp-turso feature 限定 ConnectionPolicy::Turso 及匹配分支/PRAGMA helper、只读本地 schema 验证、local restart 隔离函数；没有添加 allow(dead_code)。该限定的 PG-only Clippy 等待集中执行，不能沿用 TLS 通过推断 lint 已通过。
