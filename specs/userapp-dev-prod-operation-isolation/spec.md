# UserApp dev/prod 操作隔离规范

日期：2026-09-18。状态：待实施。本文与 plan.md、tasks.md 配套。

## 需求

同一个 app_id 对应 dev 开发容器与 prod 运行容器，两者有独立计算资源和存储。独立资源上的操作不得仅因 app_id 相同互相阻塞；应用整体删除、生命周期换代等跨环境操作仍需统一保护。

核心验收：prod 的 Start/traffic wake 进入 RecoveryRequired 后，dev 已有容器可重启，dev 容器已回收时可通过 ensure 重新创建；prod 的未知结果保护和租约仍保留。反方向同样成立。

## 操作规则

| 在途操作范围 | 新 dev 操作 | 新 prod 操作 | 新应用整体操作 |
|---|---|---|---|
| dev | 冲突（既有幂等重放除外） | 可受理 | 冲突 |
| prod | 可受理 | 冲突（既有幂等重放除外） | 冲突 |
| application | 冲突 | 冲突 | 冲突（同一请求幂等除外） |

- Pending、Running、WaitingRetry、RecoveryRequired 均占用其范围；不能按超时自动释放。
- 作用范围由服务端从操作种类、命令和实际资源推导，不能相信客户端自报 scope 来绕过保护。
- 生命周期身份仍为应用级，dev/prod 不各建一份互不关联的应用身份。
- 同一环境操作排他、多副本单胜者、物理 UID/代次核验、PVC 保护、请求幂等必须保留。
- 整体操作在两个环境均可安全进入时原子受理；受理后阻止两边的新操作。本轮不实现强制抢占。
- 冲突信息应说明阻塞环境、操作 ID、类型、状态；只读状态查询不能因另一环境繁忙而失败。

## 接口边界

保留 app_id、app_stage 和现有正常控制路径；不要求 Java 删除 app_id。dev restart 仍重启物理 builder，prod restart 仍走生产运行时协调器。

本轮保持严格 restart 语义：资源不存在时明确返回不存在，应由 ensure 恢复开发环境。不要把新建能力偷偷混进 RestartBuilder。若产品要求一个“恢复开发环境”按钮，另定义受控 restore 操作及其幂等契约，不用 handler 内 ensure 再 restart 的无保护拼接。

## 非目标

不实现 force 跳过锁、不删除现场 operation/租约、不更改现场数据库密码、不批量清理 PVC；不为此重写 app-cli。生产应用启动失败的修复与未知操作恢复是关联任务，不能用本次隔离变更冒充已经恢复 prod。
