# Custom Page 多副本预览修复：独立复核与开发提示词

请先独立复核本文与实际代码，再实施。目标是修复 Custom Page 主 Pod 内 Vite 的跨副本路由与生命周期，不是 UserApp builder/生产 Deployment，不要混用它们的身份或资源模型。

## 仓库与工作边界

- 主仓：`/Users/soddy/Documents/git-workspace/rcoder`
- JS 源码：`/Users/soddy/Documents/git-workspace/nuwax-file-server`
- 镜像/部署仓：`/Users/soddy/Documents/git-workspace/build-agent-docker`，重点 `k8s/`。
- 先读各仓 AGENTS.md，git status/diff，保留并行修改。构建仓 Chart.yaml/VERSION 已有未提交修改，不覆盖、不擅自 bump。
- 分别维护 `specs/custom-page-preview-routing/spec.md`、`plan.md`、`tasks.md`，本文只是移交，不是已实现证明。
- 不访问生产或旧共享业务测试环境。真实验证使用 rcoder 的 remote-k8s 专属环境，读取 `.env.local`，不得把主机和凭据写入提交文件。
- 用户当前授权继续研发与隔离测试；不自动 commit/push/tag/npm 发布，不升级共享 release。

## 背景与源码已确认事实（请复核）

旧链路：Java 将 `/page/{projectId}-{agentId}/dev/...` 转成 `/proxy/{port}/page/...`，发送 rcoder 8088 Service；Vite 由命中副本的 file-server 启动，只有宿主有该进程。

用户提供线上取证：三副本中只有一个监听目标端口，不同出口 IP 请求分别固定 200/502。线上事实未在本移交中独立复验，不能作为新实现验收。

实际配置：

1. `k8s/helm/nuwax-platform/values-k8s-test.yaml` 的 rcoder 为 3 副本，`fileServerProxyPolicy: ts_first`。
2. `templates/rcoder/configmap.yaml`：60000 是分流代理；Rust upstream 8086，JS upstream 60001。ts_first 下 UserApp 标记走 Rust，其余普通流量走 JS。不能照旧文档说 Express 直接监听外部 60000。
3. `templates/rcoder/service.yaml`：8088 预览代理和60000文件服务通过同一类 Pod selector 暴露；sessionAffinity 由 values 控制。ClientIP 不提供项目归属。
4. `templates/rcoder/deployment.yaml`：已有 POD_NAME；此次读取未发现 POD_UID/POD_IP 注入，需要独立确认并补 Downward API。共享 workspace 不等于共享进程或端口。
5. `build_config/rcoder/Dockerfile:127,194` 用 `NUWAX_FILE_SERVER_VERSION`（默认 latest）从 npm 安装 JS 包。本地 JS 修改不会自动进入镜像。
6. `rcoder-proxy/src/service/handlers/port_proxy.rs` 按端口查显式映射，否则默认主机；没有项目宿主识别。
7. JS `src/utils/build/keepAliveDevUtils.js` 探活失败会删本地登记、尝试 stop、再 start，不只是写保活时间。Rust `file-server/src/service/dev_server/mod.rs` 同样存在探活失败重启。
8. JS `src/service/codeService.js` 等有内部 restartDevServer 调用；只改 HTTP 路由不覆盖全部入口。

使用发布脚本的真实 values 合并顺序渲染 Helm，确认端口、亲和、replicas、PG、挂载与环境变量；不能只看一个 values 或用 dev 的 all_rust 证明 test 的 ts_first 正确。不要输出 Secret 内容。

## 必须纠正的设计

- 不允许先连 localhost:port，成功就使用。同端口可能属于别项目；探活成功不证明归属。
- 注册表不能只以 port 为键。先定义 canonical preview identity，再用 port 做兼容参数校验。
- 预览身份需核对 project/agent/tenant/space 与真实目录映射；不要未经验证随意拼接或忽略 agentId。
- pid、Pod IP、Pod name 都不能单独作为实例身份；进程重启和地址复用必须可区分。
- heartbeat 与用户 activity 分开。宿主心跳不能导致无人使用的项目永不回收。
- 心跳过期只说明未知/不可用，不能证明旧进程已停止，也不能授权跨副本并发安装依赖或重新创建。
- 缓存失效必须识别“端口可连接但属于新实例”的情况；只靠连接拒绝自愈不足。

## 推荐实现

### 1. 单一协调层与存储

在 Rust 中实现 Custom Page 专用协调器和语义存储接口，复用已有 SQLx/PG 基础设施，不把 Custom Page 塞入 UserApp lifecycle 表。跨 crate 业务契约放 shared_types。

候选字段：preview_key、instance_id、revision、operation_id、host_id（Pod UID＋本地进程管理器启动代次）、pod_name/IP、pid、port、state、last_heartbeat_at、last_activity_at。字段范围最终由调用链决定。

状态至少区分 Starting/Ready/Stopping/Stopped/Failed/Unknown。条件受理启动/重启；启动成功后条件发布实例；旧心跳、旧停止、迟到失败只能更新其原实例。网络等待不在 SQL 事务内。

PG 故障不伪装不存在或受理成功。远端操作结果不确定时保留执行边界，不通过 TTL 自动接管。复用现有已证明安全的操作模式，不另造不受保护的租约。

Compose 复用同一契约，可选择 SQLx SQLite 适配；必须显式配置和迁移，不假定已有仅覆盖 UserApp 的 SQLite 自动支持本模块。单实例也验证进程身份。

### 2. 进程管理和控制入口

JS 保留成熟的 Vite/依赖安装逻辑，通过受保护内部接口接入 Rust 协调，不自行实现第二套 SQL 状态机。

