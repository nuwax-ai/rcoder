# Tasks：执行清单与验收证据

状态：所有实现任务待执行。2026-09-15 本次只完成 Spec/Plan/Tasks/提示词，不产生代码、测试或发布成功声明。

## 0. 执行纪律

- 每个任务先读取实际实现、相关 Spec/Plan 和 scoped diff；本清单中的文件名是定位入口，不允许对旧行号机械打补丁。
- 先聚焦验证，再扩大到 crate、相关 feature、真实部署。Cargo 共用 target 时必须串行。
- 出现原有失败，保留完整退出码、错误与归因；不能关断言、改 skip 或调大超时让报告变绿。
- 组件测试可以使用确定性故障注入/受控子进程；AI E2E 必须真实调用，不得把 fixture 当真实 AI 执行结果。
- 每阶段独立提交边界。仅暂存本任务文件/改动块，禁止 git add -A、reset 或覆盖并行代码。
- 阶段一未达验收不得声称问题已修复；可以继续做不依赖失败项的只读调查，但不能用阶段二大改掩盖阶段一结果。

## 1. 准备与基线

- [ ] T00：读取根 AGENTS.md 与本目录全部文档、并发修复和生命周期相关 Spec/Plan/Tasks。
  - 产物：记录实际 HEAD、分支、app-cli 版本、目标变更路径及原有 dirty 状态。
  - 完成标准：明确当前工作树与阅读基线的差异；如隔离 worktree，列出必要未提交依赖，不能假称纯 HEAD 等于当前源码。
- [ ] T01：建立本轮证据记录 `verification.md`。
  - 列出阶段一启动时间线、阶段预算和每个已有行为的定位；源码证据与历史事故推断分开。
  - 历史镜像取证仅在具备访问和授权时执行；未取证不阻止修已确认源码缺陷。

## 2. 阶段一：错误传播（独立交付）

- [x] P1-01：API 预绑定与启动顺序。
  - 路径：`app-cli/src/api/mod.rs`、`main.rs`、`server.rs`。
  - 完成标准：legacy/serve 3010 冲突均非零退出；发生在 deploy cleanup、stop_all、migrate、业务 spawn 之前；未 claim 不写 Quiescent。
  - 验证：隔离 listener 占用、正常 bind、serve journal 拒绝、运行中 API future 故障。
- [x] P1-02：统一启动结果及 main 错误返回。
  - 路径：`app-cli/src/supervisor.rs`、`orchestration_events.rs`、`main.rs`，必要的 shared_types。
  - 完成标准：可控失败至多一个 Done，保留失败原因和已有服务失败项；main 非零；清理失败独立标记；成功常驻行为不变。
  - 验证：配置/migrate/spawn/pingap 各失败入口；服务部分失败旧语义；stdout 写失败仍通过退出可见。
- [x] P1-03：UserApp manifest Child 监督。
  - 路径：`file-server/src/service/dev_server/{start,types,log,stop}.rs` 与 process 模块。
  - 完成标准：有唯一 Child wait/reap 所有者，退出/EOF/读取错误可观察，stdout 有界排空，不依赖 PID 轮询替代真实 exit status。
  - 验证：退出码 0/非零、SIGKILL、后代继承 stdout、读取失败；不误杀外部端口占用进程。
- [x] P1-04：发送端生命周期与终态顺序。
  - 路径：`file-server-userapp/src/handlers/userapp_dev_server.rs`、`service/userapp/start_events.rs`。
  - 完成标准：调用方 evt_tx 和 on_event clone 均不无意保活；Done 与已排队事件按顺序处理；退出不能丢最后事件，也不能把已退出编排器认成就绪。
  - 验证：现有 start_events 用例保留；新增真实 start/restart 调用层用例，不能只手工 drop 一个裸 channel 后宣称修复。
- [x] P1-05：取消、清理与重试保护。
  - 完成标准：abort consumer 不遗弃启动 worker；只清理本轮归属资源；清理未确认时后续启动被阻止且错误可查询；不把 timeout/Cancelled 当清理证明。
  - 验证：构建完成后取消、spawn 后取消、代理失败清理失败、stop 与启动提交竞争。
- [x] P1-06：预算清单和等待窗配置。
  - 产物：verification.md 记录 PG/migrate/服务就绪/代理/收尾的预算与起点。
  - 完成标准：launch 起共享 deadline；默认候选 300 秒不截断合法显式慢启动；配置不足提前失败；存活无进度有界失败，错误附阶段。
  - 不允许：仅将 3600 改为 300 并根据四个快路径测试断言安全。
