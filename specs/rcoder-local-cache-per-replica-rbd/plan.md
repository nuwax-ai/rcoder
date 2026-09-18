# RCoder 主服务每副本缓存：审查与修订实施方案

日期：2026-09-18。状态：待实施；本轮仅静态核查源码、Helm 源码和 Kubernetes 官方文档，未连接集群、未部署。同日二次复核增补：pnpm 执行位置证据、桥接 selector 时序修正、性能预期、values 补漏、compose 范围界定。

审查对象：`build-agent-docker/specs/rcoder-local-cache-per-replica-rbd/plan.md`。
源码基线：build-agent-docker `5a0f41d`，RCoder `8adc8db2`，包含当时工作树；保留其他未提交改动。
下文 B 路径相对 `/Users/soddy/Documents/git-workspace/build-agent-docker`，R 路径相对 RCoder，T 路径相对 `/Users/soddy/Documents/git-workspace/nuwax-file-server`。

## 1. 结论与目标

**STS + 每副本独立 RWO 文件系统 PVC 可行，但原方案不能原样上线。** 它隔离不同副本的 pnpm store，并保留同一 ordinal 的缓存；不会隔离同一 Pod 内并发安装，也不会解决多个副本操作同一共享项目目录的竞争。性能预期方向为正：现状 store 位于 CephFS FUSE 路径，store v10 索引是海量小文件 hash 校验与随机读写、元数据密集，恰是 FUSE 最不利负载；改造后 store 位于 RBD 块设备上的常规本地文件系统（ext4/xfs），无 FUSE 用户态转发与 CephFS 元数据开销，store 侧预期显著改善。边界：install 总耗时 = store 读 + 项目目录写，node_modules 写入仍在共享 CephFS，改善幅度取决于 store 侧占比；RBD 是网络块存储，小 IO 延迟高于本地 SSD，不等于本地盘，幅度必须实测（见 §6 门禁 3 前后对比）。

可行性基础（网页应用链路）：pnpm install 全部在 rcoder 主 Pod 内执行——pnpm 封装层直接 `Command::new("pnpm")` 本地 spawn（R `crates/file-server/src/service/pnpm/cli.rs:141,191`），dev_server 同进程管理 dev 命令；store 路径由主 Pod env 注入项目 `.npmrc`（R `crates/file-server/src/service/pnpm_config.rs:60–67`，`npmrc_optimal` 96–105 行自适应覆盖不匹配的旧 store-dir）；agent-runner/user-app-builder 容器虽内嵌同一份 file-server（`RCODER_EMBED_FILE_SERVER=true`，注入于 B `_kubernetes-config.tpl:178`），但其 Pod 未注入 store env、不挂 local-cache（docker_manager/container-runtime/tpl 全 grep 无引用），`pnpm_config.rs` 在 env 缺失时不写 store-dir 行 → runner 内 pnpm 走容器内默认临时 store，与 local-cache 正交，改造前后行为不变。因此每副本 RWO RBD 只挂主 Pod 即覆盖网页应用全部 store 消费者。

范围界定：本方案仅覆盖 K8s chart 形态。Docker Compose 模式为单实例（compose 文件注明 Single rcoder instance），不存在跨副本共享 store 竞态；compose 未注入 store env、也未挂 /local-cache，pnpm 使用容器内默认 store；STS/VCT/RBD/CSI 在 compose 无对应概念。compose 模式不在本方案范围，如需持久缓存另行挂载 named volume，与本方案互不影响。

目标是缓存副本隔离、重启复用、模板内容正确、迁移可控和可回滚。缓存不能承担业务权威状态，不迁移或删除 workspace/agent/UserApp 数据卷。不要预先承诺 RCoder 业务代码一定零改动；先完成身份/路由回归，确有缺陷再单独修复。

本次不要求 pin pnpm。原日志证明发生重取、后来成功，不能证明旧新镜像实际 pnpm 版本不同，更不能证明格式不兼容；10:17 rollout condition 与约 10:40 Pod 创建也不能作为精确同分钟因果证据。需要旧新镜像 digest、各自 pnpm 版本、索引样本/源码或受控复现才能确证。不得写“每次镜像重建必然全量重取”或把无 WARN 作为唯一成功标准。

## 2. 阻断项（实施前必须纳入修订）

### B1：Helm 不提供跨 kind 的就绪后原子替换

