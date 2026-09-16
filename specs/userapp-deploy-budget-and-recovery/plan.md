# UserApp 部署确认预算与恢复机制改进方案（plan）

日期：2026-09-16。状态：**提案，待立项**——本文档只做方案设计，不含实现。
关联：第三轮审查 R 系列（操作内核状态机）、`stop-restart-delete-lock-fail-fast`（d38ed66c）。

## 1. 背景与事故证据

当前部署确认链路：

- handler 侧 300s HTTP 等待预算：超时返回 "deployment confirmation timed out;
  reconciliation continues"（协调器脱离 HTTP 继续收敛）；
- 协调器侧 1800s（30 分钟）deploy-stage 确认预算：超时 → 操作转 `RecoveryRequired`；
- 租约保留：`k8s_app_operation.rs` 设计为"无过期、无自动接管"——未完成变更
  保留 ConfigMap 租约（写保护），要求 **operator recovery**；
- 存储层围栏：`RecoveryRequired` 的当前操作阻断该应用一切新操作
  （`OperationInProgress`），`retry` 接口对无最终证据（step 未到
  `deployment_completed`）的操作按 B′ 围栏拒绝自动重放。

2026-09-16 131 环境（rcoder-e2e-soddy）实测事故链：

| 时刻 | 事件 |
|---|---|
| 18:47 | cold deploy 受理（runtime 镜像入口脚本丢失 +x，属构建缺陷已修） |
| ~18:48 | prod pod 第 1 分钟即 CrashLoopBackOff（exec permission denied） |
| 18:52 | handler 300s 超时返回；协调器继续持有应用锁 |
| 18:47–19:17 | stop/delete 全部立即 ERR_CONFLICT（同实例进程锁与跨副本 K8s 租约两层均实证生效）——**快失败语义按设计工作，冲突立即可见且带持有者操作 ID** |
| 19:17 | 协调器 1800s 预算耗尽 → `RecoveryRequired`，租约保留 |
| 之后 | `retry` 拒绝（无最终证据）；需人工：删租约 ConfigMap + 手工回收 K8s 资源（操作员裁决"确定性失败、无在途写"） |

问题定性（不是"30 分钟不够长"）：

1. **必然失败的部署也在傻等满额**——pod 第 1 分钟已确定性 CrashLoop，却等满 30 分钟；
2. **失败后要人工恢复才能解锁**——`RecoveryRequired` + 租约保留 + B′ 围栏三者叠加后，应用对 API 完全不可操作（连删除都不行），直到操作员介入；
3. **固定总额两难**——加大预算对卡死部署无效，减小预算会截断合法慢部署（大镜像、慢 DB 迁移）。

## 2. 改进一：确定性失败信号早停（P0，性价比最高）

等待不是盲计时，而是主动观察。确定性失败信号出现 → 立即判失败（Fail Fast），
不消耗剩余预算：

| 信号 | 判据 | 说明 |
|---|---|---|
| CrashLoopBackOff | `restartCount ≥ 3` 且从未 Ready | 从未 Ready 的反复重启不可能自愈 |
| 拉取永久失败 | ImagePullBackOff 且 reason 非限流（NotFound/Denied/AuthRequired） | 限流退避（toomanyrequests）不算，归无进展看门狗 |
| 不可调度 | Pending + Unschedulable condition 持续无缓解 | 资源不足不会自愈 |
| 反复 OOMKilled | OOMKilled ≥ 2 次 | 同上 |

- 实现挂点：部署协调器等待循环内已有的 pod 状态轮询处追加判据；只读观察，
  不新增写路径；错误信息携带信号细节（restartCount/事件 reason）。
- 效果：本类事故失败判定从 30 分钟降到 **2–5 分钟**；1800s 预算只留给
  "慢但合法"的部署。
- 风险与对策：误判（如节点抖动短暂 CrashLoop 后恢复）→ 阈值保守（≥3 次）、
  阈值可配置；判错不销毁资源，仅操作转失败，可 retry。

