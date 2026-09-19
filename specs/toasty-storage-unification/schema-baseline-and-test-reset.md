# SQL 初始化基线整理与个人测试 PG 重建

原规划日期：2026-09-19（当时仅完成文档，未连接远端或清库）。

实施更新（2026-09-20）：四份最终 SQL 已实现于 `crates/rcoder-storage/schema/`，分别为 `userapp-pg-v1.sql`、`project-pg-v1.sql`、`preview-pg-v1.sql`、`userapp-turso-v1.sql`；统一初始化与组件/独立 PG 反例已有验证，详情见 [验证记录](verification.md)。下文目标目录是原规划布局，当前使用上述四文件布局，业务域与版本账本要求不变。个人 K8s 目标库切换、双副本部署及完整业务验收仍待执行，不能由独立测试库通过替代。

配套：[需求](spec.md)、[技术计划](plan.md)、[库表设计](schema-design.md)、[任务清单](tasks.md)。表结构以库表设计为准，本文件规定 SQL 文件整理、初始化及测试环境切换。

## 1. 已确认的决策与授权

- 数据库后端未进入上一正式版本，没有线上历史数据库兼容包袱；本次直接采用最终设计的新 schema。
- 用户明确：个人两节点 K8s 测试集群里的目标 PG 库全部属于 RCoder，数据允许删除，可用于本方案的部署验证。
- 实施阶段可以停止该测试环境的 RCoder 写入者、重建这个 PG database、部署新版本并测试；无需再次请求相同范围的授权。实际主机、SSH 身份、认证与连接信息使用未提交的 `.env.local` / 现有集群 Secret，不写入本文件或报告。
- 授权不等于每次启动自动清库，也不扩大成删除整个 K8s namespace、PG 实例、所有数据库或 PVC。数据库重建不需要删除 PG 数据卷；agent PVC 和共享 CephFS 数据继续遵守 AGENTS.md 的保护规则。
- 本次整理为首次正式发布的基线。正式发布后保留此基线，后续新增 `0002`、`0003`；不能因想减少文件数再次改写已发布历史。

## 2. 当前文件为什么多，哪些可以收敛

在源码 `2786632a` 上，共 15 个 SQL 文件：

| 现有目录 | 数量 | 内容与处置 |
|---|---:|---|
| migrations/ | 6 | Project/Container/Session、活动、旧 metadata、代次、user_id 增删；按最终设计拆回 project/userapp 域 |
| migrations-userapp-pg/ | 7 | 逐步增加生命周期、请求别名、执行输入、资源绑定、租约、期限、scope；合为 UserApp PG 最终初始化 |
| migrations-userapp-turso/ | 1 | 旧 UserApp 最终态 schema；按新设计重写，不能仅改目录 |
| migrations-preview-pg/ | 1 | Preview；补最终约束与并发方案后作为新的初始化 |

现有四套版本账本也同时退役：PG 主域 `_sqlx_migrations`、UserApp `_sqlx_userapp_migrations`、Preview `_sqlx_preview_migrations`，以及 Turso `_turso_userapp_migrations`。

依据：`src/pg/mod.rs:113`、`src/userapp_lifecycle/postgres.rs:37`、`src/preview_lifecycle/postgres.rs:39`、`src/userapp_lifecycle/turso/migrations.rs:16`。以上相对路径前缀均为 `crates/rcoder-storage/`。

### 目标目录

```text
crates/rcoder-storage/migrations/
├── postgres/
│   ├── userapp/0001_init.sql
│   ├── project/0001_init.sql
│   └── preview/0001_init.sql
└── turso/
    └── userapp/0001_init.sql
```

**逻辑上三个业务域、物理上四个 SQL 初始化文件。** 两后端的 UserApp 表语义一致，保留必要的 DDL 方言差异。Project/Preview 继续按现有部署范围使用 PG，不为追求目录对称增加 Turso 实现。

UserApp activity 归 UserApp 域；PG 继续启用该持久化能力，Compose 不因目录调整自动新增 activity 后端。Turso 的 UserApp 逻辑模型可包含对应表定义以保持公共模型一致，但本期不新增原本没有的后台持久化装配；须在能力矩阵注明该差异，不能虚报已启用。

不合成一个跨数据库的大 SQL 文件，也不把 `ADD user_id` 和 `DROP user_id` 等历史步骤机械串接。生成最终 CREATE TABLE / INDEX / 约束，移除退役表；业务字段、注释、索引一并审核。

## 3. 统一版本账本与迁移执行

### 版本账本

