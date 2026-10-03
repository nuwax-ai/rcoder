# UserApp 容器状态与业务就绪

`GET /api/v1/userapp/{app_id}/{app_stage}/readiness?user_id={user_id}`
返回只读快照。`app_stage` 为 `dev` 或 `prod`，`user_id` 仍必填。
HTTP 成功与 `code=0000` 表示查询成功，`data.ready` 才表示业务可用。

## 新增容器状态

现有业务字段不变，`data.container` 独立表达容器控制进度和实际状态：

```json
{
  "ready": false,
  "status": "starting",
  "container": {
    "status": "restarting",
    "operation": {
      "operation_id": "accepted-operation-id",
      "action": "restart",
      "state": "running",
      "stage": "stopping",
      "revision": 3,
      "error_code": null,
      "error_message": null
    }
  }
}
```

| container.status | 含义 |
| --- | --- |
| missing | 没有观察到计算资源，且无已停止意图 |
| starting | 计算资源正在创建、调度或启动 |
| restarting | 当前 Restart 控制操作已受理，尚未完成 |
| stopping | 当前 Stop 控制操作已受理，或实际资源正在退出 |
| running | 计算资源在运行；不保证应用或管理 API 已就绪 |
| stopped | 已停止；Stop 删除计算资源后保留的停止意图也属于此状态 |
| failed | 运行时报告计算资源失败 |
| recovery_required | 当前容器控制操作还有待核验的结果，详情见 operation |
| unknown | 本次无法确认物理状态 |

Restart 的停止、启动、验证阶段均为 `restarting`。物理 Start 当前复用
Restart 协调器，因此也按该真实控制动作展示。应用热部署、应用进程重启
不因此变成容器重启：容器可以 `running`，同时业务 `starting`。

Docker 强制结束进程可能留下非零退出码（例如 137）。当前 Stop 已确认成功、
停止意图仍有效且实例确实未运行时，返回 `stopped`；失败或待核验的 Stop
不能用这条规则掩盖故障，后来实际运行的实例也不会被历史 Stop 显示为停止。
业务编排已明确失败时，顶层原因为 `ORCHESTRATION_FAILED`，服务级探测明细保留。

`container.operation` 指向当前生命周期、当前 dev/prod 控制头的准确操作，
包括终态 `succeeded / failed / superseded`；未终态为
`pending / running / recovery_required`。操作失败不抹掉当前实际运行状态。
`revision` 只比较同一个 operation_id，不能跨操作或跨环境比较。

新意图可能替换控制头，也可能清除其关联，因此 `operation=null` **不表示**
之前的操作成功。原操作可继续通过
`GET /computer/pod/operations/{app_id}/{operation_id}` 查询。
旧版本没有 `container` 时按未知处理，不能当作未部署。

## 调用方流程

1. 点击容器重启后立即显示处理中；接口仍为异步受理，不增加固定等待。
2. 保存受理结果的 operation_id，再轮询 readiness 或原操作查询接口。
3. 丢弃点击前发起、之后才返回的旧 readiness 请求；轮询尽量串行。
4. `starting / restarting / stopping / recovery_required` 时展示进度或原因，
   不因业务 `not_deployed` 自动追加部署，不在客户端隐式排队控制请求。
5. 按关联的原操作终态展示成功/失败，再独立判断业务 ready。
   `ready=false` 本身不授权再次启动或部署；失败后用户仍可显式重试。

查询总预算保持 8 秒。超时使用 `OBSERVE_INCOMPLETE`，保留本次已经完成的
控制信息读取；未观察的物理状态不猜测。查询不唤醒容器、不刷新闲置时间，
也不清锁或推动恢复。它是一次观察，不是后续写请求的准入凭证。

## Java 与前端接入说明

现有 Java readiness 的 `data` 是 Map，新增 `container` 可直接透传。
本批不修改 Java/前端。以下两项仍需 Java 单独修复与联调：

- UserApp 错误信封的顶层 `operation_id` 和 `blocker` 应保留到最终响应；
  目前不能因为 Rust 已返回这些字段就宣称前端已收到。
- `prod/restart` 应消费调用方传入的 `releaseId`，或明确拒绝不支持的输入；
  当前 Controller/应用服务丢弃该参数后选择最新构建。`prod/start` 正常传递。

业务重启和容器重启是两个操作；如果调用方确需串联，必须等待第一步
原操作成功，不能把 `accepted/pending` 当作完成后马上发第二步。

## 部署与升级

修复前部署基线为镜像 `0.1.317`。修复部署代次混用需要更新容器内 app-cli，
仅更新 RCoder 不能改变存量容器内二进制。修复版保留监督会话代次与部署
代次各自用途，仅对证据匹配的混用 journal 自动归一；不重放未知迁移，
不改写历史业务结果。发布和存量容器更新的验收独立记录。
