# Plan: per-agent 存储配额 + file-server Rust 重写

> Spec: `specs/per-agent-storage-quota/per-agent-storage-quota-spec.md`（所有可行性已调研坐实）
> 本 plan 是 spec §5 三阶段的落地细化（3 阶段 × 14 task）

## Context
spec 已确认方向（per-agent CephFS subvolume 配额 + rcoder 静态 PV 挂根聚合 + file-server Rust 重写留 rcoder）+ 所有可行性坐实（CSI 配额生产实证、rcoder 挂根 ceph-csi 静态 PV、subvolume 映射读 PV、配额经 PVC/CSI、gix 对齐、dev server 新旧等价、数据迁移全停迁）。本 plan 把 spec 的 3 阶段拆成可执行 task。

## 关键勘探结论（plan 落地依据）
- `KubernetesRuntime`(`kubernetes_runtime.rs:155-161`): 已有 `client/pvcs()/pod_cache(Arc<RwLock<HashMap>>)`；**缺 `pvs()` + `subvolume_path_cache`**
- `K8sPvcOps` trait(`k8s_pvc.rs:46-74`): 有 `workspace_pvc_name/ensure_workspace_pvc`；`wait_for_pvc_bound`(L255-281 dead code 可启用)；**缺 `resolve_subvolume_path/resize_workspace_pvc/delete_workspace_pvc/wait_for_pvc_removable`**(L42-43 trait 注释提到未实现)
- `create_container`(L665-726): L665 Web/Computer 跳过 + L706-726 共享 PVC match，需删；L728-768 xattr 块已注释(阶段 2 物理删)
- `stop_container_by_identifier`(L1326-1405): L1392 保留 PVC → 改删；`cleanup_all`(L1526-1624): L1605 跳过 PVC → 改 sweep
- UserApp(`k8s_deployment.rs`): 5 处——L92-95 `app_workspace_pvc_name`/L339-353 volume/L267-290 storage/L644-712 create/L750-799 delete
- `app_manager/service.rs`: `get_container_app_dir`(L1235 被 9 处调)静态拼接→运行时解析；`list_apps`(L441 read_dir)→list Deployment by label
- RBAC(`rbac.yaml`): 有 pvc verbs，**缺 `persistentvolumes get/list/watch`**
- `ContainerRuntime` trait(`runtime_trait.rs`): 无 PV 方法，**需加 `resolve_workspace_path`**
- Cargo workspace `members=["crates/*"]`(自动纳入 file-server)；**workspace 缺 `gix` dep**

## 阶段 1: file-server Rust 重写（留 rcoder，配额未上）

### Task 1.1 — file-server crate 骨架 + WorkspaceResolver
- 新建 `crates/file-server/`（Cargo.toml + src/{main,lib,workspace}.rs）
- `WorkspaceResolver` trait: `fn resolve(id) -> PathBuf`。阶段 1 唯一实现 `LocalWorkspaceResolver`（读 env，等价 nuwax 现状）
- axum app（`#[tokio::main]`, FILE_SERVER_PORT=60000, /health）
- workspace `Cargo.toml` 加 `gix`(0.85, GitButler 验证)
- **验收**: `cargo build -p file-server`；起服务 /health 200
- **依赖**: 无

### Task 1.2 — project/code 路由（tree + CRUD + zip + multipart upload）
- `/api/project/*` + `/api/code/*`（对齐 projectRoutes.js + codeRoutes.js）：get-file-list/files-update/upload-file[-batch/-single/-project/-attachment]/download-all/zip-workspace/create[-delete/-copy/-import/-export]-project/backup[-rollback]-version/create[-delete]-workspace/init-template/install/execute-command/push-skills[-v2](.claude/skills + syncAgents)
- 路径防穿越: canonicalize + 限定 workspace 根（spec §4）
- **改文件**: 新建 `crates/file-server/src/routes/{project,code}.rs` + `src/service/{project,upload,zip,skills}.rs` + `src/handlers.rs`
- **验收**: 对照 nuwax 路由表逐 path curl；zip/upload round-trip
- **风险**: multipart 大文件流式（防 OOM）；nuwax 双重 URL 解码（safeDecodePath）需对齐