## 3. 改进二：无进展超时替代固定总额（progress watchdog）

deadline 从"总共 1800s"改为"**N 分钟无阶段推进才超时** + 绝对护栏上限"：

- 阶段推进信号（任一出现即重置无进展计时）：
  镜像 Pulled → 容器 Created → Started → readyReplicas/探针计数变化 →
  app-cli `/v1/deploy/status` 阶段推进。
- 默认参数建议：无进展窗口 10 分钟；绝对护栏 60 分钟（防无限续期）。
- 效果：合法慢部署可超 30 分钟（回应"极端情况确实不够用"）；卡死部署在
  停滞处提前失败。这是活性（liveness）/死线（deadline）分离的经典做法。
- 遵守 AGENTS.md："不能通过放大超时掩盖错误传播问题，也不能任意缩短预算
  截断合法任务"——本方案同时放宽了对合法任务的总预算、收紧了对死任务的判定。

## 4. 改进三：RecoveryRequired 分层自动恢复

现状不区分两种情形，一律人工：

- **(a) 静默可证明**：协调器已写出失败终态（error_code/revision 已推进）+
  executor 无心跳（executor_id 无任何 rcoder 副本认领，或租约 token 对应
  pod 已消失且过宽限期）→ 无在途写。
- **(b) 真不确定**：无终态写入、或 executor 无法确认消失（可能仍在写）。

(a) 由对账器自动恢复：

1. 把操作闭环为 `Failed`（携带恢复证据：判定依据 + 观察快照）；
2. 经现有 receipt 校验路径释放租约（复用 `release_captured_application_operation`
   的身份核验，不新增旁路）；
3. 应用解锁，API 恢复可用；全程审计日志。

(b) 维持现状（人工裁决 + 只读观察证据）。

与 B′ 围栏的关系：B′ 是**请求级**自动重放防线（防迟到写），本方案是**系统级**
对账收敛——不改写操作结果语义，只把"已确定失败但被围栏"的状态收敛掉。
风险：自动判据出错误放 → 判据保守（宁 (b) 勿 (a)）、先灰度、保留手动回退。

## 5. 阶段四（战略，独立立项）：等待期取消语义

部署等待期的 stop 不应返回 Conflict，而是**取消请求**（stop = 取消未完成
部署 + scale 0）。用户开始大部署后想停，产品正确形态是可取消。前置依赖：
操作内核状态机（第三轮审查 R01–R06）——取消受理与执行身份、终态单调性
未闭环前实现取消会放大竞态。明确不在本方案第一批实施。

## 6. 实施顺序与验收

| 批次 | 内容 | 验收反例（修复前必失败） |
|---|---|---|
| 1 | 改进一（信号早停） | 入口必崩镜像 fixture：部署应在 ~3 分钟内失败而非 30；错误信息含重启计数 |
| 2 | 改进二（progress watchdog） | 慢启动 fixture（可控延迟探针）：无进展 N 分钟失败；持续进展可超旧 1800s |
| 3 | 改进三（分层自动恢复） | 注入"协调器写出终态后消失"：应用应在宽限期内自动解锁可删除；注入"写中途消失"：保持人工围栏 |

每批：反例先行 → nextest 聚焦 → 受影响默认/全 features → compose/K8s 分层回归。
共享 Cargo target 串行；K8s 场景用 `make remote-k8s-verify`（快照/镜像 digest 绑定）。

## 7. 开放问题（立项时定）

- 无进展窗口 / 绝对护栏 / 信号阈值是否暴露为配置（全局 or per-app）？
- 自动恢复的宽限期取值与 executor 心跳的实现载体（复用租约 annotation or 新增）？
- 失败信号是否透传给 Java 端作为可展示文案（涉及信封 message 契约）？
- 绝对护栏 60 分钟与 Java 侧调用超时（prodRestart 360s / prodStop 30s）的协调。
