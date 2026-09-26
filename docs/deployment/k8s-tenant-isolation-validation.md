# 个人 K8s 租户入站隔离验证

镜像与业务由 remote-k8s 工作流部署；隔离策略来自 build-agent-docker 的真实 Helm chart。测试工具不维护第二份手写 NetworkPolicy，不修改共享 Gateway 或全局 CNI。

## 准备

1. `.env.local` 选择个人专用集群、context 和 `rcoder-e2e-*` namespace。
2. 部署当前 RCoder，主 Pod template 必须带 `app.kubernetes.io/component=rcoder-main`，不可变 selector 保持原值。等待滚动完成后才加载隔离策略。
3. 通过 RCoder 建立至少两个 Ready 租户 Pod，尽可能覆盖 builder/prod 及两个节点。缺少目标时工具明确失败，不临时启动一个假监听服务。
4. `.env.local` 配置 `REMOTE_K8S_ISOLATION_CHART=/path/to/build-agent-docker/k8s/helm/nuwax-platform`。

```bash
make remote-k8s-verify SUITE=smoke
make remote-k8s-tenant-isolation
# 同一部署追加实际业务/Gateway 验证
make remote-k8s-test SUITE=userapp
make remote-k8s-test SUITE=gateway
```

`tenant-isolation` 在 remote-k8s 环境锁内运行，检查 namespace/策略归属、租户 SA 与组权限、主 Pod 标签后，安装 chart 的 Ingress fallback。它只应用一份当前测试环境拥有的 NetworkPolicy，保留策略供后续测试使用。已有未知 Cilium 策略或来源 Egress 限制需要单独核实，不能用它们造成的失败证明目标入站规则正确。

## 报告与含义

报告写入 `.remote-k8s/<环境ID>/isolation/<运行ID>/`：

- chart 输入摘要、渲染摘要、最终对象、namespace、policy UID/resourceVersion。
- 前后只读 audit：策略并集、实际标签/端口/探针、SA 有效权限。
- 来源/目标 Pod UID、节点、地址族；Pod IP、匹配 Service 和已有 NodePort 的实际 TCP 结果。
- 每项租户阻断前后都有 RCoder 到同一地址端口的正向对照；工具错误、监听缺失不会算成功。
- 未覆盖的跨节点、IPv6、NodePort、租户家族单列。`observed-matrix-passed` 仅说明报告中的矩阵通过。

前置权限或策略检查失败时仍保留 `audit-before.json`。来源无路由、源地址不可用、文件描述符耗尽等错误报告为 `inconclusive`，不能证明目标隔离；timeout/refused 也必须结合前后正向对照及策略审计判断。最终还会检查观测到的同一批 Pod UID 仍 Ready 且未进入删除过程。

矩阵不替代 HTTP/WebSocket/SSE 业务验收。kubelet 探针需核对 Pod Ready 和事件；DNS、npm、制品下载以及 Gateway → RCoder → UserApp 需继续实际请求验证。宿主机来源、hostNetwork、Cilium host-firewall 及直接 Gateway → UserApp 另行设计，本批不开放例外。Docker Compose 网络隔离未实施。
