# Java 接口交接：PG 立即改密（2026-09-20 更新）

本文件取代开发期“保存配置、下次启动生效”的交接要求。Java 项目本轮未修改、未联调。

## 保存入口

调用已有 `POST /api/v1/userapp/db/{app_stage}/reset-password`，`app_stage` 明确区分 dev/prod。
请求字段以当前 OpenAPI 与 `UserappDbResetPasswordRequest` 为准：`app_id`、`password`、可选 `username`、`request_id`、`lifecycle_id`。建议调用方提供稳定请求身份和当前 lifecycle；不得使用 user_id 或 x-user-id 定位应用。相同请求重试沿用原 request_id，不使用新身份绕过未决操作。

- 指定 username：存在则修改该账号密码，不存在则按既有接口创建账号。
- 不指定 username：目标为 PGDATA 持久记录的初始化管理员，不猜测容器环境变量中的角色。
- 成功表示数据库已确认更新且新 TCP 连接验证通过；可以提示“数据库密码已更新，请按需更新应用连接配置”。
- 已认证会话继续使用；新建/重连需要新密码。调用方自行决定是否重启应用，保存入口不自动重启应用或容器。
- prod 未运行时明确报错，用户先启动容器。该入口不自动唤醒业务。

退役 `/api/v1/userapp/{app_id}/prod/runtime-configuration`，不要调用其旧保存/查询路径，也不要展示“待下次启动生效”。

## 未知结果与联调

HTTP 超时不等于未执行。按响应或状态记录的原 operation_id 和原 request_id 查询/恢复，使用现有 reset-password/recover 的 OpenAPI 契约；不得自动换请求身份再次写密码。恢复结果需区分已提交、已取消和仍未知，不能统一展示保存成功。

联调至少覆盖：运行账号改密、长连接保留、新旧密码新连接、prod 未启动、dev/prod 隔离、同请求重放、响应丢失与显式恢复。不得回传或记录密码。详见 [方案](immediate-password-update-2026-09-20.md) 和 [回执恢复协议](password-recovery-receipts.md)。
