# Claude Code 跨电脑复核与开发提示词

将下面内容作为新任务发送给 Claude Code。仓库路径以另一台电脑的实际 checkout 为准，不依赖审查电脑的 `specs/`、二进制或临时证据目录。

```text
请接手 RCoder/UserApp 本轮审查复核与修复。仓库 rcoder，分支 feature-userapp。

先执行 git status --short、git branch --show-current、git rev-parse HEAD，读取根 AGENTS.md。
不要 reset 或覆盖其他人的改动。核对远端和本地状态，在安全条件下取得最新分支。

阅读 docs/development/reviews/2026-10-08-userapp-startup/README.md 和 fixtures/README.md。
审查范围 ace54df66cadb1384b60e324eb6a15fdd632f726..76772b5752296658572bfddac1851c6b1520d979。
当前 HEAD 可能已继续推进：以当前源码、真实调用链和本轮复现为准，不能采信历史报告代替验证。
specs/ 是 gitignore 的本地目录，缺失不影响本任务；随仓记录已列出需求与反例。

按 R1→R2/R3→R4→R5/R6/R7→R8 推进。先逐项核验并补修复前会失败的行为反例，
再做小范围修复，不扩大重构；若已被修复或判为误报，给当前源码及反证结果。

优先处理：
1. app-cli journal→stdout 在原操作非终态时输出成功 Done，原启动随后可能被 Stop 取消；
   消费者遇到第一个 Done 就成功结束。done_forwarded 每轮重置也会重复或矛盾输出。
   对外终局必须绑定捕获原 operation_id 的权威结果，恰好一次；仅增加 done_seen 不够。
2. file-server run 非零退出、管理 owner 在线时，start-dev/keep-alive 不能吞错返回启动成功。
   管理面接管与原启动成功分开；读取真实监督退出与原操作结果，保留具体阶段和原因。
3. 正常移交后退休/转换原 run 登记，重复 Start 走同一 owner 控制链；旧观察不能覆盖新实例。
4. pod/ensure 创建观察和 owner 移交的内部 runtime/store/HTTP/sleep 均受共享父 deadline 钳制。
   耗尽只结束观察并保留原请求，不自动停止未知执行。creation_observed 当前只指对象存在，
   不等于本次写入生效或业务 Ready；不要无依据改变公开字段语义。
5. WakeFailure 根据结构化阶段与启动证据分类，前置查询超时不能一律宣称 Starting。
6. 内置错误页按结构化 cause 显示，不解析翻译标题；三语言、默认英文、固定标签都要验证，
   新变量同步上传白名单、文档和示例，保留转义及原 HTTP/JSON/HEAD/SSE 语义。
7. Exited→ServiceStartOk→Done 的合法成功路径保留原任务的 event:log 成功摘要；
   失败摘要保留真实 service_id/原因，重复不刷屏，不丢原任务身份。
8. 修复严格 Clippy 的参数组织与两处结果处理，不整体降低 lint。

固定边界：
- serve 是唯一常驻 owner，run 经同一控制链；稳定内核锁不删除、不重建抢占。
- 普通旧状态、PID/登记、缺失旧 ZIP/.run、历史版本差异不能永久阻挡新 Source 请求。
- Source 请求重读当前授权源码根、manifest、构建输入和新运行配置；有效且一致的锁保留。
- 管理面先于业务就绪。业务失败/Stopped 时管理与磁盘日志仍可用；只读查询不启动业务。
- Stop 取消启动意图并确认原受管业务退出，保留管理服务；旧回调不得再次启动。
- 优雅退出宽限保持3秒；它不是整个构建、RPC或恢复的总预算。
- 已确认退出与清理的普通 run.migrate 失败仅告警继续启动，不退回迁移失败阻断策略。
- 活 owner 持锁不响应、外应用/实例、清理未确认、权限/存储/端口故障、不可逆写入结果未知
  保留具体保护。不得杀未知 PID/进程名/端口，不因拒连推定后代已退出，不重放未知迁移。
- 显式凭据优先；不恢复历史密码、默认密码或自动改数据库账号；响应与日志不泄露凭据。
- 操作/实例/代次/生命周期/租约/条件写入保持全链路；物理 Stop/Restart 期间不同启动快失败。
- Java 平台仓只读，有接入问题写交接建议，不修改 Java 项目。不新增用户身份协议。
- Rust 用最新 stable 并记录实际版本；Node 保持22，不顺带升级包管理器主版本。
- Pingap v0.15 升级延期，本任务不顺带升级。构建/缓存/临时文件放工作盘，Cargo 串行。

验证要求：
- 随仓 fixtures 是协议/组件/DOM取证工具；某些 exit0 只证明缺陷存在，不能当修复通过。
  阅读安全边界后，只对自建隔离目录/进程/容器注入故障，保留卷和数据。
- 将有效反例纳入正式测试，覆盖真实调用链与关键时序，不能只测 helper 或内存标记。
- 先受影响 crate 默认 features，再 all features（含 Kubernetes）；nextest --no-fail-fast。
- app-cli 是独立 workspace，额外做 fmt、clippy --all-targets -- -D warnings 与 nextest。
- 跑受影响 Compose/隔离容器场景，验证重复Start、两调用者唯一owner、构建期间Stop、
  正常移交与失败移交、原操作终态、具体日志与身份；K8s按实际可用环境单独验证。
- 缺前置/跳过/空筛选不算通过，保留失败日志并归因。不要把受控Pingap/DOM/组件当真实E2E。

完成后交付每条复核结论、改动、修复前后反例、实际命令/退出码/源码与二进制身份，
以及未验证项。严格分开实现、组件通过、真实容器、部署和发布；不自行部署或发布，
提交与推送按当前会话授权。用户要求明确后持续实施，不只停在分析或计划。
```