依据：B `templates/rcoder/deployment.yaml`（完整前缀为 `k8s/helm/nuwax-platform/`）3、12–19 行；`service.yaml`24–25 行。
Deployment 与 StatefulSet 是不同资源，三方合并不会把前者原地变成后者。核查本机 Helm v4.3.0：`pkg/kube/client.go`575–605 先遍历创建/更新目标资源，657–684 再删除旧差集，删除为 Background；并非等待新 STS Ready 才删除旧 Deployment。Helm 3.17.3 同样有先目标后差集的更新结构。现场 Helm 版本/执行参数仍需记录，不能只凭本机版本认定远端行为。

因此既可能出现两代 Ready Pod 同时被 Service 选中，也可能旧 Pod 已退出、新 PVC/Pod 尚未 Ready 导致断流。正常 ownerReferences 下，两个控制器不会简单互相抢走已归属 Pod；但 selector 重叠仍造成 Service/PDB/拓扑计数混合和孤儿 Pod 接管风险。`--wait`、自动回滚均不等价于原子切换。

**修订：使用第 5 节桥接版本和显式停写/排空顺序；不得直接改 kind 后一次 upgrade 当作无缝迁移。**

### B2：删除旧 PVC 模板不会自动安全保留旧 PVC

依据：B `templates/rcoder/local-cache-pvc.yaml:1–15` 没有 keep 注解。移出 release manifest 后会成为 Helm 删除对象；PVC protection 只延迟使用中的删除，Pod 退出后仍可能删除并按 PV reclaimPolicy 回收数据。

修订：迁移和回滚观察期内继续在 chart 中渲染旧 PVC，保留原 SC、容量和 RWX，不让新 RBD values 改写它。后续清理单独发布/执行。若选择 keep，先在前置版本中落入 release 并验证实际 Helm 行为，不能仅在已移除的模板上写注释。本方案优先继续管理旧 PVC，避免孤儿资源回滚接管问题。

### B3：持久模板标记会长期跳过新模板，解压失败也可能写 ready

依据：B `templates/rcoder/deployment.yaml:66–85` 仅检查 `.cache-ready`，脚本没有可靠的错误短路；`unzip` 失败仍可能执行 ready 写入。T `src/utils/common/templateCacheUtils.js:157–164` 同样仅检查标记。B deployment 23 行只给 config.yml 做 checksum，没有模板 ConfigMap 内容 checksum。

修订采用第 3 节分离存储：**pnpm store 持久，解压模板使用独立 emptyDir**；同时增加模板 checksum 触发重建，并修复初始化错误传播。不能以“initContainers 不动”交付。这样保留主要下载缓存，避免为小型模板引入永久缓存版本管理。

### B4：values 的源文件和发布路径遗漏

依据：B `k8s/scripts/merge_chart_values.py:118–139,173` 从 values-default + 环境文件生成 values.yaml；`makefiles/14-k8s-helm.mk:255,287` 描述同一路径。仅改 values.yaml 会在打包时被覆盖。

必须同步 values-default.yaml、实际发布用的环境 overlay，以及生成后的 OCI chart 默认值。`values-k8s-test.yaml:209–214`、prod:194–199、dev:192–197、`values-rcoder-k8s-prod.yaml:96–103` 都有缓存配置。AKS overlay `values-rcoder-aks-prod.yaml:103`、`values-rcoder-aks-test.yaml:100` 均为独立 `azurefile-csi`，不能一律改为 ceph-rbd。`values-rcoder-k8s-test.yaml` 与 offline/online 四个 overlay 继承 values-default 空 SC（emptyDir 分支），渲染矩阵须显式覆盖该分支，确认 STS 模板改动不影响。按平台选择可用 RWO CSI class；未验证平台明确保持未迁移。

## 3. 修订架构与模板契约

### 3.1 数据布局

