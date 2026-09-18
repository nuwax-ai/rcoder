# R01–R11 / B01–B05 修复状态（2026-09-18 轮）

基线：review 基线 `19dfd381` → 本轮 `3b931eb8`（rcoder）、`a4a4522` → `59b7a0f`（build-agent-docker）。

## 已完成（含反例与验证）

| 项 | 提交 | 反例/验证摘要 | 未尽范围 |
|---|---|---|---|
| R01 | `2574f8c5` | spawn_managed 接入 start_service/migrate/pingap/supervise 全链；三处真坑（wrapper wait 缓存短路、reap 后 id()=None、macOS 僵尸窗 killpg EPERM）修复；tests/tree_lifecycle 真实编排链反例（忽略 TERM 持端口孙进程：正常停机整树收束 + 失败兜底清理 + 二轮可启动）；app-cli 202→207 全绿 | Windows/Linux 真机矩阵本轮复验（见下"未验证"） |
| R04 | `7e74cd0b` | Cancelled 不再等价成功：复用 Restart 终态 Cancelled → 报错不登记 external；Stop Cancelled → 报错保留登记。mock 反例：cancelled_stop_is_not_success、owner_probe（restart 侧仅 Succeeded 登记） | — |
| R05 | `7e74cd0b` | 登记先取快照、Succeeded 确认后才摘除；在途停止操作受理即持久化、重试按原 operation_id（反例：failed_stop_keeps_registration_and_retry_reuses_operation_id——受理数恒 1）；external 控制关系持久化（token 不落盘，空哨兵 + 状态根重读，反例：external_registration_survives_manager_restart_without_token_on_disk）；stop_userapp_dev 禁 ps 扫描（登记缺失 + owner 在 → 拒绝，反例：userapp_stop_without_registration_refuses_when_owner_listens） | 完整 task-operation 关联持久化随 R02/R03 批 |
| R06 | `7e74cd0b`（双侧） | 事件消费从 SSE 长连改 after_seq 游标重放轮询（消 10s 总超时/EOF 竞态/UTF-8 跨 chunk）；终态后 join + 游标排空再完成；Completed/Failed → orchestration_done 映射（mock 断言真实 owner 成功产生 Done）；kernel Failed 事件带错误载荷；orchestration EVT 事件桥进操作 journal（服务事件对复用路径可见，kernel 反例 orchestration_bridge_appends_to_active_operation） | — |
| R07 | `578ac9f6` | 期望三态（Captured/NoOwner/ObservationFailed）；观察失败 → 迟到提交明确拒绝不刷新期望（反例：observation_failure_blocks_late_submission_and_pg_travels_on_wire——token 补齐 + revision 推进后旧提交仍拒、零操作提交） | 期望与构建 task 的强绑定（并发构建互斥靠 dev lifecycle generation，R02 批统一） |
| R08 | `578ac9f6` | RuntimeOperationRequest.run_config（不参与幂等摘要，serde default 兼容）；app-cli DispatchAction→server 槽→run_with_cancel→start_service last-wins 注入；持久化密码脱敏。三层反例：supervisor env dump（新凭据覆盖 spec.env）、kernel dispatch+脱敏、wire run_config.pg 契约 | — |
| R09 | `1fcc3860` | 新 crate runtime-state-layout（app-cli 与 file-server 同一契约）：显式 env → PROJECT_ID 段 → standalone 登记表（跨进程互斥、canonicalize/symlink 折叠、.run 别名回源码根、损坏 fail-fast）。反例：兄弟项目独立根、source/.run 同根、symlink 同根、损坏 fail-fast；file-server token 查找经登记表命中。附带修复 legacy 迁移在 /var↔/private/var 前缀差异下的 EINVAL 真坑 | Windows junction 行为随真机轮验证 |
| R10 | `3b931eb8` | wait_deploy_stage 消费 bind-once 持久 deadline（首次执行与重放同窗）；热部署协调预算走 deploy_budget.absolute_budget_secs（不再固定 1800s）。app_manager 194/195（唯一失败为既有 RCODER_RUNTIME_IMAGE_DIGEST 环境前置） | deploy_control 执行链中各子阶段的逐段剩余预算传递（当前 wait 主链已统一）；绑定失败收束语义 |
| R11 | 本文件 | 任务勾选审计：见下"修订" | 错误类型化（error-string-matching-elimination 五处）**未实施**——按 review 要求先修计划缺陷（RpcFault 字符串化/Option Display/anyhow downcast/Vite 链/pnpm 专属参数）再实施，本轮未启动，仍为待办 |
| B01 | build-agent-docker `59b7a0f` | 参数序修复 + 本机真实二进制复现旧序 exit 2/新序成功；容器态 ps 确认 `app-cli --workspace ... serve` | — |
| B02 | build-agent-docker `59b7a0f` | startsecs=0；dev-rcoder-agent-runner 镜像 + 真实 supervisord 容器验证：managed=0 EXITED 预期退出无 BACKOFF/FATAL（对照实验证明旧配置 FATAL）；managed=1 RUNNING + /health alive | — |
| B03 | `936b8cf6` | service env APP_CLI_MANAGED=1 → 自动注入真实 workspace（code/ 解包根，非 empty）+ 按创建部署凭据；单元反例锁定（关闭不注入）。managed E2E 随镜像重建 + Compose 轮 | Docker 路径对称注入；managed 模式真实 builder E2E |
| B04 | `1fcc3860`（R09） | 状态根/凭据查找统一到共享契约（显式 env 权威；find_owner_token 契约化 + legacy 兜底） | K8s 真实 builder 全链验收随镜像轮 |
| B05 | 未实施 | Secret 轮换 rollout 标识需 Helm 模板改造 + 集群验证——本轮未动 | 全项（含双令牌过渡设计） |

