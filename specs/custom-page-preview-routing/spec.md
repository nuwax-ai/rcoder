# Custom Page 多副本预览路由与生命周期规范

状态：独立复核完成（复核记录见文末），按批次实施中；完成与验证证据登记在 tasks.md。

## 背景与问题

Custom Page（存量前端项目，Vite dev 预览）在 K8s 多副本下的两条断链：

1. **预览路由**：Java 将 `/page/{projectId}-{agentId}/dev/...` 转成 `/proxy/{port}/page/...` 发给 rcoder 8088 Service；`/proxy/{port}` 由命中副本的 Pingora 代理到**本 Pod** `127.0.0.1:{port}`。Vite 只在宿主 Pod 存在，非宿主副本一律 502（线上取证：三副本只有一个监听目标端口，不同出口 IP 分别固定 200/502，与本仓 `port_proxy.rs` 逐行复核一致）。
2. **生命周期**：`/api/build/start-dev|stop-dev|restart-dev|keep-alive|list-dev` 等接口命中任意副本后都在**本机**执行（TS 与 Rust file-server 双实现均为纯内存注册表 + 本机探活 + 本机 ps 扫描）。keep-alive 探活失败会"删登记→stop→start"，在非宿主副本上会误停他机进程登记、并在错误副本重建实例；端口池/注册表进程重启即失忆；多副本可对共享 node_modules 并发安装。

## 目标

- 任意副本收到的 Custom Page 预览请求与生命周期操作，都能定位到唯一合法宿主并正确执行或转发。
- 预览身份、实例身份、宿主身份三层可区分、可校验；进程重启、端口复用、Pod 重建不产生误杀、误连、误重建。
- Java 外部 URL 与既有 HTTP 接口参数/响应信封保持兼容（零改动为验收基线，degraded 场景的语义差异显式登记）。
- Compose 单实例复用同一契约（显式 SQLite 适配），单实例也验证进程身份链。

## 行为不变量

**身份**

1. 预览身份（preview_key）由调用方项目上下文（project_id + isolation：tenant/space/agent）经 WorkspaceResolver 解析出的真实目录共同构成；禁止只以 port、pid、pod IP、pod name 任何单项作为身份。
2. port 只是实例属性与入口兼容参数：路由解析可以用 port 做首查键，但命中后必须校验 canonical preview_key / instance_id / host_id；端口在协调层全局唯一分配，杜绝"两个宿主同 port 不同项目"的入口歧义。
3. host_id = Pod UID + 本地进程启动代次（boot_id）；实例写入携带 operation_id 与 revision，旧心跳、旧停止、迟到失败只能更新其原实例，不得覆盖新实例。

**状态机**

4. 权威状态只存协调层存储（K8s=PG，Compose=显式配置的 SQLite），至少区分 Starting/Ready/Stopping/Stopped/Failed/Unknown；启动/重启条件受理（活跃实例存在时按条件等待/拒绝/走恢复），成功后条件发布（CAS 于 operation_id+revision）。
5. PG/SQLite 故障不伪装"不存在"也不受理成功：生命周期接口 fail-fast 返回暂不可用；预览路由解析失败降级为既有本地行为（不劣于现状），且不得缓存错误负结果。
6. 心跳（host 上报的进程健康）与用户 activity（keep-alive/访问）分开记录。宿主心跳不得延长无人使用项目的寿命；空闲回收只看 last_activity_at。心跳过期只得出 Unknown，不推断旧进程已停止。
7. Unknown 恢复（在别处重建）需要充分证据：原宿主 Pod 确认终止（K8s 查无该 Pod UID）、或原实例确认已停止；不允许仅凭 TTL 超时接管。本 Pod 容器重启（同 Pod UID、新 boot_id）可将其旧实例判停（容器 PID 命名空间销毁是直接证据）。
8. 远端操作结果不确定（超时/分区）时保留执行边界：不自动重放远端写，不跨 Pod 按裸 PID kill；补偿只处理本次捕获的、路径/登记匹配的进程组。

**执行边界**

