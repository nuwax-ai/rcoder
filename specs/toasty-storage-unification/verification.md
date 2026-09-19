# Toasty 可行性审查与验证记录

## 2026-09-19：规划前源码审查

结论：适合进入全量替换 SQLx 的规划；基础能力存在，关闭、错误隔离和迁移安全需项目层补齐。此结论是源码可行性，不是已运行双后端验收。

### 已取得证据

| 项目 | 证据与结论 |
|---|---|
| RCoder 基线 | `a31b0e52`；期间出现其他开发者修改，未覆盖 |
| 本地 Toasty | `552e5b5e446f87ec9276b63bbf35bd4c3782cded`，未修改 |
| 发布包 | 下载读取 `toasty-0.10.0.crate/.cargo_vcs_info.json`，发布提交 `f3411327b6b57fb03deac9e49f7021d1448176be` |
| 发布版本 | crates.io-index 的 Toasty 0.10.0 与 Turso 0.7.2 均未 yanked；driver 0.10.0 明确依赖 `^0.7` |
| 模型/事务/raw SQL | Toasty `src/db/tx.rs:25,183,199,219`、`src/sql.rs:1`；支持配置/提交/回滚，Drop 回滚不等待结果；raw SQL 不改写方言 |
| 连接回收 | `src/db.rs:81` Connection Drop 归还池；`src/db/pool.rs` 无公开 await close；`src/db/connection_task.rs:77` 内部后台任务句柄 |
| 故障连接隔离 | `src/db/connection_task.rs:259` respond 基于 is_valid；`toasty-core/src/driver.rs:214` 默认 true，Turso driver 未覆盖。不能把 error classification 等同连接已淘汰 |
| 迁移 | `src/migration/embed.rs:56` 按 ID 跳过，无 checksum 校验；PG driver `src/lib.rs:607` 单迁移有事务，但不等于 RCoder 多副本启动锁及历史校验 |
| PG 会话锁 | RCoder `pg/project_store/leader.rs:98` acquire 后 detach；迁移不能直接变为 pooled Connection Drop |
| 范围 | SQLx 不仅存在于 UserApp，还在 Preview、ProjectStore、metadata/activity、迁移和测试模块 |

相同路径前缀：Toasty 为 `/Users/soddy/Documents/git-workspace/toasty/crates/`，RCoder 为本仓 `crates/rcoder-storage/src/`。行号为本轮本地源码；核心 db/tx/pool/migration 另用 git diff 核对发布提交至 HEAD 未变，Turso 驱动使用发布提交另行读取。

### 外部核验入口

