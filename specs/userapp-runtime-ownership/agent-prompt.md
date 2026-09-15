# 给开发 Agent 的提示词

复制下面整段作为新开发任务。三个阶段均在方案范围内，阶段一优先形成独立可交付结果；不要将本任务误解成重新讨论方案或只修改一个超时常量。

```text
请在 RCoder 仓库实施“UserApp 运行态单一所有者”方案。你负责开发、聚焦测试、必要集成验证和交付记录；不是只做方案评审。

仓库：/Users/soddy/Documents/git-workspace/rcoder
方案目录：/Users/soddy/Documents/git-workspace/rcoder/specs/userapp-runtime-ownership
阅读基线：a4380b37a77d54797473c0417f70f03e66480b71
文档核对时 app-cli：0.3.5（开发时重新确认版本，不能直接假定下一个版本号）。
建议分支名：codex/userapp-runtime-ownership（尚未创建；先核对现有分支及 dirty 状态）。

先读：
1. 根 AGENTS.md，以及将修改路径适用的嵌套 AGENTS.md。
2. 方案目录 README.md、spec.md、plan.md、tasks.md，完整阅读。
3. specs/concurrency-quality-repair/{spec,plan,tasks}.md 中 D01–D06、Q10 与恢复保护；specs/userapp-lifecycle-convergence/{spec,plan,tasks}.md 中停止意图/取消/实例归属；specs/remote-k8s-dev/README.md 和 tests-e2e/tools/README.md。

基线工作树已有大量其他开发改动（app_manager、docker_manager、shared_types、builder、preview、K8s、E2E）。必须先 git status --short 和相关路径 diff；不得 reset、覆盖、整仓暂存或把他人改动提交进来。隔离 worktree 不能假定纯 HEAD 包含这些未提交依赖；只移入确实需要且核对过的改动并记录来源。

执行顺序：

第一阶段先独立完成错误传播，不等架构切换：
- legacy 和 serve 的 API 真正 bind 成功后才允许修改运行态；绑定失败非零退出，不停别人的服务。
- main 不吞 supervisor 错误；统一启动失败出口，尽力输出一次 Done，保留原始错误与清理结果；不能只补 pingap 分支。
- file-server UserApp manifest 路径保留可监督 Child：独立等待退出、stdout EOF/读取错误、stderr 尾部；排空有界，不因后代持有 stdout 卡死。
- 修复所有 sender 保活，包括 evt_tx 及 caller 持有的 on_event Arc 闭包。只 drop(evt_tx) 不够。
- 明确事件排空、进程退出与终态提交的顺序；保留服务事件先于终态；退出码 0 但缺 Done 仍失败。
- consumer abort/超时不等于进程停止；取消有真实 worker 收尾，清理未确认保持保护。
- 先列启动各阶段预算再处理 3600s；300s 只是候选默认底线，不能截断合法 manifest 慢启动。使用从启动起算的共享 deadline。
- 按 tasks.md A 组逐层独立测试，特别防 3010 fail-fast 遮住后续缺陷。提交本轮阶段一结果与可独立评审的 diff，再继续阶段二；不将三阶段揉成一个大提交。

第二阶段复用现有 serve admission/journal/recovery，形成唯一运行态所有者：
- shared_types 统一身份/操作/事件/错误；runtime_instance 与 deployment_generation 分开。
- 稳定显式 state_root，source/.run/别名同锁域，旧 journal 安全迁移。
- start/stop/restart/deploy 同一 worker，operation ID + request digest 持久重放；受理/终态落盘；未知结果不释放互斥。
- stop 持久化意图并提升 revision，阻止旧构建稍后启动；cancel 不伪装成 stop 或立即清理完成。
- builtin/supervisord 共用运行计划，保留 devrun/devbuild 源码态；源码不强制 zip、不替换源码根。
- artifact 准备、.run 激活、服务启停均归 serve；不能只统一进程而继续允许 file-server 写 active 目录。
- 有序操作事件可重放，file-server 保存 task-operation 关联；旧 UI 事件名尽量不变，真实 active version 与新 build version 分开。
- 固定 owner program、业务 program、attach 语义和恢复清晰；用户 stop 后重建不复活；切换中断不猜测回滚。
- 保留生产旧 /v1/deploy 的兼容和 D 系列语义；不实现增量/零停机部署。

第三阶段平台统一调用与迁移：
- start/restart/stop/list/task cancel 及所有运行态旁路逐项清点并收敛，Custom Page/Web 非本次改造对象。
- managed 是应用环境级持久模式。API 超时/403/缺能力/身份不符不得回退 legacy spawn。
- 镜像使用固定控制 program 和持久状态根；新空 workspace 能 Idle。
- 先在全新隔离环境验收，迁移既有实例需核验并停止已授权的旧 owner；未知用户 program 仅报告，不杀、不删除、不自动改脚本。
- 完成 Compose 与 remote-k8s UserApp 验证，必要时 Gateway 实际请求；验证镜像内真实版本及 digest，不能只看宿主机二进制。

实现约束：
- 遵循 SOLID、Fail Fast；生产 Rust 不新增 unwrap/expect/unsafe。
- DashMap/Mutex guard 不跨 await；并发读写检查锁顺序。
- HTTP 接口使用 utoipa 完整 OpenAPI；共享业务契约放 shared_types。
- 不将 PG/K8s 依赖引入独立 file-server/npm 形态，不修改无关 Custom Page 生命周期。
- 不按端口抢杀，不无条件删除 app-svc 前缀组，不以健康旧实例证明新操作成功。
- root agent 可绕过协议：正常 CLI 幂等、异常冲突明确失败，不虚构强安全隔离。
- 若发现源码已变化，核对目标不变量后调整实现和文档，记录理由；不要降低验证标准来适配现状。

验证与交付：
- 按 tasks.md 分阶段执行，先聚焦再扩大；Cargo 任务串行，不共用 target 并发。
- 明确区分组件故障注入、真实进程/容器、Compose、K8s、AI 和发布证据。
- K8s 用 make remote-k8s-*，读取既有 .env.local，不输出凭据、不覆盖配置、不碰其他环境/共享 Gateway，不删 agent PVC/共享 CephFS 根。
- 环境阻塞时完成可执行层级并准确列出未验收项，不伪造通过，不用旧报告。需要新增 E2E 时注册严格 suite/case/report 身份，检查无空筛选/skip/aborted。
- 在方案目录新增 verification.md，记录实际基线、每个任务的命令、退出码、日志/报告、镜像/实例/操作以及未运行项；有证据才勾选 tasks。
- app-cli 逻辑验证后准备版本和依赖契约；记录匹配 tag、release-app-cli.yml、六个 npm 包与 builder/runtime 镜像各自发布要求。默认不执行远端 push/tag/workflow 或生产发布，除非用户在开发任务中明确授权；未发布必须写待发布。
- 最终中文报告：改了什么、为何这样改、分阶段提交或 diff、验证命令和退出码、实际通过范围、剩余风险/未运行项。不以测试数量代替所有权/恢复/真实请求证明。

请从 T00/T01 和阶段一开始实际开发。阶段边界形成独立交付与报告，测试通过后依赖顺序继续，不需要为文档已经明确的常规实现选择反复请求确认。
```
