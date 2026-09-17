# RCoder 提交后审查与修复交接

## 基线与证据边界

- 审查日期：2026-09-17；分支 feature-userapp；HEAD `19dfd381`。
- 开始时 `git status --short` 无输出：没有待审的未提交业务代码。2026-09-15 起共有 92 个提交，本轮阅读提交历史并重点追踪所有权/跨平台、平台迁移、部署预算和错误类型化相关调用链，不声称逐行审完全部提交。
- 重点提交：`054aef7b`、`4dcc1c3d`、`d0727ec6`、`73b45144`、`429144f6`、`2e1c2ba6`、`a217a88a`、`3caebc0b`、`801b4ef5`、`d3e99549`、`8acbcd66`、`9e827aad`、`3af45f7d`、`23772535`。
- 本轮为源码审查；没有执行 Cargo、Compose、K8s、Windows/macOS 跨平台实测，没有改业务代码。历史验证记录是被审查材料，不作为本轮独立复测证据。
- 下列路径与行号对应本轮 HEAD；修复时重新定位，保留其他正在进行的改动。

## 结论

不能判定单 owner、三平台真实生命周期、平台迁移和预算改造全部完成。有已实现基础设施，但存在未接入生产链、终态错误、恢复信息丢失及旁路写入。优先修复 R01–R08，再完成 R09–R11；不通过修改报告或增加 helper 测试替代实际调用链修复。

## R01 / P1：Windows 进程树封装没有接入生产 supervisor

证据：`crates/app-cli/src/platform/process_tree.rs:36` 定义 spawn_managed；仓库引用均在该模块测试内。真实服务在 `supervisor.rs:574–592` 使用普通 Command.spawn，准备命令和 pingap 在 626、717 同样走 process_group_command；非 Unix 分支 944 返回普通 Command。停止在 890 只 start_kill，849–868 只验证直接 Child，非 Unix 忽略 groups。

影响：Job Object helper 的测试通过不能证明真实 app-cli 能停止 npm/node 等子孙，旧进程可能继续占端口或修改目录，新轮却已开始。

修复：把准备命令、业务服务和 pingap 的真实 spawn/日志/监督/超时/停止链统一接入受管进程抽象；保留日志句柄和原执行身份。停止完成要求整个受管树收束，不能只 wait 直接 Child。顺便核查 ManagedChild.stop 中 wait 的平台语义，不把根进程退出误当全部成员退出。

验收：通过真实 app-cli serve + manifest 启动带孙进程且孙进程持有端口的 fixture；Windows stop/restart/准备阶段超时后验证孙进程与端口均收束。Linux/macOS 验证父进程先退出、子孙忽略 TERM 的反例。断言最终业务状态及新一轮不能抢跑。

## R02 / P1：CLI 转交未完成，legacy 仍绕过 owner 锁

证据：`main.rs:60` 的 None 继续 legacy；该路径只 bind API，不获取 OwnerGuard，也不提交 runtime operation。`server.rs:888` 只有显式 attach 才连接已有实例；1005–1061 的 attach 只是等端口可绑定后重新执行，300 秒后失败，不提交新启动目标。`runtime_kernel.rs:801–819` 非 Stop 忙时仍直接拒绝。

影响：agent 重复无子命令调用仍报 3010 冲突；更换端口的 legacy 可以绕过新 owner 锁操作相同目录。“最后受理有效请求生效”尚不存在。附着等待不能代替启动客户端。

修复：明确 managed 与 standalone 策略，旧命令也进入身份/锁/客户端分派；默认重复启动转交唯一 owner，attach 单独定义。最新待执行目标、Superseded、revision、停止屏障和崩溃恢复必须进入持久化内核，不通过循环重试 busy 实现。build/gen-lock/部署目录写入同步检查所有权边界。

验收：两个真实 CLI 同时首次启动、已有 owner 后无子命令启动、改端口重复启动；A 执行/B 等待/C 替代、旧构建晚完成、Stop 插入和 owner 崩溃。检查只有一个 owner、一棵有效业务树、旧请求不覆盖新目标。

## R03 / P1：平台仍在 owner 外激活 .run，制品请求被改成 Source restart

证据：`crates/file-server-userapp/src/handlers/userapp_dev_server.rs:434–439` 在调用 start_dev/restart_dev 前 prepare_run_dir + activate；source 分支也先 ensure_dev_lock。`crates/file-server/src/service/dev_server/start.rs:476` 之后复用 owner，`owner_client.rs:95–155` 恒提交 Restart + Source。

影响：身份不符或 revision 拒绝发生前，运行目录已经改变；旧服务仍可能使用被替换内容。artifact/source 语义未保留，原方案“唯一目录 writer”没有实现。

修复：平台只准备独立且可校验的 staging；由 owner 在通过身份/revision 后激活。实现受限本地制品输入适配；source 运行计划由 owner 准备，不能简单把产物模式请求当源码重启。提交拒绝不得改变 active 内容。