9. 所有可执行路径（HTTP 入口、60000 分流、间接自动重启）使用同一协调受理与同一操作身份；dev 生命周期 7 个精确路径在 60000 分流层**所有策略**导向 Rust 协调（TS 不执行 dev 生命周期，TS 仓零改动）；Rust 执行器为进程内 DevServerManager（票据参数化：外部端口 + preview_key 键、按登记 pid 组停止）；协调未注入的形态（agent-runner 内嵌、独立 file-server）保持现状本机行为。
9a. **发布隔离（npm/Electron 硬约束，2026-09-15 用户补充）**：file-server-proxy 与 file-server 的 npm/Electron 发布形态**永远不携带 PG/协调器依赖**——file-server-proxy 保持纯分流器（kube-free/storage-free，`coordinated_dev_lifecycle` 独立形态恒 false）；预览协调与 PG 访问只存在于 rcoder 主进程装配链（preview-coordinator trait 注入 + rcoder-storage feature 门控），不进入 npm 发布产物的依赖闭包。
10. 共享目录的依赖安装/删除互斥：同一 preview_key 的并发 restart 只有一个合法当前实例；跨副本并发受理按状态机串行或明确冲突。
11. 内部接口（跨 Pod 执行/转发）必须鉴权（内部令牌），限制目标与转发次数；外部传入的内部令牌/身份头一律剥离或校验失败拒绝；不允许客户端自选 pod_ip 或任意 URL。旧 Pod IP 复用时实例不匹配必须拒绝。

**路由与缓存**

12. 保留 Java 外部 URL `/proxy/{port}/...` 与既有显式端口代理契约；未识别为 Custom Page 的 `/proxy/{port}` 走原逻辑。转发到宿主受保护的内部预览入口，宿主校验 instance_id/host_id/本地登记后才转 Vite。
13. 缓存按 canonical 键（含 instance/revision/host 身份），TTL 不超过剩余有效期；"端口可连接但属于新实例"必须识别并拒绝后至多刷新一次重解析。已发送写请求、流式 body、已升级 WebSocket 不自动重放。
14. 保留 URI/query/base、WebSocket Upgrade、HMR 路径与必要 Host 行为；使用实际锁定 Vite 版本验证；strictPort 失败按有界重试换端口，登记端口以实际回报为准。

**兼容**

15. Java keep-alive/stop/restart 参数与响应信封保持；任意副本可更新 activity；存活判定以宿主验证或新鲜心跳为准，探活成功不证明归属。旧 pid/port 与当前实例不匹配时不得停止新实例；旧接口缺 instance_id 的迟到 stop/restart 风险显式登记（完整防护需后续可选令牌协议，本轮不承诺）。降级响应取 HTTP 200 + success:false + reason（决策记录）。
16. ts_first / all_rust 下 dev 生命周期 7 路径统一进 Rust 协调（分流层对所有策略改路，TS 仓零改动）；不得全局强制 all_rust 掩盖问题。不以 sessionAffinity 或副本缩 1 治本。空闲回收机制实现但默认关闭（TTL=0 保持现状；测试环境显式开启验证）。

## 范围

- 覆盖入口：`/api/build/start-dev`、`stop-dev`、`restart-dev`、`keep-alive`、`list-dev`、`get-dev-log`、`port-pool-status`（60000 与 8086 两个进入面）；`/proxy/{port}` 预览路由与 HMR WebSocket。
- 覆盖运行形态：rcoder 主进程（K8s 多副本 / Compose 单实例）；agent-runner 内嵌 file-server 与独立 file-server 形态保持现状（协调未注入时本机行为）。
- 部署侧：Helm 补 POD_UID/POD_IP 注入、内部令牌与存储配置、NetworkPolicy 核对。存储后端：K8s=平台 PG（已是硬依赖，不新增故障域）；Compose=进程内实现（单节点无共享读者）。不涉及 npm/JS 包发布（TS 零改动）。

## 非目标