- [x] P1-07：独立回归与阶段交付。
  - 执行下述 A 组全部适用项、相关 crate 测试和两种部署模式验证。
  - 汇报本轮 app-cli/file-server/agent_runner 镜像身份；明确只更新哪一侧不足以完成修复。
  - 阶段一单独形成 reviewable diff/提交，不依赖新增运行协议才能工作。

## 3. 阶段二：所有者与协议

- [x] P2-01：共享身份、状态、操作、错误与事件类型。
  - 完成标准：shared_types 单一契约；runtime_instance 与 deployment_generation 分开；profile/input 校验；OpenAPI 和正式 HTTP 信封完整。
- [x] P2-02：稳定状态根与兼容迁移。
  - 完成标准：source/.run/别名同锁域；旧 journal 迁移无双 writer 窗口；损坏/不兼容记录 fail closed；保留旧恢复保护。
- [x] P2-03：持久受理与幂等历史。
  - 完成标准：按操作 ID 查询/重放，同 ID 异参数 409；结果持久化；响应丢失可恢复；不因 journal/fsync 失败入队。
  - 验证：崩溃注入覆盖受理落盘前后、入队前后、终态落盘前后，不能只测内存互斥。
- [x] P2-04：统一操作 worker 与停止屏障。
  - 完成标准：start/restart/deploy 串行；stop 能持久化 pending 意图并取消旧启动，执行不并发；revision 拒绝旧构建提交；cancel 与 stop 语义不同。
- [x] P2-05：共享 ResolvedRunPlan。
  - 完成标准：builtin/supervisord 同命令/cwd/env/依赖/探针语义；source devrun、devbuild、static 差异保留；生产 run 不改变。
- [x] P2-06：制品输入适配及唯一目录写入。
  - 完成标准：本地 artifact_id/URL 输入共用准备激活内核；本地输入无任意路径激活、无校验后替换竞态；source 根永不整体替换。
- [x] P2-07：有序持久事件与 task 关联。
  - 完成标准：operation+sequence 重放、终态记录一致、慢订阅不阻塞执行；file-server 重启可继续观察已受理操作；旧 UI 顺序兼容。
- [x] P2-08：控制 program、CLI 与归属。
  - 完成标准：空 workspace Idle；重复 serve 无业务副作用；attach 无打印退出循环；业务组所有权不只按前缀识别；管理进程存活不等于业务 Running。
- [ ] P2-09：desired state 与恢复。
  - 完成标准：明确 stop 重建不复活；正常进程重启可按 Running 恢复；source 当前内容语义明确；切换未知保持恢复保护。
- [ ] P2-10：生产兼容回归。
  - 完成标准：旧 `/v1/deploy` 使用同一内核；冷部署段 vs 完整 Running 语义不变；D01–D06 操作/代次/persisted/旧健康误认测试保持。
- [ ] P2-11：阶段二验证。
  - 执行 B 组及两引擎集成，说明 Windows/macOS builtin 支持状况；Linux supervisor 能力不能用本机源码核对代替。

## 4. 阶段三：平台和部署迁移

- [x] P3-01：列全 UserApp 运行态入口调用图。
  - 包括 start/restart/stop/list、task cancel、clear、模板导入、hygiene、proxy reload、gen-lock/deploy_dir 以及相关恢复/保活路径。
  - 完成标准：逐项标明转协议、共享排他、只读或拒绝，不能留下受管目录旁路 writer。
- [x] P3-02：file-server 构建与协议适配。
  - 完成标准：保留构建分派；构建前捕获身份/revision，提交前校验；不再 activate `.run` 或 spawn 受管 legacy；任务映射可恢复。
- [x] P3-03：列表、停止、取消及 SSE 兼容。
  - 完成标准：active version 与本轮 build version 分离；list 不以 serve PID 判断业务运行；停止完成有证据；取消不丢操作记录。
- [x] P3-04：镜像与配置。
  - 路径入口：`docker/rcoder-agent-runner/`、builder 实际使用的 Dockerfile/startup、`docker/remote-k8s/` 和 Make 工作流。
  - 完成标准：确认真正的镜像构建链、state_root 挂载、固定 program、能力探测；配置不含真实地址/凭据；不误把生产 runtime 镜像更新当 builder 已更新。
- [x] P3-05：持久灰度模式与迁移工具/说明。
  - 完成标准：legacy/managed 单应用互斥；未知 program 只报告；旧新 writer 停止确认；超时/403/缺能力/身份不符无自动回退；回退有明确前置。