验收：已有 owner 服务运行期间，构建后制造 revision 不匹配和身份冲突；断言 active 目录及正在运行版本不变。分别验证 devrun/source 与 artifact 完整链。

## R04 / P1：Cancelled 被当作启动和停止成功

证据：`start.rs:526` 附近与 `stop.rs:120` 附近均接受 `Succeeded | Cancelled`。前者随后登记 external DevProcess，后者返回停止成功。

影响：取消操作既不证明启动成功也不证明业务已停。列表可能显示并未启动的服务，停止可能误报完成。

修复：按操作种类、终态和运行结果分别映射。Cancelled 返回取消语义；不得自动等价为 Succeeded。只有持久成功及相应运行证据才能登记成功或确认停止。

验收：真实取消启动、取消停止、取消与成功提交竞争；校验 API、task、列表与实际进程一致。

## R05 / P1：停止失败前删除 owner 登记，后续可能退回 ps 扫描

证据：`stop.rs:16` 先 remove(project_id)，随后才调用 stop_external_owner。网络失败、身份过期或超时返回时不恢复登记；第二次 stop 因无 external_owner 会进入通用 ps 扫描。external 登记也仅存在于内存。

影响：第一次请求可能已被 owner 执行但响应丢失，平台失去操作身份；重试不再遵守同一 owner 控制链。file-server 重启也不能恢复这份控制关系。

修复：持久化应用模式、owner 身份和 task-operation 关联；先保留登记并记录 stopping，确认终态后再更新/移除。未知结果保留保护，按原 operation_id 查询，不产生新 ID，不转 ps 扫描。managed 模式下登记缺失也不能授权 legacy 清理。

验收：Stop 已受理后丢响应、轮询断连、file-server 重启、重复 stop；断言原操作可恢复且不发送任意进程组信号。

## R06 / P1：SSE 终态与平台消费者协议不匹配

证据：`runtime_kernel.rs:962` 终态事件名称是 Completed/Failed；`owner_client.rs:297` 的 to_legacy_evt 仅复制名称；`userapp_dev_server.rs:58–90` 只将 orchestration_done 解析为 Done。`start.rs` 在 wait_terminal 完成后立即 abort stream_task，然后 handler 仍调用 event_pipe.finish（约 478 行）。

影响：真实 owner 成功不产生平台所需 Done，可能报告事件通道关闭/启动终态缺失；mock 手工发 orchestration_done 不能证明真实协议兼容。流还复用了总 timeout=10 秒的 reqwest client，长部署事件流会中断；无 cursor 重连与 UTF-8 跨 chunk 保护。

修复：明确适配 operation 终态和服务事件；成功屏障来自已核验持久操作，事件按 sequence 排空后再完成，不盲目 abort。保留失败清单。流客户端采用适合 SSE 的超时、断线续传和增量解析；坏事件不能静默丢掉后宣称完整。

验收：真实 owner → OwnerClient → DevEventHooks → task 的成功/失败用例；部署超过 10 秒、终态事件晚于轮询、断线续传、中文字符跨 chunk，断言只有一个正确 task 终态且先前服务事件不丢。

## R07 / P1：构建前身份捕获失败被吞掉，提交时刷新 revision

证据：`dev_server/mod.rs:77–99` 将探测、凭据、status 失败全部转 None 并删除期望；`start.rs:476–490` 无期望时取最新 revision。期望只保存在按 project_id 索引的内存 Map，不绑定构建 task。

影响：构建前短暂网络失败，构建期间用户 Stop，提交时读取新 revision 后仍可 Restart，绕过“旧构建不得迟到复活”。并发构建还可能互相覆盖或消费期望。

修复：构建任务持有自己的不可变、可恢复 admission context；区分确认无 owner 与观察失败。managed 观察失败明确阻断，不能清空后刷新。首次创建也需要统一受理顺序，不能用“没捕获到”绕过停止屏障。

验收：预检断连 → 用户 Stop → 网络恢复 → 旧构建结束；必须拒绝旧提交。两个任务交错捕获/提交不得串用 revision。

## R08 / P1：复用 owner 丢弃用户传入的新 PG 凭据

证据：`start_dev_manifest` 收到 pg，但在构造 POSTGRES_USER/PASSWORD 的 legacy env 之前调用 reuse_or_refuse_owner；后者无 pg 参数；OwnerClient.submit 设置 request_context=None。

影响：此前已修复的“修改数据库密码后 dev 重启”在 owner 复用模式回归：仍使用 owner 旧环境，业务数据库连接失败。

修复：为统一控制协议定义受控的每操作运行配置更新并传递必要凭据；secret 不写公开事件/日志/普通明文操作摘要。明确能力门控与不支持时拒绝，不能忽略用户输入。

验收：修改 PG 密码后平台显式携带新凭据 restart，复用同一 owner，实际业务连库成功；源码和制品模式分别测，日志无泄漏。

## R09 / P1：跨平台默认锁根仍按 parent 推导