### Task 1.3 — git 路由（gix 薄封装）⚠️ 阶段 1 最高风险
- `/api/git/*`（对齐 gitService.js）：status/add/commit/unstage/discard/log/diff/file-content/reset/revert/checkout/branches[-create/-delete/-switch]/tags[-create/-delete]
- gix 3 薄层: `git add`(Repository::index_mut().stage(), ~15 行)、`checkout 切分支`(edit ref + worktree-state, ~20 行)、`reset`(ref+index+checkout, ~30 行)
- statusMatrix → `Repository::status()` Item 流；diff → `diff_tree_to_tree`（替 nuwax ~200 行手搓 patch）
- **改文件**: 新建 `crates/file-server/src/routes/git.rs` + `src/service/git.rs`
- **验收**: 临时 git repo 逐 path 对比 nuwax 输出（status/diff/log 字段对齐）；分支/tag CRUD round-trip
- **风险**: gix corner case——限定 nuwax 实际用的非 bare 场景；预留对比测试时间

### Task 1.4 — build + dev server + computer 路由
- `/api/build/*` + `/api/computer/*`: start[-stop/-restart]-dev/list-dev/keep-alive/build/get-dev-log/parse-build-error/port-pool-status/build-agent-package/install-project + computer CRUD
- dev server 进程池: `tokio::process::Child` + 端口池(4000-55000) + log pipe（替 pm2/cross-spawn/tree-kill）
- 模板缓存: 读 TEMPLATE_CACHE_DIR，warmup
- **改文件**: 新建 `crates/file-server/src/routes/{build,computer}.rs` + `src/service/{dev_server,build,port_pool,template_cache}.rs`
- **验收**: start-dev 起 vite，经 rcoder-proxy /proxy/{port} 可访问；stop-dev 无僵尸；HMR WS 透传
- **风险**: dev server 进程生命周期（SIGTERM/进程组）；vite HMR WS 经 port_proxy.rs（已确认长连接 OK）

### Task 1.5 — file-server 镜像 + chart 集成 + cutover
- file-server 二进制打进 rcoder 镜像（改 `build_config/rcoder/Dockerfile` 加 file-server 编译阶段，仿 Dockerfile.rust + DOCKER_MIRROR）
- `start-services.sh` 改启 Rust file-server 替 node cli.js
- env 兼容（FILE_SERVER_PORT/DEPLOYMENT_MODE/DIST_TARGET_DIR/UPLOAD_PROJECT_DIR/TEMPLATE_CACHE_DIR，chart 已有）
- **回滚开关**: 保留 node file-server 入口，env 切换
- **改文件**: `build_config/rcoder/Dockerfile` + `start-services.sh`
- **验收**: rcoder pod 三进程(nginx+rcoder+Rust file-server)；前端全功能回归(tree/git/build/dev/skills/upload)，对比 node 版无退化
- **风险**: rcoder 镜像编译时间增长；功能对齐充分回归

**阶段 1 整体验收**: Rust file-server 替换 node，rcoder 仍挂共享 PVC subPath，前端全功能回归，无配额改动，可独立上线。

## 阶段 2: per-agent PVC + rcoder 挂根聚合 + 配额管理

### Task 2.1 — RBAC + PV 读取 + ContainerRuntime trait 扩展
- RBAC 加 `persistentvolumes get/list/watch`（rbac.yaml L11 后）
- `ContainerRuntime` trait 加 `resolve_workspace_path(identifier, service_type) -> Option<PathBuf>`（默认 None；KubernetesRuntime impl 读 PVC→PV subvolumePath；Docker impl 返回本地路径）
- `KubernetesRuntime` 加 `pvs()`（`Api::<PersistentVolume>::all(client)`, cluster-scoped）
- **改文件**: `k8s/config/rbac.yaml` + `container-runtime-api/src/runtime_trait.rs` + `kubernetes_runtime.rs`(L259 旁加 pvs)
- **验收**: workspace 编译；RBAC apply 后 rcoder SA 能 `kubectl get pv`；单测 mock PVC/PV
- **依赖**: 可与阶段 1 并行

### Task 2.2 — k8s_pvc.rs 扩展（resolve/resize/delete + 缓存）
- `resolve_subvolume_path(id, st)`: PVC volumeName → PV subvolumePath → `/volumes/csi/...`；缓存 `Arc<RwLock<HashMap>>`（照搬 pod_cache，KubernetesRuntime 加 subvolume_path_cache 字段）
- `resize_workspace_pvc(id, st, size)`: patch PVC requests.storage（只扩，CSI auto-resize）
- `delete_workspace_pvc(id, st)` + `wait_for_pvc_removable`: 两阶段（Pod 404→删 PVC→等 CSI finalizer，超时 60s warn + 异步清，opt-in 强 patch 兜底）
- 启用 `wait_for_pvc_bound`（L255-281）
- **改文件**: `k8s_pvc.rs`(trait L46-74 + impl L78-282) + `kubernetes_runtime.rs`(L155-161 加 cache 字段)
- **验收**: 单测 mock PVC/PV；集成(kind+cephfs SC) create→resolve→resize→delete 全链路
- **风险**: CSI finalizer 慢（MDS 端）——超时+后台清兜底

