# 个人 K8s 集群能力核对与补全方案

日期：2026-09-16。范围：个人测试集群；生产仓库仅只读对照，没有操作生产集群。

## 结论

可以作为 RCoder 的真实 K8s 回归环境。amd64 构建、镜像推送、双副本部署和直连 smoke 已通过，但目前不能宣称 Gateway、UserApp、Chat 全链路可用。主要障碍是现有网络链路异常，而非缺少整套基础组件。以下方案尚未全部实施。

生产对照来源：`build-agent-docker/k8s/cilium/values.yaml`、`k8s/scripts/deploy-cilium.sh`、`k8s/gateway/README.md`、`k8s/helm/nuwax-platform/values-rcoder-k8s-prod.yaml` 和 `values-rcoder-k8s-test.yaml`。RCoder 独立部署关闭多数平台中间件，因此个人环境无需复制整套平台。

## 本轮已确认事实

| 能力 | 实测状态 | 处理 |
|---|---|---|
| Linux amd64 构建、私有仓库推送与节点拉取 | 本轮 verify smoke 已成功 | 保留现有 Make 工作流 |
| RCoder 双副本、直连 API | Ready、HTTP 200 | 保留，增加持续探测 |
| Cilium / Envoy / Gateway API | 已安装，Cilium 1.19.6，与生产配置版本一致 | 排查数据转发，不能以对象 Ready 代替请求验证 |
| Gateway 到 RCoder | 节点内访问约 5 秒后 503；Mac 请求此前超时 | P0 阻断 |
| 控制面连接 | 间歇 TLS handshake timeout；worker operator 反复访问 Lease 超时、重启 | P0 稳定性问题 |
| CephFS RWX / RBD RWO | StorageClass 已存在，Retain；现有 PVC Bound | 继续做挂载和数据保留验收 |
| Ceph 健康 | HEALTH_WARN：前后端 OSD heartbeat 最长约 3.6–3.8 秒 | 联合排查节点网络、CPU/IO 压力 |
| DNS、metrics-server、Hubble | 组件存在并 Ready | DNS/观测链路还需功能测试 |
| UserApp / Chat | 本轮未跑完整套件 | 源码稳定后以固定快照验收 |

### Gateway 的证据链

1. Gateway Accepted/Programmed、HTTPRoute Accepted/ResolvedRefs 均为 True。
2. Service/BPF 表包含 Gateway NodePort 和 Envoy 代理端口。
3. worker 本地请求进入 Envoy，最终返回 `503 upstream connect error ... connection timeout`；控制面节点入口也能复现 503。
4. 抓包显示 Envoy 向 RCoder Pod 发 SYN，Pod 返回 SYN-ACK，但连接仍持续重传；同一后端的普通直连可成功。
5. Cilium drop monitor 未发现能解释该连接的 drop；这不等于网络没有丢包。
6. 已重建 worker 上单个 Envoy Pod，新 Pod Ready 后仍复现，未解决。

因此当前定位到代理上游连接的回程/宿主机数据路径，尚未证明具体根因。上游存在相似报告，但不能据此认定为同一缺陷：
- https://github.com/cilium/cilium/issues/47769
- https://github.com/cilium/cilium/issues/35559

### 配置与环境风险

- Helm 保存的 Cilium API Server URL 仍为旧网段，实际 ConfigMap 已改到当前控制面。运行中的 operator 使用当前地址，故不能把旧 Helm 值直接当成本次超时原因；但下一次 Helm 操作可能覆盖正确配置。
- worker Cilium 自动选择物理网卡和 Tailscale 网卡，MTU 为 1280，跨节点路由 MTU 1230；需要核对是否符合预期，不能直接归因为 Tailscale。
- 宿主机仍有 KUBE-* 规则与 Cilium kubeProxyReplacement 并存。须判断是活跃 kube-proxy 还是残留规则，并核对命中计数；不可直接清空 iptables。
- worker 物理网卡有两个 IPv4 地址，需要核查 Kubernetes node IP、路由源地址及地址分配是否稳定。
- Ceph 心跳慢与控制面超时可能共享网络/负载原因，目前只是排查方向。

## 实施顺序

### P0：先恢复网络可信度

