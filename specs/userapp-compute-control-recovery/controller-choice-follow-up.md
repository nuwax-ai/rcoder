# 控制器选型待评估：暂不调整

用户要求：先记录，后续评估 dev 是否确有 StatefulSet 的必要；本轮不迁移控制器。

## 需要核实的历史原因

当前 dev 复用 agent-runner 的稳定实例名称和工作区模型。用户指出，可能主要为了统一 Docker Compose 与 K8s 的定位逻辑，按稳定名称发现实例、获取 IP/端口，而非依赖 StatefulSet 的有序副本或专属存储能力。该历史原因尚未通过提交历史完整证实。

Compose 中没有 K8s Pod，稳定定位对象是容器名称；K8s 的 Pod 名称、控制器身份和 Service 地址不能混为一谈。

## 后续比较范围

- 保留 dev StatefulSet，或改为单副本 Deployment + Service；比较真实调用链对固定 Pod 名和 ordinal 的依赖。
- 两种控制器均保留原 PVC；不能因采用 Deployment 推断应用无状态，也不能采用会产生并存写入的更新流程。
- 控制器/Pod/容器的 UID 或 ID 用于操作身份核验，稳定名称仅用于发现；IP 可能变化，不能作为持久身份。
- 不为了统一 Kubernetes 抽象而要求 Compose 提供不存在的 Service/Pod 能力。

## 必须覆盖的业务链路

1. Docker 容器被 kill、退出或实例被替换，等待一段时间后，新的 chat 请求按期望运行状态恢复实例。
2. 区分意外退出与用户明确 Stop：后者遵守已确认的停止/显式启动语义，不被普通预览和文件请求唤醒。
3. Pingora 从当前运行时重新取得容器 ID、IP 和端口，失效旧路由缓存及连接信息，再代理真实业务流量。
4. K8s 对应验证 Pod 换代、Service 访问和直接 Pod IP 路由；不凭控制器或 Service 存在就宣称业务可达。
5. 新旧实例不同时写同一工作区，RBD 挂载限制及 PVC 数据保留不变。

本记录不代表选定 Deployment，也不代表上述链路已测试。
