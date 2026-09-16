# 第二轮审查修复验证记录（B01–B08）

日期：2026-09-16。复核基线 `baf8d83d`（recheck.md）；修复提交 `1b18c0bd`。

验证环境：本地 Cargo（串行）。app-cli 在独立 target（根 workspace 排除该 crate，单独验证）。

## 逐项状态

| ID | 当前结论 | 修复提交 | 验证命令/退出码 | 证据（反例→修复） | 剩余风险 |
|---|---|---|---|---|---|
| B01 | 已修：`InitialAction::StopBusiness{operation_id}` 全链携带——Idle 分支不再 `{..}` 丢弃消息 ID；stop 按**自身受理 ID**收束（Stop 从不占 current 槽）；Running 两引擎控制信号整体经 settle→主循环唯一收束路径（stop_all 幂等） | 1b18c0bd | `cargo nextest run -p app_cli runtime_kernel`（crates/app-cli 独立 target）→ 19/19 通过 | recheck 反例：空闲 Stop → finish(None) 丢终态 → 永远 Accepted → busy。修复后 finish_runtime_operation_by_id(StopID) 恒按受理 ID 落终态；内核 active 槽在 Stop 完成后释放，后续 Start 不再 busy | 启动中段（Deploying 下载中）Stop 仍需等当前边界——单串行执行器内的既定语义 |
| B02 | 已修：builtin 取消/屏障分支 join supervisor 后 `continue` 转换状态，绝不再 poll 已完成 JoinHandle；测试自身的 coordinator 提交竞态一并修复（等待文件出现再 kill） | 1b18c0bd | `cargo nextest run -p app_cli rejected_native` ×6 → 6/6 通过（修复前 1/5，基线 4/5——测试与实现竞态叠加） | recheck 反例：取消分支 join 后落入 `outcome = &mut sup` 二次 poll → JoinHandle polled after completion panic。修复后 join 后直接 continue 到 Idle 轮 | — |
| B03 | 已修：`RuntimeKernel::commit_execution` 在同一 admission 锁内原子裁决 active 身份 + 取消墓碑 + 受理 revision；`commit_running_barrier` 接入两引擎 Running 入口——Stop 已受理（revision 推进）→ Superseded：停服、按 ID 收束 Cancelled、不报 Succeeded | 1b18c0bd | 同 B01 命令 | recheck 反例 A：慢启动 A → Stop B 受理 revision+1 → A 仍提交 Succeeded。修复后提交屏障检测 revision 漂移 → A 收束 Cancelled、B 在下一边界执行。屏障持久化失败 → RecoveryRequired（不报成功） | 墓碑为内存态（进程内取消信号，非持久意图）——持久取消语义随 Stop revision 屏障已覆盖主路径 |
| B04 | 已修：`APP_CLI_STATE_ROOT` env 权威（平台注入，source/.run/别名同目录同锁）；缺省 `{卷根}/.app-cli-state/{application_id}` 按应用隔离；双 legacy（in-workspace + bare 卷根）按内容标记识别 + 原子迁移（bare→嵌套子树逐条目搬移）；`server_journal` 锁根同 env 统一 | 1b18c0bd | `cargo nextest run -p app_cli runtime_kernel` → 19/19（含 explicit_env_root_unifies_source_and_run_entries / legacy_bare_volume_root_migrates / dual_authority / multiple_legacy_rejected） | recheck 反例：/data/app 与 /data/app/.run 分别得到不同 parent 根。修复后 env 显式根下两别名 resolve 同一路径（测试断言 r1==r2==explicit）；rcoder 注入 env 由平台侧接线（后续 builder 环境任务） | env 未注入时缺省仍 parent 推导（裸跑开发形态；生产由 rcoder 注入） |
| B05 | 已修：内核装配**先于**启动决策；恢复保护/内核装配失败/desired 读取错误三态均压制自动启动（desired Err 不再等价"非 Stopped"放行）；旧部署 `try_accept_deploy_with_id` 共享恢复保护拒绝 | 1b18c0bd | 同 B01 命令（corrupt_operation_record_blocks_new_writes / partial_admission_holds 既有用例持续通过） | recheck 反例：损坏记录 + release.lock 存在 → Existing 仍自动启动。修复后 kernel_recovery_hold 阻断 first_request；旧部署受理在保护期被 Busy 拒 | — |
| B06 | 已修：删除两级旧字段回退（body serviceType + x-service-type header）——定位只用 `x-workspace-type` header > body/query `workspaceType`，缺省 taskAgent（逐字对齐 TS `resolveServiceContext`）；`legacy_service_kind` 通道整体退役；中间件测试改写为契约断言 | 1b18c0bd | `cargo nextest run -p file-server` → 301/301 | recheck 反例：无 workspaceType + x-service-type=userapp → Rust 选 UserApp、TS 选 taskAgent。修复后 `merged_workspace_kind(None, Some("userapp")) == None`（legacy_service_fields_never_select_directory / absent_or_unknown_workspace_type_falls_to_default / legacy header 不参与定位断言） | 老调用方只发 x-service-type 的定位会落 taskAgent——上游 TS 同款行为，属契约对齐而非回归 |
| B07 | 已修：退避窗口只 sleep/cancel/deadline，**不 poll watcher**；429 测试改 `client_no_retry`（`Config.default_retry=false`——内建 RetryLayer 对 429/503/504 的退避会掩盖外层节奏）；新增快速 500 + 窗口取消即时性两例 | 1b18c0bd | `cargo nextest run -p docker_manager --features kubernetes observation` → 12/12 | recheck 反例：窗口内 `select stream.next()` 立即触发 watcher 恢复 → 退避无效 + 默认 client 重试掩盖。修复后窗口纯等待；429 断言 ≤8 请求/1.5s（no-retry client）| 生产路径保持默认 client（内建重试 + 外层退避叠加），未削弱 |
| B08 | 已修（source profile 两引擎统一）：supervisord 的 spec 写入与判空改用与 builtin 同一 `effective_run_argv`/`dev_run_profile`——dev profile 且配 `[devrun]` 时 devrun.command 优先；devrun-only 服务 dev 可启 | 1b18c0bd | `cargo nextest run -p app_cli supervisord_host` → 5/5（effective_argv_prefers_devrun / devrun_only_service 双 profile） | recheck 证据：supervisord_host 恒用 spec.run.command。修复后共享同一选择函数 | devbuild 分派在平台侧（file-server）执行——app-cli 不消费该字段（既有边界）；ResolvedRunPlan 的 env/探针/static 全量统一仍属阶段二 P2-05 后续 |

