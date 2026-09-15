# Tasks：UserApp 移除用户绑定

状态：实现、数据库升级、运行验收、发布均待执行。本次仅完成文档。

## 1. 开发步骤

- [ ] T00 基线：读根及相关 AGENTS、本目录 Spec/Plan、生命周期/并发相关规范；git status 与 scoped diff，记录 HEAD 和并行修改。推荐分支 `codex/userapp-remove-user-id-binding`，本轮未创建。
- [ ] T01 依赖清单：搜索 UserApp user_id/owner/x-user-id/USER_ID_HEADER/复合 key，列出保留的 Computer、历史 migration、兼容测试命中；核对所有 HTTP/WS/流式入口、Java URL 生成和 app_manager 生产请求。
- [ ] T02 身份与路径：纯 app_id builder identifier、固定 userapp 路径 helper；统一字符和资源长度预算。同步 dev/prod 挂载、清理、孤儿检测和公共 validator。
- [ ] T03 共享契约：移除 UserApp DTO、ExecutionContext、Admission、metadata、binding、control、locator 的用户参数；保留 lifecycle/operation/executor/physical UID/服务族等不变量。
- [ ] T04 生命周期：所有 ensure/create/control/recovery/adoption/dev_cleanup 分支去 owner/协作者双轨；去用户 fingerprint，明确旧摘要版本。检查 validate_owner caller 保留其他条件。
- [ ] T05 数据库：追加 migration，覆盖 PG/SQLite 列和版本化 JSON；严格 reader 旧记录兼容、旧 input/alias 不自动重放、未知操作保留保护；准备升级/回退说明。
- [ ] T06 转发和代理：删除 header/body/query/static/tasks 用户解析与回退；UserApp 专属转发边界移除旧 x-user-id；URL pattern 不变，params 不读取 user_id；新 URL 固定 0。
- [ ] T07 Computer 分支：computer_intercept 的 UserApp 分支同步 T06；普通 Computer untouched；常量无用途删，有 Computer 用途迁出 UserApp 契约，不全局清 header。
- [ ] T08 容器侧与生产侧接口：file-server-userapp、app_manager 的 JSON/query/multipart/user scope/响应及 OpenAPI 全部去用户；旧多余字段窄兼容；无假用户补值。
- [ ] T09 Docker/K8s：标签、env、annotation、subPath、资源查询/缓存/锁/清理按服务族更新；UserApp 无宽松旧 selector/复合 key fallback，Computer 保持。
- [ ] T10 单元与契约：运行下表 A/B 组，保留已有 sender/进程监督/取消测试；旧多用户测试转换为同实例与隔离断言，不直接删覆盖。
- [ ] T11 持久化验收：SQLite 文件库和真实独立 PostgreSQL 旧数据升级、重启、中断恢复；记录迁移版本和数据保护证据。
- [ ] T12 隔离部署：新建本轮环境，按下表 C 组跑 Compose/K8s；核对各实际二进制/镜像与挂载，不使用现有用户资源凑验收。
- [ ] T13 Java 契约交付：更新 OpenAPI、无 header 示例、固定 URL 占位说明；Java 项目由同事修改，本仓报告不宣称已完成其上线。
- [ ] T14 交付：新增 verification.md，记录命令、退出码、实际范围、镜像/物理身份、报告、未验收项；小范围提交或 diff，不自动远端发布/清理旧数据。

按编译依赖可将 T02–T09 分组实现，但每个提交应可评审，不能仅靠最后删测试让整仓编译通过。数据库切换前必须具备端到端兼容证据和旧 writer 停止方案。

## 2. 验收矩阵

### A：请求与业务契约

| ID | 场景 | 预期 |
|---|---|---|
| A01 | UserApp 无 x-user-id、无 body/query/form user_id | dev/prod 合法请求正常受理，不尝试 owner 回退 |
| A02 | 旧 header 是不同字符串或违反旧业务 identifier 规则的合法 header 值 | 不解析/不因用户格式拒绝，命中同实例，UserApp 上游不收到该 header |
| A03 | 旧 JSON/query/multipart 多余 user_id | 被忽略，不改变定位/fingerprint，不建立用户模型；其他必填/严格规则保留 |
| A04 | 代理 URL 0/alice/bob 占位 | 同 stage/app 同一实例，原 pattern 与 HTTP/WS 路径兼容 |
| A05 | tasks/static/新接口/门面/上传/生产存储 | 无隐藏 user_id 必填；不将流式请求改为无界缓冲 |
| A06 | computer_intercept + X-Service-Type:userapp | 无用户 header 也正常转发，旧 header 移除 |
| A07 | 普通 Computer 两个用户 | 原用户隔离/header/定位保持；不被 UserApp middleware 清理 |
| A08 | 缺 app_id、非法 stage、缺其他必要参数/认证 | 既有校验仍有效，不因去用户一并放宽 |
| A09 | OpenAPI 与新 URL 生成 | UserApp 无 user_id/x-user-id 契约，URL 固定 0；普通 Computer 文档不误删 |

