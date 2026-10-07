# 本机 K8s 核心回归

OrbStack 可以验证 K8s API、实际 Lease、跨控制实例的应用操作、Pod 身份和保卷启停。
执行前记录 context、API 地址、节点架构、StorageClass、源码、二进制与镜像摘要。
单节点 local-path 的结果不能代替 CephFS/RBD、跨节点隔离或其他存储锁验收；在 arm64
节点执行 amd64 镜像时，也不能作为原生 amd64 性能证据。

## 隔离和输入

使用独立 namespace 和唯一应用，namespace 标记本轮 owner。不要修改日常 DevSpace、
共享 Gateway 或已有应用。计算停止和故障注入保留 PVC；不要以 purge、删除 namespace、
清库或修改旧操作终态收尾。源码、Cargo、临时文件和报告放在工作盘。

宿主机控制实例必须启用 `kubernetes,deploy-host,rcoder-pg`，使用显式私有 kubeconfig、
独立端口和日志。两个实例使用同一隔离元数据库与 namespace，分别记录实际 PID、
启动时间、冻结二进制 SHA 和构建输入 SHA。完整 K8s 配置不需要附加无关 Docker 配置。
Prod 的 `PROJECT_ID` 来自平台应用身份；显式 env/secrets 中的异应用值在准备资源前拒绝。

脚本要求 run 目录内的 `identity.json` 和实际构建、进程 receipt。私有数据库 URL、
kubeconfig 和凭据不进入公开报告。详细输入见 [测试启动器说明](../../tests-e2e/tools/README.md)。

## 场景与证据

| 场景 | 必须验证 |
| --- | --- |
| 并发 ensure | 两个实际调用者复用同一应用；唯一 STS、Pod 和 PVC |
| 计算 Stop/Restart | 原操作 GET 到终态；重复请求返回原 ID；新 Pod UID、原 PVC/PV 和原数据 |
| 只读状态 | manifest 错误不挡计算 Stop；Stopped 后 readiness 不唤醒 Pod |
| Source 构建与迁移 | 原任务 GET/SSE、真实包 SHA、运行资源目录包含；迁移失败保留原因且实际 HTTP 可用 |
| 构建期间 Stop | 原任务 Cancelled；独立确认编译进程退出；迟到回调不能重新启动业务 |
| owner 故障 | 精确核验 native owner、可执行文件、进程代次和内核锁；原入口恢复，新 owner、同容器/卷/数据/锁 inode |
| Prod 冷/热部署 | 原请求/操作、实际 HTTP 与 worker；热更新保持 Pod/卷，准备期间旧业务可用 |
| 两控制实例争用 | 实际 Lease UID/版本与真实持久 holder；新错误码七字段；拒绝请求不能冒充受理操作 |
| Restart 等待 | 释放后同请求受理一次；默认 30 秒耗尽无新受理；释放后不得迟到执行被拒绝的请求 |

Prod 地址、日志和 exec 目标必须属于捕获的 Deployment：核对当前 ReplicaSet 模板、
controller owner UID、非 terminating Pod 及 `app` 容器。同 app-id 的 builder 不能成为
Prod 目标；查询失败不能被转换成“没有 Pod”。

无制品的业务 Restart 可以在 rollout 指令确认后返回 `Succeeded` 和 `Starting`。
新 Pod 与真实 HTTP 就绪另行观察；不能使用仍待退出的旧 Pod、旧 worker 或 Ready 副本数
替代当前实例证据。`/computer/pod/restart` 是独立计算协调操作，以其原 status URL 查询。
30 秒是 Restart 等待受理的预算；后续业务就绪观察使用独立有界阶段并共享父 deadline。

## 入口

- `local_k8s_core.py`：保卷 Dev 计算回归。
- `local_k8s_business.py`：真实 Source、迁移和构建 Stop；失败保留任务与现场。
- `local_k8s_owner_fault.py`：在精确前置核验后续跑 owner 故障。
- `local_k8s_prod.py`：构建真实 A/B 包、原任务 GET/SSE；`build_ready` 不表示已部署。
- `local_k8s_prod_execute.py`：原包冷/热部署、跨实例占用和 Restart 等待。
- `local_k8s_prod_expiry.py`：核验原失败与实际恢复前置后，独立完成剩余预算耗尽场景。

续跑必须核对原报告、原任务/操作和物理身份，写新报告，不能覆盖旧失败。
控制面修复后可以部署原不可变制品：分别记录历史构建与当前控制面的源码身份，核验
原 ZIP/lock 摘要，不能要求两个源码版本相同或改写迁移身份。

协议单测只验证脚本保护与契约。真实验收必须有实际 API、进程、Lease、Pod、卷、HTTP
及原操作的证据。每轮分别报告组件检查、真实 K8s、部署和发布状态；未测试的真实断连、
API 写入结果未知、跨节点和存储平台明确列出。