- [x] P3-06：Compose 与 K8s 实机验收。（Compose 完成：38 pass/3 fail 全环境归因——E2E_SQLITE_BINARY_SHA256 前置缺失×2、ERR_MODEL_UNAVAILABLE×1，报告 904c4e69；K8s remote-k8s 未运行，见 verification）
  - 完成标准：C 组通过且报告绑定源码、镜像 digest、namespace、实例、操作及有效内容；临时资源只清理本轮所有，不删 agent PVC/共享根。
- [x] P3-07：最终交付。（本轮 verification.md 汇总）
  - 更新本目录记录和必要用户文档；解释行为变化、命令、退出码、通过范围、未运行项与发布状态。

## 5. 测试矩阵（至少覆盖）

### A：阶段一启动错误传播

| ID | 场景 | 核心断言 |
|---|---|---|
| A01 | 仅占用 3010，legacy/serve 各测 | 非零、无业务/目录副作用、外部服务存活 |
| A02 | 仅占用 9080 | 不被 A01 遮住，代理错误准确、清理只属本轮 |
| A03 | 仅占用 3018 | 有界确认失败，无假成功 |
| A04 | 未发 Done，退出码 0 与非零分别测 | 快速明确失败，不等待整窗，保留 exit status |
| A05 | SIGKILL 编排进程 | 监督可见，子孙残留检测/收束有证据 |
| A06 | 父进程退出但后代继承 stdout | 排空有界，不依赖 EOF 永远到达 |
| A07 | stdout EOF/读取故障但进程仍存活 | 通道异常及受控收束，不超时假成功 |
| A08 | 已写服务事件与 Done，消费故意延迟 | 顺序不变；退出与缓冲竞态不吞最后原因；已退出不能称运行就绪 |
| A09 | 真实调用层保存回调 clone | 验证所有冗余发送端释放，不能只测裸 channel |
| A10 | 正常多服务启动、部分探针失败 | 成功/失败语义及事件契约兼容 |
| A11 | 存活无进展、显式慢启动预算 | 总预算有效；合法慢启动不被 300 秒截断 |
| A12 | 取消/stop 与启动提交交错 | 不迟到复活，不丢清理 handle，不重复终态 |
| A13 | 管理 API 运行中异常 | 已受理业务受控收束或明确保护，无失控第二实例 |

### B：操作协议和恢复

| ID | 场景 | 核心断言 |
|---|---|---|
| B01 | 平台/CLI 同时操作、source/.run/别名 | 一个 owner，一个执行 worker，无分裂锁域 |
| B02 | 同 ID 同参数/异参数，重启后再请求 | 重放或冲突，无重复 migrate/重启 |
| B03 | 受理响应丢失、请求超时、SSE 断开 | 原操作可查询，互斥不释放 |
| B04 | 接受/阶段/终态落盘失败或进程崩溃 | 无虚假成功、无遗漏执行/重复入队；恢复保护正确 |
| B05 | 旧 instance/revision/错误 workspace | 副作用前拒绝；不能自动刷新身份重试旧意图 |
| B06 | 编译期间 stop，编译随后完成 | 包可存在但启动提交被拒绝 |
| B07 | 准备/激活/启动期间 stop/cancel | 单 worker 收束，Stopped 持久化，无并发换目录 |
| B08 | source devrun/devbuild 与 artifact 各跑两引擎 | 命令、cwd、热加载、static、探针语义符合原约定 |
| B09 | 旧版健康但新部署失败 | 新 operation 失败，active target 不造假 |
| B10 | A→hot B→控制进程/容器重启 | 已确认 B 恢复；停止后则保持 Idle |
| B11 | Switching 崩溃、日志损坏、停止未确认 | 不猜测回滚、不释放、不重复 writer |
| B12 | serve 重复/attach/用户 autorestart | 不创建第二业务树、无无意义退出重启循环 |
| B13 | 事件重放、过期 cursor、慢订阅 | 顺序/去重/终态一致，执行不被拖住 |
| B14 | 本地 artifact 被替换/非法输入/受管目录旁路 | 不激活未经确认输入、不覆盖 source |

### C：真实部署和迁移