- [Toasty registry 索引](https://raw.githubusercontent.com/rust-lang/crates.io-index/master/to/as/toasty)
- [Turso driver registry 索引](https://raw.githubusercontent.com/rust-lang/crates.io-index/master/to/as/toasty-driver-turso)
- [Turso registry 索引](https://raw.githubusercontent.com/rust-lang/crates.io-index/master/tu/rs/turso)
- [Toasty 0.10.0 发布源码](https://github.com/tokio-rs/toasty/tree/f3411327b6b57fb03deac9e49f7021d1448176be)

crates.io API 在本环境返回 403，Python HTTPS 请求另遇本机证书链错误；改由 GitHub 官方 registry 索引及 static.crates.io 发布包交叉核实，没有关闭 TLS 校验。

### 未执行

- 未修改 Cargo.toml/Cargo.lock 或业务实现。
- 未运行 Toasty 探针、nextest、真实 PG、Compose、远端 K8s。
- 未验证 Turso 0.8 生成的开发数据库可被 0.7 读取，因此计划明确禁止直接降级打开。
- ManagedDriver 的可等待关闭与错误隔离为待验证设计，未宣称实现完成。

后续每批追加实际基线、命令、退出码、证据及未完成项，不覆盖本段历史结论。

## 2026-09-19：首次发布库表设计与 SQL 基线补充

### 决策更新

- 用户明确数据库从未正式发布，允许现在调整表结构，目标是 9 月底首次数据库版本前完成。
- 用户明确个人两节点 K8s 的目标 PG 库全部属于 RCoder，允许清空重建及测试；无需再次取得同一范围的授权。凭据和主机不写入方案。
- 新增 [schema-design.md](schema-design.md) 和 [schema-baseline-and-test-reset.md](schema-baseline-and-test-reset.md)，同步 Spec/Plan/Tasks；替换原“尽量维持旧表形状”的建议。

### 源码依据与检查范围

库表审查基线为 `d9e8fd32`，SQL 文件/配置引用复核时 HEAD 为 `2786632a8300a791b6085cdf96b3e227ac7da913`。工作区存在其他审查报告，未编辑或提交它们。

| 检查 | 结果 |
|---|---|
| `git status --short`、`git rev-parse HEAD`、`git log -3 --oneline` | 命令成功；记录当前版本及并行开发推进，不把后续提交合并成原审查基线 |
| `rg --files crates/rcoder-storage` 后筛选 SQL | 15 个 SQL 文件：主域 6、UserApp PG 7、UserApp Turso 1、Preview PG 1 |
| migration/旧账本/include_str 调用检索 | 找到四套账本与多个测试直接引用旧目录/旧迁移；实施清单已包括这些消费者 |
| UserApp schema/domain/shared types/PG与Turso实现 | 识别整记录 JSON、三槽位、请求命名空间、子记录身份关联与旧 metadata 路径；新设计保留既有生命周期语义 |
| Preview schema 与 accept_start | 普通端口索引不是 UNIQUE；行锁不覆盖首次不存在的记录或不同 preview_key，方案补 DB 唯一约束与分配事务锁 |
| ActivityPersistence、activity_repo、registry 与 domain 策略提交 | 当前活动批量覆盖 stopped/wake_blocked/last_accessed；方案改成带代次的活动时间，避免控制意图被旧活动快照覆盖 |
| Project/Session/Container schema、rows/store_repo | 明确 container 登记代次、关系代次、write-behind CAS 与墓碑保护的调整；没有声称现有所有竞态均已动态复现 |
| remote_k8s/manifests.py 与 build-agent-docker values | 源码存在 rcoder 专用数据库配置；不是远端当前连接实测。实施时辨别 Helm 与 remote-k8s 实际部署目标 |

一次 `rg` 批量检索包含不存在的 `crates/rcoder/src/app_state/` 目录，输出了路径错误；其余实际文件和全仓命中用于定位，未把该报错当业务或测试失败。

### 本轮交付边界

- 仅新增/完善规划文档，没有改 Rust、SQL、Cargo 依赖或部署配置。
- 未连接测试集群、未读取远端 Secret、未删库/删资源、未发布或提交。
- 未运行 T0、nextest、真实 PG/Turso、Compose 或 remote K8s；S01–S12 与迁移门禁均待开发阶段执行。
- 当前结论是“设计可以作为实施基线，Toasty 驱动关键行为须先通过 T0”，不是迁移完成或生产可用验收。

### 文档验证

- 本目录 7 个 Markdown 文件的相对链接、代码围栏、私有地址/连接串模式检查通过，脚本退出码 0；另检查行末空格。
- `git diff --check` 退出码 0。新文档仍未跟踪，不能以该命令替代上述直接文件检查。
- 收尾时发现其他开发工作修改了 `file-server-proxy` 三个文件及 `rcoder/src/file_server_embed.rs`；未编辑、暂存或回滚这些改动。
- 可直接交接的 [实施提示词](implementation-prompt.md) 已生成，包含测试库重建授权与部署验收边界。

## 2026-09-19：第二轮 T0 与基础层实现（尚未切换生产存储）

源码基线 HEAD `541a93a6073974b51cad5d405fe94d0e70896538` 加本任务工作树。
本轮新增 `crates/rcoder-storage/src/db/{owner,driver,models,schema}.rs`，
四个 `schema/*-v1.sql`，由独立探针直接引用编译。生产 `lib.rs` 尚未启用，
现有 SQLx/直接 Turso 后端仍在，不能称为迁移完成。

### 实现

- 完整事务队列、取消不取消已受理写、关闭排空、runtime 销毁、线程 join 后完成通知；初始化失败也等待退出。
- 官方 Toasty PG 连接器保留 URL/TLS 语义，未复制实验 NoTls 工厂；本轮真实 PG 测试只配置了非 TLS 临时连接。
- 事务边界错误隔离物理连接；Turso 每连接设置并回读 WAL、FULL、FK。
- UserApp 关系模型及四个基线 DDL。CR10 增加不可变配置版本、配置头、操作配置捕获表，保存与应用不会争写同一凭据载荷。
- schema runner 在 PG 事务迁移锁后读取/创建账本；DDL 与账本同事务。检查 checksum、未来版本、未知组件/旧表及实际 catalog 指纹。
- 普通模型无 Debug/Serialize；共享 PG 凭据的 Debug 改为脱敏，新增嵌套配置测试（该 root workspace 新测试待集中执行）。

### 实际验证

| 检查 | 结果及证据 |
|---|---|
| 独立探针 `cargo nextest run --manifest-path tools/toasty-probe/Cargo.toml --no-fail-fast` | 最后完整轮 9/9，无跳过，退出 0；`/tmp/rcoder-toasty-foundation-complete-tests.log` |
| 真实 PG owner 探针 `RCODER_TOASTY_OWNER_PROBE=1` | 官方连接器，8 并发 CAS 恰好一胜，关闭前锁不能被竞争者取得、关闭后真实取得，退出 0；`/tmp/rcoder-toasty-official-owner-pg.log` |
| 真实 PG schema 探针 `RCODER_TOASTY_SCHEMA_PROBE=1` | 两个独立 owner 并发初始化 UserApp/Project/Preview，各组件只一条账本；模型读写通过；删除 Preview 活跃端口唯一索引后拒绝启动，退出 0；`/tmp/rcoder-toasty-official-schema-pg.log` |

真实 PG 使用新建且只属于本任务的本地临时 PostgreSQL 16 容器；没有连接个人集群或故障现场。
探针第一次 DDL 用例因按分号切分了注释而失败，修正为去掉独立注释行并明确限制基线 SQL 语法后重跑通过；
两次新增代码编译错误（借用类型、Result 错误类型推导）均修复后重跑，不归为基线问题。

### 待继续

T0 仍未整体结束：真实文件跨进程恢复、真实目录锁、断网/未知提交的完整业务恢复、TLS 实测仍未完成。
下一步接入共用 UserApp 事务算法及转换层，随后 Project/Preview/Activity 消费链、依赖清理、CR10 API/执行流程和跨仓库配套。
本轮没有跑完整 Compose、remote K8s、三平台 E2E，没有清理测试集群数据库，也没有提交或发布。

补充：独立探针 Clippy `--all-targets` 退出 0，日志 `/tmp/rcoder-toasty-foundation-clippy.log`；
探针普通 binary 未构造 Turso 策略分支，报告一条 dead_code warning（测试 binary 实际覆盖该分支），未宣称零警告。
探针 fmt 检查及 `git diff --check` 退出 0。临时 PostgreSQL 容器已停止并自动删除。
新增 `userapp_lifecycle/common/codec.rs` 的列/领域转换初稿，检查未知版本、枚举、scope、终态、槽位与整数范围；
尚未挂入生产模块，暂未编译，不计入上述 9 个通过测试。

## 2026-09-19：共用 UserApp 事务、CR10 配置存储、Preview 迁移

基线仍为 `541a93a6073974b51cad5d405fe94d0e70896538` 上未提交实现。新增证据在
[evidence/2026-09-19-common-preview](evidence/2026-09-19-common-preview/)。目录中的 SHA-256 对应本轮实现文件，不代表之后源码改变后的版本也已验证。

### 实现范围

- UserApp 全部现有 trait 方法已用同一套 Toasty 事务代码实现；Turso/PG 导出均指向该实现。旧 UserApp SQLx 实现及宏文件已删除。ProjectStore 等其他 PG 模块仍未迁移，因此**当前整体迁移尚未完成，不应部署这个中间状态**。
- 接入真实目录排他锁；Turso 在打开引擎前验证版本标记，旧开发数据库没有标记时拒绝降级打开。PG 继续用官方连接器保留 URL/TLS 配置，配置预热连接数语义改为启动预热，不声称 Toasty 会持续保留最少空闲连接。
- CR10 私有配置存储实现 save/status/capture/bind/result；保存 revision CAS；重放不能重新提升旧配置；受理同事务捕获版本；自动 traffic wake/EnsureBuilder 只取 applied；显式启动类选 saved；热部署遇 pending 在事务提交前拒绝。结果不明不能被终结为 Failed/Succeeded。
- 配置应用和业务启动分别记账；已应用后业务失败保持 applied；读取私有配置要求当前 lifecycle、slot、executor、fingerprint 一致；绑定物理目标后不可改写。
- 新 save/status HTTP 路由、OpenAPI 和 AppState 装配源码已写入。**尚未在根 workspace 编译，也未执行 HTTP 路由测试**。Java 接口交接在 `../userapp-prod-wake-timeout-pg-credential/java-runtime-configuration-handoff.md`。
- Preview SQLx 改为 Toasty 模型和 owned 事务。固定 PG transaction advisory lock 覆盖端口选择及同 key 首创，部分唯一索引兜底。publish 不能改已分配端口；stop 重放不增加 revision，旧 stop 不能因新实例已 stopped 而被当成成功。
- Preview Unknown 恢复证据增加 instance/revision 绑定；同步修改 Compose 进程内实现及协调器传参。活动 flush 只统计实际推进的时间戳。

### 本轮发现并修复的实际失败

1. Toasty raw SQL 无法推断无类型 NULL：初次共用存储测试有 27 个失败。对 nullable bind 指定 Text/Boolean/Integer 类型后，28 个旧 UserApp 业务契约通过，后来扩展的全集仍通过。
2. Toasty PG 0.10.0 对 raw query 的 `void` 返回值触发驱动 panic。Preview 的 advisory lock 改用 `SELECT 1 FROM pg_advisory_xact_lock(...)`，避免读取 void；锁仍属当前完整事务，未跳过锁。
3. Preview 旧时间戳 flush 原本返回匹配行数，违反共享契约中“只计实际推进”的断言。新增 `< incoming` 条件，保留 GREATEST；没有降低断言。

### 执行证据

独立 `tools/toasty-probe` 直接引用生产源文件，避免每次编译整个 workspace。这里的通过不替代根 workspace/default/all-features/app-cli 的集中检查。

- `cargo nextest run --manifest-path tools/toasty-probe/Cargo.toml --all-features --no-fail-fast --run-ignored all`
  - 配置本轮新建、任务专属可删除 PG 容器的 `RCODER_USERAPP_PG_TEST_DSN`。
  - **退出码 0，57/57，0 skipped**。包含正式 PG UserApp 事务/重连/重放契约、Turso 共用契约、7 个运行配置反例、owner/schema/旧库保护、Compose 进程内 Preview 共享契约。
- `RCODER_TOASTY_COMMON_PG_PROBE=1 ... cargo run --manifest-path tools/toasty-probe/Cargo.toml --features userapp-turso`
  - **退出码 0**。两个独立 PG owner 并发受理、CAS 单胜者、scope 隔离、独立关闭；跨 owner 配置保存竞争、固定版本执行、未知写终态拒绝、业务失败保留 applied。
- `RCODER_TOASTY_PREVIEW_PG_PROBE=1 ... cargo run --manifest-path tools/toasty-probe/Cargo.toml --all-features`
  - **退出码 0**。完整共享 Preview 契约及独立 PG owner 同 key 首创、不同 key 同端口争用。
- PostgreSQL 为本轮新建的本机临时 PG16 容器，随机 loopback 端口，未访问业务数据库或远端集群。它不是 Compose 全套 E2E，不是 K8s 验收。

### 剩余工作（不能据上述通过勾为完成）

- 根 workspace/默认 feature 的编译、Clippy/nextest，app-cli 最终复验，HTTP 路由与 OpenAPI 验证；按用户要求在代码主体完成后集中执行。
- Project/Session/Container、write-behind/leader/sync/load、activity 迁移，删除剩余 SQLx、旧 schema/metadata/import。
- 旧 Turso 0.8 测试夹具仍保留 test-only，需迁移结构/故障反例后连同直接依赖删除；当前依赖图仍不是最终状态。
- CR10 冷切换、PG 管理身份拆分、TCP 校验、管理账号 reset-password 保护、两个 app-cli 引擎/迁移凭据注入、journal 和 wake 结构化诊断尚未接完。当前配置接口存在不代表这些功能完成。
- T0 的真实提交中断/网络断开、完整取消故障矩阵尚需补完；现有 owner 测试和正常 PG 关闭不能替代它们。
- 完整 Compose、remote K8s、三平台、Java 联调及测试库切换均未运行，按约定交后续集成验证。


## Project 存储迁移中间记录（2026-09-19，尚未编译）

本记录不覆盖上轮 57/57 的源码基线；这些修改发生在该轮验证之后。

- 新增 Project/Container/Session 及三类墓碑的 Toasty 模型、严格类型投影和事务内读取。加载检查 project/session/container 的复合代次，不按名称猜测归属；字段解码失败不再静默跳过。
- 排队结构快照捕获 expected_revision；活动 touch 携带代次，单调更新时间而不修改结构 revision。容器登记身份独立于物理 UID，占位绑定和物理换代分开。
- Session 写入移除冗余 container_name；新增 Toasty 事务方法，旧 session 不能跨 project 归属复用，删除同步清理确切归属的 latest_session。批量清理核对 project 代次，缺失行仍记录已捕获 session 代次墓碑，以阻断迟到登记。
- 容器 upsert 使用显式 predecessor 与 revision；同代次已绑定 UID 不允许旧占位清空；换代先退休原登记、解除确切旧代次引用。活动时间不随整行快照回退。
- persist_upsert 在发布任何队列写意图之前构造并验证两个快照，避免 project 构造失败后留下单独的 container 写意图。
- 新增快照身份缺失/UID 不符/重放 revision 不变的测试源码，尚未执行。

### 验证边界与下一步

执行了 `cargo fmt --all` 与 `git diff --check`。未运行新的 Cargo 编译或 nextest；按要求待主体整合后集中验证。

**当前 Project 迁移仍是不可部署的中间状态**：write repo 的 project/delete 方法、writer/durable/leader、load/sync 的连接执行器还存在 SQLx，不能把新增 Toasty 方法当作实际运行链完成。需要继续统一完整事务 owner；处理 CAS Superseded 与重放结果、latest_session 最终一致性、注册表启动加载/同步、同名换代及延迟写反例。随后继续 Activity、旧依赖删除、CR10 实际运行凭据/恢复链和集中检查。


## Project 完整事务接线进展（2026-09-19，待集中编译）

- Project repo 的结构写入/删除已转换 Toasty；项目快照捕获会话身份，成员登记和 latest_session 指针在同一事务内处理。复合容器身份不存在时返回 Superseded，不绑定同名新资源。
- writer 与 durable 共用 execute_registered，在 DatabaseOwner 中执行完整 BEGIN/COMMIT/显式 ROLLBACK。请求取消不会中止已受理数据库事务；故障 fallback 保留原快照。完整性错误识别改为 tokio-postgres SQLSTATE 类型链。
- load/backfill/sync 使用单个 RepeatableRead 只读事务。启动恢复容器登记表；回源校验抓取前后的本地镜像身份，禁止覆盖期间发生的本地变更。恢复完整持久会话集合和选中指针，不盲目合并已退役本地会话。
- leader 改为单独 Toasty owner、单连接持有 session advisory lock；超时/断连/取消后关闭整个专属 owner runtime，再重连。业务连接池不接收选主连接。shutdown 传播实际关闭结果，不无条件宣布成功。
- Project 生产模块已无 SQLx 直接引用；PG 门面暂存的 pool、Activity/旧模块与测试工具尚未移除。新 direct tokio-postgres 依赖仅用于结构化错误分类，仍须同步 Cargo.lock。

这些改动尚未执行编译/nextest，不能沿用此前 57/57。下一轮继续完善删除时容器捕获身份、同步注册表换代/重放结果反例、Project PG 测试工具，迁移 Activity 并删除旧依赖；CR10 实际运行链仍未完成。


## Activity 共用后端与旧 PG 适配器移除（2026-09-19，待集中验证）

- ActivityPersistence 在 ToastyUserAppStore 中实现，两后端共享完整事务；行携带 app/lifecycle/epoch，只持久化 prod 访问时间。锁应用根后校验 Active 和当前代次，时间单调；删除明确携带 lifecycle。
- AppState 在创建 AppService 前加载并注入共用 activity 存储；不再从 ProjectStore 借 SQLx pool。flusher 扩展到已注入的本地/PG 后端，失败重试保留捕获的删除代次。
- 内存收集在身份锁下捕获 lifecycle，较小 epoch 的延迟绑定被拒绝；同名重建不会改写已取出的批次。访问追踪改为异步捕获持久身份，代理/keepalive 调用已 await；此处新增每次访问的根记录读取，后续组件/性能验证需检查该成本，不能用无身份缓存回退规避。
- 回收扫描按 app+lifecycle 合并跨副本活动；存储读取失败跳过扫描，不再忽略其他副本流量。merge_accessed 改为 entry 内原子 max，消除并发读后写的回退窗口。
- 移除旧 pg/userapp（metadata/activity）适配器及不再被业务调用的 AppMetadataPersistence；业务 AppMetadataRecord 暂留供查询投影和待删除 import 接口。旧适配器测试随退役移除，新增共用 Activity PG/Turso 契约与注册表反例；领域 metadata CAS 测试继续保留，尚需最终覆盖核验。
- PgStore 删除 SQLx pool/connection；shutdown 在队列排空后关闭 Toasty owner。旧 Project 测试仍使用 pool()/SQLx，需要下一步转换，当前根测试代码尚非可编译交付。

尚未编译/执行新增测试。继续完成：活动控制写点的代次传递及关机末次 flush、旧 import/直接 Turso 测试迁移、Project 测试、所有依赖图移除 SQLx、CR10 运行凭据及恢复链，然后集中 nextest/Clippy。完整部署 E2E 仍未执行。


## SQLx 完整移除与 PG 测试迁移进展（2026-09-19）

- 删除 UserAppLifecycleStore 的 import_application 及两后端实现；没有生产消费者。身份/分页测试改走 ensure_identity + patch_metadata，保留创建时间不回退、Deleted tombstone 不复活的断言。应用查询过滤测试用真实创建时间而不再借导入接口注入旧行。
- Project PG 测试改为 Toasty owner/实际 SQL 查询：保留重启、跨副本同步、durable、取消、失败关机和物理替换的断言。取消测试的 advisory barrier 在独立 owner 作业内等待释放，连接不逃逸。
- 旧自动 backfill 测试改为“拒绝未版本化 schema 且不改原表”反例，符合首次基线设计。移除 14 个无消费者的旧 PG 初始化 SQL；旧 Turso 测试夹具仍引用最后一个旧 SQL，待其故障测试迁完一起移除。
- Cargo.toml 中 SQLx 依赖/feature 已删除。cargo metadata --offline 首次退出 101：仅缺 redox_users 0.5.3 缓存；随后正常 cargo metadata 下载该依赖并退出 0，Cargo.lock 已刷新。
- 对实际 dependency graph 检查：sqlx/sqlx-core/sqlx-postgres 均为空，Toasty 0.10.0；Turso 同时含正式 0.7.2 和 test-only 0.8.0-pre.11，后者尚未移除。依赖图证据 `/tmp/rcoder-dependency-graph.json`（临时文件；最终验证应保存可重现摘要）。

格式化和 git diff --check 已执行；没有运行 Cargo 编译或测试。新 PG 测试的实际通过、旧 Turso 故障反例迁移、Project/C10 剩余语义及集中组件验证仍未完成，整体仍不可部署。

## 2026-09-19：UserApp 根控制版本 CAS 与事务等待保护

本轮为用户追加的并发控制要求，保留现有迁移工作树，不提交、不发布、不操作外部集群。

### 实现

- UserApp common/repo 不再使用 SELECT FOR UPDATE；新增存储内部 control_revision，通过 lifecycle/state/revision 条件 UPDATE 建立受理、删除、重建共同竞争点。
- slots 写入校验三个旧槽位；旧操作不能以过期快照释放新操作。
- 已回滚 VersionConflict 最多完整重试三次，原请求身份/参数不变；commit、连接、rollback 结果异常不自动重试。
- PolicyDriver 每条 PG 连接设置 lock_timeout（最大 2s）和 idle_in_transaction_session_timeout（最大 30s），保留 statement_timeout 和事务边界失败连接隔离。
- 保留身份复合外键，无新增级联删除/更新。原因与适用边界见 userapp-concurrency.md。
- 修复前一轮尚未编译的 Activity 错误构造和身份分页测试变量；更新已删除根的测试为 ensure 拒绝、get 读取墓碑，而不是自动恢复。

### 实际验证

使用独立 probe 工程直接编译包含的生产 UserApp common、models、driver、owner 源码，复用既有临时 CARGO_TARGET_DIR。不是 rcoder 完整 workspace 编译。

1. 首次 nextest 未设置复用 target，及时终止该次编译（143），不是测试通过；之后统一复用原 target。
2. 首次复用 target 编译 101：发现上一轮未验证改动的 trait re-export、旧测试变量、Activity error 构造问题，修正后重跑。
3. `cargo nextest run --manifest-path tools/toasty-probe/Cargo.toml --all-features --no-fail-fast`：退出 100；58 个执行，57 通过、1 失败、3 PG 用例未显式启用。失败定位为已删除应用 ensure_identity 测试预期错误，保留拒绝保护并修正查询方式。
4. 独立临时 PostgreSQL 16，两个 DatabaseOwner 各限制一个物理连接：显式执行 independent_pg_owners 测试，退出 0。验证 dev/prod 双槽位保留、删除/受理单胜者、失败方无 operation、锁超时后原请求重试、idle 超时后池恢复。
5. `cargo nextest run --manifest-path tools/toasty-probe/Cargo.toml --all-features --no-fail-fast --run-ignored all -E 'test(pg_) | test(postgres_real) | test(turso_paginated)'`：退出 0；5/5。包含失败分页用例复跑、PG 完整生命周期契约、活动数据契约、新的双连接竞争测试以及命中筛选的配置未知结果测试；56 个未选中不能计入通过。

证据：`evidence/2026-09-19-userapp-cas/`。PG 由本轮新建的独立本地容器提供，验证后删除，仅操作本轮自建资源；真实环境密码未写入文档。

### 验证边界

没有单独运行修复前反例，不能称为已取得红绿双证据。已有 PG 契约包含重建、幂等、执行者 CAS、租约与未知结果保护，但尚需进一步加入可控门闸的删除完成/重建/旧代次迟到写竞争和 CAS 冲突整笔回滚反例。

本次没有完成整个 workspace、Project/Preview 受连接超时策略影响的全量回归、Compose、remote K8s、三平台验收。按本轮用户要求，大型迁移仍在开发阶段，集中检查安排保留；本报告不能替代整个数据库迁移和 CR10 的完成验收。

补充检查：`cargo fmt --all -- --check` 与 `git diff --check` 均退出 0。probe 的 `cargo clippy --all-targets --all-features` 退出 0，但仍有 probe 未使用代码及现有 common 代码的 collapsible_if/clone_on_copy 等告警，不宣称 Clippy 零告警或根 workspace 已通过。

## 2026-09-19：Activity 装配、身份缓存与最终 flush（实现阶段）

修正前轮偏离 schema-design §4.9 的两处实现：Compose 不再加载/注入 Activity 持久化；HTTP touch 不再每次直接查询数据库，改为有 TTL、按失效版本隔离、并发 miss 合流的身份缓存。缓存失败仍可重试，并返回未记录状态，keepalive 不伪造时间。

补充 generation-bound peer merge、lifecycle-bound 删除完成清理、周期/关机统一 flush。最终 flush 位于 HTTP/代理/background/recovery/operation 全部排空之后、控制存储关闭之前；失败中止成功关闭报告。

新增反例：迟到身份查询不能恢复已删除活动、旧 PG 快照不能写入新 lifecycle、写入/删除失败保留待重试项、旧删除完成不能清掉新 lifecycle。测试已编写，尚未执行；按用户要求留到相关改动收敛后集中编译与 nextest。未启动 Compose/K8s/三平台验证，未提交或发布。

Activity 本轮静态检查：格式检查曾发现两处新代码排版，执行 fmt 修正后 `cargo fmt --all -- --check` 退出 0；`git diff --check` 退出 0。没有把格式检查当作编译或反例通过。

## 2026-09-19：移除预发布 Turso 测试后端与旧 SQL

- 原 `userapp_lifecycle/turso/` 中的 worker/格式/恢复/事务反例迁至生产 `db/tests.rs` 与 `common/local_tests.rs`，探针用 path 引入同一份源码。映射见 test-migration-map.md。
- 修复 owner catch_unwind 后只通知单次调用、但关闭可能错误报告成功的问题：任务 panic 返回 OutcomeUnknown，同时停止新任务受理、排空已接收任务，关闭持久报告失败；独立 worker 线程 panic 也保留共享失败结果。
- 删除旧 Turso 实现六个文件和仅供其测试使用的预发布依赖；删除最后一个旧 migrations-userapp-turso SQL。`rcoder-storage` 当前恰有四个 SQL 基线。
- `cargo metadata --offline --format-version 1` 退出 0：完整包清单无 sqlx 系列包，Toasty=0.10.0、Turso=0.7.2，旧 0.8.0-pre.11 不再解析。Cargo.lock 已更新。
- `CARGO_TARGET_DIR=<既有探针缓存> cargo nextest run --manifest-path tools/toasty-probe/Cargo.toml --all-features --no-fail-fast` 退出 0：72 执行、72 通过、3 PG 用例未启用。包含新增 panic/队列/损坏恢复/跨表回滚反例，以及迁移前已存在的共用生命周期与配置契约。
- 对 rcoder crates/tools/tests-e2e/make 及 build-agent-docker 的执行代码检索未发现旧迁移目录或 SQLx 消费者；历史文档允许保留原基线说明。

证据：evidence/2026-09-19-owner-tests/nextest.log、dependency-summary.json。此证据不代表根 workspace、ProjectStore 或 Activity 消费方已编译通过，也不替代 PG/Compose/K8s/三平台验证。未提交、未发布。

owner 收尾补充：任务 panic 在通知调用方之前同步关闭 Shared admission，消除“调用方已收到失败、owner 尚未观察 task completion 时仍能排入新写”的间隙；通过 Weak 引用避免 queued job 与 owner 的强引用环。针对 task/worker panic、取消 shutdown waiter、last-owner drop 的 4 个用例聚焦复跑，4/4、退出 0，日志 panic-rerun.log。`git diff --check` 退出 0。

## 2026-09-19：ProjectStore 删除代次与跨副本镜像收敛（实现阶段）

- 排队容器删除固定受理时的容器名、注册代次和物理 UID，项目删除同时核对项目 generation；不再仅凭执行时 UID 搜索授权删除对象。
- peer 同步把容器注册游标与项目镜像一同更新；快照读取期间出现本地注册意图时跳过关联项目，避免镜像与后续写入游标跨代次。注册游标仅在内存项目插入成功后发布。
- session 删除增加所属项目 generation 比较，旧项目快照不能仅凭相同 session 标识清理新项目。
- 扩充真实 PG 契约：同 UID 但错误容器名/注册代次的两种删除命令均 Superseded，项目/容器保留且无墓碑；另一副本载入旧代次后同步换代，项目与注册游标须同时为新代次。
- 新增断言尚未编译运行；格式检查初次只发现新增测试排版，已 cargo fmt --all 修正，git diff --check 退出 0。按用户要求暂不重复整体构建，留待 ProjectStore 剩余语义收敛后一并运行 PG 组件测试。不能把旧探针 72/72 当作本次 ProjectStore 证据。未运行部署验证，未提交发布。

## 2026-09-19：容器换代前驱 revision 保护（实现阶段）

- ContainerPersistenceIdentity 和队列 ContainerSnapshot 增加 predecessor_revision；替换时固定前驱 generation + revision，数据库端在退休前验证二者，注册游标应用相同条件。初建/数据库 hydration 不携带前驱，换代请求缺失前驱 revision 明确拒绝。字段属于运行时写入意图，不增加数据库历史前驱列。
- 扩充容器生命周期真实 PG 契约：先推进旧容器 revision，再提交旧快照捕获的替换，必须 Superseded，旧 service_url 保留且无墓碑；重读后的正确 revision 可以换代，peer 同步项目及注册游标。
- 条件删除使用 container_entry_key，与入库键完全一致，避免空 container_name 的回退键被写入和删除两条链解释不同。
- 修正上一轮新增测试对 Option<ContainerBasicInfo> 的字段访问。cargo fmt --all -- --check 退出 0；git diff --check 退出 0。新增 PG 断言仍未编译执行，集中验证待做，不宣称反例已通过。
- 尚需继续处理：ProjectStore 提交后取消的准确重放结果、结构性关联写入的整组原子性、CR10 运行时凭据生效链；本轮不视为全部完成。

## 2026-09-19：ProjectStore 注册整组原子性（实现阶段）

- 生产注册统一为 RegisterProject 一个队列命令，携带容器、项目、会话快照。普通 insert、带 session insert 和 durable 路径共用构造器；降级入队和批失败隔离均不能拆开容器/项目。移除生产独立 UpsertProject/UpsertContainer 旁路（后者仅保留测试注入）。
- 命令内建立 PG savepoint，任一条件写 Superseded 时回滚完整注册；SQL 错误仍向上返回，由完整事务回滚。项目写入不再忽略 add_session 的归属冲突。
- 新增单队列命令测试、项目 CAS 失败回滚先行容器更新的 PG 断言、会话被其他项目占用时回滚新项目的 PG 反例。此前只有顺序入队，无法保证批边界/拆单重试时一致提交。
- cargo fmt --all 与 git diff --check 退出 0；新增反例未编译运行，PG/根 workspace 集中验证尚未完成。未提交发布。事务提交后重放的准确结果仍待完善，未将本项实现等同于全部存储交付。

## 2026-09-19：CR10 app-cli 编排凭据贯通（实现阶段）

- server 将取出的每操作 PG 凭据同时传入 supervisord/builtin；各引擎编排开始时解析一次运行凭据，显式操作优先于容器环境，容器环境优先于制品默认值；环境只配置用户名或密码时明确拒绝。
- 两引擎迁移命令、业务服务启动、PG availability 探测使用捕获的凭据。迁移命令补齐 manifest env，使用与服务相同的凭据覆盖函数；supervisord 服务 spec 序列化该配置，自动拉起不依赖已消费的请求。
- supervisord 与 builtin 共用是否需要 PG 的声明判定，纯前端不再无条件等待 PG。pg_isready 只表示服务器可用，不是密码验证，真实 TCP 登录与凭据 applied 状态提交仍待平台应用链实现。
- 新增配置覆盖/spec roundtrip 组件测试及 Unix 真迁移子进程环境反例；未执行，尚无测试通过结论。独立 app-cli cargo fmt 与 git diff --check 退出 0。未操作 PG/集群、未发布。
- 本轮未完成 CR10 的容器换代、管理员通道、TCP 验证、journal/wake 整链及 Java 联调；继续实施，不能将 env 贯通视为全部交付。

## 2026-09-19：CR10 改密副作用证据（实现阶段）

- shared_types AlignError 增加 CredentialMutationEvidence：NotAttempted、Unknown、AppliedButUnverified。验证/角色查询失败为本次未派发改密；ALTER 派发后的错误保守保留未知；ALTER 明确成功后复验失败单独表示已改密但未通过 TCP 复验。调用方不需要匹配 stage 字符串判定副作用。
- AlignCredentialsRequest 自定义 Debug 隐去密码。新增六种执行失败时序断言与 Debug 脱敏断言。
- 此字段当前仅由共享执行器生产，AppService 既有字符串错误映射尚未完成升级；必须在后续运行配置执行链中消费该证据、持久记录并控制终态，不能声称生命周期闭环已完成。
- cargo fmt --all 与 git diff --check 退出 0；新增断言未运行，未操作现场或发布。

## 2026-09-19：CR10 平台错误传播与恢复保护（实现阶段）

- AppOperationError 增加 CredentialApplication，保留 CredentialMutationEvidence；转换时隐藏明文密码，未知/已改密待复验映射 ERR_RECOVERY_REQUIRED。align_db_credentials 不再把这些证据压成 Backend 字符串。
- deploy_controlled 后置 align 失败直接返回错误，不再生成 pg_aligned=false 的成功结果；完整版本化冷切换仍待接入，未宣称该后置顺序已符合最终 CR10 方案。
- OwnedOperation::fail 对凭据不确定性强制 RecoveryRequired，即使 checkpoint 仍为空；reject_without_mutation 遇到这类证据转入 fail，不能旁路 Failed 解锁。
- 新增真实存储驱动的 OwnedOperation 组件反例（未知/已改密待复验两种情况保持操作槽位）及错误脱敏/类型保留测试；尚未执行。cargo fmt --all、git diff --check 退出 0。未部署、未提交发布。
- 下一步仍需接入配置 store、物理 target 绑定和受控换代；当前 exec(app_id) 不是已验证的代次绑定管理通道，不能用于宣称完整新链路完成。

## 2026-09-19：CR10 AppService 配置装配与热部署入口补漏

- AppService 构造显式接收 UserAppRuntimeConfigurationStore，与 lifecycle store 一起由启动工厂注入同一后端；更新全部构造及测试 fixture，不引入可选内存回退。
- StartDeployment/RestartDeployment 的 deploy_mode=hot 入口在容器副作用前核验该操作已捕获配置与 applied_version，一旦不同返回 HotDeployEnvChange，避免只保护独立 HotDeploy kind。读的是原操作 capture，而非保存头的最新密码。
- 新增存储组件反例：保存 v1→受理 StartDeployment→再保存 v2，操作仍固定 v1，热部署要求冷切换且无 target/凭据应用副作用。测试尚未执行；不视为完整 HTTP/运行时 E2E。
- cargo fmt --all、git diff --check 退出 0。冷部署应用、物理目标绑定及完整凭据恢复仍待接入；本轮未提交发布。

## 2026-09-19：CR10 独立 PG 管理通道与 TCP 验证命令

- 核对 rcoder/docker 与 build-agent-docker/build_config 的 PG 脚本：当前初始化/管理均复用 POSTGRES_USER，尚未改镜像。不能直接覆盖该变量后声称旧 PGDATA 管理身份已处理。
- shared_types 新增 PgAdministrationTarget（显式管理员用户名、本地绝对 socket 目录）与 align_pg_credentials_with_admin；角色检查/ALTER 使用此管理账号，前后验证仍使用业务账号 TCP。现有 align 入口保留至平台捕获管理员链完成，未将不完整接线部署出去。
- TCP/显式管理命令设置 PGCONNECT_TIMEOUT 与 statement_timeout，使用 psql -X -w/ON_ERROR_STOP，不读取 .psqlrc、不交互等密码；清除 PGHOSTADDR/PGSERVICE，避免继承环境把显式管理路径改到其他主机/服务。
- 新增管理员与业务命令分流/完整对齐过程测试。cargo fmt --all、git diff --check 退出 0；测试未执行，build-agent-docker 未修改。管理员身份持久捕获、镜像启动脚本接入、目标绑定和冷切换仍待完成。

## 2026-09-19：CR10 PGDATA 初始化管理员记录（跨仓实现）

- rcoder/docker 与 build-agent-docker/build_config 中 app-runtime-base、rcoder-agent-runner 四个入口新增同一 pg-admin-identity.sh，Dockerfile 打包该文件。PGDATA/.rcoder-admin-user 仅存用户名，mktemp 0600 后用原子 link 发布，不覆盖已有记录。
- 已记录的管理员优先于业务 POSTGRES_USER；显式 APP_PG_ADMIN_USER 与记录不同、空/损坏/非法记录均拒绝。首次 initdb 使用独立管理员变量，成功后记录。旧目录缺记录时仅在本地 socket 查询确认候选账号是 superuser 后后台补录，不建号、不改密、不猜测其他角色。超过验证窗口保持无记录并明确输出未验证。
- 验证：python3 tools/test_pg_admin_identity.py 退出 0，3 测试分别对两份 helper 验证重启改业务账号仍保留管理员、冲突/损坏拒绝、迟到记录不能覆盖；检查 0600 且目录无密码文件。四份 helper/入口 sh -n 通过，两仓对应脚本逐字节一致；两仓 git diff --check 退出 0。
- 这是 shell 身份协议验证，不是真 PG 初始化/旧卷升级/镜像构建或部署验证。尚需受控冷切换读取并验证管理员记录、账号应用以及跨节点集成验证；未发布或提交。

## 2026-09-19：CR10 K8s exec 不再把未知结果当成功

- 源码确认原 app_exec 在 Status 缺失/通道结束时返回 exit 0，stdout/stderr 读错和 join 错误也仅记日志。改密链可能据此把未知 ALTER/复验当成功。
- 改为仅明确 Success 接受 0，Failure/NonZeroExitCode 且有合法非零退出码才返回对应码；缺状态、畸形状态、读输出失败、join 失败返回执行错误，保留结果未知语义。
- 核对锁定 kube-client 4.2.0 源码的 stdout/stderr/take_status 返回拥有型 pipe/future（use<>），改为并发排空双流与消费状态，避免顺序读导致 stderr 反压死锁。
- 新增退出状态反例（缺失、Failure、非法退出码、矛盾状态）和缺失/无效 UTF-8 输出错误用例。cargo fmt --all 与 git diff --check 退出 0；用例未编译执行，K8s 组件/部署验证待集中运行。
- exec 的物理目标绑定尚未完成；本次修正真实传输错误语义，不把 label/name 定位当作 UID 授权，后续仍需独立接入管理目标协议。未部署、未提交发布。

## 2026-09-19：CR10 Docker 物理执行与配置应用执行器

- UserAppDeploymentRuntime 新增 exec_app_configuration_target，不支持的后端明确拒绝且不回退 app_id/name。Docker 实现先按捕获物理 ID inspect，验证应用/生命周期/资源族及唯一 APP_DEPLOY_GENERATION_ID，再按同一 immutable ID create_exec，防 inspect 后同名替换。
- Docker exec 共用传输函数，退出前未明确 Running=false 或缺退出码时返回未知错误，不将 -1 包成普通完成。新增物理 UID/生命周期/dev 资源族/缺失或重复代次字段反例，尚未执行。
- 新增 AppService::apply_captured_runtime_credentials 内部执行器：读取原操作 capture、绑定目标、持久 Applying、按独立管理员执行对齐、分别记录 Applied/Failed/Unknown。未知及 Applying 记录拒绝自动重新执行；状态落盘失败转类型化未知，保留恢复保护。
- 该执行器还未接入冷部署主流程，K8s 对应绑定执行未实现；因此未宣称完整配置已生效。后续必须完成旧业务停止/管理通道捕获/镜像代次/PG Secret 的真实编排，再集中验证。cargo fmt --all、git diff --check 退出 0，未编译测试、未提交部署发布。

## 2026-09-19：K8s 管理 exec、Project 写回执与 E2E 清单修正（尚未集中编译）

- K8s 增加配置目标 exec：校验当前 lifecycle 对应 Deployment、捕获的 Pod UID、Pod→ReplicaSet→Deployment 的 UID 控制链；直接注入 downward API 的 `RCODER_PHYSICAL_POD_UID`，容器内再核对 UID 和 `APP_DEPLOY_GENERATION_ID` 后执行。避免仅 GET 校验后仍按可复用 Pod 名 exec 的竞态。不要求业务 Ready。Docker 已使用不可变容器 ID。
- 添加真实 shell 子进程反例：UID/代次不匹配不得运行写文件载荷；命令参数保留原字面值。该 Rust 用例已写入，尚未编译执行。K8s exec Status 的重复/缺失 ExitCode 原因不得被过滤成一个“有效”退出码。
- Project 注册增加同事务 `project_write_receipts`：队列注册时固定 request_id，容器/项目快照生成摘要；回执与注册或拒绝结果一起提交。提交回包丢失后重试返回原结果；同 ID 不同输入拒绝；旧回执不会重放已经退休的实体。新增回执表属于尚未发布的 project-pg-v1 基线，初始化 SQL 仍为四个。回执不写明文模型配置，暂不自动清理。
- Project 生命周期反例补充相同请求重试不增加 revision、输入变化拒绝、删除后原请求重放不复活。调整旧用例的回执结果断言为原 Committed，保留项目/session 不存在断言，不能把回执成功解释为当前资源仍存在。
- E2E 实测前检查发现旧模块路径/旧库自动导入测试名已失效：改为 Toasty 公共后端/owner 测试，并保留事务回滚、checksum、关闭、取消、损坏隔离与旧库保护覆盖。PG 套件增加注册原子性和回执用例。执行目录仍是固定白名单，未改成自动发现后“有什么测什么”。
- 已执行：`python3 -m unittest discover -s tests-e2e/tools -p 'test_storage_contract_cases.py'`，3/3，退出 0；`cargo fmt --all` 退出 0；`git diff --check` 退出 0。不是 Rust 组件或部署通过证据。

### 最新交付顺序（用户追加授权）

完成全部实现后集中组件验证；阅读并使用 `make dev-restart` / `make dev-hot`。后者只更新 RCoder，不更新 app-cli/PG 脚本；本轮需构建 agent-runner 与 `make docker-build-app-runtime`，再执行真实 Compose E2E。Compose 通过后精确暂存本任务改动、commit/push RCoder；随后在 build-agent-docker 执行 `make setup k8s-helm-rcoder-version-publish ENV=test AMD64_ONLY=1`。当前尚未执行容器构建、全套 E2E、提交、推送或发布。

CR10 冷切换主链、代次 Secret、管理通道启动屏障、凭据应用与业务结果持久状态的完整接线仍需继续；本节不得视为 CR10 完成。

### E2E 工具预检结果

执行 `python3 -m unittest discover -s tests-e2e/tools -p 'test_*.py'`：初轮 90 项中 3 错误，定位为 Docker aggregate Make 测试 fixture 缺少已新增的 Pingap 版本门禁脚本，尚未到达两个模拟构建子目标；没有启动 Docker。修正 fixture 后新增“门禁失败不启动任何镜像构建”断言，再执行 91/91，退出 0。日志：`evidence/2026-09-19-e2e-catalog/python-tools.log`。这是启动器/契约工具单测，不是完整 test-e2e 通过。

另外修正凭据错误传播：含密码的 exec 命令出现传输或 SQL 错误时，原始错误可能回显 shell/SQL 转义后的密码，纯明文 replace 无法可靠遮蔽。此类步骤只传播阶段、退出码和 typed mutation evidence，不把原始敏感命令诊断写入公开错误。补转义密码反例，尚待集中 Rust 测试。

## 2026-09-20：CR10 管理先行启动门控（接线继续中）

- app-cli 新增受管理配置启动门控，仅在 `APP_RUNTIME_CONFIGURATION_VERSION` 明确配置时启用；要求完整部署操作/代次以及启用部署令牌通道，普通本地 app-cli 使用保持原启动流程。
- 管理 listener、所有权和内核装配先完成；指定配置的激活回执出现前，自动部署/迁移/业务启动不执行，ready 保持 initializing。新增受部署令牌保护的 `/v1/runtime/configuration/activate`，绑定 operation_id、generation、config_version，持久化并读回后确认。与关闭受理共用锁；恢复保护或未完成内核初始化时拒绝。receipt 无密码，旧代次/损坏数据不能自动开闸。
- 补 app-cli 门控反例：无回执阻塞、错误身份拒绝、重复确认幂等、进程重开复用同代次已生效回执、新代次不能复用、损坏/取消不能启动。这些 Rust 测试尚未运行。
- 平台增加管理目标捕获：Docker 固定 container ID；K8s 只选择本操作模板的唯一 running 管理容器，校验 UID 控制链并执行只读代次探针，不等业务 Ready。目标 exec 仍采用此前的不可变 UID/容器内代次双检。
- 平台补 PG 管理就绪等待与激活确认方法：从 PGDATA 已核验的管理员标记读身份，使用管理 socket 验证；以调用方总 deadline 为界。PG 已应用后才允许提交业务激活；HTTP 确认未知时保留 Applied + business Unknown，不回退数据库密码。
- 修复一处状态转换漏项：Applying 是提交前的持久意图，执行器证明 NotAttempted 时可转 Failed；Unknown 仍不能转 Failed。补数据库反例，确认不提升 applied_version、释放已确认失败的操作后可重新受理。
- 尚未集中编译/组件测试。本节新增管理能力及 helper 不代表冷部署主流程已接通；部署参数、受控停旧/换代、最终业务状态采集及恢复接线继续开发。Compose/提交/推送/发布尚未执行。

## 2026-09-20：受管理冷部署和显式 start/restart 主链接线（未集中验证）

- `execute_deploy_input` 读取受理时固定的配置，拒绝请求 pg 与捕获版本不一致；受管理冷部署强制构造新容器参数。清除旧业务账号 env，将指定账号密码写入该代次 secrets（Docker 合并为容器 env），写入操作/代次/配置版本，确保管理令牌可用；旧 secrets 不能覆盖平台代次。无制品 URL 的启动去掉旧下载种子，使用已确认 workspace。
- 已接入管理容器捕获、PG 管理就绪、凭据对齐/TCP 验证、持久 Applied、app-cli 激活回执、Running readiness 与持久 Ready。沿用受理时绝对 deadline，超时/确认未知保持原身份和恢复保护；凭据已应用不改回旧密码。
- 显式 `start_app_controlled` / `restart_app_controlled` 存在捕获配置时进入受控替换链；原有无配置操作保持既有语义。按原操作绑定一次绝对 deadline，不为重试刷新。
- 新增通过真实 AppService/Store、脚本化运行时的调用链反例：保存 v1→受理→保存 v2→容器参数仍是 v1→先 PG 验证后激活→成功后 applied=1/saved=2/pending=true。也断言旧密码 env/旧下载 URL 被清除、整个管理链持有运行时租约。此为组件测试，未执行，不代表容器内真实 PG 验证。
- 业务失败观察从同一物理目标的管理 API 读取，确认 phase=failed 后记录业务 Failed；未取得清理证据仍不释放恢复保护。Idle 基础设施 ready 不能当作业务 Running。
- 待继续：相同已生效配置的热部署与自动 wake/恢复接线；旧 journal 的结构化迁移/失败恢复；管理账号改密入口约束；集中编译与全部验证。当前实现不构成全任务完成，尚未 commit/push/发布。


## 2026-09-20：已生效版本观察与 Secret 读回（尚未集中编译）

本轮继续接线，不代表 CR10 或整体迁移完成：

- 流量唤醒绑定操作租约，并按已生效配置捕获当前物理代次；只执行 TCP 凭据验证与业务就绪观察，不执行改密或激活另一代次。已运行实例同样经过生命周期受理。恢复路径同步调用该观察流程。
- 受管理热部署使用已生效版本；热协议不可用时明确要求冷部署，不静默退回冷切换。业务观察与冷配置应用共用有界就绪检查。
- K8s Secret 的 ByteString 已完成 wire Base64 解码，读回改为 UTF-8 转换，消除二次解码；非法 UTF-8 明确失败，不静默丢弃整个 secrets 集合。
- 新增两个 Secret 反例：合法明文恰好符合 Base64 字符集仍原样保留；单项非法 UTF-8 不能导致 secrets 消失。
- 冷部署测试增加完整连续场景：版本 1 被操作捕获，版本 2 随后保存；部署激活版本 1 后，流量唤醒保持同一物理代次，仅验证版本 1，版本 2 仍 pending。Mock 记录实际捕获 target，执行时必须使用已捕获身份。

验证：`cargo fmt --all` 退出 0；`git diff --check` 退出 0（其后增加 target 断言，最终格式化另行核查）。本轮没有运行 Rust 编译、nextest、Docker/Compose 或 K8s；新增反例均未执行。

继续处理的缺口：reset-password 仍有绕过运行配置的直接改密路径，不能用“查询配置后再改密”作为并发修复；需与配置保存及生命周期受理共用原子竞争边界。PG 初始化还存在临时启动/建库错误被吞与管理账号、业务账号分离不完整的问题；journal 的确认失败与迁移未知恢复仍需继续实现。上述未完成项不能因本轮接线而标记完成。


## 2026-09-20：PG 业务库初始化不再吞错

app-runtime 与 agent-runner（RCoder 开发镜像和 build-agent-docker 发布镜像同步）统一使用 `pg_bootstrap_database`。每次启动验证实际 superuser、确认身份标记、查询业务库；缺失时创建并再次确认。createdb 非零仅在复查库存在时接受为幂等竞争，否则停止 supervisor 跟踪的 postgres PID。app-runtime 不再临时 pg_ctl 启停后无条件忽略错误；agent-runner 不再把任意 createdb 失败都当成“已存在”。目录权限修复失败也明确退出。

证据：`python3 tools/test_pg_admin_identity.py`：6/6，退出 0；包含缺库创建失败、库已存在不创建、创建成功、并发创建后非零但确认存在。两仓四入口及四 helper 的 `sh -n` 均退出 0。这些是 shell 协议测试，不代表实际 PostgreSQL 启动/镜像/Compose 验收。角色分离与业务账号授权仍需继续完善，未标记 CR10 完成。


## 2026-09-20：捕获管理员下的业务账号创建

CR10 受管理配置应用现在允许通过显式 `PgAdministrationTarget` 创建缺失的 LOGIN 业务角色，然后以目标密码 TCP 复验；传统无管理员参数的 align 入口保持“仅重置已有账号”的原契约。创建与改密共享持久 Applying 意图及 Unknown 结果保护，不解析错误字符串，不在错误消息中保留可能回显密码的远端输出。

新增未执行的 Rust 反例：捕获管理员创建账号再 TCP 验证；建号传输断连保留 Unknown 且错误文本不泄露密码。`cargo fmt --all` 退出 0。本轮尚未集中编译/nextest。账号创建不代表权限配置已经完成；新管理员默认值、业务数据库/schema 权限和旧数据库账号切换仍需继续完成并通过真实 PG 验证。


## 2026-09-20：业务库就绪及 DATABASE_URL 凭据覆盖

核对模板实际连接实现后，补充两处遗漏：

- 配置应用的 PG 前置检查通过捕获的管理员和本地 socket 连接 `POSTGRES_DB`，不再仅连接始终存在的 postgres 库。这样不会越过镜像后台 createdb 尚未完成的窗口，且依然不依赖业务账号或业务 Ready。
- app-cli 服务环境统一处理已有 `DATABASE_URL`（服务声明优先，否则读取继承环境）：保留数据库路径、host、port 和 query，以 URL API 替换账号密码并编码特殊字符。该结果同时用于迁移、builtin 子进程和 supervisord 持久 service spec。配置了运行凭据但连接串非法时明确失败，错误不回显原始连接串。两个引擎在迁移和启动前预检全部服务环境。

新增未执行反例覆盖数据库就绪检查的目标账号/库，URL 特殊字符和原有连接参数保留，以及运行配置覆盖制品旧 URL。格式化已执行；Rust 测试、真实 PG 和 Compose 尚未执行。本轮没有声称数据库权限迁移已完成：新业务账号的数据库/schema/旧对象权限还需继续实现与实测。


## 2026-09-20：迁移执行凭证与确认启动失败

新增 app-cli `migration_journal`，以不可变 release 的规范化摘要和 service 身份定位凭证。凭证只保存摘要与完成标志，不保存命令环境或密码。两个编排引擎共用执行包装：排他取得迁移文件锁、持久未完成意图、执行并确认进程树退出、持久完成。相同制品已完成迁移恢复时跳过；未完成/损坏记录阻止同一或新制品自动迁移。启动恢复和失败收束均核查该记录；结果未知保留 runtime recovery hold，不伪造 Failed 终局。

部署 journal 增加 `startup_failed`：仅在旧进程清理完成、迁移记录无未知、已持久 Activated/Active 边界且卷上制品身份匹配时记录。保留确认制品供显式操作重试；服务重启读取此记录时保持 Failed 等待显式操作，不自动启动。旧 Failed/Switching/Activated 记录仍保留原恢复保护，不按错误文字猜测。

新增未执行单测覆盖迁移完成后重开跳过、执行锁互斥、中断后同/新制品均受阻、损坏记录拒绝，以及 startup_failed 保留制品而旧 failed 不自动恢复。独立 app-cli fmt 退出 0，尚未编译或运行 Rust 测试。未知迁移的人工结果核验/恢复操作、服务级恢复 E2E 与旧记录场景仍需继续核对；不能以本轮源码存在宣布完整验收。


## 2026-09-20：集中编译接线修复与依赖分配器冲突

已执行：

- `cargo check --manifest-path crates/app-cli/Cargo.toml --all-targets`：退出 0。
- app-cli 聚焦 nextest（migration_journal/configuration_gate/运行凭据环境）：8/8 通过，231 个未选中；退出 0。原筛选项 `server_journal` 未命中嵌套 `server::journal` 模块，不把 journal 单测算作已验证。
- `cargo check --workspace --all-targets --all-features`：前四次退出 101，修复后第五次退出 0（45.45s）。完整日志位于 `evidence/2026-09-20-concentrated-check/`。存储模块仍有 10 条测试档告警，尚未宣布 Clippy 全绿。

编译暴露并修复：SHA-256 新返回类型不支持整体 LowerHex（改为逐字节稳定编码）；Project 查询测试仍使用旧连接签名；删除成功消费操作后又访问操作身份（改为成功提交前保存原 lifecycle）；测试 AppService 漏传 runtime_configuration；E2E 离线观察器引用已删除旧模块。

离线观察器改用 Toasty owner，持有实例目录锁直到关闭；只校验已存在 schema，绝不建库、迁移或执行 Running 隔离。新增“在线时拒绝观察、关闭后观察保留 Running、重复观察不改变状态、缺库不创建”的真实本地存储反例，尚待本轮 nextest 结果。

依赖组合发现：官方 toasty-driver-turso 0.10.0 启用 Turso 默认 mimalloc 全局分配器，与项目 hotpath-alloc 的全局分配器冲突。根 `[patch.crates-io]` 引入 `vendor/toasty-driver-turso`，源码逐字节保持发布包，仅 Cargo 依赖关闭 Turso default-features 并保留默认 fts。说明及上游来源见其 RCODER-PATCH.md。不通过禁用现有诊断 feature 绕过问题；根全 features 编译已验证该组合。

当前启动的根 workspace 聚焦 nextest 涉及 rcoder-storage/app_manager/docker_manager/shared_types，过滤配置、离线观察、冷部署和 Secret 场景；结果尚待收集。没有启动 Compose、K8s 或发布，也没有提交/推送。


## 2026-09-20：按用户决定撤销 allocator 本地补丁

用户明确 hotpath-alloc 仅用于开发评估，遇到冲突可不用。因此撤销前一节的驱动本地补丁，删除本轮新增 vendor/toasty-driver-turso，根 Cargo patch 移除，锁文件恢复带 crates.io source/checksum 的官方 toasty-driver-turso 0.10.0；Turso 保留官方默认 mimalloc/fts。rcoder 和 agent_runner 同时删除 hotpath-alloc feature（避免 workspace feature 合并再次启用），保留 hotpath/hotpath-mcp，并更新 Make 示例和 AGENTS.md。旧 feature 不做静默空实现。

此前带补丁的聚焦测试已结束：24 例中 23 通过、1 失败，退出 100；失败是冷部署配置反例在 MockRuntime 捕获目标时未持有模拟租约，待核查是测试装配还是实际调用链缺失，未降低断言。日志为 evidence/2026-09-20-concentrated-check/focused-before-allocator-decision.log。此结果属于补丁撤销前基线，不作为官方依赖组合的最终验证。

`cargo metadata --offline --format-version 1` 退出 0，锁文件已恢复官方驱动及 mimalloc 依赖。官方组合的全 features 编译另行复核，整体任务仍在实施。

官方依赖组合复核：`cargo check --workspace --all-targets --all-features` 退出 0；日志见 `evidence/2026-09-20-concentrated-check/official-toasty-check.log`。全局分配器冲突消除，存储模块既有本轮告警仍待清理。

## 2026-09-20 官方依赖聚焦回归补验

- 修正 cold_deployment 凭据测试 fixture：显式使用 K8s runtime lease 路径，与 MockRuntime 的持租约断言一致。没有移除租约断言，Docker 文件锁路径需单独验收。
- 命令：`cargo nextest run -p rcoder-storage -p app_manager -p docker_manager -p shared_types --all-features --no-fail-fast -E 'test(configuration) | test(cold_deployment) | test(offline_observer) | test(secret_environment) | test(captured_admin) | test(unknown_role_creation) | test(business_database_readiness)'`。
- 官方 Toasty/Turso 依赖组合（无驱动本地 allocator 补丁）：退出 0，24/24 通过，829 未选择；run `03006232-aa02-44cf-8a29-471bbe85fe0f`。包括此前失败的 cold_deployment 测试。
- 编译仍报告 10 条 storage 测试相关警告，尚未宣称 Clippy 清零。该轮是组件与本地 Turso 测试，不是独立 PG 连接或 Compose/K8s 验收。

## 2026-09-20：数据库管理入口的退役字段与前置校验

源码复核发现 reset-password/create-database 仍要求 user_id，且账号/数据库字段直到容器 ensure/wake 之后才验证。这与已批准的 app_id + dev/prod 定位、Fail Fast 要求不一致。

本轮已实现：

- 两个共享请求 DTO 移除 user_id；HTTP 路由仍以 app_stage 隔离 dev/prod，OpenAPI 随共享类型更新。
- 请求 validate 在 resolve_exec_target 之前执行，统一校验 app_id、账号/数据库/owner，以及非空且无 NUL 的密码。非法请求不再先启动容器。
- 改密请求 Debug 脱敏；写密码命令的传输错误和 stderr 不直接透传，避免远端输出回显含密码的 SQL。保留写入阶段和退出状态；传输失败明确说明结果未知。
- 未改变旧接口的即时数据库管理语义，未将其静默替换为保存配置。

反例先行：`cargo nextest run -p shared_types --no-fail-fast -E 'test(db_admin_requests_use_app_and_stage) | test(upsert_rejects_nul)'` 修改前退出 100，2/2 失败（run `31005da3-d907-416f-9991-ea2a155abd3c`）：缺 user_id 拒绝反序列化；NUL 密码进入不应调用的远端 runner。

修复后 `cargo nextest run -p shared_types --no-fail-fast -E 'test(userapp::db_admin)'` 退出 0，12/12 通过、269 未选择（run `b8862e42-ba13-4ddb-bb9f-3c1c7db4a71e`）。覆盖请求形状、Debug、非法输入、带敏感 SQL 的错误、正常已有/新建账号及建库冲突。`cargo fmt --all` 退出 0。本轮未重跑完整 workspace/Compose/K8s，也未证明 HTTP 实机与真实 PG 改密已通过。

**仍需继续的 CR10 核心工作**：运行账号保护不能仅查询配置再执行 SQL，否则与并发保存/应用配置存在 TOCTOU。即时 DB 管理需在短事务中通过根 control_revision CAS 与生命周期受理、配置保存建立共同竞争点，持久化操作身份和环境槽位后再做远端 I/O；执行期间不持数据库事务。检查当前已生效、正在应用及待生效的账号；不让旧接口改写受管理账号。捕获物理身份，未知写结果保留原操作与恢复保护，不按 HTTP 超时释放。非运行账号管理仍须可用；不能以全面禁止旧接口宣称完成。当前补丁尚未实现该操作链，不能据此关闭 CR10。

## 2026-09-20：CR10 改密操作的存储受理边界

已增加 `ResetDevDatabasePassword`/`ResetProdDatabasePassword` 两个环境专属操作 kind，以及只携带环境与目标账号、不携带密码的 `ResetDatabasePassword` command。PG/Turso 未发布初始化 schema 的 scope CHECK、codec 与生命周期策略分派同步更新。

- 改密受理走既有 `repo::ensure/claim_app` 根 CAS；在同一个短事务中检查配置账号，再提交 operation、请求映射及环境槽位。受管理账号拒绝后事务整体回滚，不留下操作或槽位。
- 检查 saved/applied/applying 三类版本，避免保存新账号后把仍运行的旧账号暴露给直接改密。
- 新配置保存同样经过根 CAS；存在同环境改密操作时返回带 blocker 的冲突，防止检查后再并发登记成运行账号。原保存请求的幂等重放可以返回既有结果；另一环境仍独立。
- RecoveryRequired 改密仍持有槽位并阻止配置变更；只有确定终态才释放。通用控制恢复不自行重放此类密码命令，避免缺少原私有请求和物理目标时猜测执行。

验证：`cargo nextest run -p rcoder-storage --features userapp-turso --no-fail-fast -E 'test(configuration_tests)'` 退出 0，11/11 通过、114 未选择（run `fee056a0-e30b-4da2-b947-6bd6d2af81e8`）。其中新增三例覆盖受管理账号拒绝/无残留、改密→保存互斥/环境隔离/未知状态保护、已生效与待生效账号双保护/普通账号允许/原保存重放/确定失败释放。此前聚焦两例 run `df0299e4-9702-4522-b321-e6d8469eb00e` 也通过，但编译期间补入保护代码，因此不把该轮称为修复前反例证据。fmt 与 diff 检查退出 0；存储编译仍有已记录的 lint 告警，未宣称 Clippy 通过。

**未完成/后续接线**：HTTP reset-password 尚未使用新的持久操作；实际运行环境里的账号（尚未保存版本化配置）、管理员默认用户名解析、物理目标与 lease、断连后协调任务、远端写前/写后证据及显式恢复仍需接入。当前组件通过不能证明旧 HTTP 旁路已关闭。app_manager 新 command 分支尚待集中 workspace 编译验证；独立 PG 连接竞争与真实 Compose/K8s 尚未运行。初始化 SQL 仍属未发布基线，不对既有测试数据库自动清库或原地改表。

## 2026-09-20：改密写入阶段与确定终态约束

HTTP 执行链接入前，补上不能由调用方错误分类绕过的存储状态机：新增共享 `DatabasePasswordEvidence`（操作完整身份、目标账号、dev builder 物理快照或 prod UID/部署代次、阶段），不保存密码。

阶段为 Captured → WriteSubmitted → Verified。持久化先后次序必须与远端写一致：先提交 WriteSubmitted 才能发送 SQL；确认 SQL/TCP 后记录 Verified，随后才能 Succeeded。存储拒绝跳阶段、替换物理目标或账号、WriteSubmitted 后直接 Failed、无证据 Succeeded。Captured 的确认无写失败仍可终止；未知结果转 RecoveryRequired 时必须保留原证据及槽位。终态租约扫描只有身份匹配的 Verified 证据才允许按原租约闭环，不重放 SQL。

验证：`cargo nextest run -p rcoder-storage --features userapp-turso --no-fail-fast -E 'test(configuration_tests)'` 退出 0，13/13 通过、114 未选择，run `34653268-525c-4893-a38c-e51ad918d791`。新增两例覆盖无证据成功、跳阶段、写后错误解锁、目标换代拒绝、未知保留，以及完整确认后的终态释放。此处是实际 Turso 存储与 domain 调用链的测试；证据由夹具提供，不等同真实 SQL/TCP 成功。

尚未完成：HTTP reset-password 仍走旧执行路径；需要把上述阶段接入运行时 SQL 调用，补 dev 物理绑定执行通道、prod 仅管理就绪的唤醒路径，以及断连独立协调任务与真实数据库验收。现有组件测试不能替代这部分；不将 CR10 标记完成。存储 lint 告警仍待集中处理。

## 2026-09-20：开发容器物理绑定 exec 通道

新增 AgentContainerRuntime::exec_builder_control_target；不支持的运行时明确拒绝，不默认退回按 app/name exec。Docker 重新检查所捕获容器的不可变 ID、生命周期/资源绑定和服务归属，再按 ID create_exec。K8s 重新确认捕获的 StatefulSet 与 Pod UID，并在 exec 内检查 Downward API 注入的 RCODER_PHYSICAL_POD_UID，防住读取之后同名 Pod 被替换的窗口。构造 argv 使用位置参数，命令文本不再拼接进检查脚本。新 builder 环境最后注入唯一的 UID 字段，覆盖同名用户配置；旧 Pod 未携带此字段时明确要求重新创建，不接受伪造常量 UID。

抽出 K8s exec_pod_container 复用既有 stdout/stderr 并发排空、退出状态及 transport join 处理；prod 继续选择原应用容器，dev 使用 agent 容器。新增真实本地 shell 测试覆盖原 UID 成功、替代 UID/缺失 UID 拒绝，以及引号/空格/shell 表达式 argv 不被二次解释。

本轮已启动集中 `cargo check -p docker_manager -p app_manager -p rcoder --all-targets --all-features`，日志 `/tmp/rcoder-cr10-bound-exec-check.log`；结果待后续追加，不把启动构建当作编译通过。尚未运行新单测或远端 builder exec 实测。HTTP 改密协调器仍需下一步接入此通道和前述存储操作、lease 与证据。

集中 check 已结束：退出 0，1m26s；覆盖 docker_manager/app_manager/rcoder 的全部 targets + features，新增操作分派与 builder exec 通道通过编译。仍有 rcoder-storage lint 告警，不是 Clippy 全绿。已启动 `cargo nextest run -p docker_manager --all-features --no-fail-fast -E 'test(builder_exec_guard_fences)'`，日志 `/tmp/rcoder-cr10-builder-exec-test.log`；尚待结果，不计为通过。

## 2026-09-20：HTTP 改密协调器接线

新增 userapp_forward/db_password.rs，reset-password 路由已转入该协调器。请求增加可选 request_id/lifecycle_id；校验后取得在途门闸并启动独立 Tokio 任务，HTTP observer 断开不取消任务。准备容器后捕获物理目标与 PGDATA 管理员，取得真实运行时 lease；受理并 claim 持久化操作后绑定 lease 与 Captured 证据。检查容器实际 POSTGRES_USER，防止尚未保存版本化配置的运行账号绕过保护。

写入流程先持久 WriteSubmitted，再经绑定目标执行 ALTER/CREATE，确认退出状态并执行 TCP 登录验证，再持久 Verified/Succeeded，最后释放租约。写意图提交或远端结果不明时保留 lease，并尝试在原身份下记录 RecoveryRequired。SQL/密码不进入公开 command/checkpoint 或错误文本。完成后按原 request_id 的成功重放不再发送 SQL；其他已有操作返回其原 blocker，不用本次新生成的 ID 覆盖原错误身份。OpenAPI 已更新运行账号限制、管理员默认值、重试与错误语义。

验证：上一轮 builder exec guard 的 nextest 已结束，1/1 通过、219 未选择，run `a3ad40cd-1e5e-45e2-9fcc-aef2284d7840`。本轮 `cargo check -p rcoder --all-targets --all-features` 首次退出 101（新增模块两处 unnecessary qualification），修复后最终退出 0，22.07s；日志 `/tmp/rcoder-cr10-password-check.log`。fmt/diff 检查退出 0。尚未执行协调器行为测试或真实数据库验证，不能据此宣布即时改密功能验收完成。

后续重点：准备阶段目前仍复用旧 ensure/wake，其中 prod 仍可能等待业务 Ready，须改为管理就绪路径；准备与 lifecycle 的完整竞态需真实调用链验证；Unknown 操作的显式原身份恢复仍未实现；私有请求不能由通用恢复器猜测重放。协调器需补断连、持久化故障、错误目标、并发保存/部署、正常非运行账号改密/建号及幂等重放反例；再进入 Compose/K8s。不得以当前编译通过关闭 CR10。

## 2026-09-20：改密重放必须先于容器准备

发现上一版协调器在查找原操作前就执行 ensure/wake 并取得租约，可能让成功重放再次唤醒容器，也可能让未知操作的保留租约挡住其原 blocker。现改为通过 get_operation_by_request 先读原记录，核对当前 lifecycle、环境 kind 和完整请求 fingerprint，然后返回已验证的成功结果或原操作状态。受理处的并发重放复用同一校验函数；Succeeded 记录缺少 Verified 物理/操作证据时拒绝假成功。Failed 重放返回原失败身份，不能自动重做。

已增加两例针对性测试（请求密码改变、dev/prod/lifecycle 改变、验证证据缺失、Unknown 的原操作 ID 与 blocker）。当前 `cargo nextest run -p rcoder --all-features --no-fail-fast -E 'test(password_replay_) | test(password_uncertain_replay_)'` 正在构建完整 RCoder 测试二进制，日志 `/tmp/rcoder-cr10-password-replay-tests.log`；结果未出，不能计为通过。fmt/diff 检查退出 0。测试本身覆盖重放判定，不宣称真实 HTTP 断连或 PG 写入已验证。

仍需核查/修正：lease 获取异常目前统一映射 Conflict，需按类型区分真实占用与运行时故障并补结构化 blocker；管理就绪的容器准备、原身份 Unknown 恢复，以及完整协调器行为测试继续保留为未完成。

## 2026-09-20：租约故障分类与 blocker 补充

上轮重放测试结束：退出 0，2/2 通过、330 未选择，run `9af7a2d4-f361-400c-8490-d86593672209`，构建耗时 2m26s。

本轮修正数据库协调器的 lease 错误映射：只有 ContainerRuntimeError::Conflict/OperationInProgress 返回冲突，ConfigurationError/ConnectionError/Timeout 和其他运行时故障返回对应后端失败说明，不解析错误字符串。对明确冲突，读取应用与同环境/Application 槽位，核验 lifecycle/scope/非终态后附带原操作 blocker；查不到可确认记录时保留无 blocker 的真实资源冲突，不拿另一环境或过期记录冒充阻塞者。

增加错误分类测试，断言后端故障不会成为 ERR_CONFLICT，且不把底层可能含敏感连接细节的错误原文回显。当前针对整个 db_password 测试模块的 nextest 正在执行，日志 `/tmp/rcoder-cr10-password-replay-tests.log`；待补实际结果。fmt/diff 检查退出 0。此改动不代表整个协调器已完成部署验证。

接线复核待办：总 deadline 还需覆盖 capture/受理等阶段（当前 Runner 单次命令已包含在同一 deadline 内）；PG bootstrap 的后台失败 helper 持有父 PID，需核对父进程提前退出时 helper 的收束，避免迟到 kill；这些发现尚未修复或验证，不能列为已关闭。

本轮 nextest 已结束：`cargo nextest run -p rcoder --all-features --no-fail-fast -E 'test(userapp_forward::db_password::tests)'` 退出 0，3/3 通过、330 未选择，run `7b7e86c2-5daa-4eea-8ede-3315934b0284`，增量构建 36.01s。不是完整 RCoder 或部署测试。

## 2026-09-20：改密协调器的执行与收尾预算

将 capture、持久受理、状态推进及实际命令纳入同一 180 秒执行 deadline；前置只读查询、原操作重放查询与 lease blocker 观察也受同一预算限制。新增共享 5 秒收尾预算，用于失败/恢复状态持久化和租约释放，防止 HTTP 已脱离的协调器仍无限占用在途关机门闸。

受理开始即保留预分配 operation_id；受理等待取消或 Storage 错误不能视为未提交。执行超时且操作可能已落盘时，保留租约，并以原身份尝试记录 RecoveryRequired。若 CAS、持久化收尾或释放超时，返回明确错误；不换身份、不报告成功、不把取消观察当作数据库事务已回滚。明确领域拒绝仍按既有冲突/校验错误处理。补充 blocker 的 app_id/operation_id 校验。

新增只读 deadline 测试，覆盖真实超时及存储 lifecycle 冲突保持错误分类；该用例不证明持久写入取消竞态或整体 HTTP 协调器已验证。首次编译退出 101（多余 Future 路径限定和新测试误用 AppError 字段），已修复；聚焦 nextest 正在复跑，日志 `/tmp/rcoder-cr10-deadline-tests.log`。完整协调器持久化故障/提交后断连反例、管理就绪准备和原身份恢复仍未完成。

复跑结束：`cargo nextest run -p rcoder --all-features --no-fail-fast -E 'test(userapp_forward::db_password::tests)'` 退出 0，4/4 通过、330 未选择，run `1f06410a-3d17-4877-96af-752c797dcdd3`，构建 40.09 秒。fmt 与 diff 检查通过。存储及链接器警告仍在，不声明 Clippy 全绿；本轮未执行 Compose/K8s 或发布。

## 2026-09-20：PG 初始化任务与 PostgreSQL 共同收束

修复前反例：实际执行两个镜像的 pg-supervisor-entry.sh（仅替换临时工具/辅助脚本路径），让 PostgreSQL 可执行 fixture 退出 7，而初始化任务稍后写标记。macOS 与个人 Linux 均观察到主进程退出后标记仍被写入；Linux 命令 `python3 -m unittest tools.test_pg_supervisor` 退出 1，两个镜像子例失败。日志 `/tmp/rcoder-pg-supervisor-linux-before.log`。该验证使用真实进程，但没有运行真实 PG 数据库。

修复：移除后台 helper 持有父 PID 后 kill 的模式。两个 Linux 镜像入口在权限检查后 exec pg-supervise.py，使用镜像现有 Python 统一拥有 PostgreSQL 与初始化进程组。waitid(WNOWAIT) 只观察退出，最后一次组信号发送前不回收组长，避免 PID 复用；helper 不再有向父进程发信号的职责。初始化成功保持 PG 运行；初始化失败、PG 提前退出或 supervisor 发 INT/TERM 时收束两组。PG 收到 fast shutdown 的 INT，25 秒宽限后强制清理并回收，位于 supervisor 30 秒停止预算内。进程停止/回收仍可能受内核不可中断 I/O 影响，完整容器退出由部署验证补证，不宣称能抵御所有外部 SIGKILL 情况。

已同步 RCoder 与 build-agent-docker 两种镜像的入口、新 manager 和 Dockerfile COPY；不涉及宿主机 app-cli 的依赖。两个仓库脚本逐字节配对、sh -n、git diff --check 均通过。

Linux 最终命令 `python3 -m unittest -v tools.test_pg_supervisor` 退出 0，2 个测试方法、8 个镜像/场景子例通过（提前退出、初始化失败、外部停止、初始化成功后继续运行）；耗时 16.739 秒。验证了 PG 收到 INT、退出被回收、初始化任务不再迟到写入。日志 `/tmp/rcoder-pg-supervisor-linux-final.log`。原管理员/数据库 bootstrap 协议测试 `python3 -m unittest tools.test_pg_admin_identity` 退出 0，6/6 通过。

本轮没有构建镜像、真实 PG/Compose/K8s 验收、提交或发布。后续必须重建 app-runtime 与 agent-runner 镜像后纳入 Compose 测试；该项不能代替 CR10 管理就绪准备、原操作恢复及其持久写入反例。

## 2026-09-20：运行中但 NotReady 的 prod 管理入口

确认之前 reset-password 无条件调用 resolve_exec_target，prod 分支先 activity.ensure_running，业务因密码失败时会先遇到 wake timeout，无法到达已有的身份绑定管理 exec。

本轮先解除已运行容器的错误依赖：prod 在调用 wake 前读取部署代次，核对 lifecycle/物理目标，并探测管理通道；可用时不走业务 Ready 等待。后续仍取得运行时租约、重新捕获目标、持久受理，并在每次 exec 核验身份。PGDATA 管理员标记和 PG socket 分别在同一总预算内等待，不依赖应用服务 Ready。

新增类型化 ContainerRuntimeError::ManagementNotRunning，只有此明确结果允许进入既有唤醒链。Docker 先核对标签、lifecycle、代次，再判断物理 running；K8s 即使无运行中候选 Pod，也先核对 workload 归属，不将外来 workload 当作普通未运行。管理探测的 Conflict、连接失败、缺失 workload、配置错误、超时均报错，不回退唤醒，不解析错误字符串。

`cargo check -p rcoder --all-targets --all-features` 退出 0，29.44 秒；日志 `/tmp/rcoder-cr10-management-check.log`。随后增加 PG 标记有限等待及错误分类反例，聚焦 nextest 正在执行，日志 `/tmp/rcoder-cr10-management-tests.log`。该用例仅验证“错误不能触发 wake”的决策，不宣称完整 HTTP/PG 链已验收。

剩余：物理容器确实停止时仍调用旧 durable wake，尚未完全拆除该分支的业务 Ready 依赖；需要独立管理准备操作/证据，不能把普通 Start 在 PG 就绪时直接标为应用启动成功。dev 首次 builder 准备、Unknown 原身份恢复、真实数据库和 Compose/K8s 验证仍未完成。create-database 的旧管理路径也尚未统一。

上一轮管理探测聚焦测试已结束：退出 0，5/5 通过、330 未选择，run `422bd6f6-ea64-45e2-9c3b-90fd1469c432`，构建 1m59s，日志 `/tmp/rcoder-cr10-management-tests.log`。

## 2026-09-20：独立管理准备操作契约

新增 PrepareProdDatabase kind/command，固定 prod scope；同步两个未发布 schema 的 scope CHECK 与持久化 codec。其成功不改变 runtime_policy、不提升待生效配置，也不能被当作普通 Start 成功。新增 DatabasePreparationEvidence：原执行身份、完整 workload UID/name/版本、部署代次、Captured/StartSubmitted/ManagementReady 阶段与就绪后的管理目标。

状态机强制 Captured → StartSubmitted → ManagementReady；不允许跳过启动证据、替换 workload/执行身份/部署代次、在 StartSubmitted 之后直接 Failed。结果未知仍进入 RecoveryRequired 并保留槽位。只有身份有效且 ManagementReady 才支持 Succeeded 和终态租约扫描。通用恢复器目前跳过该新命令，避免执行器尚未接入时猜测普通 Start 语义；这是未完成项，不作为恢复功能已实现。

`cargo check -p rcoder --all-targets --all-features` 退出 0，48.70 秒，日志 `/tmp/rcoder-cr10-prepare-contract-check.log`。新增加真实 Turso 存储测试，覆盖拒绝空成功/跳步/错误物理目标/未知写入释放，确认正常完成释放槽位但 runtime_policy 保持不变。`cargo nextest run -p rcoder-storage --features userapp-turso --no-fail-fast -E 'test(configuration_tests)'` 正在执行，日志 `/tmp/rcoder-cr10-prepare-contract-tests.log`。

下一步必须实现该操作的身份绑定运行时执行器与原请求重放，然后替换 db_password 中物理未运行时的旧 resolve_exec_target 调用。当前只完成契约与保护，不代表停止容器管理唤醒已可用；完整 PG/Compose/K8s 与同身份恢复仍未验收。

该轮 Turso 测试已完成：退出 0，14/14 通过、114 未选择，run `48a13988-1a13-48cd-aa1c-49e8a772fbcb`。包含新增管理准备证据测试；不能替代其尚未接入的运行时执行器或真实 PG/部署验证。

## 2026-09-20：独立管理准备执行器与改密入口接线

新增 AppService::prepare_prod_database 及 AppServiceTrait 转发，db_password 对明确 ManagementNotRunning 改调该流程，移除 prod 停止分支对旧 resolve_exec_target/business Ready 的依赖。准备操作 request/operation identity 由原请求、app 与 lifecycle 派生，原完整 fingerprint 用于拒绝改参重放。先确认 lifecycle/原请求，再取得操作 guard；受理与执行 claim 分开处理，防止受理已落盘后把 claim 错误当作“未受理”解锁。未知提交保留 lease。

执行器持久 Captured、StartSubmitted 后仅启动绑定 UID/版本的 workload；按原部署代次捕获管理容器，通过绑定 UID 的 exec 等待 PGDATA 管理员标记与 PG socket SELECT 1。完成持久 ManagementReady/Succeeded 后释放 lease。全程不读取业务 Ready，不提升 pending 配置。错误/超时在有限收尾预算内记入原操作；提交启动后的失败保留 RecoveryRequired/槽位/lease。通用 Pending 恢复器接入同一 execute_database_preparation，保留原操作 ID，不再跳过该命令；已是 RecoveryRequired 的远端结果未知操作仍需要专用核验，尚未实现其显式恢复。

真实运行时核对发现普通 K8s start 会写 wake_on_traffic=true，不能用于此路径。因此新增 start_app_management_target，Docker 复用身份绑定的物理启动，K8s 仅按 UID/版本设置 replicas=1，不修改 wake annotation；未实现运行时明确拒绝，不回退普通 start。测试替身增加独立管理入口调用计数，避免走错入口仍通过。

验证历程：首次 rcoder check 退出 101（AppServiceTrait 漏转发），已补；首次执行器两测试失败于 fixture 使用 Docker 文件锁而模拟通道断言 K8s runtime lease，已改为匹配的 Kubernetes 模式，未删除断言。之后两例通过；再新增 Pending 恢复用例，3/3 通过、206 未选择，run `65c77258-56ee-48e2-bc67-7d1505230387`。覆盖业务 Error 但管理成功、pending 凭据不生效、原请求成功重放无二次启动、未知状态重放返回原 blocker、Pending 恢复保留原 ID，以及终态/租约/槽位。

新增专用管理启动方法后的最终 `cargo check -p rcoder --all-targets --all-features` 已退出 0，21.91 秒；聚焦 nextest 正在复跑，日志 `/tmp/rcoder-cr10-prepare-executor-tests.log`。这些是 Turso + 可控 runtime 的组件测试，不是实际 Pod/PG/Compose 验收。Docker 文件锁实测、跨副本真实 PG、HTTP 断连/超时及 Unknown 显式恢复仍须补齐。

专用管理启动入口最终复跑完成：nextest 退出 0，3/3 通过、206 未选择，run `51de133f-9613-4bdd-90db-6bb075827ee8`，构建 22.06 秒。未执行镜像构建、真实 Compose/K8s 测试或提交发布。

## 2026-09-20：PG 事务回执基础及独立连接验证

为未知改密恢复补上远端幂等依据，方案与限制见 `../userapp-prod-wake-timeout-pg-credential/password-recovery-receipts.md`。正常改密已使用事务回执，重复同操作不会再 ALTER；此项仍不等于显式恢复 API 已完成。

最初 `cargo check -p rcoder --all-targets --all-features` 退出 0（17.04 秒，`/tmp/rcoder-pg-receipt-check.log`）；之后调整了 helper 的命令选项构造和短 DDL 引导事务，最终 `cargo nextest run -p shared_types --no-fail-fast -E 'test(pg_utils::tests)'` 退出 0、21/21 通过、262 未选择，run `f7bbec4a-a124-4c3e-8f52-e561677b8534`，日志 `/tmp/rcoder-pg-receipt-tests.log`。fmt/diff 检查通过。

真实 PG 探针：`python3 tools/test_pg_password_receipt.py --run --host <个人测试 SSH 目标> --namespace nuwax-k8s-test --pod nuwax-k8s-test-pg-2`，最终退出 0。个人 .131 集群实际版本 PG 17.9；创建和清理独立临时库/角色。覆盖正常写入、后续写入、旧请求迟到不覆盖、改参拒绝、失败事务无回执、断开未提交事务无回执且密码不变、TCP 正误密码与两独立连接并发重复。初版并发错误 `tuple concurrently updated` 已通过分离短 schema/ACL 引导事务修复，不能将初版结果写成通过。

本轮 SQL SHA256：事务主体 `5e6bcc7313c16e4361b940175e8d65f67ebb5778874008f0fa7cff015f5eaa0d`；schema 引导 `56533d0b1d55f99f13f772c68899908f829586e5038f4aaa5cf8f1a38ab87da7`；探针 `6f69c1be549aeb7beba37f63401f49f76f4d4e7b039859c87383e2a19e1f779d`。

继续开发：取消墓碑/迟到请求隔离、原身份显式恢复和控制存储终态 CAS；回执缺失仍保持保护，不按等待时间解锁。应用镜像 PG 16、真实 HTTP/Compose/K8s UserApp 路径未由本轮 PG17 探针证明。未提交、推送或发布。

## 2026-09-20 改密取消墓碑数据库机制

- 增加回执 outcome 与取消 SQL：同一唯一键竞争；取消先提交拒绝迟到写入，写入先提交返回 committed；指纹/账号不匹配返回空结果，不能授权释放。成功回执查询排除 cancelled。
- 个人测试集群 PG 17.9 独立临时数据库/角色实测退出 0：取消前后顺序、改参拒绝、两个独立连接写入/取消竞争，以及原有事务回滚/TCP/重复请求场景均通过，临时资源已清理。第一次探针失败为 psql 输出含 INSERT/COMMIT 标签，测试误取倒数第二行；改为检查完整 outcome 行，保留所有业务断言后重跑通过。
- cargo nextest run -p shared_types --no-fail-fast -E 'test(pg_utils::tests)' 退出 0，21/21，262 未选择，run 0e97bae5-4d42-42ed-aa16-fac592d7620b。日志 /tmp/rcoder-pg-cancel-tests.log。
- cargo fmt --all -- --check、git diff --check 退出 0。
- 未完成：恢复 HTTP 接口、控制存储原身份 CAS 与不同终态证据接入；尚无完整恢复链验收。PG16、Compose、K8s UserApp、三平台当前增量验证仍需完成。未提交、未推送。

## 2026-09-20 密码恢复控制存储 CAS

- 新增 DatabasePasswordStage::Cancelled 及 finalize_password_recovery：根记录竞争点、全旧记录 CAS、原租约/执行身份、账号及物理目标验证；结果分别提交 Succeeded/Failed，原物理租约留待终态扫描精确清理。无远端 I/O 进入数据库事务。
- 新增反例覆盖缺少租约、改变物理目标、过期 revision、重复提交，并验证取消不误记成功、操作身份不变、槽位释放而原租约保留。
- cargo nextest run -p rcoder-storage --all-features --no-fail-fast -E 'test(database_password) | test(password_recovery_finalization)' 退出 0，3/3，154 未选择。run 1ed3e79d-0b56-4653-a151-4ce341f14a89；日志 /tmp/rcoder-password-recovery-cas.log。本轮是 Turso 存储组件测试，不能替代独立 PG 控制存储并发或 HTTP 恢复验收。
- cargo fmt --all -- --check 与 git diff --check 退出 0。存储既有 9 个编译告警尚未清零，不能声称 Clippy 全绿。
- 下一项：显式恢复 HTTP 协调器接入远端回执/取消命令、原输入核验、TCP 验证及此 CAS；完整任务仍未完成。未提交或推送。

## 2026-09-20 显式改密恢复 HTTP 协调器

- 新增 POST /api/v1/userapp/db/{app_stage}/reset-password/recover，DTO、路由和 OpenAPI 均注册，Java 接入说明已更新。请求必须携带原输入、原 lifecycle/operation/revision，不创建新操作，不唤醒/替换容器。
- 原物理执行通道读初始化管理员；原事务已提交则 TCP 核验后提交 Succeeded；取消墓碑先提交则 Failed。终态后仅清理原租约，清理失败返回 lease_cleanup_pending。HTTP 断连不取消协调任务，错误输出不回显私有执行诊断。
- 增加 receipt_protocol=1 持久标记；旧记录缺少标记时不能用新墓碑推断旧写入已被阻止。控制存储检查该标记，正常阶段转换不能改变它。
- 新反例验证：旧协议、错环境/原输入/执行者/版本被拒，终态取消不误判成功，远端非零退出即使输出 committed 仍失败，空/歧义回执拒绝，TCP 失败不提交成功，恢复不执行 ALTER/CREATE ROLE。
- cargo check -p rcoder --all-targets --all-features 退出 0（22.63s）；后续增加的测试与文档修正由 nextest 编译覆盖。
- 聚焦 nextest 首轮 44/48：恢复/存储均通过，4 个 OpenAPI 契约失败。核对源码后修正正式 UserApp HTTP 200 错误信封文档、配置端点摘要、枚举描述与必填 original 内的 app_id 检查。第二轮 47/48，run 35515219-d25e-4e32-98ae-86b7d427c807；剩余新增操作 kind 的字段说明缺枚举值，已修，单独复跑 OpenAPI。
- 真实 PG 17.9 回执探针再次退出 0，使用安静输出模式验证写/取消双连接竞争、回滚与 TCP，临时库/角色已清理。未操作现有业务库。
- 未完成：真实 HTTP + 原容器恢复流程、PG16 应用镜像、完整 Compose/K8s、当前增量三平台验证、全部 workspace 测试和 Clippy。仍有存储编译告警。未提交/推送/发布。

OpenAPI 最终复跑：cargo nextest run -p rcoder -p rcoder-storage -p shared_types --all-features --no-fail-fast -E 'test(openapi)' 退出 0，16/16，762 未选择，run 162aade2-de46-44c8-bc8e-32243a58951e。日志 /tmp/rcoder-password-recovery-openapi-final.log。修复后的嵌套必填 app_id、错误信封、摘要及枚举文档全部通过；此前恢复/存储/SQL 用例结果仍对应第二轮 47/48 的源码，最后仅更新操作枚举说明。最终 fmt --check、diff --check 退出 0。

## 2026-09-20 Compose 凭据 E2E 契约更新

- 阅读 make/dev.mk、make/docker.mk、make/test.mk：dev-restart 重建 builder/master，dev-hot 仅更新主服务；app-cli/PG 启动脚本变更还需 docker-build-app-runtime 与 builder 镜像配对。Docker server 29.4.0 当前可用，但本轮没有重建/部署镜像。
- dev 改密场景改为受管理账号拒绝、独立账号首次写入、原请求幂等重放、同身份改参拒绝、新操作改密及迟到旧请求不覆盖新密码（docker exec 真实 TCP 验证）。取消盲目创建新改密请求的 initdb 重试循环，使用固定请求身份和覆盖协调器 180s 预算的 195s HTTP 观察窗。严格验收清单新增对应必测断言。
- prod 场景改为独立账号改密，并将该验证放在热部署前，移除绕过历史凭据同步缺陷的测试排序；后续热部署及迁移仍需通过。运行账号保存待生效/显式启动应用的完整场景仍待补。
- 源码核对发现新改密协调器把应用不存在降成 ERR_NOT_FOUND；已恢复 ERR_APP_NOT_FOUND，不改原 E2E 契约。操作不存在仍使用通用 not-found。
- 两仓 app-runtime-base/rcoder-agent-runner 的 pg-supervisor-entry.sh、pg-admin-identity.sh、pg-supervise.py 六组逐字节一致。
- cargo check -p rcoder-e2e --tests：首次发现新报告 detail 需要 String，修正后退出 0（4.11s）。日志 /tmp/rcoder-e2e-credential-check.log。fmt --check、diff --check 退出 0。
- 上述仅是编译与静态前置证据，不是 Compose 用例通过；未运行 make test-e2e，未提交/推送/发布。

## 2026-09-20 配置保存/显式重启 E2E

- 完整部署链新增 CR10 必测断言：读取原 lifecycle/物理 UID，旧凭据 TCP 登录；保存只增加 pending 版本、applied_version 不变且响应不含密码；容器不变、旧密码有效、新密码暂不可用；显式重启后 UID 换代、applied_version 命中新版本、新密码 TCP 登录；dev 物理 UID/原凭据不变；最终生产代理返回真实 React HTML。
- 同步 acceptance_steps.json，不以提前 return 或未执行断言算通过。请求中的测试密码不写入报告；所有 PG 命令输出仅用于布尔判断。
- cargo check -p rcoder-e2e --tests 退出 0（2.70s），日志 /tmp/rcoder-cr10-config-e2e-check.log；这不是实际 Compose 运行结果。
- 清理 start/restart DTO 和 handler 中已退役 user_id 的误导说明、错误提示；修正“凭据对齐失败仍成功”的过时描述。上述注释/提示修改尚未重跑 app_manager 测试，将合并下一批检查。
- 新确认的剩余缺口：deploy_control.rs 的 captured_configuration=None 且 request.pg=Some 分支仍在部署后执行 align_start_pg。虽然已不吞错，它仍没有保证迁移和业务启动提前使用这份凭据。需要统一进入版本化启动配置链，不能仅将本次 E2E 添加视为 CR10 全部完成。
- cargo fmt --all、git diff --check 退出 0。未启动 Compose 套件、未提交/推送。

## 2026-09-20 request.pg 统一为受理时配置版本

- 新增原子 admit_with_configuration：首次显式 pg 与 operation/input/request/slot 同事务初始化配置版本并捕获；已有配置必须匹配 saved_version，不允许启动参数覆盖已保存配置。配置保存与操作受理仍通过同一应用根 CAS 竞争。
- 重放按原 operation 的配置捕获记录核验；后来保存的版本不污染原操作，不重新初始化/提升版本。不同参数重放拒绝。
- AppService 部署受理传入完整私有输入及 pg；执行器拒绝“pg 存在但没有配置捕获”的旧输入。删除部署完成后 align_start_pg 分支及无消费者 helper，凭据统一走已有冷切换、管理通道、PG 验证、激活及业务启动链。
- 新增存储反例：首次版本与操作捕获、后续保存后原请求重放、改参拒绝；配置冲突后 operation/request/slot 不残留。现有冷部署管理调用顺序及 hot 配置门禁一并复跑。
- 首次实现内部 save token 加前缀超过 validate_identifier 的 64 字符上限，被反例捕获；改为带用途域分隔输入的完整 SHA256 十六进制 token（64 字符），未放宽校验。
- cargo nextest run -p rcoder-storage -p app_manager --all-features --no-fail-fast -E 'test(inline_deployment) | test(lifecycle::deploy_control)' 最终退出 0，4/4，364 未选择，run da884495-15b4-4c30-b27d-61a06805a9b6；日志 /tmp/rcoder-inline-config-tests-final.log。
- fmt --check、diff --check 退出 0。Java 接入文档及 DTO 说明同步。未执行完整 Compose/K8s/三平台回归，未提交推送，整体任务继续。

## 全 workspace 回归与夹具修正（2026-09-20，未提交工作树）

命令 `cargo nextest run --workspace --all-features --no-fail-fast`，run ID `75e10af9-cbc1-4b04-a623-e0c4ed322d04`，退出 100。2455 个执行用例中 2444 通过、10 个断言失败、1 个人工中断；另有 16 个跳过。日志 `/tmp/rcoder-workspace-all-features-final.log`。这是失败的一轮，不代表完整验收通过。

失败分类及修正：

- 9 个 activity_registry 唤醒用例：运行时替身 TestWakeLease 未实现 receipt，当前持久化绑定拒绝无身份租约。补齐与应用绑定的 Kubernetes 测试租约回执；保留并发去重、替换资源拒绝、取消及未知结果保留租约等原断言。
- operation_queries_validate_owner_and_hide_internal_checkpoints：夹具使用短字符串指纹，被新表 SHA256 长度 CHECK 拒绝；改用合法 64 字符测试指纹，不放宽约束。
- configured_file_is_durable_and_invalid_directory_fails：原测试只释放 store/control，新增 configuration/activity Arc 仍持有数据库所有者，使所谓有界轮询实际无限等待。这一用例运行 106 秒后主动中断。改为释放完整装配结果，并给重新打开设置 5 秒总预算；恢复读取后显式 shutdown。没有修改生产锁释放规则。

针对上述改动已启动聚焦复验：`cargo nextest run -p app_manager -p rcoder --all-features --no-fail-fast -E 'test(activity_registry::tests) | test(operation_queries_validate_owner) | test(configured_file_is_durable)'`，日志 `/tmp/rcoder-workspace-regression-fixes.log`。结果待回填；不把夹具修正推断为通过。格式化命令 `cargo fmt --all` 退出 0。

聚焦复验已完成，退出 0：35/35 通过，512 个未选中。包含原 11 个未通过用例；数据库重开用例 0.353 秒完成。日志 `/tmp/rcoder-workspace-regression-fixes.log`。已使用缓存编译产物启动第二轮完整 workspace 回归，日志 `/tmp/rcoder-workspace-all-features-rerun.log`，不能以本次聚焦结果替代其整轮结果。

E2E 启动器工具检查：`python3 -m unittest discover -s tests-e2e/tools -p 'test_*.py'` 退出 0，91/91 通过，日志 `/tmp/rcoder-e2e-tools-current.log`。这是 Python 工具层检查，未运行真实 Compose 场景。再次核对 Make：dev-restart 依赖 dev-build/docker-build，dev-hot 只替换 rcoder；app-cli 与 PG 入口变化还需要 docker-build-app-runtime 以及 builder 镜像重建。

第二轮完整 workspace 回归完成：`cargo nextest run --workspace --all-features --no-fail-fast` 退出 0，2455/2455 通过，16 skipped，执行阶段 23.447 秒；日志 `/tmp/rcoder-workspace-all-features-rerun.log`。本轮只修正上述夹具，保留行为断言与数据库约束。被跳过的环境用例不计入通过数，也不作为真实 PG/K8s 或 Compose 证据。独立 app-cli 全 features 247/247 通过，详情见 native-desktop-runtime 的本轮验证记录。默认 features、Clippy、当前镜像重建、真实 E2E/远端平台验收仍需继续。

全 features Clippy 首轮：`cargo clippy --workspace --all-targets --all-features` 退出 101，日志 `/tmp/rcoder-clippy-all-features-current.log`。阻断项为 deploy_control 的凭据测试中 MutexGuard + 借用 scripts 的词法作用域跨后续 await；虽然已有 drop(commands)，改为显式代码块保证 guard 与借用均先结束，未更改业务断言。已启动 app_manager 定向 Clippy 复验，日志 `/tmp/rcoder-clippy-app-manager-followup.log`。另有新代码警告（大枚举、私有接口可见性、未用投影/方法、可简化条件和显式丢弃返回值等）待集中处理；当前不得声明 Clippy 全绿。

Clippy 清理复验：app_manager 定向检查退出 0。随后集中收敛本轮新增警告：大型 Dev 密码目标改为 Box（Serde wire 形状不变）、内部 writer spawn 可见性收窄、移除未使用的会话恢复入口及容器投影字段（仍执行字段校验）、明确异步结果丢弃、简化条件分支，并将 UserApp 关闭所需句柄组成资源参数，保持原关机顺序与 deadline。`cargo clippy --workspace --all-targets --all-features` 再次执行退出 0，无 warning/error，日志 `/tmp/rcoder-clippy-cleanup.log`。这不是新一轮组件测试通过证据。

已启动根 workspace 默认 features 测试：`cargo nextest run --workspace --no-fail-fast`，日志 `/tmp/rcoder-workspace-default-current.log`。测试仍在执行，不能将此前全 features 结果代替默认路径结果。

默认 features 回归结束：`cargo nextest run --workspace --no-fail-fast` 退出 0，2292/2292 通过，12 skipped，日志 `/tmp/rcoder-workspace-default-current.log`。本轮包含 Box 目标表示、关闭资源参数及 lint 清理后的源码。环境跳过仍不是真实 PG 或部署验收。

镜像阶段启动：本地 Compose 当前 rcoder 与观测组件均处于 running。`make dev-build` 已启动（日志 `/tmp/rcoder-compose-dev-build-current.log`），Pingap 版本门禁通过，builder→master 按 Make 顺序执行。尚未得到镜像构建结果，尚未替换部署，也没有运行本轮 Compose E2E。

`make dev-build` 已结束，退出 0。产物：master `sha256:add6642bd1a8fc68088903a997e7a33a69402031f31e1e9502b9ae2ed874e365`，agent-runner `sha256:490c00ad2709652097406c5b285d0e51d885c943bf12a64324200b4133d3b599`。随后 `make docker-build-app-runtime` 已启动，日志 `/tmp/rcoder-compose-app-runtime-current.log`，仍在执行。原生验证发现 owner_dispatch 受理 DTO 错配并已修复，因此这些镜像包含修复前 app-cli；最终业务验收必须重新构建受影响产物，不以刚完成的构建代替最终版本。原 Compose 部署未切换。

### 真实 PG 独立连接竞争复验

2026-09-20 使用独立临时 `postgres:17` 容器（随机本机 loopback 端口、随机凭据、用后删除本轮容器；不挂载现有卷、不连接现有数据库）。`cargo nextest run -p rcoder-storage --all-features --no-fail-fast --run-ignored all --test-threads 1 -E 'test(independent_pg_owners) | test(postgres_real_transactions) | test(pg_activity_is) | test(pg_preview_store)'` 退出 0：4/4 通过，155 未选中，run `9ee2d833-e55d-4608-aaf5-47bd122a8846`。日志 `/tmp/rcoder-real-pg-contract-current.log`。

涵盖独立数据库 owner 的 dev/prod 并发受理、删除与受理同根 CAS 单胜者、过期槽位不能释放新操作、SQL 间持有事务时第二连接有界超时并以原请求重试、idle-in-transaction 超时后连接可恢复，以及 UserApp 事务/重开、活动时间和 Preview 契约。不是 K8s 双副本部署、网络分区或完整 ProjectStore 验收。

app-runtime 构建已结束，退出 0，日志 `/tmp/rcoder-compose-app-runtime-current.log`，镜像 config SHA256 `0e9ba4c9601953bcc2bbd49fb55465a2a2e1a5375281600817ca96826f503b1c`。仍是最新 owner receipt / proxy 修复前的镜像，需再次更新后才能用于最终 Compose 验收。

### ProjectStore 真实 PG 反例与修复

独立临时 PG 串行运行 ProjectStore tests/lifecycle_tests：首轮退出 100，17 个中 12 通过、5 失败，日志 `/tmp/rcoder-real-pg-project-current.log`。未将未配置 PG 时的历史组件通过当作这组测试的基线。

确认并修复：

1. 持久化 hydration 已恢复领域对象中的 sessions，但内存 `ProjectAdapter::insert` 没有恢复 `session_index`，导致重启、跨副本同步及 session miss 回源后仍无法通过镜像 session 定位。insert 现同步恢复反向索引，移除该项目已不拥有的旧索引时检查索引归属；不调用 add_session，以保留时间戳、latest_session 和持久化代次。既有三条真实 PG 反例修复后通过。
2. leader 使用空 components 建库连接，被 schema initialize 的“至少一个组件”规则拒绝，永远不能获主。独立连接 owner 改为 Project 组件初始化；保留独立单连接池及关闭等待，不绕过 schema 校验。选主互斥用例修复后通过。
3. 跨副本用例在异步 write-behind 入队后立即让 B 查询，没有等待 A 提交；新增写方排空屏障，保留 B 可见与删除断言。B 的排空只能覆盖 B 自身。
4. 旧库拒绝用例读取 information_schema.column_name（PG name 类型），触发 Toasty raw SQL 类型推断 todo；查询显式转为 text，保留拒绝旧库及未修改字段数断言。

复跑：`cargo nextest run -p rcoder-storage --all-features --no-fail-fast --run-ignored all --test-threads 1 -E 'test(pg::project_store::tests) | test(pg::project_store::lifecycle_tests) | test(adapter::)'`，退出 0，62/62 通过（17 个真实 PG + 45 个适配器回归），97 未选中。run `b0e25fce-a9d3-4ec1-83e2-5d75a62565e0`，日志 `/tmp/rcoder-real-pg-project-fixed.log`。两个临时容器均已删除，不影响现有库。此证据不替代 K8s 多副本部署验收。

ProjectStore 修复后 `cargo clippy -p rcoder-storage --all-targets --all-features` 退出 0、无 warning，日志 `/tmp/rcoder-storage-pg-fixes-clippy.log`；根 fmt check 退出 0。已串行启动最新源码的 `make dev-build` → `make docker-build-app-runtime`，日志分别 `/tmp/rcoder-compose-dev-build-final.log` 与 `/tmp/rcoder-compose-app-runtime-final.log`，构建结果待收取。本次包含前述数据库及原生修复，不能沿用上一组镜像身份。尚未切换 Compose、提交或发布。

### 严格 E2E PG 清单补齐

原 pg_storage_faults 仅固定 ProjectStore lifecycle_tests 子集与一条 UserApp 聚合测试，漏掉本轮发现问题的选主、重开会话索引、session miss 回源；也没有固定独立 PG owner 的 CAS/锁等待反例。新增 11 个显式目标至 storage_contract_cases.py，执行器核对二进制包含所有目标并逐个严格执行（ignored 场景必须 include-ignored）；contracts.py 同步必需断言。没有降低旧清单或成功判据。

工具测试 92/92 通过，退出 0，日志 `/tmp/rcoder-e2e-tools-expanded-pg.log`。实际执行 `E2E_SUITE=pg_storage_faults make test-e2e` 退出 0，报告 `tests-e2e/reports/50c547c7ae1c494ebe21f8c0b326f310/summary.json`。单个外层场景通过，内部 23 项必需断言全部通过（21 个真实数据库用例、环境就绪、归属资源清理）；不将外层 1 PASS 当作整个 UserApp 组通过。

最新镜像 dev-build 完成：master `sha256:d518a506b0b66c82c61d5f2dfc16c6542ce7ea9b28992fcfa709a7407a5e46ea`，builder `sha256:9797684a6af819a41c9591ce64776047f2acd11df8a66cb38068629a9f1c36f7`。从本轮刚构建的 master 提取 `/app/bin/rcoder` SHA256 `34515481700e634f45cc0183c1adc5da4b3db5b94c696a5bfbc58e2c22b5904f`，作为后续 Turso 容器重建门禁预期值。app-runtime 构建继续，尚未部署验收。

### 三份 Compose 的真实 Turso 重建门禁

`E2E_TURSO_BINARY_SHA256=34515481700e634f45cc0183c1adc5da4b3db5b94c696a5bfbc58e2c22b5904f E2E_SUITE=turso_compose_runtime make test-e2e` 退出 0。报告 `tests-e2e/reports/82935c2a53134d7cbdccd050dda99675/summary.json`；三份 Compose 各 7 项必需断言，共 21 项全部通过，包含首次创建扇入、单 builder 身份、HTTP 持久化、容器重建身份保留、非法配置启动拒绝及本轮资源清理。使用本轮新 master/builder 镜像与隔离数据目录，旧日常 Compose 与旧库未被替换。

检查日常启动脚本时另发现 target volume 的旧 rcoder 优先于镜像 binary，会令 dev-restart 测到旧代码。`dev-hot-build.sh` 本已把产物原子安装到 `/app/bin/rcoder`，因此 start-rcoder.sh 现统一执行该路径：重启保留容器内热更新，重建容器使用镜像；缺少可执行文件明确失败。删除热编译中无必要的另一 target 产物删除，更新 dial9 重建说明。bash -n 与 diff check 通过；日常 Compose 的实际 PID1 二进制核验待部署后执行。

### PG 初始化 service 环境回归（未完成 E2E）

dev 改密场景报告 `tests-e2e/reports/a0d46771f24d481fb6e19df7a3e3f928` 未通过：PG 接受连接但 dev 数据库不存在，受管理账号保护断言得到 PG 未就绪；整轮同时被源码漂移门禁判失败，不能计入验收。独立 PG 16 镜像复现：将 PGSERVICE 设置为空串仍会查找空 service，psql 报 `definition of service "" not found`；初始化检查重试、createdb 未执行。两仓 pg-admin-identity.sh 与 shared_types 命令统一改为 `env -u PGHOSTADDR -u PGSERVICE`。独立测试容器替换脚本并重启后自动创建 dev 库，实际 SELECT 1 成功。Dev 改密准备改为只 ensure builder 管理通道，后续继续通过捕获的管理员身份检查 PG，不再前置依赖业务库。

新增环境隔离行为反例通过。首次 pg_utils 测试 20/22：旧前缀断言未更新；SQL 私密字面量测试将 PATH 限为临时目录导致 env 不存在。修正预期与测试 PATH，保留 SQL 完整性、特殊字符不执行和 TCP 等断言。复跑 `cargo nextest run -p shared_types --no-fail-fast -E 'test(pg_utils::tests)'` 退出 0，22/22 通过，日志 `/tmp/rcoder-pg-service-env-tests-fixed.log`。shell 协议测试 6/6 通过。最新修改仍需 rcoder 编译检查、更新实际镜像和改密 E2E 复验；不能用这些组件结果声明改密链已完成。

### 并行验证与最新 PG 编译检查

用户授权 Compose 与三平台并行。三平台子任务仅操作远端独立快照、独立 target 和 /tmp 报告，不修改本地源码；Compose 仍由主任务串行更新镜像和运行 E2E。最新 PG 环境修复后 `cargo clippy -p rcoder -p shared_types --all-targets --all-features` 退出 0，无 warning，日志 `/tmp/rcoder-pg-management-clippy.log`。

源码漂移排查发现新隔离数据库目录 `docker/data/rcoder-toasty-20260920-92bcpv7a/` 的数据库和 WAL 在 Git 未跟踪文件清单内，被 E2E 源码指纹纳入。将该确切本地数据目录加入 Git info/exclude，不改公共源码漂移判据，不忽略其他源码。后续正式测试仍必须通过前后指纹一致检查。镜像更新正在执行，不能沿用旧镜像通过记录。

最新 PG 修复聚焦验证：`cargo nextest run -p rcoder -p shared_types --no-fail-fast --all-features -E 'test(db_password) | test(pg_utils)'` 退出 0，30/30 通过，592 未选中；日志 `/tmp/rcoder-pg-management-tests.log`。PG 初始化及进程树脚本回归在 Linux builder 容器内执行，8/8 通过，日志 `/tmp/rcoder-pg-shell-linux-tests.log`。此前误在 macOS 上执行同组 Linux-only supervisor 测试失败（waitid 能力拒绝），记录在 `/tmp/rcoder-pg-shell-final-tests.log`；不改生产平台边界来迎合该错误执行环境。真实改密业务 E2E 仍待更新镜像。

### 2026-09-20 阶段性 Git 检查点（WIP）

按用户要求保存当前代码，不代表验收或发布完成。包含 Toasty 全量迁移、新 schema/CAS、版本化运行凭据及 PG 管理恢复、app-cli/file-server-proxy 原生修复和配套测试文档；镜像仓库的 PG 启动脚本另行提交。

追加实机反例：Linux 空格/中文 workspace 的 Stop 修复前返回 409，修复后两个测试及真实 Vue devbuild/devrun、重复 serve、Stop 全链通过。Windows owner 强杀的进程树反例修复前失败（run 66857e08-9218-49e3-aea6-3291af9b5e26），添加 KillOnDrop 后同反例通过（run 399d0599-17ac-41d0-ab61-316daf1b5ebe）；不会据此声称整个三平台矩阵通过。file-server 的 canonical 项目身份、opaque workspace_id 消费和真实 202 回执解析已完成代码及格式检查，组件检查待补。

检查点明确未完成：file-server 外部操作的 durable intent/响应丢失重放保护；本地 ArtifactId 在源目录 owner 下的路径核对及能力声明；本轮追加改动的统一组件回归；最终镜像更新及完整 Compose、remote K8s、三平台矩阵。现有构建仍运行，不能将此前组件通过或旧镜像结果视为该检查点全量验收。尚未 push、npm 发布或 K8s 镜像发布。

### PG 改密 Compose 反例复验通过

固定4e51f476源码快照执行 `E2E_SUITE=compose_userapp_dev E2E_FILTER=userapp_dev_pg_reset_password` 严格启动器，退出0、1/1通过，无skip。报告 `tests-e2e/reports/571616cd72a64a1fb1b45180f8c7277f/summary.json`，前后源码摘要均为 `ef1a124c984e7075bb6ace03a2691e800c7d06f6a6acbdd169407ce7e903785e`。测试程序从独立快照编译，不使用活动工作树的漂移豁免。被测master镜像 `sha256:a28b82ac1e7e2f822f82538cce85349216564aa824ab8e9bce4ae8e94df73c2f`，builder镜像 `sha256:1574125121c618308789ba8fd22e58d98651cbc562d2f452b47341f41cef9a6c`。

验证包含：开发容器注册与数据库初始化、受管理运行账号拒绝直接改密、独立账号创建/改密、同请求改参拒绝、新操作改密、旧请求迟到重放及真实TCP验证未覆盖新密码。修复前对应场景PG接受连接但dev库未创建；本轮该链通过。不代表prod待生效配置/冷切换或完整Compose组通过。后续file-server持久意图和ArtifactId目标修复不在本轮被测镜像内。

默认存储feature新增PG专用枚举分支条件编译后，`cargo clippy -p rcoder-storage --all-targets`退出0、无warning；日志 `/tmp/rcoder-storage-default-clippy-checkpoint.log`。

### 外部 owner 持久意图与路径回归集中检查

首次file-server编译发现测试变量与sha2摘要格式化不兼容，修正后342例中341通过；唯一制品拒绝测试仍共用默认日志目录，未发预期请求。隔离测试状态目录并保留严格提交/拒绝/目录未改变断言后，合并native路径写入修复的 `cargo nextest run -p file-server --no-fail-fast --all-features` 348/348通过、0skip，日志 `/tmp/rcoder-file-server-intent-path-tests.log`。Clippy首次有collapsible_if提示，等价修正后 `cargo clippy -p file-server --all-targets --all-features` 退出0、无warning，日志 `/tmp/rcoder-file-server-intent-path-clippy-clean.log`。

仍需补外层Start仅凭登记快成功的问题和稳定调用方请求身份；348通过不证明该尚未修改的外层链已完成。Windows读侧junction入口也在继续核查，不能把写入修复称为全部路径访问已验收。

`make dev-build`及`make docker-build-app-runtime`串行任务退出0；本轮app-runtime镜像`sha256:0b963b611bcf65876c233905bfa294a269cad7427444f49d2897192e5cc7d1bf`。之后继续固定4e51f476快照的userapp_deploy_full_chain场景，结果待收取；镜像未含后续持久意图/目标目录修复，最终仍须统一更新。`make remote-k8s-doctor`退出0，仅证明SSH/集群/CRD/存储/registry前置可达，没有执行本轮K8s部署。npm launcher本地包内容回归8/8通过，未做npm发布。

### 2026-09-20 固定快照部署链失败：PG 接受连接早于业务库创建

固定4e51f476快照的 `userapp_deploy_full_chain` 退出1，报告 `tests-e2e/reports/6baf36d8f0f948a2ac90273079518546`。制品构建、部署受理、操作重放断言通过，但七路业务服务就绪全部失败。归档容器日志显示22:02:23.486 app-cli报告PG ready，22:02:23.563迁移报数据库dev不存在。`pg_isready`只确认服务接受连接；容器PG与建库任务并行运行，建库轮询存在1秒间隔，不能作为迁移前置就绪证明。该缺陷正在修复，不能将部署API成功记为业务成功。

E2E资源清理前日志采集追加PG stdout/stderr，仍沿用环境凭据脱敏；避免失败清理后丢失初始化证据。最新恢复接口与请求身份改动已以4a34eb93保存，尚待集中组件验证；未push或发布。

### 恢复接口与 app-cli 集中组件回归

- file-server/file-server-userapp 首轮编译发现 AppError→anyhow 的错误转换缺口，已修；第二轮437/439，两个失败为新增ToSchema类型未集中在models、OpenAPI缺少summary，已按原工程契约修复而非降低断言。最终 `cargo nextest run -p file-server -p file-server-userapp --no-fail-fast --all-features`：439/439，0skip，退出0，日志 `/tmp/rcoder-recovery-handlers-pass.log`。Clippy的测试模块布局与无用常量告警已清理，等待workspace最终核验。
- app-cli首轮260项中259通过，唯一失败是新目标恢复测试在macOS比较了/var与/private/var两种同目录写法；测试改为与生产契约一致的canonical目标，未改生产路径保护。修正后260/260，0skip，Clippy退出0；日志 `/tmp/rcoder-app-cli-pg-ready-final.log`、`/tmp/rcoder-app-cli-pg-ready-clippy.log`。随后PG URI凭据优先级/环境继承补充修正尚需追加验证，不能将此结果套用到后续未测试源码。
- 镜像内真实psql证明 `PGDATABASE=postgresql://...` 不解析URI，仍连接默认socket；已改为解析后传libpq环境，密码不进入argv。PG延迟建库真实反例正由独立Linux测试运行。
- 恢复接口的Completed记录仅能查询；原操作404不得重新发送历史Stop/Restart。Stop清登记新增原operation ID与owner instance核对，旧Stop迟到不得删除同实例新Restart的登记。真实handler及对应时序反例已包括在439项中。

本轮未更新运行中Compose镜像，完整Compose/K8s与发布仍未完成。

### 实际 PostgreSQL 延迟建库反例与全仓接线复核

`tools/test_pg_readiness_real.py` 已纳入仓库，可对已有PG16镜像执行。Linux隔离fixture：先确认PG能登录postgres库，目标delayeddb尚不存在，首次探测后延迟4秒建库；显式执行ignored的真实数据库测试，4.244秒后通过。nextest run ID `ec50b29a-b23f-4138-8f6d-2816a402bb6f`，1项选中且通过，其他261项为本次筛选排除，不能计入通过。日志 `/tmp/rcoder-native-linux-followup/native-linux-pg-real-reproducible.log`。fixture容器已清理，未操作K8s或现有数据库。普通组件套件不自动运行该真实环境测试；脚本缺Docker/镜像/工具会明确失败。

PG探测还统一TLS/session配置继承、managed凭据覆盖URI所有user/password查询项、成功出口取消复查。Linux聚焦4/4通过，日志 `/tmp/rcoder-native-linux-followup/native-linux-pg-probe-final.log`。该证据是实际PG探测，不等同整个业务Compose部署链通过。

完整workspace全features回归2476项：2470通过、6失败、16环境门控跳过，退出100。四项PG失败是已批准`env -u PGHOSTADDR -u PGSERVICE`修正后的旧前缀夹具不匹配，修正断言保留真实验证命令和执行次序保护。另两项揭示新recover路由未接主服务转发与runtime枚举描述缺项，正在补齐；不归为基线失败，不降低原路由/OpenAPI断言。日志 `/tmp/rcoder-workspace-final-allfeatures.log`。

### 主服务接线修正后的集中回归通过

新恢复API已实际注册到RCoder转发路由，使用body app_id定位dev builder；header/body身份冲突拒绝，prod header不改变固定dev语义，缺失原容器不自动创建。新增真实Router→forward handler→runtime反例，补全RuntimeOperationView枚举文档。

- `cargo nextest run --workspace --no-fail-fast --all-features`：2477/2477通过，16环境门控跳过，退出0；`/tmp/rcoder-workspace-recovery-allfeatures.log`。
- `cargo clippy --workspace --all-targets --all-features`：退出0，无warning；`/tmp/rcoder-workspace-recovery-clippy.log`。
- `cargo nextest run --workspace --no-fail-fast`：2313/2313通过，12环境门控跳过，退出0；`/tmp/rcoder-workspace-recovery-default.log`。发现PG专用测试夹具在默认features无消费者，已把closed_for_test限定test+pg；不影响生产行为，默认Clippy追加核验。
- app-cli独立全features：261/261通过，1项真实PG测试ignored（已由专用工具显式通过）；Clippy退出0；`/tmp/rcoder-app-cli-final-allfeatures.log`、`/tmp/rcoder-app-cli-final-clippy.log`。
- 两个Cargo项目fmt检查及git diff --check均退出0。

开始`make dev-hot`→`make docker-build-agent-runner`→`make docker-build-app-runtime`更新Compose二进制与镜像；本段记录时构建尚未结束。新的完整Compose、remote K8s与发布均未验收。

默认workspace Clippy追加核验退出0，无warning：`/tmp/rcoder-workspace-recovery-default-clippy.log`。PG专用closed_for_test的feature限定已生效。

### Compose 更新与固定输入准备

`make dev-hot`退出0，Linux容器内release构建6分10秒，重启后健康。为隔离Turso场景使用实际新二进制，将该次`/app/bin/rcoder`单独COPY到原镜像基础上（没有docker commit、没有打包运行数据或环境）。测试镜像 `dev-master-rcoder:toasty-4de23bcbd6fc`，ID `sha256:87e53899e9e0a8a53a2af07856f137b5e5c76838db69f2853babd50f5c3f8728`，二进制SHA256 `4de23bcbd6fc73c9b053677bf7015bd51a93165d8a7455bfe992762983d6c426`。日常Compose的rcoder已按该镜像重建并健康，保留原Turso数据目录。

E2E独立快照基于4a34eb93及本轮未提交源码，测试程序预编译退出0。为固定三个Compose输入，Turso测试工具新增可选`E2E_BUILD_AGENT_DOCKER_ROOT`；两份配套仓库Compose被复制到快照内部并纳入输入指纹。三份配置解析全部退出0，配套仓库基线fb12fa6。最终测试输入指纹 `a41f64a459f8688879fdcdf088616558fe24d12b924d87fab991d898576f03ae`（包含本次测试工具与配套配置）；主服务镜像构建时快照指纹为 `d896a659b2e34c165313bc302e2a5fe29c5ef465946c4a143bf7ed2d327fe6d9`，两者生产Rust/Cargo输入逐文件一致，差别是之后补齐的测试目录参数、说明和配套配置。镜像证明与测试输入证明分别记录，不混称同一全仓快照。

本段记录时`docker-build-agent-runner`仍执行，之后串行构建app-runtime；没有开始最终完整Compose，K8s未部署、未push/发布。运行句柄及临时元数据留在当前任务，不能以本段准备工作宣称验收完成。

### e3959112 阶段保存后的继续修复

RCoder 阶段提交 `e3959112` 已创建，未 push。builder 镜像构建退出0，实际镜像 `sha256:037f7637bf3be042291ee346375e8f6927f32bce97b70d24f95c652253fb8d95` 的命令验证退出0：app-cli 0.3.6、Pingap 0.14.3。app-runtime 构建仍在执行，尚不能据此启动新镜像验收。

新增确认的恢复缺口分别处理：

1. app-cli 进程重启后，file-server 旧 pending 意图即使在 owner 磁盘上已有终态，也被 instance 检查挡在查询之前。已新增新 owner 身份核验后的只读原回执查询、终态与摘要校验以及旧登记 CAS 收束；10组 HTTP 场景已写入，尚未运行本轮组件验证。404、未知与身份错误不重放旧请求。
2. PrepareProdDatabase 在 StartSubmitted 后观察失联、但尚未持久化 ManagementReady 时，原槽位缺少显式安全核验恢复路径。已具有 ManagementReady 的旧操作可用既有最终证据恢复，不属于该缺口；正在补原身份与原物理代次下的只读核验及专用存储 CAS。

此前默认/全feature检查绑定 e3959112 基线，不能作为上述后续修改的通过证明。固定快照 Compose 仍验证其原始输入；后续修改需要补对应组件及部署验证。

### 两项恢复补丁的组件验证

`cargo nextest run -p app_manager -p rcoder-storage -p file-server --all-features --no-fail-fast` 退出0：721/721通过，4项环境门控跳过，日志 `/tmp/rcoder-recovery-followup-tests.log`。覆盖新 owner 原终态只读收束的10场景，以及管理准备迟到成功、断连、代次/UID替换与本地存储完整snapshot CAS。此时新CAS的独立真实PG竞争证据仍需补充，不能由Turso用例代替。

`cargo clippy -p app_manager -p rcoder-storage -p file-server --all-targets --all-features -- -D warnings` 退出0，日志 `/tmp/rcoder-recovery-followup-clippy.log`。根fmt检查与diff检查退出0。两项补丁仍未纳入正在构建的固定Compose镜像，因此不能声称此组件结果代表部署验证。

### 真实 PG 恢复竞争与镜像构建完成

本轮 `tests-e2e/tools/pg_contract.py` 独立PG17测试退出0，24/24必需断言通过，新增 `PG preparation recovery independent owner CAS and retained fence` 包含真实独立连接同snapshot单胜者、原槽位/lease保留及旧revision拒绝。证据 `/tmp/rcoder-preparation38281297824e4ed5/pg-contract/assertions.json`；启动日志 `/tmp/rcoder-preparation-pg-contract.log`，测试自身已执行所属资源清理。这是存储组件实际PG验证，不是remote K8s验收。

`make docker-build-app-runtime` 退出0，runtime镜像ID `sha256:53d3078f8034f161cc378c7af161ab443910c5100250e3293e0778b1b353619b`。与builder配对工具退出0，两者Python ABI均为cpython-313-aarch64-linux-gnu，Java major均25；证据 `/tmp/rcoder-compose-final-toolchains.json`。固定快照的 `userapp_deploy_full_chain` 已启动，日志 `/tmp/rcoder-compose-final-focused.log`，结果待定，未更换为包含后续恢复补丁的镜像。

### 原生恢复后的显式 Restart 衔接

Mac真实链已发现并修复“恢复时清旧登记后，Restart先Stop被无登记保护拒绝”的问题。UserApp Restart现在先通过既有同项目身份核验与认证入口，提交单个Restart；不先Stop，也不扫描进程接管。无owner时保留原本本地路径。新增反例核对单次POST、原revision和新请求context，异项目请求零POST。

聚焦两项反例2/2通过（其余352为筛选排除），日志 `/tmp/rcoder-native-restart-followup-tests.log`。追加 file-server 完整回归354/354、0skip、退出0，日志 `/tmp/rcoder-native-restart-regression.log`。原生Mac完整Restart链仍在实测，不能用组件结果或Start通过代替。

### 固定快照部署链：PG建库修复闭环，CR10代次交接缺口暴露

报告 `tests-e2e/reports/750d8ede9d7a4bd3a0c973251bf0ab51`，退出1，63个硬断言通过、3个失败。七服务构建（270秒）、全部真实代理就绪、热部署与制品身份断言通过，原PG建库早报ready故障未复现。失败根节点是CR10显式凭据Restart操作 `5967fc71-0870-4daa-b855-9afd581e9210`，随后Stop与Delete被RecoveryRequired保护拒绝，两者是后果而非独立根因。

归档prod日志明确：新owner报告 `deployment journal generation does not match this owner; explicit deployment required`。控制面在没有制品的配置Restart中注入新operation/generation并移除旧URL，但保留旧卷/journal；没有可信旧→新代次交接，且PG修改先于业务激活确认。不可删除journal、放宽所有代次检查或清锁绕过。正在实现受控交接和改PG前的管理准备屏障，同时覆盖已停止容器的显式Start，不用普通wake先拉起业务。

现场资源已由严格E2E按所属身份归档与清理，Turso仍保留操作保护；不把测试清理当操作恢复。完整Compose/K8s验收仍未通过。
