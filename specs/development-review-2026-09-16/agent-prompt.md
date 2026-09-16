# 修复 agent 提示词

你接手 RCoder 前序开发结果的修复，仓库：`/Users/soddy/Documents/git-workspace/rcoder`。

先阅读：

1. 仓库 AGENTS.md。
2. `/Users/soddy/Documents/git-workspace/rcoder/specs/development-review-2026-09-16/review.md`。
3. `specs/userapp-runtime-ownership/`、`specs/userapp-remove-user-id-binding/`、`specs/kube-runtime-adoption/` 的 Spec、Plan、Tasks 和 verification。
4. file-server 同步对应提交及当前 `/Users/soddy/Documents/git-workspace/nuwax-file-server` 源码；kube API 以 Cargo.lock 锁定版本源码为准。

目标：修复 review 的 R01–R09，并用针对反例的测试证明；不是只回复计划或重新跑现有测试。审查基线为 `7b690562b0b03bc3390bb787a9662ab9b85fa2f7`，源码证据没有经过本轮运行验证。先针对当前 HEAD 复核每项；已经被其他 agent 修复的项目不重复修改，列出提交和新验证。若发现审查误判，用具体调用链/测试说明，而非照单改代码。

## 并行开发与环境边界

- 开始先 git status --short、读取相关 diff，保留其他工作。审查开始时 `crates/app_manager/src/utils.rs` 和 `tests-e2e/tools/acceptance_steps.json` 有他人改动，不覆盖或顺手提交。
- 另一 agent 正在集成测试。其测试完成前，不改变被测工作树、同步快照或共享部署。需要先开发时使用独立 worktree、独立 Cargo target；不启动同一环境的第二套同步/部署。等本轮报告保存后再安排修复部署验证。
- 不重置/清理他人改动，不用 git add -A；按本任务改动块提交。不读取输出真实凭据，不删共享资源、agent PVC 或 CephFS 根数据。

## 开发顺序

1. 简短记录当前事实与原 Spec 的差距，更新 plan/tasks 的必要修订，然后直接实施已明确的修复。
2. 优先 R01–R04：控制消息在启动和运行阶段均可处理；操作 ID 绑定执行而非全局最近受理值；Stop 屏障与 revision 防旧提交；稳定状态根；Stopped 恢复；真正的取消/profile 执行；损坏及部分持久化失败保持保护。
3. 修 R05：退出与 Done 竞争。保留有界排空和末尾诊断，但不把已退出的编排进程判为成功运行。
4. 修 R06/R07：workspaceType 与 serviceType 分离，Computer/Git/multipart/OpenAPI 全通道一致；UserApp 路径用户段仅为占位，不解析校验，x-user-id 不参与业务；普通 Computer 用户隔离保持原契约。
5. 修 R08：PG/SQLite 存量持久 JSON 的有限迁移/兼容，不丢生命周期与物理 UID 保护，不清库，不全面关闭严格解析。
6. 修 R09：真实退避与错误分类；取消及 deadline 必须约束退避等待。watch/cache 只观察，不授予删除或接管权限。

所有未实现 profile/操作组合必须明确拒绝；不得用成功空实现、静默忽略请求、假取消或自动退回 legacy spawn。局部禁用可用来止血，但必须报告需求仍未完成。

## 验证与交付

- 先为 review 每条触发路径补有区分力的回归，再修复；测试不只检查实现细节或 HTTP 202，要检查真实选择目录、服务状态、操作归属、恢复屏障、数据保护和请求频率。
- Cargo 任务串行；按实际 crate/package 名运行聚焦测试，再执行所需 fmt/clippy/编译。默认与 kubernetes feature 分别验证。
- 运行时和代理相关修改按 AGENTS.md 验证 Compose 与 K8s；远端使用既有 `.env.local` 和 `make remote-k8s-*`，从源码构建验证使用 verify，而不是仅对旧部署 test。按影响选择 userapp/gateway/chat，smoke 不能替代功能验收。
- 外部环境阻塞时保留完整命令、退出码、日志和未覆盖项，不把 skip、历史报告、编译成功当本轮集成通过。
- app-cli 逻辑改变要核对仓库发布约定，区分本地修复、镜像部署与 npm 发布状态，不能只改源码就宣称已发布。
- 按领域小批提交，更新对应 Spec/Plan/Tasks/verification；未做的 kube B/C/D、平台迁移、shared-skills 等独立范围继续明确标记未完成，不擅自扩大本次修复。
- 最终给 R01–R09 逐项状态表：当前结论、修复提交、验证命令/退出码、证据、剩余风险；附独立的未完成需求清单。

现在开始核对当前工作树与测试环境占用，并完成以上修复。
