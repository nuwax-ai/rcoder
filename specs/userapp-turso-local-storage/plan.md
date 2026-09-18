# Turso 本地存储实施方案

## 1. 审查基线与参考

- 本地 Turso 源码：`/Users/soddy/Documents/git-workspace/turso`，审查 SHA `502e48583c7ef1660ee6583a2fd60cd72b1a7a05`，workspace 版本 `0.8.0-pre.11`。
- 必读其 `COMPAT.md`、`bindings/rust/src/connection.rs`、`transaction.rs`、`bindings/rust/Cargo.toml`；官方 https://github.com/tursodatabase/turso 明确已有生产使用，但实验功能另有边界。
- 本文是源码分析后的实施设计，尚未运行 Turso 兼容性或性能测试。实施时记录新的源码基线与实际依赖版本，不将本地 checkout 版本冒充 crates.io 已发布版本。
- RCoder 实际入口：`crates/rcoder/src/config/userapp_storage.rs`；存储：`crates/rcoder-storage/src/userapp_lifecycle/{mod,domain,sql,sqlite,postgres}.rs`；现有 SQLite 独占与恢复逻辑位于 `sqlite/`。

## 2. 依赖、模块和配置

### 2.1 后端边界

**2026-09-19 范围确认：保留 PG 与 Kubernetes 的现有 feature/初始化限制；不新增 userapp-pg，不实现 Compose PG 或多副本。业务 trait 按 [trait-design.md](trait-design.md) 调整。**

新增 `rcoder-storage/turso`，默认 RCoder feature 改为 `userapp-turso`。移除 `sqlite` / `userapp-sqlite` feature、SqliteUserAppStore 和 SQLite 专用 SQLx 依赖。SQLx 只用于 PG；检查 app_manager 的 dev-dependency、tests-e2e 及所有 feature 转发。不要误删其他业务需要的 SQLx。

推荐 `turso` 可选依赖关闭默认 features，避免无需求的 FTS/分配器；核查实际 feature 传播。选择可复现的正式发布依赖或明确 git revision，禁止发布配置依赖开发机绝对 path。本地源码可用于开发探针。提交必要 Cargo.lock 更新，不要求无关发布构建统一加入 --locked。

模块建议：`turso/{mod,worker,transaction,migrations,restart}.rs`，独占目录校验抽到后端中性模块。按职责拆分即可，不机械制造全部文件。保留领域校验；SQLx `sql.rs` 宏仅 PG 编译，避免 Turso-only 构建解析 SQLx 符号。不要为了两种驱动构造新 ORM。

### 2.2 配置一次性切换

- enum：Auto / Turso / Postgres；Auto 在 Docker 选 Turso，K8s 选 PostgreSQL，K8s 显式 Turso 拒绝。
- 新变量：`RCODER_USERAPP_STORAGE_BACKEND=turso`、`RCODER_USERAPP_TURSO_PATH=/app/data/userapp.turso.db`。
- 默认相对路径：`data/rcoder/userapp.turso.db`，启动时转绝对路径并校验。
- 移除旧 sqlite 值与旧路径字段的回退；检测到非空旧变量应报配置错误，不静默忽略。明确配置其他合法后端时不能被旧字段悄悄覆盖。
- 新目录正常创建；既有目录中的旧 userapp.sqlite3 不删除、不自动打开、不自动迁移。新库不存在且旧库存在时 fail-fast，指导使用独立目录；已有新库时按新配置正常打开。

## 3. 数据库执行与事务所有权

第一版采用一个专用数据库线程，线程内运行适合 Turso API 的异步执行器并独占连接；主 Tokio runtime 通过有界队列发送完整存储操作并用 oneshot 接收结果。这样隔离可能同步执行的磁盘 I/O，并避免不同请求交错使用同一连接的事务。连接在该线程内创建和销毁，不强行声明 Send/Sync，不引入 unsafe。

队列单位是完整 UserAppLifecycleStore 方法，不是 SQL 语句。线程内顺序完成全部语句、commit/rollback，再处理下一任务；runtime HTTP/K8s/Docker 调用不能进入数据库事务。请求参数使用拥有所有权的数据，返回值保持 shared_types 契约。

### 取消与 deadline