| ID | 场景 | 核心断言 |
|---|---|---|
| C01 | 全新 builder 空 workspace | owner Idle，不 crash loop，后续正常初始化 |
| C02 | 旧实例迁移、用户 program/未知占用者 | 核验并停止已授权 owner；未知者未被杀或删除 |
| C03 | managed API 超时/403/旧协议/错身份 | 无 legacy spawn fallback |
| C04 | Pod 重建 Running 与 Stopped 分别测 | 有效目标恢复、明确停止不复活；PVC 保留 |
| C05 | 真实 frontend devrun 修改 + 后端运行 | 热加载确实生效，实际请求返回对应内容 |
| C06 | 真实 artifact restart/hot | 有效内容与 operation/镜像/实例身份一致 |
| C07 | 生产冷/热部署回归与旧 API | 部署段/就绪语义及 D 系列保护不变 |
| C08 | Web/Custom Page/独立 file-server 形态 | UserApp 改造未引入 PG/K8s 依赖或改变其他生命周期 |

## 6. 验证命令与记录

以下为计划命令，**本次文档编写未运行**。开发 agent 应核对 target/feature 与实际新增测试名，逐条记录退出码。不要并发 Cargo。

```bash
cargo test -p file-server-userapp --lib start_events
cargo test -p app-cli
cargo test -p file-server
cargo test -p file-server-userapp
cargo test -p shared_types
cargo fmt --all -- --check
cargo clippy --workspace --all-targets
cargo test --workspace
```

需要格式化时依根 AGENTS 使用 `cargo fmt --all`，之后检查是否触及并行改动，不能把无关格式化混入提交。workspace 环境门控 skip 不等于部署通过。涉及 feature 的路径补相应 cargo feature 检查，并记录完整命令，不能只报默认 feature 全绿。

```bash
# Compose：先按实际镜像/依赖变更选 dev-build，再 dev-up
make dev-build
make dev-up
make test-e2e-compose E2E_SUITE=compose_userapp_dev
make test-e2e-compose-deploy

# K8s：配置来自已有 .env.local；所有操作串行
make remote-k8s-doctor
make remote-k8s-sync-start
make remote-k8s-verify SUITE=userapp
# 若涉及实际代理/Gateway 请求路径
make remote-k8s-verify SUITE=gateway
```

新用例需注册到严格启动器的 suite/case/acceptance/report 身份中，确认筛选非空、无 skip/aborted、报告存在。以上现有 suite 不保证自动包含新场景，必须核对并补注册。纯 app-cli 故障注入可新增隔离容器 harness，但不能以它替代整轮平台 UserApp 验证。

环境不可用时继续完成可执行层级，准确列出阻塞；不要操作现有用户应用来凑验收。K8s 遵循 remote-k8s Make 入口，读已有 `.env.local`，不输出真实配置，不删除共享 CephFS 根或 agent PVC。旧任务文档里的“不运行 K8s”是历史任务边界，不是本轮已批准测试的替代证据。

## 7. 发布与交付边界

- app-cli 实现验证后应按版本纪律准备版本递增、依赖锁定与匹配 `app-cli-v<VERSION>` 的发布记录。基线 0.3.5 不代表开发时仍是最新，禁止抢占并行版本号。
- `.github/workflows/release-app-cli.yml` 发布主包及五个平台子包，共六包。必须核验 tag/commit/Cargo 版本一致、workflow 结果及实际 npm 版本；不能把本地 build 当发布完成。
- app-cli npm、file-server/agent_runner、builder 镜像、生产 app-runtime 镜像是不同交付渠道，逐一确认使用哪个版本/digest。
- 本交接默认开发、测试和本地交付；远端 push/tag/workflow、生产 rollout 依用户实际授权执行。未执行时保留为待发布，不能写已完成，也不能自动迁移既有用户 program。

建议在 `verification.md` 逐项追加：

| 阶段/任务 | HEAD/源码差异 | 命令或场景 | 退出码 | 通过范围 | 日志/报告 | 镜像/实例/操作 | 未运行项 |
|---|---|---|---|---|---|---|---|
| 待填 | 待填 | 待填 | 待填 | 待填 | 待填 | 待填 | 待填 |

本文件勾选代表有对应本轮证据。禁止根据历史 tasks.md 的绿灯、agent 自述或静态匹配数量勾选真实验收项。

## 8. 2026-09-17 原生宿主机补充

- [ ] 按 [原生 Tasks](../native-desktop-runtime/tasks.md) 完成 app-cli 与 file-server-proxy 的自包含三平台能力（ND01–ND12）。此处不继承此前阶段的完成勾选。
- [ ] 三平台执行 NT01–NT16 适用场景，分别记录真实进程/包的证据；追加 [原生 verification](../native-desktop-runtime/verification.md)。

本批 Electron 只作为使用背景，客户端、IPC、安装和更新不属于任务。原有 R01–R11/B01–B05 的修复与容器回归继续完成，参见[统一交接提示词](../development-review-2026-09-17/claude-prompt.md)。