### B：身份、并发、数据库

| ID | 场景 | 预期 |
|---|---|---|
| B01 | 两个调用者并发 ensure 同 app | 一个 builder/资源集，一致运行登记 |
| B02 | 同 app dev/prod | 不同物理实例、挂载、运行缓存/服务族；查询不串用 |
| B03 | 旧生命周期 stop/delete/recovery 打到同名新实例 | 物理 UID、operation、generation 校验拒绝 |
| B04 | 旧复合 key/旧 user label/不带族 selector | UserApp 不回退接管；Computer 相关旧行为不变 |
| B05 | 新路径与字符/长度边界 | 四类 dev/prod 路径精确，内部名不泄露；派生资源合法，无路径穿越 |
| B06 | SQLite/PG 旧 schema 和含 user_id 的严格 JSON | 升级可读取，新活跃记录不含用户维度，无关业务字段不丢 |
| B07 | 嵌套业务配置也含 user_id | 不被数据库迁移递归误删 |
| B08 | 旧 pending/RecoveryRequired/input/alias/fingerprint | 不转成功、不绕过保护、不自动重放为新身份 |
| B09 | 迁移失败/中断/重复启动 | 可恢复/明确失败，无半迁移被当成成功，无旧 writer 混写 |
| B10 | stop 与迟到 build/start 并发 | 不重新复活共享 dev，已有取消/监督回归保持 |

### C：真实部署

| ID | 场景 | 预期 |
|---|---|---|
| C01 | Compose 无用户参数完成初始化、文件、构建、dev 启停与查询 | 真正的新镜像和新路径，服务实际可访问 |
| C02 | Compose 生产部署/文件/存储查询 | 无用户参数，prod 与 dev 内容和数据分离 |
| C03 | K8s UserApp 两阶段资源及并发创建 | STS/Pod/Service/PVC label/subPath 正确，实际请求成功 |
| C04 | 重建与旧记录/旧资源共存 | 不查找/adopt 旧用户实例，不复用旧路径，未知执行仍阻塞 |
| C05 | Gateway/工具代理 HTTP、WebSocket | 固定及不同占位均正确，header 不必填、不透传 |
| C06 | 存储 clear/destroy/孤儿检测 | 仅本轮已授权目标，dev/prod 不串删，agent PVC/共享根保留 |
| C07 | 实际部署版本检查 | rcoder/proxy/file-server/agent_runner 等均是本轮版本，不能宿主机成功容器仍旧 |

## 3. 验证命令

以下是计划命令，本次没有运行代码测试。实际执行前核对 feature 与新测试名，串行 Cargo：

```bash
cargo test -p shared_types
cargo test -p rcoder-storage
cargo test -p file-server-userapp
cargo test -p app_manager
cargo test -p docker_manager
cargo test -p rcoder-proxy
cargo test -p rcoder
cargo fmt --all -- --check
cargo clippy --workspace --all-targets
cargo test --workspace
```

根据各 crate 当前 Cargo.toml 补 PostgreSQL/Kubernetes 等 feature 测试和独立 file-server/npm 依赖闭包检查，不能用默认 feature 证明条件代码。无数据库的环境门控 skip 不等于 PG 通过。真实 PG 使用本轮独立数据库，不触碰已有服务；SQLite 使用持久文件并关闭重开验证。

```bash
make dev-build
make dev-up
make test-e2e-compose E2E_SUITE=compose_userapp_dev
make test-e2e-compose-deploy

make remote-k8s-doctor
make remote-k8s-sync-start
make remote-k8s-verify SUITE=userapp
make remote-k8s-verify SUITE=gateway
```

严格启动器新增用例要注册 suite/case/report/acceptance 身份，确认非空筛选、无 skip/aborted、报告存在。上述 suite 名不是“新用例自动被覆盖”的保证，开发时逐项映射。

K8s 使用已有 .env.local 与 remote-k8s Make 入口，不泄露/覆盖配置；构建部署测试串行，不替换正在测试的环境。遵循根 AGENTS、remote-k8s README、tests-e2e/tools/README。缺前置只报告未验证，不修改真实用户环境凑结果。

## 4. 交付证据

verification.md 至少记录：实际 HEAD/scoped diff、每组命令及退出码、失败归因、数据库版本/迁移数据断言、镜像 digest/实例 UID、实际 dev/prod 请求内容、报告链接、未运行项、剩余 user_id 命中分类。

不得把源码搜索无命中当完整通过，不得只删旧用户断言。保留启动错误传播并行工作，仅暂存本任务路径/改动块，不 git add -A。

默认本地开发与验证交付；不自动 push/tag/发布、停既有用户服务、DROP 真实业务库列或清理旧数据。环境切换依用户实际授权执行，发布未做时注明。本任务若不改 app-cli 逻辑，无需为了身份改造凭空触发 app-cli 发布；实际受影响组件和镜像分别记录。