- 不改 Java；不迁移 Vite 到 agent 容器；不引入 CRD/Operator/新中间件。
- 不动 UserApp builder/生产 Deployment 生命周期（`/api/v1/userapp/*`、file-server-userapp 域），不与 UserApp lifecycle 表混用。
- 不改其余显式端口代理的语义；不做公网 OpenResty error_page 行为调整（登记为独立端到端验收项）。
- 本轮不自动 commit/push/tag/npm 发布/共享 release 升级。

## 复核记录（2026-09-14，独立复核）

移交文档 9 项事实全部复核一致，关键锚点：

- `values-k8s-test.yaml:168` `fileServerProxyPolicy: ts_first`、`:192` rcoder `replicaCount: 3`；`deploy.sh:627-630` 渲染顺序 = `values-default.yaml`（含 ClientIP 亲和）+ 环境文件。
- `configmap.yaml`：60000 分流，rust=8086 / ts=60001。
- `service.yaml`：8088(proxy)/60000(file-server) 同 Pod selector 暴露。
- `deployment.yaml:142,207`：仅 POD_NAME，无 POD_UID/POD_IP。
- `Dockerfile:127,194`（build-agent-docker）：`nuwax-file-server@${NUWAX_FILE_SERVER_VERSION}`（默认 latest），本地 JS 改动不自动进镜像。
- `rcoder-proxy/src/service/handlers/port_proxy.rs:114-118`：backends miss 回落本机 127.0.0.1，无宿主识别；`backends.rs` 动态注册全仓无生产调用方。
- JS `keepAliveDevUtils.js:29-63`：探活失败→`deleteRunningProcess`→stop（忽略入参 pid、按 projectId 扫 ps）→完整 start；无 TTL/时间戳。Rust `file-server/src/service/dev_server/mod.rs:52-80` 同构。
- JS `stopDevUtils.js:424-426` stop 无条件按 projectId 扫描；`codeService.js` 间接 restart 已被硬编码 false 关闭（死入口确认）。
- 端口池 4000-55000（跳 8000-9000）、vite `--strictPort --host 0.0.0.0`、注册表纯内存、dev 进程 detached 存活（JS 重启后"进程在登记无"）。
- Rust `start_dev` 具备完整依赖安装（pnpm install + dev-inject + npmrc），all_rust 执行器可行。
- NetworkPolicy 已放行同 namespace 跨 Pod 访问 8086/8088，跨副本转发无网络阻碍。
- 补充事实（移交未提）：`/api/build/*` 在 Rust file-server 与 TS 双实现并存；`all_rust` 模式 60000 有路径白名单（`/api/*` 放行）；`file-server` 刻意 kube-free（trait 注入模式 `WorkspaceResolver` 已确立）；rcoder 主进程不读自身 Pod 身份。

## 主要风险

1. Java 对 degraded 响应（HTTP 200 + success:false）的处理不在本仓可见范围内——E2E 直接驱动 API 验证信封，Java 实际容忍度需与 Java 同事确认。
2. ts_first 下 Custom Page dev 生命周期执行从 TS 实现切到 Rust 实现（同仓移植版），行为等价性依赖响应信封逐字段对齐（四信封已取证）+ 启动链等价性审计（批次 2 T2.3，rollup 变体检测/symlink 修复/模板缓存等差异项在 Rust 侧补齐）。
3. 旧接口缺 instance_id：迟到 stop/restart 只能靠 port/pid 弱校验，无法完全阻断信息缺失问题（已按不变量 15 显式登记）。

## 决策记录（2026-09-14 计划评审）

- 发现层：K8s=PG 注册表（硬依赖复用+真 CAS+仓库范式）；Compose=进程内实现（用户澄清单节点）。K8s 原生 Lease/一致性哈希/gossip 不采纳；headless 域名只解决寻址不解决发现，暂不需要。
- 执行器纯 Rust；TS 仓 nuwax-file-server 零改动（用户约束：JS 不归本项目负责）。
- keep-alive 降级信封 = HTTP 200 + success:false + reason。
- 空闲回收默认关闭（TTL=0，机制完整、测试环境显式开启验证）。
- Java 直连 pod IP 的短期绕过方案不采纳（需改 Java + 不解决误重建）。
