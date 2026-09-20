# Java 容器控制接入说明（开发中）

## 请求与响应

保留 RCoder `/computer/pod/stop`、`/computer/pod/restart`。UserApp 通过 `app_id`、`app_stage=dev|prod` 定位，不通过 user_id。可传 lifecycle_id 校验生命周期；request_id 用于原请求幂等，网络重试保留同一 request_id。

UserApp 返回 HTTP 202，data 含 operation_id、app_id、lifecycle_id、scope、action、state、stage、revision、error_code、error_message、status_url。202 仅表示受理，不能向前端宣称容器已完成启停。普通 agent 保留原同步响应。

GET `/computer/pod/operations/{app_id}/{operation_id}` 查询进度。阶段包含 draining_previous、stopping、stopped、starting、verifying、completed。Succeeded 才表示协调器完成；RecoveryRequired 表示仍有具体阶段待核验。不能将停止前的历史操作成功覆盖当前状态。

Stop 进行中，新的 Start/Restart 返回冲突；服务端不排队。Stop 可以中断 Restart。Java 透传 operation_id、blocker 和错误原因，前端等当前操作结束后再显式发起新请求。

## 原操作恢复

POST `/computer/pod/operations/{app_id}/{operation_id}/recover`，JSON 为 `{"expected_revision": 查询得到的revision}`。保留原 operation_id；旧 revision 拒绝。

当前支持以下恢复：

- draining_previous 且尚未取得运行时租约、未执行计算写：继续原操作。后台确认全部阻塞操作终结且原租约已释放后也会自动续行；未解除时保留恢复状态，不要求调用方不断重试。
- Stop/stopped 或 Restart/verifying：按已持久化写完成边界核验并补交终态，不重启第二次。prod Restart 同时复核物理身份与就绪。后台扫描也使用相同流程。

- K8s dev Restart/stopped：核验原租约、停止回执和旧 Pod 退出后，沿原请求继续启动；CAS 竞争只有一个执行者能提交。
- K8s dev Restart/starting：原启动回执匹配且当前代次、Pod 修订与 Ready 已确认后补交终态；已提交而未就绪只观察。新协议单写记录在确认尚未提交、原停止回执与物理身份后可条件重试，旧记录不进入该分支。
- K8s dev Stop/stopping：核验 StatefulSet 原缩容回执、原身份、replicas=0 和 Pod 全部退出后补交终态，不重发缩容。
- K8s prod Stop/stopping：原缩容回执与操作身份、控制器 UID 匹配且旧 Pod 全部退出，可补交终态。
- K8s prod Restart/stopped：确认原租约与停止回执后，后台继续原请求的剩余启动步骤；恢复响应仍需按 status_url 轮询，不能把恢复请求已受理当作启动成功。
- K8s prod Restart/starting：原启动回执匹配且当前代次已就绪时补交终态；已提交但尚未就绪时只观察，不再次启动。新协议记录带单写能力及 PVC UID 见证时，可核验原租约、停止回执、卷身份与控制器条件版本，再以原操作身份认领一次条件启动重试；旧记录不进入该分支。
- Stop 收束被替代的 Restart 时，若旧操作已持久化 stopped/verifying 完成边界，可按精确 revision 和检查点收束旧执行、释放其原租约。新协议 K8s dev/prod 的 starting/stopping 还可通过原条件版本隔离旧计算写后收束；随后继续真正的缩容与退出确认。该行为不将旧 Restart 标为成功，也不把没有隔离证明的未知结果当作完成。

没有匹配回执的旧版本操作、其他后端或其他未知阶段仍要求进一步核验，不能将本入口当作通用清锁接口。恢复响应中的 state 可能已经为 succeeded，调用方按实际 state 展示。

## 未完成联调

Java 项目本轮未修改。202 处理、前端轮询、结构化错误透传及真实聊天链路尚未联调。该文档描述当前开发接口，不是部署验收证明。