- STS 每 ordinal 一份 claim，模板名继续 `template-cache`，兼容原 `/local-cache/pnpm-store` 路径；RWO、Filesystem、目标环境核实的 SC，容量按预算设置（10Gi 可作为起点，不是经容量验证的永久答案）。
- `/local-cache/templates` 增加独立 `template-sources` emptyDir 嵌套挂载，主容器和 warmup init 都挂载。`TEMPLATE_CACHE_DIR` 路径不变，旧 PVC 下 templates 不再作为来源。确认生成的嵌套挂载实际可读写。
- `rcoder-project-init` 继续 emptyDir，seed 从模板 ConfigMap 复制 zip。
- init 使用明确失败退出，正确处理包含隐藏文件的单层包裹目录；避免用掩盖任意失败的 `mv ... 2>/dev/null`。确认预期文件完整后才写 ready，失败不得启动主容器。
- 给 Pod template 添加模板 ConfigMap 渲染内容摘要；ZIP 变化触发 Pod 更新。源码消费者自行写缓存的路径要与此目录一致。
- TS nuwax-file-server 存在运行时兜底 warmup（T `src/utils/common/templateCacheUtils.js:160`：marker 缺失即自行解压，路径来自 chart 注入的 TEMPLATE_CACHE_DIR），与 init 行为一致；不要求修改 nuwax-file-server（非本仓职责），验证时确认目录一致即可。
- 如坚持模板也持久化，须另实现 zip 内容 hash、暂存目录、验证、原子发布和失败重试，不能继续仅用布尔 ready 文件；本轮不推荐增加这个复杂度。

### 3.2 三种渲染结果必须明确

| enabled | storageClass | 渲染 |
|---|---|---|
| false | 任意 | 不渲染缓存 claim、缓存 mount/env、warmup；种子模板维持既有路径 |
| true | 非空 | volumeClaimTemplates: template-cache；Pod volumes 不再手写同名卷；另有 template-sources emptyDir |
| true | 空字符串 | 无 claimTemplates；Pod volumes.template-cache 为 emptyDir；另有 template-sources emptyDir |

VCT 同名卷在 Kubernetes 定义中优先于 Pod template volumes；不能依赖这个优先规则隐藏模板错误，测试应禁止双重声明。空 SC 在本 chart 表示临时缓存，不是“使用默认 StorageClass”。保持此契约并写清楚。

已创建 STS 的 VCT 不能通过普通 upgrade 改 SC、容量或在有/无 VCT 间切换；参数变更预检拒绝并给迁移说明。单个 PVC 在 SC 允许扩容时可扩容，但还需解决 chart/VCT 模板的一致性，不能说 PVC 容量永远不可改；禁止缩容。不要让 `--reset-values` 意外改变既有 STS 存储形态。

### 3.3 STS 与网络

- STS/headless 按 rcoder.enabled 同步渲染。headless 为 ClusterIP None；selector 精确选择 STS，不引入业务调用切换。
- DNS 的 A/AAAA 记录不要求声明业务端口；保留命名 http port 有利于契约和 SRV，使用 `.Values.rcoder.service.port` 与 `targetPort: http`，不硬编码 8086。headless 也可被直接访问，不要声称它技术上“不承载流量”。
- 默认仅发布 Ready endpoint。当前无 Pod DNS 引导依赖，不必设 publishNotReadyAddresses；未来引入互相发现时再分析启动依赖。
- 原 labels 保留给 NetworkPolicy；新增 workload 标签区分控制器。稳定 Service 地址、端口、ClientIP affinity、Gateway 引用保留。
- DNS 走调用方到 DNS 服务的 egress，不经过“headless Service 的代理”。B `templates/rcoder/networkpolicy.yaml:29–35` 主 Pod 只放 UDP53，应补 TCP53，并核查实际 CoreDNS/NodeLocalDNS 路径；同文件独立策略还限制外部 egress，冷 store 必须实测 registry 下载。全局策略与独立策略两分支分别验收，不能直接断言无需调整。

### 3.4 滚动、调度与资源

- Parallel 用于扩缩容，默认 RollingUpdate 仍按 ordinal 逐个替换、等待 Ready。`maxUnavailable: 1` 不能给默认行为提速；大于 1 才可能增加并发，支持情况取决于版本/feature gate，本轮不依赖该扩展。
- PDB 约束 eviction，不控制 STS 自己的更新或直接删除；当前 `pdb.yaml:11–15` 可配置 maxUnavailable=0，会阻止自愿驱逐，不能据此声称升级零不可用。
- 保留软 topologySpread 默认；硬约束须覆盖节点不可用、PV topology、资源不足、卷挂载数限制。不得承诺 DoNotSchedule + RBD 必然能跨节点迁移。
- RBD detach/attach 在故障节点上可能持续数分钟或长期阻塞，不是固定几秒；不得用强删 Pod/强制卸载绕过 fencing。缓存卷挂载失败会阻止整个主 Pod 启动，因此影响并不局限于缓存命中率。
- PVC 留存采用 Retain/Retain（目标版本支持则显式设置，否则核查默认及实际 ownerReferences）。RWO 不保证进程单写，避免人为启动使用同 claim 的替代 Pod。
- 3×10Gi 仅是申请容量之和，不代表 Ceph 物理用量等价；计入后端副本、文件系统开销、缩容残留卷和观察期旧 30Gi 卷。监测磁盘/inode、安装延迟、命中率、init/attach 时长；初期不新增并发 prune。

