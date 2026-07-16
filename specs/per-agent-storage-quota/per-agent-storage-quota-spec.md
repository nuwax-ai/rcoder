# Spec: per-agent 存储配额 + file-server Rust 重写

> 状态: 调研完成 / 待评审
> 仓库: rcoder(Rust) + nuwax-file-server(Node,待 Rust 重写) + build-agent-docker(chart)
> 关联: BACKLOG 待办 2(CephFS 配额)、待办 5(路径拼接 PathBuf)

## 1. 背景

### 1.1 业务诉求
rcoder 为每用户/项目动态创建 agent 容器(computer 沙箱 / web agent / UserApp),工作区数据落 CephFS。核心诉求:
- **限制每个容器的磁盘上限**(防单容器写爆共享卷)。
- **RWX 共享**: 一个 CephFS PVC 多容器共用。
- **agent 工作区管理**: 自动创建目录、接收外部推送的文件(skills/hooks 配置)、提供文件 HTTP 服务。

### 1.2 现状
- 两个共享 CephFS RWX PVC: `rcoder-workspace`(web agent + UserApp + project_zips + project_nginx)、`rcoder-computer-workspace`(computer 沙箱)。rcoder 主 pod **挂根**直接 fs 读写 agent 子目录。
- **nuwax-file-server 是 rcoder pod 内 Node 进程**,与 rcoder 共享挂载,按 `projectId`/`userId+cId` 精确寻址,提供 build/git/static/CRUD/upload/skills-hooks 分发。
- 三类容器(Web/Computer/UserApp)全复用共享 PVC + subPath,**无真正独立 PVC**。

### 1.3 已尝试方案:xattr 目录配额(已禁用,2026-07-15 EDQUOT 事故)

rcoder 曾用 `xattr::set(quota_dir, "ceph.quota.max_bytes", N)` 给每 agent 子目录设配额(`kubernetes_runtime.rs:728-768`,已注释禁用)。事故不是"配额没设上",而是 **"配额被意外设上了、真生效了、却不可见不可改"**:

1. **旧 caps 时期**(`allow rw`): 在某 caps × kernel × ceph 版本组合下,client 侧 `setfattr ceph.quota.*` 虚拟 xattr **被意外放行 → 设成功**(10Gi),代码盲设(`:753-763` 只有 Ok→info/Err→warn,**无 read-back 校验**),配额悄悄生效。
2. **用户写超 10Gi**: MDS 拒写 → `EDQUOT (errno 122)` → file-server 报 500 → 前端"请求没反应"。
3. **重设放宽失败**: 环境已变,kernel client 直接 denied(连 `client.admin allow *` 都 denied,`:737`);想 verify 却读不回(`getfattr -d -m -` 隐藏 ceph.* xattr,需 `-n ceph.quota.max_bytes` specific 才能读,但代码没做)→ 死锁式困境。
4. **止血**: node admin `setfattr -x` 清旧配额 + 代码整块禁用。

**根因三要素叠加**(非数值/逻辑 bug):
- ① **机制漂移**: client 侧 setfattr 放行与否取决于 caps × kernel × ceph 版本,生产连 admin 都 denied。
- ② **不可观测**: getfattr 默认读不回 ceph.* xattr + 代码盲设无校验,配额成"薛定谔状态"。
- ③ **EDQUOT 锁死**: 一旦 MDS 拒写,重设/查看/清除都做不到,只能 node admin 强清。

chart 的 `fix-cephfs-caps-job` 改 cephx caps 让 xattr 可写,是治标(仍走 client setfattr 不可靠路)。

### 1.4 为什么 shared PVC + 子目录配额是社区 discourged

K8s 无原生 subPath 配额;client 侧 `setfattr ceph.quota.*` 在 kernel mount 下**行为不可靠**(依赖 caps × kernel × ceph 版本,生产实测连 `client.admin allow *` 都 denied)且默认 `getfattr` 不可观测(详见 §1.3 事故)。