### Task 2.3 — create_container 切 per-agent PVC + WorkspaceResolver 切 subvolume
- 删 create_container Web/Computer 跳过(L665) + 共享 PVC match(L706-726)，所有 service_type 走 ensure_workspace_pvc + wait_for_pvc_bound
- 物理删 xattr 注释块(L728-768)
- WorkspaceResolver 阶段 2 实现 `SubvolumeWorkspaceResolver`: 经 runtime.resolve_workspace_path() 拿 subvolumePath → `/app/cephfs-root/{subvolumePath}`
- **改文件**: `kubernetes_runtime.rs`(L665-768) + `crates/file-server/src/workspace.rs`(SubvolumeWorkspaceResolver) + rcoder 启动注入
- **验收**: 三类容器产 per-agent PVC；subvolumePath resolve；file-server tree 经根读到 agent 数据；EDQUOT 压测
- **风险**: 共享容器(pod_id)多项目复用——per pod_id PVC 保证语义；identifier 映射不能错

### Task 2.4 — stop_container + cleanup_all 删 PVC
- stop(L1392 保留→改删，Pod 404 后调 delete_workspace_pvc)
- cleanup_all(L1605 skip→改 sweep，Pod 终止后 PVC delete_collection label selector)
- **顺序铁律**: Pod 真消失才删 PVC（pvc-protection finalizer）
- **改文件**: `kubernetes_runtime.rs`(L1392 + L1605)
- **验收**: stop 单容器 PVC 消失；cleanup_all 清空 managed PVC；Pod 未终止不误删
- **风险**: CSI finalizer 慢（2.2 兜底）；多副本 rcoder 误删（现状单副本 OK，future leader election）

### Task 2.5 — rcoder 静态 PV 挂 CephFS 根（chart）
- Secret `cephfs-root-secret`(userID/userKey, 专用 `client.rcoder-aggregator` restricted caps)
- 静态 PV(rootPath:/, staticVolume:true, fsName:myfs, clusterID:rook-ceph)
- PVC(storageClassName:"", volumeName:静态PV)
- rcoder deployment 加 volumeMount /app/cephfs-root + env RCODER_CEPHFS_ROOT
- **改文件**: 新建 `templates/rcoder/cephfs-root-{secret,pv,pvc}.yaml` + `deployment.yaml`(L226+L291+L121) + values
- **验收**: rcoder pod `ls /app/cephfs-root/volumes/csi/` 见全部 subvolume；kernel mount（无 fuse/privileged）；restricted caps 验证
- **风险**: cephx caps 配错——先 client.admin 验证链路再切 restricted

### Task 2.6 — UserApp per-app PVC + app_manager 改造
- k8s_deployment.rs 5 处: ① app_workspace_pvc_name(L92-95)→per-app ② volume/mount(L339-353) subPath None ③ create_app_resources(L644)加 ensure_app_workspace_pvc ④ storage_size(L267-290)双投 ephemeral+PVC ⑤ delete_app_resources(L750)加删 PVC
- app_manager service.rs: `get_container_app_dir`(L1235 静态→运行时 subvolumePath 解析，9 处调方跟随) + `list_apps`(L441 read_dir→list Deployment by label) + `create_app_dirs`(L1252 跟随)
- **改文件**: `k8s_deployment.rs`(L92/L229/L339/L644/L750) + `app_manager/service.rs`(L1235/L441/L1252)
- **验收**: create UserApp 产 per-app PVC；tree/git/upload 经根读写；list_apps 列 Deployment；delete 清 PVC
- **风险**: app_manager 9 处 get_container_app_dir 行为变——逐处回归；list_apps label selector 对齐 build_app_labels

### Task 2.7 — dist/project_nginx 挪根下 + 退役 xattr/caps
- project_nginx/project_zips: 现共享 PVC subPath → `/app/cephfs-root/project_nginx`（普通目录，零额外 PVC）
- file-server DIST_TARGET_DIR/UPLOAD_PROJECT_DIR env 指向根下；nginx root 路径同步改
- 退役 fix-cephfs-caps-job.yaml（xattr 路已死）
- **改文件**: `deployment.yaml`(删 project_nginx/zips subPath 挂载) + `fix-cephfs-caps-job.yaml`(删除/禁用) + nginx configmap + values
- **验收**: build dist 落根下；前端预览正常；caps Job 不部署
- **风险**: project_nginx 现有数据迁移（阶段 3）；nginx root 路径同步