## 4. 跨仓引用与遗漏检查

必改/核验：B deployment→statefulset、headless、旧 PVC 保留模板、values-default/各 overlay/生成 values、NOTES（`templates/NOTES.txt:32` 仍 exec deploy/...）、初始化脚本、模板 ConfigMap checksum、PDB/Service/workload selector、网络策略、部署/诊断脚本和测试。

B deployment:140–156、228–232 明确注入 POD_NAME/POD_UID/POD_IP；不能以“没有 Pod 身份假设”跳过检查。POD_NAME 在 142、229 重复声明（值相同无行为影响），改写 statefulset.yaml 时顺手去重。R `crates/preview-coordinator/src/identity.rs:24–37` 有宿主身份；验证同 ordinal 换 UID、同 Pod 重启换 boot_id 后的恢复和路由，旧发布任务/预览不得认新进程为旧执行者。未发现这些代码必然因 STS 失效，但“PG 同步一切内存态”的概括不能替代测试。

R `tools/remote_k8s/main.py:358,811` 有固定 deployment/rcoder，是独立开发部署路径；**不能全局替换成 statefulset**。先确认其清单来源，保留独立 Deployment 测试环境；为本次正式 chart 单独验收或新增明确 chart 模式。普通 remote-k8s smoke 不证明生产 chart 迁移通过。

## 5. 可执行迁移与回滚（推荐维护窗口，避免未经验证的双活）

采用桥接 chart，短期显式参数控制 activeWorkload=deployment/statefulset 与两个 replica 数；不是永久支持任意双活配置。旧 Deployment spec.selector 保持原样（不可变），为其 Pod 增加 `workload=deployment`；新 STS selector 使用原标签 + `workload=statefulset`。**桥接期（A 与排空）Service 保持原 selectorLabels 不变**，仅在切换 B 时把 selector 切到 `workload=statefulset`；不能在 A 中就让 Service 显式选 `workload=deployment`——若与旧 Deployment 加标签同轮 upgrade 生效，upgrade 瞬间 0 个 Pod 带新标签，Service 无任何 endpoint，空窗持续到滚动完成，回滚点 A 本身不安全。排空顺序（旧 N 新 0 → 旧 0 新 N）已保证无双代同时在线，原 selector 在桥接期不会混选两代；workload 标签仅用于 PDB/拓扑的精确选择与防误扩。PDB/topology selector 对应正在维护的控制器；旧 Deployment 的宽 selector 保留但禁止 orphan 操作。

1. **预检**：记录实际 Helm/K8s/CSI 版本、release revision、渲染清单、镜像 digest、PVC UID/SC/容量/reclaimPolicy、SC provisioner/bindingMode/allowedTopologies/扩容能力和节点资源。核实旧缓存没有其他消费者、无业务数据。保存 values/manifest 时脱敏，不把 Secret 明文提交。
2. **桥接版本 A**：旧 Deployment 仍 N 副本并服务；新 STS=0；旧 PVC 保留原 spec；新 VCT 使用独立新配置，避免把旧 CephFS PVC 改成 RBD。Service 保持原 selectorLabels（原 selector 仍匹配带新标签的旧 Pod，无空窗）；新增标签可能滚动旧 Deployment，须先完整验证。记录 A 为明确回滚点。
3. **窗口排空**：停止接收新构建/发布/长连接请求（具体停流机制——Gateway 摘除 HTTPRoute、前端入口停写等——须在执行前定义并写入 runbook，当前方案未定义），等待现有业务完成；未知操作保持既有恢复保护。通过 bridge values 将旧 Deployment 缩到0、新STS仍0，等旧 Pod 全部真正退出。不要只手工 scale 后让 Helm 下一轮还原副本数。
4. **切换 B**：同一 Helm release 升级，旧 Deployment=0、新 STS=N、Service选择 statefulset；旧 PVC仍保留。启用 wait 并显式确认 VCT 所生 PVC、Pod、EndpointSlice、实际 HTTP、业务构建成功后解除维护。接受并记录维护中断，不宣称零停机。
5. **回滚**：重新进入维护、排空；先通过桥接版本将 STS=0，确认全部退出，再升回 A（旧 Deployment=N、Service选旧、新STS=0、旧PVC原spec）。不要直接依赖一次 helm rollback 保证顺序。STS RWO PVC 留存不妨碍 Deployment 使用旧 RWX；无需复制 pnpm store。若旧 PVC 已丢失，缓存可以冷重建，但这不是原保留式回滚已验证。
6. **稳定后清理 C**：移除已缩零的旧 Deployment，旧 PVC保留至回滚期限结束；明确最低可回滚版本。删除缓存卷独立审批/执行，核对 release/namespace/PVC UID/挂载者；绝不按宽标签批量删除 workspace PVC。移除桥接参数前完成失败回滚演练。

