# 审查修复验证记录（R01–R09）

日期：2026-09-16。基线：审查基于 `7b690562`；修复提交 `154e126e`（R01–R05）、`85c69022`（R06–R09）。

验证环境：本地 Cargo（串行），`cargo nextest run --workspace --all-features --no-fail-fast`，默认与 `kubernetes` feature 分别 clippy。Compose/K8s 集成验证未在本轮执行（见未完成清单）。

## 逐项状态

| ID | 当前结论 | 修复提交 | 验证命令/退出码 | 证据 | 剩余风险 |
|---|---|---|---|---|---|
| R01 | 已修：执行身份不可被并发受理覆盖（set 拒绝有值覆盖；Stop 不抢占）；finish 按显式 ID 收束；supervisord/builtin 两个 Running 等待环接入 control_rx | 154e126e | `cargo nextest run -p app_cli runtime_kernel`（在 crates/app-cli 独立 target）→ 17/17 通过 | `finish_by_id_does_not_complete_a_different_operation`（A 启动中受理 Stop B，A 收束不误完成 B）；测试内核身份覆盖拒绝 | 启动期（Deploying/Orchestrating 中段）Stop 仍只能在下一边界生效——执行器单串行设计内可接受；两引擎 Running 集成测试待 Compose |
| R02 | 已修：状态根迁至 workspace 父目录（卷根，与 `.previous`/DEPLOY_STATE_FILE 同域，跨热部署换代稳定）；旧 workspace 内状态一次性原子迁移；双权威域 fail-fast | 154e126e | 同上 | `state_root_lives_on_volume_root_not_workspace`、`legacy_in_workspace_state_migrates_once`、`dual_authority_domains_fail_closed` | 迁移只覆盖本仓引入的 legacy 位置（无生产存量） |
| R03 | 已修：desired=Stopped 压制自动恢复（Existing 路径；显式 env 部署不受压制）；request_cancel 落墓碑 + 派发前/编排完成两个取消检查点；kind×profile 组合前置校验（未实现组合在任何持久化前结构化拒绝，能力声明收窄 deploy-artifact-url） | 154e126e | 同上 | `request_cancel_marks_tombstone_and_finish_clears_it`、`unsupported_profile_combinations_are_rejected` | 取消检查点在执行边界（派发前/编排完成提交前）——Deploying 下载中段不可中断；ArtifactId 本地解析器仍未实现（显式拒绝+声明收窄，非静默） |
| R04 | 已修：恢复扫描的目录枚举/读取/解码失败以 blocked 上报并保持恢复保护（fail-closed 实行为）；Accepted 落盘后 desired 写失败 → 操作转 RecoveryRequired + 恢复保护（不再留幽灵 Accepted） | 154e126e | 同上 | `corrupt_operation_record_blocks_new_writes`、`partial_admission_holds_operation_and_protection` | 受理→desired→dispatch 三步仍是多文件写入（无跨文件事务）；一致性靠显式围栏+恢复保护，崩溃窗口见测试覆盖 |
| R05 | 已修：退出先于 Done 被观察到 → 迟到成功 Done 不再判成功（exit 0/非零同拒）；失败 Done 保留清单诊断；Done 先到后退出 = 成功提交后健康变化不追改 | 154e126e | `cargo nextest run -p file-server-userapp start_events` → 6/6 | `success_done_after_observed_exit_never_succeeds`（exit 0/1 双档）、`failed_done_after_observed_exit_keeps_failure_detail`、`success_done_before_exit_still_succeeds`（对照组） | — |
| R06 | 已修：模型层 workspaceType 独立字段（不再与 serviceType 同槽别名）；合并序 header > body workspaceType > body serviceType > 旧 header；git serviceContext 用 workspaceType 通道；multipart 双名分流 | 85c69022 | `cargo nextest run -p file-server` → 300/300 | 4 例优先级链测试（旧 header 抢占修复/header 优先/双 body 字段独立反序列化/回退链） | OpenAPI 文档字段级同步未逐个核对（IntoParams/ToSchema 自动跟随字段，schema 名变化待 compose 验证） |
| R07 | 已修：require_user_id + validate_identifier(user_id) 从 dev_terminal/dbx/dev_app_proxy 全部移除；占位段仅存在性检查 | 85c69022 | `cargo nextest run -p rcoder_proxy` → 53/53 | 用户占位值不再进任何校验/定位路径（grep 验证无 validate_identifier(user_id) 残留） | redirect handler（userapp_terminal_proxy_api）透传占位段进 Location——原样转发不解析，符合 spec |
| R08 | 已修：UserAppResourceBinding 自定义 Deserialize 容忍退役 user_id（其余未知字段仍拒绝）；其余 deny 类型核实为嵌套 ExecutionContext（无 deny，默认容忍） | 85c69022 | `cargo nextest run -p shared_types resource_binding` → 4/4 | `legacy_binding_with_user_id_deserializes`、`unrelated_unknown_fields_still_rejected` | SQL 迁移未加 JSON 清理（读取兼容已足够——重写后旧键自然消失，符合"只移除退役字段"） |
| R09 | 已修：暂态错误指数退避（200ms→2s 封顶，等待受统一 deadline/cancel 控制）；401/403/404/405/415/SerdeError/BuildRequest/NoResourceVersion 快速失败 | 85c69022 | `cargo nextest run -p docker_manager --features kubernetes observation` → 10/10 | `rapid_transient_errors_back_off_between_retries`（连续 429 下 1.5s 预算内请求 ≤10 次——无退避时热循环远超）；既有 9 例契约测试全保留 | 抖动（jitter）未加（单对象观察、单客户端场景抖动收益有限）；Retry-After 头 4.2 的 Status 不透出，无法遵循不声称 |

## 全量测试

- `cargo nextest run --workspace --all-features --no-fail-fast` → **2247/2249 通过**（退出码 1）。
  - 2 失败均为本轮前既有基线失败（已用基线 worktree 验证复现）：
    1. `app_manager service::tests::storage_expansion_receipt_is_bound_to_the_update_operation` — 需真实 `RCODER_RUNTIME_IMAGE_DIGEST` 镜像（环境门控）。
    2. `docker_manager runtime::k8s_app_create::conditional_tests::update_rejects_replacement_lifecycle_before_pvc_or_config_requests` — 基线 `c5782efe` 同败。
- `cargo fmt --all -- --check` 通过；`cargo clippy --workspace --all-targets` 与 `cargo clippy -p docker_manager -p rcoder --features kubernetes --all-targets` 均 0 警告。

## 未完成清单（本轮不擅自扩大）

- Compose/K8s 集成验证（R01 双引擎 Running→Stop、R02 连续两代部署、R06 目录矩阵端到端、R09 真实集群）：按 agent-prompt 约定等正在执行的集成测试完成并固定其源码/镜像身份后再安排。
- kube-runtime 批次 B/C/D（Builder 双资源观察、Events、删除观察）：未实施（verification.md 已记录批次 A）。
- runtime ownership：ArtifactId 本地制品解析器、attach、平台迁移（阶段三）未实施（组合显式拒绝）。
- app-cli 发布：仅源码修复，npm/镜像发布未执行。
- file-server shared-skills manifest 同步：未核对（独立范围）。
