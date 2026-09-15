# UserApp 移除用户绑定：需求修订入口

修订日期：2026-09-15。状态：**文档方案；代码、数据库、容器未由本次任务修改**。

本版本替代此前初稿，以用户最新决定为准：UserApp 只按 `(stage, app_id)` 定位，移除 user_id 业务绑定；`x-user-id` 也从 UserApp 请求契约中删除，Java 调用方不再发送。

## 正式文档

- [Spec：业务要求、边界和 Java 契约](spec.md)
- [Plan：改造范围、数据库升级与环境切换](plan.md)
- [Tasks：实施任务和验收矩阵](tasks.md)
- [开发 Agent 提示词](agent-prompt.md)

## 与初稿的关键差异

1. UserApp 不读取、解析、校验、生成或依赖 x-user-id，不提供 metadata owner/body/query 替代回退。旧请求意外携带时忽略其值，UserApp 转发边界移除该 header，避免下游重新解释。
2. 普通 Computer 业务维持用户语义；computer_intercept 的 X-Service-Type: userapp 分支属于 UserApp，必须改。不能按函数名划边界。
3. 路径 helper 固定 userapp 命名空间，优先只接 app_id，不接任意 service_type 字符串。
4. 覆盖 tasks/static/body/query/multipart、app_manager 生产管理、locator、恢复与清理，不只删除容器侧 request 字段。
5. 移除用户校验，保留应用、服务族、生命周期、操作、物理实例及代次保护。
6. 数据库升级包含列和序列化记录；旧容器不迁移、不接管，不等于自动删除旧数据或跳过数据库升级。
7. 多用户测试改为不同占位输入命中同一实例，不直接删除覆盖。

## 基线与协作

仓库：`/Users/soddy/Documents/git-workspace/rcoder`。

阅读 HEAD：`e8c1708cbf09b6133244ae291eba7402b7985a91`。工作树已有启动监督、UserApp dev、共享删除类型、E2E 等未提交修改，开发时重新核对 status/diff，不覆盖其他 agent 工作。

本轮只重写此入口并新增上述四份文档，没有创建分支、执行迁移、重建容器或运行代码测试。

与 [运行态所有权方案](../userapp-runtime-ownership/README.md) 的顺序：启动错误传播阶段可独立继续；身份简化先于新运行协议定型，后续以 stage/app_id 为逻辑身份，不引回 user_id。
