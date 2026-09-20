# Java 接入：运行凭据保存与生效分离

## 状态与边界

保存/状态接口、Toasty 版本存储、受理捕获、容器换代与 app-cli 凭据注入已实现并通过本轮组件编译验证。代次交接已补齐改密前准备屏障及激活后重启的原回执恢复；最新镜像的完整 Compose/K8s 验收仍待完成，详见 `../toasty-storage-unification/verification.md`。**此文是接口交接，不是端到端验收报告。** Java 代码由同事实现，本轮不修改 Java 仓库。

## 保存

`PUT /api/v1/userapp/{app_id}/prod/runtime-configuration`

```json
{
  "lifecycle_id": "当前应用生命周期ID",
  "request_id": "本次保存的稳定请求ID",
  "expected_revision": 0,
  "pg": {"username": "business", "password": "用户设置的新密码"}
}
```

- app_id 路径定位；不传、不解析 user_id。
- 先查询应用生命周期和配置状态；未配置时 expected_revision=0，否则用读取到的 revision。
- 重试沿用原 request_id、revision 和凭据。复用 ID 改内容被拒绝。重新编辑产生新 request_id。
- 保存不唤醒容器、不连接业务 PG、不停止业务。成功提示应为「配置已保存，将在下次显式发布、启动或重启时生效」。
- 并发编辑返回 ERR_CONFLICT，重新读取后让用户处理版本冲突，不能盲目覆盖。
- 存储连接中断时提交结果可能未知：使用原保存 ID 重试，不生成新 ID。
- 不记录请求体、密码或完整连接串。

响应 `data`：

```json
{
  "config_version": 2,
  "status": {
    "lifecycle_id": "当前应用生命周期ID",
    "scope": "Prod",
    "revision": 2,
    "saved_version": 2,
    "applied_version": 1,
    "applying_version": null,
    "applying_operation_id": null,
    "pending": true
  }
}
```

`config_version` 是本请求创建的版本；旧请求重放时，它可能小于最新 saved_version。客户端不得把旧请求的 config_version 当成当前版本，也不得因此自动重新提交旧配置。响应没有密码。

## 查询

`GET /api/v1/userapp/{app_id}/prod/runtime-configuration?lifecycle_id=当前生命周期ID`

`data=null` 表示尚未保存配置。存在配置时返回上面的 status，不含凭据。生命周期不匹配返回冲突，不自动跨代重试。

- saved_version：最新保存版本。
- applied_version：已确认在数据库生效的版本，不代表业务 Ready。
- applying_version/applying_operation_id：正在应用或写入结果不明的原操作，不能据此发新操作抢占。
- pending：saved 与 applied 不同，等待后续显式启动类操作。

业务启动结果仍以绑定原操作身份的操作状态为准，不能仅因 pending=false 就宣称服务启动成功。自动流量唤醒使用已生效版本；执行期间保存的新版本留待下次显式操作。

## reset-password 与联调

旧 reset-password 是即时数据库管理语义，不能悄悄改成保存接口。运行账号必须走版本化配置流程；普通数据库账号管理仍属独立功能。运行账号保护与启动执行链尚待本轮后续开发完成，Java 不要提前把“保存成功”显示为“密码已生效”。

联调必须验证：保存后旧密码仍可用；显式启动后新版本生效；业务失败仍报告已应用凭据版本；并发保存不污染当前执行；自动唤醒不提升待生效版本。当前尚未执行 Java 联调、Compose 或 remote K8s 集成验收。

### 即时数据库管理入口开发进展（2026-09-20）

`reset-password` 请求已增加可选 `request_id` / `lifecycle_id`，不使用 `user_id`。调用方应保留原请求身份用于重放；未传 request_id 会生成新操作，不能用于可靠重试。运行账号不允许在该入口直接改密，请使用上文的版本化配置保存接口。

源码已接入持久化操作、物理目标和写后 TCP 验证，成功 message 统一为“密码已设置”；未知结果保留原操作与恢复保护。此项仅通过编译，尚未完成端到端验证或发布，Java 不应据此切换线上调用。显式未知结果恢复与仅管理就绪唤醒仍在开发。

### 显式即时改密恢复接口（新增，尚未发布/联调）

`POST /api/v1/userapp/db/{app_stage}/reset-password/recover`

请求示例（占位值，original 必须与最初请求逐字段一致）：

```json
{
  "lifecycle_id": "originalLifecycle",
  "operation_id": "originalOperation",
  "expected_revision": 5,
  "original": {
    "app_id": "104",
    "request_id": "originalRequest",
    "lifecycle_id": "originalLifecycle",
    "username": "independent_account",
    "password": "<original-private-password>"
  }
}
```

这是显式“确认已提交，否则取消原写入”，不是重新改密。已提交时验证原密码的 TCP 登录并返回 state=Succeeded；取消先提交时写入防迟到墓碑并返回 state=Failed。HTTP 成功仅表示恢复已确认结果，请检查 data.state，不能把 Failed 显示成改密成功。

响应仅包含 operation_id、lifecycle_id、revision、state、lease_cleanup_pending，不返回密码。lease_cleanup_pending=true 表示数据库结果已确认，原物理租约仍待精确清理；不得创建新身份绕过。相同请求的终态重放返回原结果；非终态 revision 已变则查询原操作后再明确重试。

只支持带事务回执协议标记的新操作。旧操作、已换代物理目标、读不到原管理员身份、回执事务未确认、TCP 验证失败均保持保护；接口不会启动/替换容器，不会执行第二次 ALTER ROLE。请求协调器脱离 HTTP 连接执行。original 中原本未传 lifecycle_id 时应继续省略该字段，外层 lifecycle_id 仍必填，否则原指纹不匹配。

本接口不代替运行账号的“保存待生效配置”流程。生产切换仍需完成 Compose/K8s 和 Java 联调。

### start/restart 请求中的 pg（统一受理）

首次带 pg 的部署/启动请求在受理操作的同一数据库事务内初始化配置版本并捕获它，不再等业务部署结束才单独改密。该版本随后走容器代次绑定、PG 验证、迁移/业务启动流程。

已有已保存配置时，请求 pg 必须与当前已保存版本一致；不同凭据应先通过配置保存接口明确更新，再发起显式启动。不能用 start/restart 的 pg 字段悄悄覆盖配置。相同操作重放使用原捕获版本，即使其后已保存了新版本，也不会重新创建配置或提升新版本。

旧版本受理的私有部署输入若含 pg 却没有配置捕获记录，执行器在运行态副作用前拒绝，不能回退到原后置改密路径。此说明对应当前开发源码，尚未发布或完成部署验收。