`rcoder_schema_migrations` 由统一 runner 在迁移锁保护下初始化，主键 `(component,version)`，列包含 `name,checksum,applied_at_us`。同一个数据库只需要一张，不让各业务域分别复制建表代码。

- PG 的 userapp/project/preview 各有自己的版本 1；Turso 本期只有 userapp。
- 初始文件 checksum 按嵌入的实际脚本字节稳定计算；执行语句清单必须由同一份脚本得到，不维护一份校验 SQL 和另一份手写执行 SQL。
- 支持多语句执行时使用经探针验证的驱动接口；否则用经过验证的 SQL 解析/构建步骤产生有序完整语句。**禁止 `split(';')`**，注释、字符串、DDL 块中的分号不能破坏初始化。
- 支持的 component、版本与依赖顺序由显式 manifest 声明。所有已启用域就绪才启动业务任务；只完成第一个域不允许服务进入 Ready。
- 启动校验 checksum、未来/未知版本、版本记录连续性、已启用域缺表和关键约束。发现旧 SQLx/Turso 账本或没有新版本标识的既有业务表，明确报旧开发 schema，不能自动标记为新 v1。
- 检查与迁移必须指定同一 database/schema；不得让迁移用一套 search_path、业务连接用另一套。当前配置没有独立 PG schema 字段，本期沿用现有明确连接配置，不虚构 `RCODER_PG_SCHEMA` 环境变量。

### 初始化事务

- PG：统一数据库级命名空间的 advisory 迁移锁，所有副本/业务域采用相同键与锁顺序；先持锁再读账本。单域 DDL 与版本写入同事务；跨域按 manifest 顺序，失败时不启动业务，下次从已经成功的域继续。
- Turso：先取得目录独占，再在同一连接的事务内应用 DDL 与版本；每条业务连接启用并验证所需 PRAGMA，尤其 FK，不能只有迁移连接启用约束。
- `IF NOT EXISTS` 只能用于在锁保护下建立账本等明确可校验路径，不能用它把缺约束/半建表的业务 schema 当成功。
- 不使用 Toasty 开发期 `reset_db` / 自动 schema push 作为生产初始化机制。

## 4. 必须同步处理的代码与测试引用

1. 原四个初始化入口改为统一 runner；根据部署模式传入明确 component 清单，不能让子仓库构造函数各自盲跑独立迁移。
2. 移除所有 `sqlx::migrate!`、旧路径 `include_str!`、旧账本查询与测试修改语句，更新 PG 测试库工厂和 Turso 初始化测试。
3. `pg/project_store/lifecycle_tests.rs` 含直接读取旧 `0001`/`0004` 的升级测试。因无已发布数据库，新基线不再承诺这条旧开发升级链；将测试改为新基线的身份/墓碑/旧写拒绝行为，不删除这些业务断言。
4. 旧 metadata 导入测试和接口在确认无运行期消费者后退役；对应元数据保存、CAS、重建隔离覆盖迁移到新的 userapps 模型契约测试。
5. 检查 Docker/Helm/remote-k8s 与 E2E 工具是否读取旧表或依赖目录；修改当前入口，不改写历史验证报告。

建议检查入口（实施时依据实际输出处理）：

```bash
rg -n 'sqlx::migrate!|_sqlx.*migrations|_turso.*migrations|include_str!.*sql' crates tests-e2e tools
rg -n 'migrations-userapp|migrations-preview|0004_lifecycle_identity|userapp_metadata|publish_tasks' crates tests-e2e tools docker
```

## 5. 个人测试 PG 的一次性切换流程

这是**已授权的开发基线重建**，不是生产迁移流程，也不是修复失败用例的通用手段。

### A. 准备可部署版本

先完成 T0–T4、聚焦组件测试和新 schema 反例验证，再构建可部署镜像。无需为文件整理提前清空当前测试环境。记录源码 SHA/摘要、镜像 digest 和四个初始化 checksum。

### B. 核对实际目标

读取 `.env.local`、当前 context/namespace、RCoder 的 `RCODER_PG_*` 或连接 URL 来源，查询并记录 `current_database()`、`current_schema()`、数据库 owner 和非敏感表名清单。核对是用户已授权的个人测试库后继续，无需重复询问授权。Secret/完整 DSN 不输出到日志。

用户已经确认该库专属于 RCoder，不再把“可能含其他业务数据”列为待确认问题。这里核对是为了防止连错机器或把 PostgreSQL 实例与目标 database 混淆。

