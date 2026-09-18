# Verification：rcoder 每副本 RBD 缓存（STS + 桥接迁移）——本地门禁

日期：2026-09-19。本文件记录 plan.md §6 门禁 1（渲染矩阵）与 helm lint 的
本地验证证据；集群侧门禁 2-7 明确未运行。

## 源码基线

- build-agent-docker 工作树（未提交，按任务边界不 commit）：改动见下"文件清单"。
- 改动起点 HEAD：`4a9897b`（fix(app-runtime): file-server-proxy 受管声明 env 补齐）。
- 参考方案：rcoder 仓 `specs/rcoder-local-cache-per-replica-rbd/plan.md`（修订版，
  含 B1-B4 阻断项与 §5 桥接顺序）；被否决的原始方案未采用。
- Helm：v4.3.0（本机）。Chart 版本 0.1.273 → **0.1.274**。

## 文件清单（全部在 build-agent-docker，未提交）

| 文件 | 改动 |
|---|---|
| `templates/rcoder/statefulset.yaml` | **新增**。gate=rcoder.enabled∧(bridge.enabled∨active=statefulset)；STS+serviceName headless+Parallel+RollingUpdate；桥接 A 态恒 0 副本；selector/pod labels=selectorLabels+workload:statefulset；VCT（SC 非空）用 **dbLabels+component**（无 chart 版本标签）；volumes 三分支（SC 非空不声明同名卷/SC 空 emptyDir/enabled=false 整段不渲染）+ template-sources emptyDir 嵌套挂载（主容器+warmup 都挂父卷）；checksum/config+**checksum/templates**+preview-secret 注解；warmup 重写（set -eu、zip 缺失 fail、ls -A 计数、find 拍平、package.json 校验、无 2>/dev/null）；POD_NAME 恰一次+POD_UID/POD_IP；topologySpread 加 workload 标签；其余 env/volumes/探针/sidecar 从 deployment.yaml 逐行搬运 |
| `templates/rcoder/deployment.yaml` | 桥接专用：gate 同理反转；pod labels+workload:deployment（selector 保持原样不可变）；replicas 用 **hasKey** 判 deploymentReplicas（`--set ...=0` 显式归零不被 default 顶回）；其余与历史文件逐字节一致（回滚保真） |
| `templates/rcoder/local-cache-pvc.yaml` | legacy 解耦：gate 与旧 Deployment 存在条件相同+SC 非空；spec 改由 **legacyStorageClass/legacySize** 驱动（default cephfs/30Gi），保持 RWX |
| `templates/rcoder/headless-service.yaml` | **新增**。ClusterIP None；selector=selectorLabels+workload:statefulset；port=rcoder.service.port、targetPort: http（不硬编码 8086）；不设 publishNotReadyAddresses |
| `templates/rcoder/service.yaml` | selector 仅 active=statefulset 时加 workload:statefulset；桥接期（含 A 态与排空）保持原 selectorLabels |
| `templates/rcoder/pdb.yaml` | 同 service.yaml 切换逻辑 |
| `templates/rcoder/networkpolicy.yaml` | rcoder 独立策略 DNS egress 补 `{protocol: TCP, port: 53}`（第二段 agent-runner 已有） |
| `templates/NOTES.txt` | 头部 `$rcoderKind` ternary；exec/rollout restart 两处 deploy→按 active 渲染 |
| `values-default.yaml` | 新增 `rcoder.workload.active: "deployment"` + `rcoder.bridge.enabled: true`（**A 态默认=回滚点**）；templateCache 注释重写（新语义：SC 非空=VCT RWO/空=emptyDir/legacy* 锁旧 RWX）+ legacyStorageClass: cephfs + legacySize: 30Gi；storageClass 维持 cephfs |
| `values-k8s-test.yaml` | 首个迁移环境：storageClass→**ceph-rbd**、size→10Gi、legacy cephfs/30Gi |
| `values-k8s-prod.yaml` / `values-k8s-dev.yaml` / `values-rcoder-k8s-prod.yaml` | 只预填 legacy（cephfs/30Gi、**cephfs/10Gi**（dev 现存即 10Gi）、cephfs/30Gi）；storageClass 不动 |
| `values-rcoder-aks-prod/test.yaml` | legacy=azurefile-csi/30Gi（AKS 未验证 RWO class，明确未迁移） |
| `Chart.yaml` | version 0.1.273 → 0.1.274 |
| `k8s/scripts/render_matrix_check.py` | **新增**。渲染矩阵断言（77 项） |
| `makefiles/14-k8s-helm.mk` | 新增 `k8s-chart-render-check` 目标 |

## 本地验证证据

| 命令 | 退出码 | 结果 |
|---|---|---|
| `helm lint k8s/helm/nuwax-platform -f values-default.yaml -f values-k8s-test.yaml` | 0 | 1 chart linted, 0 failed |
| `python3 k8s/scripts/render_matrix_check.py --chart-dir k8s/helm/nuwax-platform`（源码目录） | 0 | **77 断言 / 0 失败** |
| `python3 k8s/scripts/merge_chart_values.py --chart-dir k8s/helm/nuwax-platform --env test --version 0.1.274 --package-dir /tmp/chart-check --acr-addr dummy --acr-ns dummy --pingap-version 0.14.3 --pingap-commit cd74a461… --no-push` | 0 | 打包产物 `/tmp/chart-check/dummy/` + tgz |
| `python3 k8s/scripts/render_matrix_check.py --chart-dir /tmp/chart-check/dummy`（打包产物） | 0 | **77 断言 / 0 失败** |

渲染矩阵覆盖：桥接 A 态默认 / 切换 B（deploymentReplicas=0）/ 终态（bridge=false+sts）/ 旧态等价（bridge=false+deploy）/ enabled=false / SC 空（emptyDir）/ rcoder.enabled=false / 8 个 overlay 的 legacy PVC 契约（SC/size/RWX/预填值逐 overlay 断言）/ VCT 无 chart 版本标签 / STS POD_NAME 恰一次 / 双 checksum 注解 / Service+PDB selector 随 active 切换且桥接期保持原样 / headless ClusterIP None+targetPort http。

### 修正过程中的两个事实核对

1. **offline/online 四 overlay 实际继承 default 的 cephfs（非空 SC）**，非方案 B4 段所述"继承空 SC=emptyDir"——矩阵按实际行为断言（legacy PVC 存在、SC=cephfs，与改动前行为一致）。plan B4 该句与当前 values 事实不符，实施以实际 values 为准。
2. **PDB/Service selector 的 YAML 注释不能放在 matchLabels 值域内**（helm parse error "unexpected /"）——注释移至模板头 `{{/* */}}`。

## 未运行（集群侧门禁 2-7，全部待迁移窗口执行）

- 门禁 2 初始化（损坏 zip/隐藏文件/失败无 ready）
- 门禁 3 真正安装（3 副本 claim 隔离/冷热对比/基线）
- 门禁 4 重启与滚动（同 PVC UID/模板更新滚动）
- 门禁 5 迁移回滚全演练（A→排空→B→排空→A、endpoint UID 记录）
- 门禁 6 调度（drain/异常节点 CSI 阻塞）
- 门禁 7 业务（构建/预览/发布恢复/Chat SSE）

以上均需真实集群与维护窗口；本任务边界明确不操作集群、不 helm upgrade、不 push。