覆盖 start/restart/stop/keep-alive/间接自动重启/闲置回收；状态和日志按归属查询。所有副本受理后定位宿主，在宿主执行本地命令。内部执行入口明确与公开协调入口分离，防止递归转发。

需要防止 Rust→JS→Rust 递归：定义命令票据和本地 executor 边界；公开请求不能伪造“已协调”标记，所有可执行路径使用相同操作身份。直接内部调用也经过协调，不只包装 routes。

持有项目控制权限再修改共享 node_modules 或启动/停止进程。跨副本重复 restart 必须串行或明确冲突，不允许两边同时安装。补偿只处理本次捕获的进程组，不按来自其他 Pod 的裸 PID kill。

宿主失联时先 Unknown；确认原实例停止/宿主终止或其他充分安全证据后才恢复。不能因为 SQL lease 超时就推断共享目录写入结束。

### 3. 预览路由

保留现有 Java 外部 URL。对识别为 Custom Page 的路径解析预览身份，读取缓存/注册表，转发到宿主受保护的内部预览入口；宿主校验 instance_id/host_id/本地登记后才转 Vite。

不要改所有 `/proxy/{port}` 语义来修单一场景，其他显式端口代理保留原契约并单独审查。没有可解析项目身份的 HMR/资源请求需先捕获真实请求确定兼容路由，不能盲目按 port 或 Referer 兜底。

内部接口可复用现有监听面，但必须鉴权、限制目标和转发次数；不新增公网 NodePort。内部签名/认证信息外部传入时剥离，不允许客户端自选 pod_ip 或任意 URL。旧 Pod IP 被复用时，实例不匹配须拒绝。

缓存按 canonical key，包含 instance/revision/host 身份，TTL 不超过剩余有效期；PG 错误不负缓存。连接尚未发送请求或明确宿主身份拒绝时，最多刷新一次。已发送写请求、流式 body、升级后的 WebSocket 不自动重放。

保留 URI/query/base、WebSocket Upgrade、HMR 路径与必要 Host 行为。使用实际锁定 Vite 版本验证，不照抄新版 Vite 配置。检查 strictPort 或实际端口回报，避免 Vite 自动换端口而登记旧端口。

### 4. keep-alive 兼容

Java 零改动作为目标，保留参数/响应和现有恢复能力，不能改成无条件成功。任意副本更新 activity，由宿主验证健康；需要恢复进入统一受理。

旧 pid/port 不匹配当前实例时不能停止新实例。明确记录旧接口缺 instance_id 的限制：完全阻止所有迟到 stop/restart 可能需要后续可选令牌协议，不能承诺不存在信息缺失问题。

### 5. 镜像与发布配置

补齐 POD_UID/POD_IP、必要内部通信鉴权与存储配置，检查 NetworkPolicy（若存在）允许专属副本内部路由。不以 sessionAffinity 或副本缩1治本，不关闭整个共享服务亲和。

JS 验收使用 npm pack 的本次 tarball 或隔离构建上下文显式安装，记录包摘要/版本及镜像内实际内容。未经授权不发布 npm；不能用 latest 宣称本地改动已进镜像。正式发布前列出 JS 包发布与主镜像重建依赖。

如需修改镜像构建入口，保留原有发布通道并提供明确本地包验证方式，避免修改全局 registry 或未固定 latest。

## 自动化验收

先补能失败的确定性测试，再实施，不放宽断言：

1. 两宿主相同端口、不同项目内容，从每个副本访问均正确，无 localhost 误命中。
2. keep-alive/stop/restart 打到非宿主只由正确 executor 执行；裸 PID 不误杀。
3. 并发 restart 只有一个合法当前实例；共享目录安装互斥。
4. 旧 heartbeat/stop/失败回写不能覆盖新实例。
5. 缓存指向可连接但错误实例时拒绝并刷新；不是只测 connection refused。
6. PG 故障、宿主分区、进程退出、Pod 重建：无假成功、无未经授权重建。
7. HTTP 文档/JS/CSS/query/尾斜杠与真实 HMR WebSocket：修改源码后页面实际更新。
8. ts_first、all_rust 各验证对应兼容路径；all_ts 若保留需明确支持边界和验证，不允许绕过协调。不得全局强制 all_rust 掩盖 JS 问题。
9. Compose 单实例回归，SQLite 重启持久化（若采用）。
10. Helm 渲染检查实际 test 合并结果，专属 K8s 多副本 E2E 直接命中各副本，不依赖 ClientIP 或仅六连200。

HTTP 原业务接口保持原信封，应用代理响应保持代理协议；区分确知不存在与暂不可用。公网 OpenResty 的 error_page 可能继续替换错误内容，应作为独立端到端验收项，未经授权不修改公网配置。

测试日志记录 run_id、源码指纹、JS tarball 摘要、镜像 digest、实例与宿主身份、断言及清理证据。环境缺失/skip/aborted/零场景不通过。只清理本次登记资源，Agent PVC 永不删。

## 执行与交付

1. 先给简洁复核结果与关键风险，完善 Spec/Plan/Tasks 后按批开发。
2. 建议批次：契约/存储 → 全入口协调/JS接入 → 预览代理/HMR → 构建配置/E2E。
3. 精确暂存本任务文件；本提示词不要求自动提交推送。
4. Rust 格式、相关默认/K8s/存储 features 测试和 Clippy；JS 单测；Helm 渲染；最后 Compose 与 remote-k8s 专属验收。不得同 target 并发 Cargo。
5. 交付说明修改、执行命令和退出码、实际通过范围、未验证项。历史环境证据不能代替本轮。

不要把这个任务扩大为迁移 Vite 到 agent 容器。长期独立 Preview Workload/Service 可作为未来方向，但本次采用 PG 协调宿主进程，保持 Java 路径兼容。
