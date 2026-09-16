# 个人双节点集群 Gateway 修复记录

日期：2026-09-16。本轮获授权修复个人开发集群网络组件；未操作生产集群、删除 PVC 或修改业务代码。

## 后续：按用户要求卸载 Tailscale

用户确认 worker 不需要 Tailscale 后，已停止并禁用 tailscaled，通过 apt purge 卸载 tailscale，移除其 APT 源和签名 key，以及本轮新增的 sysctl 配置、systemd drop-in 和兼容脚本。清理后无 tailscale 命令、tailscale0 接口、Tailscale 策略路由及 `/var/lib/tailscale` 目录；`src_valid_mark` 保持内核默认值 0。下文兼容脚本方案仅保留为历史记录，当前不再依赖它。

卸载后两个节点的 Gateway 和直连健康接口均返回 HTTP 200、code 0000；Gateway 正式套件退出 0，报告 `12dd6843f58540479de75d8e05691106`；smoke 退出 0，报告 `7b7a6bea9583433691161179329f968a`。

## 结论

Gateway 已恢复，`make remote-k8s-test SUITE=gateway` 与 `SUITE=smoke` 均退出 0。

直接原因经同机对照确认：worker 的 `net.ipv4.conf.all.src_valid_mark=1` 与 Cilium 透明代理的 fwmark 路由存在冲突。设为 1 复现连接超时，设为 0 立即恢复。单纯重新部署同版本 Cilium/Envoy 不会解决此宿主机参数问题。

worker 运行 Tailscale 1.102.3。重启 tailscaled 实测会重新设置该参数为 1；其官方仓库有对应行为报告。Linux 文档说明该参数使源地址校验采用包的 fwmark，且 all/interface 取最大值。由此结合对照结果，确认应修正宿主机兼容配置，而不是重建 Gateway 对象。

参考：
- https://github.com/tailscale/tailscale/issues/19796
- https://kernel.org/doc/html/latest/networking/ip-sysctl.html
- 核对到的当前 Tailscale 实现：`wgengine/router/osrouter/router_linux.go` 中启用 connmark 时写入全局 src_valid_mark（当前上游源码仅用于解释，现场版本行为以重启实验为准）。

## 排查与变更

1. 保存 Helm values、manifest、实际 ConfigMap 和网络证据到忽略目录 `.remote-k8s/<环境ID>/gateway-repair/`，目录 0700、备份文件 0600。Helm manifest 可能含证书私钥，禁止提交或公开。
2. 发现 Helm 持久值仍使用旧控制面地址，运行态则已手工改正。用同版本 Cilium 1.19.6 Chart 修正 `k8s.apiServerURLs`、`k8sServiceHost`、`k8sServicePort`，保留原私有镜像及其他用户配置。最终 Helm revision 为 7。
3. 重新部署 Cilium/Envoy/operator 后仍复现，排除仅重装即可解决。
4. 临时验证 legacy host routing，一部分请求恢复；进一步发现 worker 的全局 src_valid_mark 与控制面不同。修改后全部入口恢复。
5. 撤销 legacy host routing 对照配置，恢复原 BPF host routing；Gateway 仍正常。因此最终未保留该绕行配置，也未改 MTU、Cilium 版本或清空防火墙。
6. BPF 模式下单项 A/B：src_valid_mark=1 时 curl 退出 28，4 秒内无响应；设为 0 时 HTTP 200，约 12 ms。
7. 一度验证物理接口 rp_filter=0，无效，已恢复原值 2。最终只持久化 src_valid_mark 的兼容修正。

## 宿主机持久化配置

仅在运行 Tailscale 的 worker 上新增：

- `/etc/sysctl.d/99-rcoder-cilium-tproxy.conf`：设置 `net.ipv4.conf.all.src_valid_mark=0`。
- `/etc/systemd/system/tailscaled.service.d/90-cilium-tproxy.conf`：ExecStartPost 调用下述兼容脚本。
- `/usr/local/sbin/rcoder-cilium-tproxy-compat`：每秒检查 Tailscale BackendState，连续 3 次 Running 后执行 sysctl 修正，最长等待 60 秒，超时明确失败。

**不能只使用 sysctl.d 或启动后立即执行 sysctl**：实测 tailscaled 在 systemd Ready 后仍会初始化路由并重新写入 1。第一版立即执行的 ExecStartPost 已被等待初始化的版本替换；实际重启 tailscaled 后验证参数维持 0，Gateway 通过。

脚本和配置副本保存在忽略目录的 `compat.py`、`tailscaled-dropin.conf`、`sysctl.conf`。此措施是当前个人节点的兼容处理，不是上游产品修复。将来升级 Tailscale、改变 netfilter 模式、启用出口节点或子网路由后需重新验收。

现场 Tailscale 未配置出口节点、未发布子网路由，也未接受其他节点路由。修复后 BackendState=Running、Online=true；原有“不接受其他 peer 发布路由”的提示未变化。没有完成 Tailscale 的跨 peer 数据传输验收，也未做整机重启验收。

## 本轮验证

| 验证 | 结果 |
|---|---|
| Mac → 两个节点 Gateway NodePort，各 10 次 | 20/20 HTTP 200、业务 code 0000 |
| worker → Gateway 本节点、对端及 ClusterIP | 已成功请求；最终正式套件另行通过 |
| tailscaled 重启后 sysctl | 维持 0，服务 Running/Online |
| 控制面 `/readyz` | 连续 5 次 ok |
| Cilium operator 两副本 | Ready，观察约 9 分钟无新增重启 |
| `make remote-k8s-deploy` | 退出 0，复用已构建的固定镜像，更新环境基线 |
| `make remote-k8s-test SUITE=gateway` | 退出 0，报告 `8a6db55c2ee14a3baa8bd279709d1b9e` |
| `make remote-k8s-test SUITE=smoke` | 退出 0，报告 `7fb1cdeb848a45779d6b43b2492d2941` |

基础设施变更后，旧部署 receipt 的外部资源保护曾正确阻止测试，因为 Cilium operator 已改变。未修改保护逻辑或手工改报告；通过正常 deploy 建立新基线后重测通过。

此次复用了 build `20260916T025927Z-853f6480` 的镜像，不代表当前并行编辑中的源码已重新构建。未执行完整 UserApp/Chat，也未据此关闭此前 Ceph 心跳告警或宣布长期稳定。

## 回滚与后续

- Helm 保持相同版本。回退参数应以本轮保存的正确 API Server 地址为基准，不能盲目 rollback 到旧地址的 revision 4。
- 要撤销宿主机兼容修正，删除本轮新增的两个配置文件与脚本，执行 systemctl daemon-reload，并按目标网络方案恢复 sysctl；已实测在当前配置下恢复为 1 会使 Gateway 故障复现。
- 后续正常使用 `make remote-k8s-verify SUITE=smoke` 构建部署，再按业务范围追加 `userapp/chat/gateway`。此前日期化报告中的 Gateway 失败是历史事实，本记录补充其修复结果。