证据：`runtime_kernel.rs:131–142` 默认 workspace.parent/.app-cli-state/application_id；`server.rs:1108` app_id 缺失为 unknown-app。平台凭据查找 owner_client.rs 同时猜父目录和当前目录。

影响：桌面 /work/a 与 /work/b 默认共用 unknown-app 状态根，两个不同项目互相阻塞；同一项目 source 与 .run 的 parent 不同又可能分裂锁域。只提供显式 APP_CLI_STATE_ROOT 不等于默认契约已实现。

修复：统一规范应用身份、稳定项目根及别名映射；容器显式根与 standalone 默认根有清晰规则；所有 CLI、journal、token、endpoint 和平台消费者复用同一解析契约。不要简单 lower-case 全路径或盲猜两处凭据。

验收：兄弟项目无 PROJECT_ID 并存；同项目 source/.run/符号链接/Windows junction 与端口变化仍同锁域。

## R10 / P1：部署 deadline 写入但首次执行仍不遵守统一预算

证据：`app_manager/src/lifecycle/deploy_control.rs:160–181` 写 deadline 后直接 execute_deploy_input，未消费返回的已有 deadline；`deploy_wait.rs:117` 等待入口重新 now+absolute_budget；循环 app_env_snapshot 无按剩余时间约束；`hot_deploy.rs:35,189` 仍固定 HOT_RECONCILIATION_BUDGET=1800s。只有 recovery.rs:212 起读取持久 deadline。

影响：首次冷部署准备时间不计入实际绝对预算；重放和首次执行不一致；热部署无法兑现配置的 3600s 护栏与分档看门狗。写入 side-record 本身不等于预算已贯穿。

修复：新受理与恢复都使用 bind-once 返回的同一截止时间，传入整个执行链；外部调用、重试、退避按剩余预算约束。热/冷复用明确的预算规则；超时仍保留未知写保护，不通过调大常量解决。绑定失败后的已受理记录也需明确收束或可恢复处理。

验收：准备阶段消耗预算后冷等待不得重置；配置热部署预算与实际一致；重放保持原 deadline；卡住的状态读取有界；未知写不会因超时被判安全释放。

## R11 / P2：任务完成标记超出证据，错误类型化尚未落地

证据：tasks.md P2-06/07 与 P3-02/03 已勾选，但上述目录/事件/关联仍有缺口。P3-06 已勾选，却明确 remote-k8s 未运行；verification 的 Compose 结果为 38 pass/3 fail，固定 owner 镜像实机重建也待执行。不得用“相关测试通过”宣称新 managed 链路完整验收。

错误类型化：download_utils/error.rs:53、agent_provisioning/error.rs:51、rcoder-gateway/cluster_cache.rs:58、rcoder-cli/commands/chat.rs:180 仍保留原字符串判断。当前 build.rs 已不含 ensure_lockfile；此前待审未提交实现并未保留为完成代码，因此 lockfile 需求仍须重新核对实现入口，不能报告已修。

修复：保留历史验证事实，另加本轮状态表；重新打开未满足完成标准的任务。执行 error-string-matching-elimination 方案前先修正此前评审问题：RpcFault 在 call 中被字符串化、Option Display 示例、anyhow downcast、Vite 完整类型链、pnpm 专属参数等。不照搬仍有缺陷的计划代码。

验收：错误重构有各产生处→消费处反例；独立 app-cli 与受影响 workspace 默认/all-features 检查；真实 managed Compose 全链及 remote-k8s UserApp 回归，新镜像与报告绑定。缺前置/失败/未运行分别列明，不能勾完整完成。

## 推荐执行顺序

1. R01、R04、R05、R06、R07、R08：先补真实调用链反例并修复错误结果及保护缺口。
2. R02、R03、R09：统一 owner、CLI/平台、目录和身份；解决架构未完成部分。
3. R10：统一持久预算；R11 的错误类型化可独立提交，避免与所有权重构揉在一起。
4. app-cli 三平台真实业务进程测试；Compose managed 模式；remote-k8s UserApp，必要 Gateway 请求。修复后更新需求完成矩阵。

不要求本轮自动发布 npm/tag/生产镜像；修复与测试后分别列出版本及发布待办。不要用已有测试总数代替本文件反例。

## 补充：无容器三平台及 file-server-proxy

用户进一步确认 app-cli、file-server-proxy 都要在 Windows/macOS/Linux 原生使用，基础能力尽量不依赖其他后台服务。新的源码缺口 N01–N10 和方案见 [原生 Plan](../native-desktop-runtime/plan.md)，实施及验收见 [Tasks](../native-desktop-runtime/tasks.md)。与上面 R 项重合的所有权、路径和进程问题合并修复，不重复实现。

Electron 只是未来使用场景，不纳入客户端开发；完整交接采用本目录 [claude-prompt.md](claude-prompt.md)。本次补充只更新文档，没有进行原生或远端测试。