1. 保存 Helm values/manifest/history、实际 Cilium ConfigMap、DaemonSet、K3s 启动参数、路由和防火墙；备份只放忽略目录，不含凭据的变更摘要可入库。
2. 以当前实际配置为基准生成个人集群 Helm overlay；修正 API Server 地址漂移。先渲染并对比，保留已有私有镜像覆盖，不直接套生产 values，也不盲目 `--reuse-values`。
3. 对控制面连接做持续探测：worker 宿主机、普通 Pod、控制面本机三条路径，记录延迟、失败时间和 operator 重启；关联节点负载及 Ceph 心跳。
4. 针对 Gateway SYN-ACK 同时抓 veth、cilium_host、物理接口，结合 Hubble trace、Envoy metrics、BPF CT 和 netfilter 计数确认回包去向。
5. 每次只变更一个候选因素。若证据指向 BPF host routing，可对照 legacy host routing；若指向接口自动发现，固定实际承载节点流量的接口。每次都留回滚记录，失败恢复原配置。不要同时升级内核/Cilium、改 MTU 和改防火墙。
6. 查明 K3s kube-proxy 状态后再决定停用和清理；禁止全表 flush。保留 SSH 和已有数据卷。

完成标准：两个节点的 Gateway 实际 HTTP 请求、Mac 到测试入口、节点内 ClusterIP 请求都持续成功；direct API 仍正常；控制面连续观察无超时及新增 operator 重启。短期观察只能证明该时间窗口，不能证明长期稳定。

### P1：补齐回归环境的能力验收

- 存储：Ceph 心跳异常得到解释/修复；RWX 跨 Pod 读写；UserApp RWO 挂载；Pod 重建数据保留；stop 保留 agent PVC。测试仅写独立测试子目录，禁止清理共享 CephFS 根。
- RBAC：使用 RCoder ServiceAccount 核查实际需要的 STS/Pod/Service/PVC/HTTPRoute/exec/log/事件权限，按业务失败补最小权限，禁止授予 cluster-admin。
- DNS/出口：Pod 内解析服务和镜像/依赖/LLM 域名，验证 HTTPS；缺少真实 LLM 配置应失败，不使用 mock。
- Gateway：除 `/health` 外，覆盖动态 UserApp 路由、端口预览、WebSocket、SSE 和重建后的路由更新；不只检查资源条件。
- 多副本：两个 RCoder 副本发起相同应用操作，检查身份、并发操作收束、取消、恢复及数据一致性。
- 保留独立 namespace、镜像 digest、源码快照和构建/部署/测试记录。不要复用生产共享 Gateway 来降低测试门槛。

### P2：完善工具与可重复配置

建议给现有 Python `tools/remote_k8s` 增加独立的只读 infra-check 入口，检查：

- Helm 与运行态关键值漂移；
- Cilium/operator/Envoy 健康及最近重启；
- Ceph 健康详情；
- API 连接稳定性与 DNS；
- 直连和 Gateway 分层请求；
- 失败时保留完整诊断，单项采集失败不能使其余诊断丢失。

现有 doctor 主要证明工具、CRD、StorageClass、仓库可访问，不能替代上述功能验收。将个人 overlay 与说明纳入版本管理，真实地址留 `.env.local` 或忽略的渲染文件。不要在文档记录密码、kubeconfig 或镜像认证信息。

## 后续执行入口

基础网络修复后串行执行：

```bash
make remote-k8s-test SUITE=smoke
make remote-k8s-test SUITE=gateway
# 源码稳定后重新构建固定快照，并做业务验收
make remote-k8s-verify SUITE=userapp
make remote-k8s-test SUITE=chat
```

业务验证期间保持本地源码稳定，避免漂移检查失效。生产 Chart 与开发镜像部署形态并不完全一致；个人工作流通过后，生产发布仍需 Chart 渲染检查与对应形态的发布验收。

## 本轮变更及证据

- 只重建了个人集群 worker 上单个 Envoy Pod，已恢复 Ready；未修改 Cilium/Helm/K3s 配置，未动生产、业务代码和 PVC。
- 该操作没有修复 Gateway 503。当前网络问题仍未关闭，不能标记“补全完成”。
- 本轮配置备份、路由、日志与抓包摘要保存在 `.remote-k8s/<环境ID>/infra-20260916/`；之前构建和测试记录见 `verification-2026-09-16.md`。