- 入队前取消/队列等待超时：保证未执行并返回明确错误；队列满不能静默丢写。
- 入队后、执行前若发现接收端关闭，可跳过尚未开始的操作。
- 已开始执行：调用方丢弃 future 不得中止数据库线程中的事务；完成 commit 或 rollback。可能已提交但回包丢失时，通过原 request_id/operation_id 查询和重试，不能宣称“超时所以未执行”。
- 不对事务 future 简单套 timeout 后 drop 并复用连接。需要超时中止时必须显式回滚、确认连接可用；失败则隔离连接并令后端不可写。
- commit 报错不能报告成功或盲目重放。对结果未知保留操作身份，连接状态无法确认则阻止新写；重新打开并核实 durable 状态后按恢复规则处理。
- API 本身支持 Transaction Drop 不代表回滚已完成；所有错误路径显式处理 rollback，保留主错误与清理错误上下文。

### 启动与关机

获取规范化目录的独占文件锁 → 创建 worker/连接 → 校验配置 → 迁移 → 重启隔离 → 对外 ready。整个期间锁归数据库 worker 的生命周期持有。

停止时先停止业务生产者与恢复扫描器产生新工作，再关闭数据库队列、处理已接收任务、关闭连接并 join worker，最后释放目录锁。为 AppState/main/shutdown 提供明确关闭句柄；不得仅依赖 Arc Drop 关闭异步事务。不得在 executor 上同步阻塞 join。关机超时需可观测，不能报告全部刷新完成；异常退出由下次恢复兜底。

## 4. SQL 与持久化

- 使用普通 WAL 事务，显式配置和读回 `journal_mode=wal`、`synchronous=FULL`、`foreign_keys=ON`；按选定版本验证 busy_timeout。
- 使用查询 API 读取返回行的 PRAGMA，不把它们当作无结果 execute；第一版禁用实验 MVCC/BEGIN CONCURRENT。
- 移植 BEGIN IMMEDIATE 或经验证具有同等排他语义的接口；保留全部原子更新和身份验证。即使单连接，也不能移除 revision/CAS/唯一约束。
- 验证参数占位、JSON null/缺失、LEFT JOIN、ON CONFLICT、唯一键、外键、CHECK 和所有 rows_affected 判定。Turso execute 返回值必须实测 UPDATE/DELETE/CAS/DO NOTHING，不能假定与 SQLx 等价。
- 本地无存量：建立新的 `migrations-userapp-turso` 初始 schema，直接采用当前 dev/prod/application 结构；无需执行旧单指针迁移。PG 迁移历史保持不变。
- 实现小型版本迁移器：版本、校验和、成功提交记录；一个迁移与版本记录在同一事务，失败全回滚；高于程序支持版本/校验和变化应拒绝启动。迁移成功前不运行恢复扫描或对外提供服务。
- 历史 SQLite 迁移用例可被新库初始化及 Turso 升级失败反例替代，但幂等、CAS、损坏阻断、作用域和恢复行为测试必须保留。
- 将 restart::quarantine 的业务规则移植到 Turso，不清租约、不重放远程写、不把未知结果转 Failed。Pending/终态及 revision 的行为与当前契约一致。

## 5. 文件系统与测试观察边界

保留独占目录、软/硬链接、相对路径、锁文件和远程文件系统约束。核实选定版本会使用哪些 WAL/SHM/其他旁文件，覆盖别名检查；不能仅复制 SQLite 的三个后缀。整目录挂载，禁止单文件挂载漏掉日志。

**必须改掉现有 Python sqlite3 直接读取活库的测试路径**：`native_crash_contract.py`、`docker_crash_contract.py`、`sqlite_runtime_contract.py` 及相关测试。Turso 与 SQLite 引擎混合访问运行中同一文件不在兼容保证内。

在线状态用已有应用查询接口。需要内部 durable 记录的故障检查，在主进程确实停止并获得相同目录独占锁后，用同版本 Turso 测试 helper 读取、输出窄化 JSON；该只读观察模式不得调用会 quarantine 的正常启动入口。保持“故障后原始记录”与“重启隔离后记录”两份证据。不得为了观察而新增生产管理后门。

## 6. 改动清单

