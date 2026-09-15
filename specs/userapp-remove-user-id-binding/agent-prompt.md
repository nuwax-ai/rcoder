# 开发 Agent 提示词

将以下内容复制给负责实现的 agent：

```text
请实施 RCoder 的“UserApp 移除用户绑定”方案。

仓库：/Users/soddy/Documents/git-workspace/rcoder
文档：specs/userapp-remove-user-id-binding/{requirement,spec,plan,tasks}.md
阅读基线：e8c1708cbf09b6133244ae291eba7402b7985a91
建议分支：codex/userapp-remove-user-id-binding（尚未创建，先核对实际状态）。

先完整阅读根及适用的 AGENTS.md、上述四份文档，以及相关生命周期/并发规范。先 git status --short 和相关 diff，保留其他 agent 的启动错误传播、Child 监督、sender 生命周期、取消与 E2E 修改。不 reset、不覆盖、不 git add -A；隔离 worktree 时不能假定纯 HEAD 包含未提交依赖。

最终业务要求：
1. UserApp 只按 stage(dev/prod)+app_id 定位。同 app 所有使用者共享一个 dev；prod 仍独立。
2. UserApp 请求不再有 user_id 绑定，x-user-id 也从契约中删除，Java 不会发送。不得要求、解析、校验、生成或以 metadata/body/query 回退解析用户。
3. 旧请求意外携带 x-user-id 时，UserApp 转发边界按名称移除，不读取值，避免下游再消费。其他额外 user_id 字段窄范围忽略，不维持假用户字段/默认用户。
4. 代理 URL pattern 原样保留 user_id 段，但 handler 不提取、不校验。新 URL 固定 0 占位，不能直接省略段，也不能从 metadata 生成用户段。
5. 普通 ComputerAgentRunner 用户/header 语义不变。computer_intercept 的 X-Service-Type:userapp 分支属于 UserApp，必须同步修改；普通分支不得被全局 header 清理影响。
6. 宿主 dev/prod 下固定 userapp 命名空间；path helper 优先只接 app_id。容器内挂载点不变。builder identifier 纯 app_id，不留旧复合 key 回退。
7. 去掉用户校验和 owner-id 用户 annotation，但保留 app/服务族/lifecycle/operation/executor/physical UID/generation/revision/租约/恢复/条件删除保护。
8. 不迁移、不接管旧每用户容器，按新模型重建；不自动合并或删除旧目录/数据库/PVC，不删除 agent PVC/共享根，不清旧未知操作解锁。

不要按初稿机械删字段。范围必须包括：
- shared_types 的 paths、validation、builder key、lifecycle、metadata、resource binding、control、locator。
- rcoder builder 创建/停止/恢复/清理以及所有转发分支：tasks/static/新接口/body/query/header。
- rcoder-proxy dev/prod 工具代理及 HTTP/WS，保留 URL pattern。
- file-server-userapp DTO/handler/multipart/task scope/OpenAPI。
- app_manager 生产 create/update/query/upload/storage/deploy/响应/URL 生成，不只改开发端。
- Docker/K8s 路径、label/env/annotation、服务族缓存和 selector。公共 Computer 逻辑不全局删除；UserApp 禁止无服务族旧 selector fallback。
- PG/SQLite metadata 列及持久 JSON、旧 input/alias/fingerprint。共享 sql.rs 不是 SQLite 专属；deny_unknown_fields 旧数据必须安全升级。

数据库：新增 migration，不改历史 migration。旧 schema/JSON 实测升级，不能递归删嵌套业务配置里的 user_id；不把 pending 改成功、不丢物理绑定。旧 writer 退出前不 DROP 列，新旧身份不能无验证混跑。真实环境切换不在默认开发授权内。

实现按 tasks.md 推进，建立本轮 verification.md。特别测试：
- 不传 header/字段全链路成功；旧不同/非法业务用户值不影响实例/摘要；上游 header 已移除。
- URL 占位 0/alice/bob 同一 dev，dev/prod 物理实例与存储分离。
- 并发 ensure 单实例；旧 stop/delete/recovery 不能影响新物理 UID。
- 普通 Computer 两用户行为不变。
- PG/SQLite 旧数据升级、损坏/中断恢复、未知操作保护。
- 原多用户测试转换为共享实例和隔离覆盖，不简单删除。

遵循 SOLID/Fail Fast；生产 Rust 不新增 unwrap/expect/unsafe；共享业务契约放 shared_types；HTTP 用 utoipa 完整注册。锁 guard 不跨 await。独立 file-server/npm 不引入 PG/K8s 依赖。

先聚焦测试，再相关 crate/feature，再按 AGENTS 执行 Compose/K8s。Cargo 串行；远端只用已有 .env.local 和 make remote-k8s-*，不输出凭据、不碰其他环境。新 E2E 注册严格 suite/case/report，检查无 skip/aborted/空筛选。环境不具备时完成可执行层级并明确未验收，不伪造通过。

本任务不顺带实施完整单一 serve 协议。启动错误传播阶段可独立继续，后续运行所有权方案必须使用 stage/app_id，不引回用户。

交付中文报告：改动及理由、scoped diff/提交、命令和退出码、数据库升级证据、实际镜像/实例/请求结果、未运行项、剩余 user_id 合法命中分类、Java 接入说明。Java 修改由同事执行，不声称其已经上线。

默认做开发、测试和本地交付，不自动远端 push/tag/发布、不自动停旧用户容器或清旧数据。实际部署/迁移按用户后续授权。请从 T00 开始开发，不停留在重复方案讨论。
```
