# Kubernetes 观察能力接入规范

状态：实施草案，尚未开发。调研依据见 [research.md](research.md)。

## 目标与范围

保持对外 HTTP/SSE、SQL 操作状态、资源归属和删除语义，通过 kube-runtime 改善 Pod 等待、builder 控制观察和 Kubernetes Events。Docker 行为不变。删除观察为独立可选后续批次。

不引入 CRD、Controller、全局 reflector、自动租约接管、数据库迁移、容量限制或新的发布协议。未经业务决策，不改变 CrashLoopBackOff/ImagePullBackOff 的现有提前失败策略。

## 固定不变量

| ID | 行为不变量 |
|---|---|
| KR01 | 通用 Pod 等待保留现有 Ready/Succeeded 成功及明确失败分类；不存在不直接成功 |
| KR02 | 要求身份的调用必须验证 namespace、类型、owner、生命周期和物理 UID/ownerReference；稳定名称不替代身份 |
| KR03 | 同一观察的绝对截止时间覆盖初始化、连接、重连、退避、复核；嵌套不得重置预算 |
| KR04 | 取消仅终止本观察，释放流和订阅；HTTP 等待者取消不取消已受理共享操作 |
| KR05 | 403/不可恢复错误及时返回；410/明确暂态观察失败仅在原预算内恢复，不重放远端写入 |
| KR06 | watch 不赋予执行权。未知写入、观察超时、断连不授权释放操作租约或重新创建 |
| KR07 | Builder 唤醒/重启/停止保留各自 STS/Pod 双资源判定；旧 Pod Ready 不完成新重启 |
| KR08 | 删除后不同 UID 必须冲突并阻止后续清理；不存在才完成相应检查点；Agent PVC 永不删除 |
| KR09 | Event 发布失败、队列满和关停超时不改变业务结果；诊断丢失具有计数及日志证据 |
| KR10 | Event 绑定真实对象身份，英文稳定 reason/action，无凭据和敏感参数；SQL 是操作结果事实源 |
| KR11 | 不依赖 Store 缺失、BOOKMARK 或锁消失确认业务成功；status 变化不能被 generation 过滤 |
| KR12 | 保留正式 HTTP 200＋HttpResult、旧 TS 兼容协议、app-cli 发布完成身份及热部署语义 |

## 验收边界

组件测试证明确定性时序与错误路径；流式 API 契约测试证明实际 list/watch 请求、RV 恢复、取消；真实专属 K8s 套件证明跨进程业务路径。前置缺失、skip、零测试和缺报告均不得通过。

测试仅操作 run 所有资源，Agent PVC 保留；不修改共享 Gateway、存储根、已有业务数据。不以停止 apiserver 或节点制造故障；网络故障用测试进程范围的协议适配器实现。

性能目标：在受控长等待场景减少重复 GET；真实场景报告请求数、等待耗时、watch 活动数及错误率，不预设未经测量的收益数字。无负载数据时不得宣称性能验收通过。