| 范围 | 必查路径/事项 |
|---|---|
| 依赖 | 根 Cargo.toml/lock，rcoder-storage、rcoder、app_manager、tests-e2e Cargo.toml |
| 配置装配 | rcoder config/userapp_storage.rs、AppState、main/shutdown；所有环境变量/配置样例 |
| 存储 | userapp_lifecycle 模块、SQLite 替换、PG 条件编译、领域规则复用、新迁移器 |
| Compose | docker/docker-compose.yml、docker/start-rcoder.sh、SQLITE.md、隐藏 .env 示例、named-volume 覆盖、Docker/git ignore |
| 测试 | lifecycle-crash-worker.rs、sqlite_compose_runtime.rs、所有 sqlite_*contract.py 及其单测；native/docker crash 脚本 |
| 启动器 | tests-e2e/tools/run.py、suite_cases.json、report_identities.json、contracts.py、cleanup.py、README.md、storage-acceptance.md；make/CI 调用点 |
| 文档 | 当前操作手册改为 Turso；历史验证记录不改写，可追加被本方案替代说明 |

配套仓库 `/Users/soddy/Documents/git-workspace/build-agent-docker` 需同步以下实际入口（改动前读取其 AGENTS 与 status）：
- `docker/docker-compose.yml`、`docker/SQLITE.md`。
- `docker-userapp-computer/docker-compose.yml`、`docker-userapp-computer/SQLITE.md`。
- `build_config/rcoder/start-services.sh`。
- 检查环境示例、目录挂载、构建 feature，以及 RCoder 启动脚本一致性检查。K8s PostgreSQL values 不改为 Turso。

改名同步所有引用，不机械替换解释 SQLite 格式兼容或历史事实的文字。删除 SQLx SQLite 后，测试代码不得重新通过 dev-dependency 将旧驱动偷偷带回。

## 7. 验收矩阵

| 场景 | 必须断言 |
|---|---|
| 空目录初始化/再次打开 | schema 版本一致，身份和记录保留 |
| 迁移中途失败/校验和错误/未来版本 | 无半迁移，启动失败，无业务写入 |
| 同域并发、跨域并发、Application 冲突 | 同域单胜者，跨域互不误清槽位，全局删除受保护 |
| 旧 revision/executor/lifecycle | 更新零命中并报告冲突；持久记录不变 |
| 操作插入后故障 | 操作、请求映射、槽位一起回滚 |
| 调用方取消、队列满、worker 故障 | 无事务串扰，无假成功，有界等待，未知提交可按原身份查询 |
| SIGKILL 在提交前/提交后回包前 | 无半事务，幂等重试无重复资源；Pending/RecoveryRequired 正确 |
| 数据损坏、只读目录、磁盘写失败 | fail-fast，不新建替代库、不退内存、不解租约 |
| 双进程与链接别名 | 第二实例被拒，不能先写库再发现锁冲突 |
| Compose 重建 rcoder 容器 | 控制身份/槽位/资源绑定保留；应用卷不受影响 |
| Docker API 在途写 + rcoder 崩溃 | 恢复保持未知结果保护，核对物理 UID，无重复创建 |
| PostgreSQL | 同一契约集用真实 PG 验证，不拿 Turso 通过替代 |

测试命令按实际 feature 补齐，建议顺序：
```bash
cargo nextest run -p rcoder-storage --features turso --no-fail-fast
cargo nextest run -p rcoder --no-fail-fast
cargo check -p rcoder --no-default-features --features rcoder-pg
cargo nextest run --workspace --no-fail-fast --all-features
cargo fmt --all -- --check
cargo clippy --workspace --all-targets
cargo clippy -p rcoder --no-default-features --features rcoder-pg --all-targets
```
单独启动真实 PG 契约环境并显式执行对应测试，不接受环境门控静默跳过。Python 合约单测、三份 Compose 配置解析、聚焦持久化/崩溃场景及完整 `make test-e2e` 均需报告；测试必须使用独立目录。若修改共享生命周期或 K8s 装配，按 AGENTS 使用 remote-k8s 工作流补验，禁止改动他人测试部署。

## 8. 交付边界

输出 verification.md：实际版本/SHA、需求映射、执行命令与退出码、报告、失败归因、未运行项。不得把代码完成当作 Compose/PG 验收通过。未授权不 push、不发包、不部署生产、不删除现有开发数据。只提交本任务相关改动块，保留并行改动。