**CephFS 配额的官方定性**(无论设在普通目录还是 subvolume,底层都是同一个 `ceph.quota.max_bytes` xattr):
- **cooperative(合作式)**: 依赖挂载 client 在达限时停写;对抗性 client 防不住([Ceph 官方 Limitations](https://docs.ceph.com/en/latest/cephfs/quota/))。
- **imprecise(不精确)**: 写入会短暂超配额(官方:几十秒内才停)。
- ⚠️ 即 **不存在"目录配额 soft vs subvolume hard"的对立**——两者同一 xattr、同样 cooperative/imprecise。

社区主流 = **per-PVC(CephFS subvolume)+ CSI `requests.storage`**,优势**不在配额精度**,而在:
1. 配额由 **ceph-csi 持 admin 凭据在服务端**经 mgr volumes 模块设(`ceph fs subvolume create --size`),**绕开 client 侧 setfattr**(根除 §1.3 的 client 漂移);
2. 服务端可查(subvolume 信息可见,解决 getfattr 不可观测);
3. `subvolume resize` 服务端执行(可改/扩容);
4. **per-subvolume path-restricted cephx 凭据**提供真正的租户隔离(client 只能 rw 自己 subvolume,这才是兜底的"墙")。

## 2. 目标与非目标

### 目标
1. per-agent **配额**(CephFS subvolume,服务端设/查/改可靠 + per-subvolume 凭据隔离;CephFS 配额本身 cooperative/imprecise,见 §1.4)。
2. **file-server 用 Rust 完整重写**(对齐 nuwax-file-server 全功能),初期兼容现有 HTTP 接口,**直接进 agent pod**。
3. rcoder 解耦"聚合视图"(管理操作 RPC 化 + file-server 下沉),为 per-agent PVC 铺路。
4. 无缝迁移(共享 PVC 数据 → per-agent PVC,可回滚)。

### 非目标
- CephFS 本身配置(SubVolumeGroup `csi` 已就绪)。
- Docker compose 模式(只 K8s)。
- BACKLOG 待办 2 其它候选(ceph-fuse / rcoder 调 Ceph admin)——本 spec 用 per-agent PVC 替代。

## 3. 方案

### 3.1 数据面:per-agent PVC(CephFS subvolume + CSI 服务端配额)
每**容器**一个独立 PVC(`cephfs` SC,SubVolumeGroup `csi` 已存在),`requests.storage = storage_size` → CSI 持 admin 凭据在**服务端**设 subvolume 配额(`ceph fs subvolume create --size`,绕开 client setfattr)。agent pod 挂自己 PVC(subPath=None)。

> 注: CephFS 配额本身 cooperative/imprecise(见 §1.4)。本方案的可靠性来自 **服务端设/查/改**(根除 client 漂移 + 可观测)+ **per-subvolume path-restricted 凭据隔离**(真隔离),而非配额精度。

**生产实证(2026-07,方案地基成立)**: 现有 4 个 cephfs PVC **全部 subvolume-backed**(PV `subvolumeName` 字段);`ceph fs subvolume info` 实证 `bytes_quota` 精确匹配 `requests.storage`(20Gi/30Gi/3Ti×2);**computer-workspace 已用 47.5%(1.56TiB/3TiB)证明配额真实约束写入**。ceph-csi v3.17.0 `subvolumeEnabled` 已废弃(subvolume 唯一模式,SC 无需改);SubVolumeGroup `csi` Ready;Ceph v20.2.1 + kernel 6.17 配额支持完备。**无前提条件需补齐**。

**PVC 粒度 = 容器标识(`ServiceType::container_identifier`,`service_type.rs:161-176`)**,按 service_type + 隔离类型区分,与 rcoder 现有容器复用逻辑天然对齐:
- **共享容器场景**(`pod_id` 存在,如 web 开发的 tenant/space 级共享容器): per `pod_id`(一个共享容器一个 PVC,多 project 共用)。
- **ComputerAgentRunner**: per `user_id`。
- **WebAgentRunner / UserApp**(project 隔离): per `project_id`。

退役: xattr 配额死代码 + `fix-cephfs-caps` Job。
**保留共享 RWX PVC(非全退役)**: `project_nginx`(build 产物 dist,nginx 预览)、`project_zips`(下载/回滚)、模板源仍共享(见 §3.5)。即"agent 工作区数据 per-agent PVC + 跨 agent 共享区保留 RWX"。

### 3.2 file-server Rust 完整重写(`crates/file-server`)
- **完整功能**(对齐 nuwax-file-server 64 文件): HTTP 接口、目录管理、上传、build、dev server、git、static、CRUD、skills/hooks 分发、zip、模板缓存。
- **兼容现有 HTTP 接口**(rcoder/前端调用方零改动): `/api/project/*`、`/api/computer/*`、`/api/build/*`、`/api/git/*`、`/api/code/*`、`/upload-file`、`/upload-files`、`/api/{page,computer}/static/*`。
- **直接进 agent pod**(初期就下沉,非 rcoder pod): 每 agent pod 跑一份 file-server,直挂自己工作区。
- Node 依赖 Rust 替代(均成熟): express→axum、fs-extra→tokio::fs、isomorphic-git→**gix**、archiver/yauzl→zip、pm2/cross-spawn/tree-kill→tokio::process、multer→axum multipart、node-cron→tokio::time、iconv-lite→encoding_rs。
- **git 选型(已评估,可行)**: 选 **gix**(纯 Rust)。调研确认 nuwax 用的全部 isomorphic-git API(init/setConfig/statusMatrix/add/remove/commit/resolveRef/readBlob/currentBranch/log/listFiles/readCommit/resetIndex/writeRef/checkout/branch/tags)在 gix 有对等或更强实现,**无功能缺口**:
  - `statusMatrix`(关键) → gix `Repository::status()` Platform→Item 流(强类型枚举 `Removed`/`Modification`/`DirectoryContents`,比 isomorphic-git 的 0/1/2/3 数字编码更清晰)
  - `diff`(nuwax 现 ~200 行手搓 patch) → gix `diff_tree_to_tree` 原生 unified diff,**可大幅简化**
  - 需封装 3 处薄层(gix 无高层): `git add`(write_blob + index entry,~15 行)、`checkout 切分支`(edit ref + gix-worktree-state,~20 行)、`reset`(ref + index + checkout,~30 行)
  - 备选 `git2-rs`(libgit2): 高层 API 更全(add_path/reset/checkout_head 一行)但 C 依赖。**不需 gix+git2 混合**,gix 单独覆盖。

### 3.3 rcoder 瘦聚合层
file-server 下沉后,rcoder pod 只保留**跨 agent 共享区**:
- `project_nginx`(nginx 直挂,前端预览产物)
- `project_zips`(下载/回滚源)
- 模板源 `project_init`
- **入口路由**: `projectId/userId+cId → agent pod IP`,转发 file-server HTTP 请求到对应 agent pod

### 3.4 WorkspaceService(gRPC)vs file-server(HTTP)职责划分
两者都做文件操作,边界:
- **`WorkspaceService`**(agent_runner gRPC): rcoder **内部管理调用**(建 scaffold、读日志、列目录、stat)——低频、程序间。
- **`file-server`**(HTTP): **前端/外部调用**(上传、浏览、build、git、skills/hooks 分发)——高频、面向用户/外部系统。
- 共用底座: 路径解析 + 防穿越(`resolve_project_cwd`)、tokio::fs、shared_types。

### 3.5 网络与存储链路(file-server 下沉 sidecar 后的可行性确认)

调研 nuwax-file-server 后确认: file-server 代码无"必须在 rcoder pod"的硬约束,三条链路均可下沉,但都不是"下沉即用":

**① dev server HMR 预览(必须新增反向代理)**:
- 现状: file-server 启 dev 进程后只返回 `{pid, port}`,浏览器直连 `host:port`(file-server 不代理端口)。
- 下沉后: dev server 监听 agent pod `0.0.0.0:<port>`,浏览器不可达 → **HMR 断**。
- 解法: rcoder 新增反向代理(如 `/preview/<agentId>/<projectId>/<devPort>/`)→ `agentPodIP:<devPort>`,给 agent pod dev 端口建 headless Service,处理 vite HMR 的 **WebSocket 透传**(Upgrade/Connection)与 `--base` 子路径对齐。file-server 侧不改返回结构,前端拼 URL 逻辑切到 rcoder 预览代理路径。
  - **调研实证**: 复用 pingora + 新 `RouteType::DevServerProxy`(~200-250 LOC,clone `handlers/ttyd.rs`/`audio.rs` 范式);WS 透传 pingora 原生支持(现有 vnc/ttyd/audio/ime 四条 WS 链路实证,`port_proxy.rs:88` 注释明示支持 Vite HMR);dev 端口进 URL(路线 A,无状态最低成本,无需新 RPC/DashMap);动态路由复用 `ContainerLookup::find_by_project_id`(已返回 agent pod FQDN/IP)。
  - ⚠️ **agent Service 现是 ClusterIP(非 headless,注释术语不准)**,只暴露 8086/50051/6080/17681 固定端口 → dev 动态端口(4000-55000)**不可达**。需改 Headless(`clusterIP: None`,`k8s_service.rs:201`)或新增独立 headless svc,**回归验证 VNC/ttyd**。

**② build 产物 dist → project_nginx(共享 RWX PVC,零改)**:
- 现状: `fs.cp(dist → /app/project_nginx/<projectId>/dist)`,nginx 直挂聚合目录预览。
- 下沉后: dist 在 agent pod → 让 agent sidecar + rcoder nginx 共挂**共享 RWX `project_nginx` PVC**,sidecar 直接 `fs.cp` 到共享卷(代码零改)。nginx 继续聚合在 rcoder。注意删旧目录+拷的缓存失效(nginx reload / open_file_cache off)。

**③ 静态文件 /api/page/static /api/computer/static(共享 RWX PVC)**:
- 现状: `res.sendFile` 直读共享 PVC。
- 下沉后: `PROJECT_SOURCE_DIR`/`COMPUTER_WORKSPACE_DIR` 仍是共享 RWX(或 rcoder 代理到 agent sidecar)。最低成本: 共享 RWX,rcoder 侧继续直读(代码零改)。

**结论**: 共享 RWX PVC(`project_nginx`/`project_workspace`/`computer-project-workspace`)**保留**,file-server 下沉后靠它处理产物/静态;dev server 端口必须新增 rcoder 反向代理链路(阶段 1/2 要做)。

### 3.6 file-server 镜像方案(`build-agent-docker/build_config/` 新增)

仿 `agent-platform-front/Dockerfile` 范式(多阶段 + `ARG DOCKER_MIRROR` 国内镜像前缀 + `.npmrc` npmmirror + pnpm):
- **Stage 1 builder**: `${DOCKER_MIRROR}rust`(或复用 `build_config/app-runtime-base/Dockerfile.rust`)+ 国内 cargo 镜像 → 编译 `crates/file-server` Rust 二进制。
- **Stage 2 runtime**: `${DOCKER_MIRROR}node:24` + `pnpm`(`registry=https://registry.npmmirror.com/`)+ 拷 Rust 二进制。**node 运行时必需**(file-server 要 spawn pnpm/vite 跑 build、dev server)。
- 国内加速: `DOCKER_MIRROR`(docker.io 镜像前缀) + `.npmrc`(`registry.npmmirror.com`) + cargo 镜像(config 端)。
- 参考: `build_config/agent-platform-front/Dockerfile`(DOCKER_MIRROR + .npmrc + pnpm install + 多阶段)、`build_config/rcoder-common-base`(Rust 基础)。

### 3.7 sidecar 共存 + per-agent PVC 创建/删除(代码可行性,已调研)

**file-server sidecar 进 agent pod(可行,改动小)**:
- sidecar 经 `translate_k8s_sidecar`(`kubernetes_runtime.rs:546-569`)翻译,`K8sSidecarSpec`(`k8s_config.rs:131-148`)已支持 `volume_mounts` → 主要改动在 **yaml configmap 加 sidecar 块**(参考现有 log-collector),Rust 代码零改。
- **端口**: file-server 60000 与 agent 现有(50051/8086/6080/17681/5900)无冲突;dev server 池(4000-55000)需避让 50051(池范围挖 buffer 或分配时跳过)。
- **共享 per-agent PVC**: Pod 卷是 pod 级,sidecar `volume_mounts` 加 `name: workspace` 即挂同一 PVC;per-agent PVC(subPath=None)最干净(file-server + agent_runner 同视图)。
- sidecar 无 `ports` 字段但不影响(进程自 listen 60000,pod spec ports 非必需)。

**per-agent PVC 创建(可行,改动小)**:
- `create_container`(`kubernetes_runtime.rs:665-668`)删 `if !matches!` 跳过 + 删共享 PVC match 分支(`L706-726`),所有 service_type 走 `ensure_workspace_pvc`。
- `ensure_workspace_pvc`(`k8s_pvc.rs:91-253`)**完全可复用**(access mode/SC/size/labels + Terminating 等待 + 409 重试);labels 已对齐 cleanup_all selector(`managed-by=rcoder-runtime`)。

**per-agent PVC 删除(需新增)**:
- `delete_workspace_pvc` / `wait_for_pvc_removable`(`k8s_pvc.rs:42-43` trait 注释提到但**未实现**)需新增: 算 pvc_name + delete + 等 404 + finalizer 处理。
- 挂载点: `stop_container_by_identifier`(`L1326-1405`,现状保留 PVC,改删)+ `cleanup_all`(`L1526-1624`,现状跳过,改扫 label `delete_collection`)。**顺序**: 必须先等 Pod 真正消失(404),否则 PVC 被 `pvc-protection` finalizer 卡住。
- **CSI finalizer 两阶段**: 等 Pod 释放(pvc-protection 移除)→ 等 CSI subvolume 删除(MDS 端,可能慢,大目录尤甚);超时(60s)默认 **warn + 异步后台清理**(复用 `sync_states` stale 扫描),强制 patch finalizer 作 **opt-in 兜底**(env flag,防孤儿 subvolume)。

**关键依赖**: per-agent PVC 切换(阶段 3)前**必须先完成** WorkspaceService + 瘦聚合层(阶段 2),否则 rcoder 失去直读 agent 工作区(spec §8 风险)。

## 4. skills/hooks 分发逻辑(外部推送模型)

nuwax-file-server 现状(`AgentWorkspaceUtils.js` + `hookConfigUtils.js`):
- **主目录 `.claude`(primary)**: 建 `.claude/skills` + `.claude/agents`。
- **syncAgents**: 把 `.claude` 的 skills/agents 同步到 `.agents`/`.opencode`/`.codex`(多 agent 类型兼容,业务只写 `.claude` 一份)。
- **hooks**: vendored 资产 `assets/opencode-hooks-plugin` + `opencode-platform-env-plugin`(随镜像),配 Codex 生命周期事件(PreToolUse 等)。
- **skills 来源**: **外部系统(rcoder/平台)HTTP 上传文件或给 url**,file-server 接收(upload-file/files)或 fetch url,落到 agent 工作区 `.claude/skills`。

Rust 重写要点:
- 上传接口兼容(multipart `/upload-file` `/upload-files`)。
- url 拉取(Rust `reqwest` 下载)。
- `.claude` 目录 + syncAgents 同步逻辑(纯 fs,Rust 直接对齐)。
- hooks vendored 资产 → Rust 静态资源(随 file-server 镜像/二进制)。

## 5. 安全(file-server 下沉 agent pod 后关键)

每 agent pod 暴露 file-server HTTP,必须:
- **鉴权**: HTTP 接口 auth(内网 + API key / rcoder 签名转发),防外部直连 agent pod。
- **路径防穿越**: 创建目录/上传路径强制走 `resolve_project_cwd` + canonicalize(复用 agent_runner 现成防穿越)。
- **上传限制**: 大小/类型/频率(防滥用 + 防绕过配额前的大文件冲击)。

## 6. 阶段路径(增量、可回滚)

1. **阶段 1 file-server Rust 重写**: `crates/file-server` 完整重写(兼容接口),**进 agent pod**(此时 agent pod 仍挂共享 PVC subPath,配额还没——先验证 Rust 服务 + 下沉)。
2. **阶段 2 rcoder 解耦聚合视图**: WorkspaceService(agent_runner gRPC)+ app_manager RPC 化(你开发中,顺势做)+ 瘦聚合层(入口路由 + project_nginx/project_zips)。
3. **阶段 3 per-agent PVC**: agent 切独立 PVC(CSI 服务端配额 + 凭据隔离),退役 xattr 配额/caps Job。
4. **阶段 4 数据迁移 + cutover**: rsync 共享 PVC 子树 → per-agent PVC(复用 `scripts/data-migrate/sync.sh`),滚动迁移,reclaimPolicy Retain 兜底,退役共享 workspace PVC。

> 存储方案切换(共享 PVC subPath → per-agent PVC)在阶段 3;file-server 重写 + 下沉(阶段 1)先于存储切换,故"初期进 agent pod 但仍共享 PVC、后续切 per-agent PVC"。

## 7. 已决策(评审通过)
1. **file-server 集成方式**: 独立 **sidecar 容器**(同 pod,与 agent_runner 分进程)。
2. **PVC 粒度**: 按 `container_identifier`——共享容器 per `pod_id`(web tenant/space 共享)/ Computer per `user_id` / Web·UserApp per `project_id`,见 §3.1。
3. **git 实现**: **gix**(纯 Rust,GitButler 0.85 验证),`git2-rs` 备选;持续调研更优 Rust git 库。
4. **迁移策略**: 停服滚动。
5. **file-server 鉴权**: **(d) 不加应用层鉴权 + NetworkPolicy**(只放行 rcoder pod 访问 agent pod 的 file-server 端口);前端鉴权集中在 rcoder 入口。未来需要防集群内横向时再升级 internal token。

## 8. 风险
- file-server Rust 重写功能对齐(isomorphic-git 行为、dev server 进程管理、模板缓存 symlink 策略、skills/hooks syncAgents)——需对比测试。
- 阶段 1 file-server 下沉后 rcoder 失去聚合视图,阶段 2 WorkspaceService + 瘦聚合层须跟上,否则管理操作断档。
- 数据迁移双系统并存期一致性。
- **CephFS 配额 cooperative/imprecise 风险**(官方定性,见 §1.4): ① imprecise——写入会短暂超配额(几十秒),阶段 3 上线前需压测单 agent 持续写入的 EDQUOT 触发阈值与超限量;② cooperative——对抗性 client 理论可绕过,靠 per-subvolume path-restricted 凭据隔离兜底(agent 只能 rw 自己 subvolume)。

## 9. 调研依据
- **Ceph 官方文档**(印证 cooperative/imprecise + path-based caps):
  - [CephFS Quotas · Limitations](https://docs.ceph.com/en/latest/cephfs/quota/) —— cooperative + imprecise + path-based mount restrictions。
  - [CephFS FS Volumes / Subvolumes](https://docs.ceph.com/en/latest/cephfs/fs-volumes/) —— subvolume `--size` = 配额、mgr volumes 模块、CSI/manila 共用此接口。
- 代码佐证: `kubernetes_runtime.rs:740` 注释"后续配额方案: 改用 ceph fs subvolume resize"——当初代码作者已指向 subvolume 方向。
- 社区: CNCF SIG-Storage / ceph-csi / Rook(per-PVC subvolume 主流)。
- 代码: rcoder create_container/ensure_workspace_pvc、agent_runner gRPC、nuwax-file-server 全量(含 AgentWorkspaceUtils/hookConfigUtils)、build-agent-docker chart。
- 历史: BACKLOG 待办 2(xattr 事故)、待办 5(路径拼接)。