**阶段 2 整体验收**: agent/UserApp 全切 per-agent subvolume PVC（配额 CSI 服务端设）；rcoder 挂根聚合(tree/git/skills/build/dev 不启动 agent pod)；xattr/caps 退役；共享 subPath PVC 准备退役。

## 阶段 3: 数据迁移 + cutover

### Task 3.1 — 迁移工具（rsync helper pod 模式 A + Job 并行）
- 模式 A(1 agent 1 helper pod): 挂源共享 PVC(subPath) + 目标 per-agent PVC(根), rsync -aH --delete
- Job 并行（单 pod 挂全部 PVC 不可行 kubelet 限制）；预创建目标 PVC
- 复用 scripts/data-migrate/sync.sh 模式（sync_pair 模板 + 退出码 + progress2）
- **改文件**: 新建 `scripts/data-migrate/migrate-per-agent.{sh,yaml}`
- **验收**: 测试环境 1 项目迁移 diff 为空；多 agent 并行无干扰
- **风险**: 双系统并存一致性——cutover 全停迁

### Task 3.2 — cutover（全停迁 + 回滚开关）
- 全停 rcoder → 跑迁移 Job(rsync 全量) → 切 SubvolumeWorkspaceResolver → 起 rcoder
- 共享 PVC reclaimPolicy Retain（不立即删，观察期回滚）
- 回滚开关: env 切 resolver 回 Local(共享 subPath)，数据在 Retain PVC 可恢复
- **改文件**: `values-rcoder-k8s-{prod,test}.yaml`(Retain + resolver 开关) + rcoder 启动 env 分支
- **验收**: 生产 cutover 后 agent 数据在 per-agent PVC + 配额生效；回滚演练(切 env→数据回共享 PVC)
- **风险**: 停服窗口；迁移耗时——预创 PVC + 并行 + rsync 增量缩短

**阶段 3 整体验收**: 数据全量迁至 per-agent subvolume；共享 PVC Retain 兜底；回滚开关验证。

## 依赖图
```
阶段1: 1.1 → 1.2 ┐
            1.3 ─┤ → 1.5 (镜像/chart/cutover, 阶段1独立上线)
            1.4 ┘
              ↓
阶段2: 2.1 → 2.2 → 2.3 → 2.4 (agent 路径)
                  → 2.5 (chart 静态PV) → 2.6 (UserApp, 依赖2.3+2.5) → 2.7 (dist挪根, 依赖2.5)
              ↓
阶段3: 3.1 (依赖阶段2) → 3.2 (cutover)
```
阶段 1 与 2.1/2.2 可部分并行（trait/RBAC/PVC ops 不依赖 file-server），但 cutover 严格串行。

## 跨阶段风险汇总
1. **file-server Rust 对齐**（阶段 1）: gix diff/status、dev server 进程、模板缓存 symlink、skills syncAgents——逐 path 对比 + 回滚开关保留 node 入口
2. **CephFS 配额 cooperative/imprecise**（阶段 2）: 写入短暂超配额（几十秒）——上线前 EDQUOT 压测；per-subvolume 凭据隔离兜底（future，当前未实现 per-agent caps）
3. **迁移双系统并存一致性**（阶段 3）: 全停迁 + Retain 回滚
4. **rcoder 挂根 cephx 安全**（阶段 2）: restricted user 定期轮转
5. **app_manager 9 处 get_container_app_dir 行为变更**（阶段 2.6）: 逐调用方回归
6. **多副本 rcoder cleanup_all 误删 PVC**（阶段 2.4）: 现状单副本 OK，多副本需 leader election（future）

## 验证（每阶段）
- **阶段 1**: `cargo build/test/clippy -p file-server`；前端全功能回归（对比 node 版）；本地 devspace 起服务测 tree/git/build/dev/upload
- **阶段 2**: `cargo check/clippy --features kubernetes --workspace`；本地 devspace + kind 测 per-agent PVC 创建/resolve/resize/delete；rcoder 挂根聚合；EDQUOT 压测
- **阶段 3**: 测试环境数据迁移 diff；cutover + 回滚演练
- **生产验证**（部署后）: `ceph fs subvolume info` 确认配额；rcoder `ls /app/cephfs-root/volumes/csi/` 聚合；tree/git/skills 不启动 agent pod 可服务