## R11 勾选审计修订

- userapp-runtime-ownership/tasks.md 的 P3-06 已勾但 remote-k8s 未运行：**维持原记录不回改**（verification.md 已如实注明未运行项），本轮仍不将其计入"容器回归完成"。
- 本轮新增各项均以上表证据为准；未验证平台/环境项在 verification 中逐条列出，不以组件测试宣称部署验收。

## 明确未完成（后续批次）

- R02（CLI 无子命令转交唯一 owner / attach 语义 / "最后受理生效"持久化队列）与 R03（平台 staging→owner 激活协议）——两项是所有权架构的剩余主体，未在本轮实施。
- R11 错误类型化五处 + lockfile 边界核对。
- B05 Secret 轮换 rollout。
- N02–N10 原生缺口中：N03/N04/N05/N06/N07/N08（file-server-proxy 侧）/N09/N10 未动（N01 已做）。
- 三平台真机矩阵 + Compose + remote-k8s 回归（本轮改动后待跑，见 verification）。

## 2026-09-18 追加：CLI 直觉形态与镜像发版

- **CLI global**（`6aa01c1e`）：用户反馈 `app-cli --workspace X serve` 反直觉——运行参数（workspace/log-dir/admin-addr/pingap-bin/attach）声明 clap `global = true`，`app-cli serve --workspace X` 与顶层顺序均合法（解析测试锁定双顺序等价；app-cli 208/208）。包装脚本改回直觉形态 `serve --workspace`（镜像仓 `b398cbb`）。
- **镜像发版**：build-agent-docker 全量 0.1.272（补齐 test 命名空间复制源：agent-platform-front/backend、mcp-proxy、mysql-migrate + 四核心镜像，双架构）+ 快速发版 0.1.273（重建 rcoder-k8s/agent-runner/app-runtime-base/app-runtime 含全部本轮修复与新 CLI；辅助镜像从 0.1.272 复制）。Helm Chart 0.1.272/0.1.273 均已推 ACR（nuwax-k8s-test，pingap=0.14.3 commit cd74a46）。
- 部署入口（用户执行）：`./k8s/deploy.sh deploy k8s-test --storage-backend ceph`。

### 既有 flaky 记录（非本轮引入）

`serve_restart::rejected_native_serve_sigterm_does_not_clear_previous_active_owner`：空 workspace 二次 serve 偶发相位停在 idle（期望 orchestrating）。基线复现（stash 本轮改动后同条件 5 连跑 2 失败/3 通过，2026-09-18）——恢复分支时序敏感，与本轮 R03/N02 无关。归因待深入（怀疑 stale-owner 恢复判定竞态）；不删断言、不放宽期望。

### 2026-09-18 第二批：N02/R03/R11(部分) 完成

| 项 | 提交 | 摘要 |
|---|---|---|
| N02 | `8adc8db2` | 路径护栏三平台统一（字面量根+实际布局根并集、组件化比较）；pingap 运行目录默认 `{log_dir}/pingap`（env 优先）；svc_spec 平台无关断言；Windows symlink 制品结构化拒绝。**Windows 186/186 全绿（原 4 失败清零）**；Linux 207/207 |
| R03 | `8adc8db2` | Deploy+Artifact(ArtifactId) 受支持组合 → owner 侧激活（共享卷 builds/ 直读不经下载）；平台 `route_artifact_restart`：owner 在=Restart(ArtifactId)（owner 核验后激活）；拒绝=`.run` 原样（反例锁定 marker 保留+无 .previous+wire 形态）；无 owner=本地 activate+spawn 不变 |
| R11(批1) | `62947c6e` | xmlrpc RpcFault typed downcast（faultCode 10 优先+措辞兜底；网络错误文案反例不再误判）；DownloadError::Http 结构化 status（is_retryable 按码；URL 文案反例）；InstallFailed 死变体删除 |

**仍未完成**：R02（CLI 转交/最后受理生效）、R11 剩余三处（cluster_cache not_found / chat timed-out / Vite PortInUse 链）、B05（Secret 轮换 rollout）、N03–N10（proxy 原生形态/端口/随包/分发）、NT 矩阵大部。

### 2026-09-18 第三批：R11 错误类型化全部完成

| 点 | 提交 | 摘要 |
|---|---|---|
| xmlrpc RpcFault | `62947c6e` | typed downcast（faultCode 10 BAD_NAME 优先+措辞兜底；网络错误文案反例） |
| DownloadError::Http | `62947c6e` | 结构化 status（is_retryable 按码；InstallFailed 死变体删除） |
| EnsurePodResponse.code | `9ac39c6c` | not_found 判定读信封 code 字段（降级策略不变） |
| AcpError::Timeout | `9ac39c6c` | 类型化（仅等待完成超时；CLI 退出码 downcast） |
| Vite PortInUse 链 | `2e835975` | AppError::ProcessPortInUse 类型变体（文案改分类不变；旧标记文案反例）+ pnpm 单通道 |
| pnpm message 通道 | `2e835975` | 只留 typed code（classify 边界解析保留） |

R11 五点全部落地（每点带反例测试）。全 workspace 2334 测试 2333 通过（唯一失败为既有 RCODER_RUNTIME_IMAGE_DIGEST 环境前置项）；fmt/clippy 零告警。

**剩余未完成（后续批次）**：R02（CLI 转交/最后受理生效/attach 语义）、B05（Secret 轮换 rollout 标识）、N03–N10（proxy 原生形态/端口计划/随包 Pingap/分发安全）、R02 相关的期望-构建 task 强绑定、NT 矩阵大部。