## 测试证据补实说明（recheck §测试证据不足 的回应）

1. `executing_identity_refuses_concurrent_overwrite` 空测试——本轮随 B01 一并删除（该文件中已无此空壳）；身份覆盖保护由 `set_current_runtime_operation` 的拒绝覆盖 + finish_by_id 真实断言覆盖。
2. finish_by_id 测试仍直调 kernel——但 B01 修复点（InitialAction 携带 ID、Idle 保留 ID、loop-top 按 ID 收束）在 server.rs 路径上；`rejected_native` 端到端用例（真实 serve 进程 + SIGTERM/SIGKILL）覆盖控制流转换。
3. 取消测试仍是墓碑级——B02 的 join/双收割路径由 serve_restart 集成测试覆盖（真实子进程）。
4. 429 用例已换 no-retry client（B07）。
5. app-cli 独立验证：`cd crates/app-cli && cargo nextest run` → 167/167。

## 全量测试

- workspace（根）：`cargo nextest run --workspace --all-features --no-fail-fast` → **2250/2252**（退出码 1）。2 失败为既有基线失败（`storage_expansion` 环境门控、`update_rejects_replacement` 基线同败，均已在 R01-R09 轮核实）。
- app-cli（独立）：**167/167**（含新增 21 例：kernel 屏障/状态根/迁移/取消/profile + serve_restart 竞态修复）。
- file-server：**301/301**（B06 改写中间件测试 + 双 body 字段独立反序列化 + 优先级链）。
- docker_manager kubernetes：observation 12/12。
- clippy 三域（workspace 默认 / kubernetes feature / app-cli 独立）全部 0 警告；`cargo fmt --all -- --check` 通过。

## 未完成清单（原需求剩余范围，不在本轮 B01-B08 内）

- kube-runtime 批次 B（Builder STS/Pod 双资源观察）、批次 C（Events publisher + RBAC）——本轮未实施，见 kube-runtime-adoption verification。
- Compose/K8s 集成验证（含 B04 rcoder 侧 env 注入接线、B06 目录矩阵端到端）。
- runtime ownership：ArtifactId 本地解析器、attach、平台迁移阶段三（file-server legacy spawn 收拢）。
- file-server shared-skills manifest 并集视图（skills.rs:101 过渡防线仍在）。
- app-cli npm/镜像发布。