当前源码 `tools/remote_k8s/manifests.py:63-74,110` 创建和连接的是该工作流 namespace 内的 `rcoder` 数据库；Helm 模板也有自己的 PG 配置来源。因此同一个测试集群上使用哪套部署就核对哪套实际连接，不能清掉 Helm 的库，却只验证另一套 remote-k8s 新库，然后宣布同一环境验收通过。运行记录明确目标部署与数据库。

### C. 停止旧写入并处理运行资源

1. 停止该环境的新业务请求和测试启动器，让可完成操作收束；在旧库仍可读取时记录当前生命周期、未完成操作及其外部资源身份。导出诊断只包含必要身份，排除执行输入/令牌。
2. 已确认可正常停止的测试业务按现有生命周期协议停止。不能收束的旧实例标记为隔离对象，保留必要身份记录；新测试使用不冲突的新 app/project ID。清库后不能仅凭同名资源就重放旧 operation 或接管旧实例。
3. 停止所有连接目标库的旧 RCoder 副本、writer/recovery/选主任务及临时测试进程；防止部署控制器/HPA/GitOps 又拉起旧版本。保持 PG 本身运行。
4. 确认旧会话/任务已退出。PostgreSQL 连接关闭仅证明 DB 写入终止，不能证明此前发出的 K8s API 写已结束；旧测试资源的收尾必须按其身份记录处理。

不为清数据库批量删除 ConfigMap 租约、agent PVC 或共享卷。旧测试计算资源确需清理时按精确资源身份单独处理；清理未知结果仍保留隔离记录，不把丢失数据库记录当作安全释放证明。

### D. 重建目标 database

从维护数据库连接执行目标 database 的 drop/create，恢复原 owner、编码及必要授权；工具对数据库名做标识符转义，不拼接未校验的 shell/SQL 输入。若仍有连接，先找出并停止对应客户端，避免旧实例立即重连建回旧表。

这是完整 schema 重建，不是 `TRUNCATE`：只删业务行不会改变旧表结构、约束、索引和四套版本账本，也无法安装新的 v1。目标库内旧数据、旧表和旧迁移记录一起消失；不删除 PG 实例和数据 PVC。

已有明确数据删除授权，不把备份或另建 database 作为额外的强制审批条件。需要保留诊断时可先作私有快照，但验收记录不能包含敏感行。

### E. 部署并验证

1. 只启动新版本，验证双副本并发冷启动：每个 component 的 version=1 只有一条记录，schema 完整，两个副本使用同一预期数据库。
2. 通过 `remote-k8s` 对当前源码快照部署验证；先 smoke，再对同一快照运行 UserApp/Chat/Gateway 及本轮补充的 Preview/PG 并发用例。不要在测试中途替换镜像。
3. 用正常接口创建应用、项目、会话和预览，验证 dev/prod 独立、请求重放、端口竞争、恢复保护和跨副本一致性。
4. 保留本轮数据，滚动重启/全部 RCoder 重启后重复查询、续用会话和恢复测试；不得再清库来获得通过。对未知结果按原身份恢复，不自动超时解锁。
5. 对账本 checksum、未来版本、半初始化等破坏性反例使用独立测试库，不篡改正在验收的库。

源码再修改则重新 `verify`；已有镜像上追加测试使用 `test`。命令与前置以 [remote-k8s 说明](../remote-k8s-dev/README.md) 和当前 Makefile 为准。此文不新增能误清任意环境的 `make reset-db` 默认入口。

### F. 失败与回退

数据库模式切换期间禁止新旧 RCoder 混跑。旧二进制不能指向新 v1 库；需要回退时先停止新版本，使用与旧二进制匹配的独立测试库/此前快照，或者重新建立旧测试 schema。若不需要旧版本，保留失败现场并修复新实现，避免反复清库抹掉证据。

## 6. 合并完成标准

- 旧 15 个 SQL 的有效结构按新设计收敛为 4 个文件；退役表/字段不出现在新库，仍需要的约束和注释没有丢失。
- SQLx 迁移调用与四套旧账本不再参与运行；新 runner 有 checksum、版本、schema 完整性与并发初始化验证。
- 空库初始化、正常重启不重复执行、两个 PG 副本同时启动、中途 DDL 失败回滚、未来版本拒绝分别有结果。
- 库表设计 S01–S12 及原有有效契约通过；缺陷修复有反例，不靠删除旧业务断言。
- 个人测试 PG 的重建目标、停止写入、旧资源处置、源码/镜像/DDL 身份与完整业务报告可对应，报告不含密码。
- 首次正式发布打点后冻结 v1 内容；后续变更添加迁移与升级测试。