若要求不中断，另做双控制器蓝绿与长任务排空协议，验证共享 workspace/后台协调器双活后才可实施；不作为此次缓存变更隐含承诺。

保留用户既定 `helm upgrade ... --reset-values` 发布方式：正式打包 chart 的默认 values 必须包含最终环境配置；迁移桥接步骤显式传本步控制参数，并核对最终渲染。不是强迫永远附带个人 values；也不能让 reset 清掉本步迁移控制。

## 6. 验证门禁（具体断言）

1. 渲染矩阵：enabled false/true × SC 空/非空；1/3 副本；实际 test/prod/AKS overlay；核对唯一缓存卷源、headless/STS关联、mount/env匹配、旧PVC spec不变、Service选中控制器正确、模板checksum随zip变化。对最终打包产物再渲染，不能只测源码目录。
2. 初始化：正常/损坏/缺失zip、含隐藏文件/包裹目录；失败无ready且主容器不启动；新zip更新后读取新模板，同Pod不受旧PVC模板标记污染。
3. 真正安装：逐Pod执行同一受控依赖安装，核实3个不同claim/文件标记不可跨副本读取；冷/热分别记录下载数量和耗时；同副本重复安装不应因方案引入额外全量下载；并保留改造前基线（同受控依赖集在旧 CephFS store 的冷/热耗时），前后对比写入 verification.md。正常包变化重取不算失败。
4. 重启与滚动：删除单Pod并确认新UID+同PVC UID+pnpm缓存保留；发布新镜像/配置/模板后滚动完成、模板更新；持续请求记录可用副本和错误率，不只看 rollout exit0。
5. 迁移回滚：旧release→A→排空→B→排空→A完整演练；记录endpoint Pod UID，保证维护外不出现未授权双代；旧PVC UID不变；新PVC回滚后留存。注入SC不存在/PVC Pending/init失败验证停在维护并能回退。
6. 调度：健康节点drain与异常节点分开验收，分别记录PDB、终止、CSI、拓扑阻塞原因；不能用强删伪造故障恢复通过。
7. 业务：UserApp实际构建启动、预览/跨副本转发、发布任务恢复、Chat/SSE重连；当前 chart + 对应镜像执行。检查旧Pod UID不再能控制新实例；如果这些场景阻塞则不能宣告整体通过。

新增 verification.md：基线、实际命令/退出码、chart与镜像身份、命中证据、PVC/Pod UID、时间线、通过/失败/未运行分别记录。本轮上述集群测试均未执行。

## 7. 官方依据

- Helm v4.3.0 更新实现（本机版本匹配；目标集群执行端版本仍需核实）：https://github.com/helm/helm/blob/v4.3.0/pkg/kube/client.go#L575
- Helm 3 对照：https://github.com/helm/helm/blob/v3.17.3/pkg/kube/client.go
- STS 更新、并行管理、PVC留存：https://kubernetes.io/docs/concepts/workloads/controllers/statefulset/
- VCT 同名优先规则：https://kubernetes.io/docs/reference/kubernetes-api/apps/stateful-set-v1/
- PDB边界：https://kubernetes.io/docs/concepts/workloads/pods/disruptions/
- Headless DNS：https://kubernetes.io/docs/concepts/services-networking/dns-pod-service/
- PVC访问/扩容/回收：https://kubernetes.io/docs/concepts/storage/persistent-volumes/
