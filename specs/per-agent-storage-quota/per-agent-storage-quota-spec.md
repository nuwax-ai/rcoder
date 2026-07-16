# Spec: per-agent 存储配额 + file-server Rust 重写

> 状态: 调研完成 / 方向已定 / 待评审
> 仓库: rcoder(Rust) + nuwax-file-server(Node,待 Rust 重写) + build-agent-docker(chart)
> 关联: BACKLOG 待办 2(CephFS 配额)、待办 5(路径拼接 PathBuf)

## 1. 背景

### 1.1 业务诉求
rcoder 为每用户/项目动态创建 agent 容器(computer 沙箱 / web agent / UserApp),工作区数据落 CephFS。核心诉求:
- **限制每个动态创建的 agent 容器的磁盘上限**(防单容器写爆共享卷)。**rcoder 主服务容器不限磁盘**(它挂根聚合管理)。
- **RWX**: agent 工作区数据,多 agent 共享同一 CephFS filesystem(但 per-agent subvolume 隔离)。
- **agent 工作区管理 + 不启动 agent 也提供服务**: tree/git/skills/static/build/dev 全在 rcoder 侧,agent pod 没启动也能浏览项目结构、查 git、看 skills。

### 1.2 现状
- 两个共享 CephFS RWX PVC: `rcoder-workspace`(web agent + UserApp + project_zips + project_nginx)、`rcoder-computer-workspace`(computer 沙箱)。rcoder 主 pod **挂共享 PVC subPath**(subvolume-backed,实证 `subvolumePath=/volumes/csi/...`)。
- **nuwax-file-server 是 rcoder pod 内 Node 进程**,与 rcoder 共享挂载,提供 build/git/static/CRUD/upload/skills-hooks/dev server——**不启动 agent pod 也能服务**(数据在共享 PVC,rcoder 直读)。
- 三类容器(Web/Computer/UserApp)全复用共享 PVC + subPath,**无 per-agent 配额**。

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

### 1.4 CephFS 配额官方定性(社区最佳实践依据)

**CephFS 配额(无论目录还是 subvolume,底层同一 `ceph.quota.max_bytes` xattr)是 cooperative + imprecise**([Ceph 官方 Limitations](https://docs.ceph.com/en/latest/cephfs/quota/)):
- **cooperative**: 依赖挂载 client 合作停写;对抗性 client 防不住。
- **imprecise**: 写入短暂超配额(几十秒内才停)。
- 即**不存在"目录配额 soft vs subvolume hard"的对立**。

社区主流 = **per-PVC(CephFS subvolume)+ CSI `requests.storage`**,优势**不在配额精度**(都 cooperative),而在:
1. 配额由 **ceph-csi 持 admin 凭据在服务端**经 mgr volumes 模块设(`ceph fs subvolume create --size`),**绕开 client 侧 setfattr**(根除 §1.3 的 client 漂移);
2. 服务端可查(subvolume info 可见,解决 getfattr 不可观测);
3. `subvolume resize` 服务端执行(可改/扩容);
4. **per-subvolume path-restricted cephx 凭据**提供真正的租户隔离。

> **CephFS subvolume 设计 = "independent CephFS directory trees"**([fs-volumes](https://docs.ceph.com/en/latest/cephfs/fs-volumes/)),每 subvolume 独立配额 + `authorize` 凭据隔离 + admin 可挂根聚合访问所有 subvolume。本方案正是用这个原生多租户模式。

## 2. 目标与非目标

### 目标
1. **per-agent 配额**(CephFS subvolume,CSI 服务端设/查/改,绕开 client setfattr;CephFS 配额本身 cooperative/imprecise,见 §1.4)。**只限动态创建的 agent pod,rcoder 主服务不限**。
2. **保留"不启动 agent 也提供服务"**: tree/git/skills/static/build/dev 留 rcoder(挂根聚合读 agent subvolume),agent pod 没启动也能浏览。
3. **file-server 用 Rust 完整重写**(统一技术栈),**留 rcoder 部署**(不下沉 agent pod)。
4. 无缝迁移(共享 PVC subPath → per-agent subvolume,可回滚)。

### 非目标
- CephFS 本身配置(SubVolumeGroup `csi` 已就绪,实证)。
- Docker compose 模式(只 K8s)。
- ceph-fuse 目录配额(性能差 + 仍 cooperative,不采用)。

## 3. 方案(核心:per-agent subvolume + rcoder 挂根聚合)

### 3.1 架构总览(数据隔离面 + 管理聚合面)

```
数据隔离面(agent pod,动态创建,限磁盘):
  每 agent 一个 subvolume → CSI 映射为 PVC(agent 挂自己 PVC = 自己 subvolume,kernel mount 性能 OK)
  配额: subvolume --size(CSI 服务端设,绕开 client setfattr → 根治 EDQUOT 事故)
  隔离: per-subvolume cephx 凭据(agent 只能 rw 自己 subvolume path)

管理聚合面(rcoder pod,不限磁盘):
  静态 PV 挂 CephFS 根(rootPath=/,cephx admin/restricted key)→ 访问所有 subvolume(/volumes/csi/.../*)
  → 提供 tree/git/skills/static/build/dev(不启动 agent pod,直接读 subvolume 数据)
  → 配额管理: 经 PVC → CSI(查读 PVC requests.storage,改 patch pvc 触发 CSI resize)
  → rcoder 永远不直接碰 Ceph admin CLI/凭据(配额层面),一切经 PVC
```

**关键**: 配额隔离(agent subvolume)和聚合访问(rcoder 挂根)**不矛盾**——它们分属不同角色。这正是 CephFS subvolume 的原生多租户模式(官方 fs-volumes)。

### 3.2 数据面:per-agent subvolume PVC

每**容器**一个独立 PVC(`cephfs` SC,SubVolumeGroup `csi` 已存在),`requests.storage = storage_size` → CSI 持 admin 凭据在**服务端**设 subvolume 配额(`ceph fs subvolume create --size`,绕开 client setfattr)。agent pod 挂自己 PVC(subPath=None,subvolume 已是天然边界)。

> 注: CephFS 配额本身 cooperative/imprecise(§1.4)。可靠性来自 **服务端设/查/改**(根除 client 漂移 + 可观测)+ **per-subvolume 凭据隔离**(真隔离)。

**生产实证(2026-07,方案地基成立)**: 现有 4 个 cephfs PVC **全部 subvolume-backed**(PV `subvolumeName`);`ceph fs subvolume info` 实证 `bytes_quota` 精确匹配 `requests.storage`;**computer-workspace 已用 47.5% 证明配额真实约束写入**。ceph-csi v3.17.0 `subvolumeEnabled` 已废弃(subvolume 唯一模式,SC 无需改);SubVolumeGroup `csi` Ready;Ceph v20.2.1 + kernel 6.17。

**PVC 粒度 = 容器标识(`ServiceType::container_identifier`)**:
- **共享容器场景**(`pod_id` 存在,如 web tenant/space 共享): per `pod_id`。
- **ComputerAgentRunner**: per `user_id`。
- **WebAgentRunner / UserApp**: per `project_id`。

### 3.3 管理聚合面:rcoder 挂 CephFS 根(ceph-csi 静态 PV)

rcoder pod 用 **ceph-csi 静态 PV** 挂整个 CephFS 根(`rootPath: /`,替代现有共享 subvolume PVC 的 subPath 聚合):

- **ceph-csi v3.17 静态 PV 实证**(文档原文 + 生产): `rootPath` 可为 volume folder path,`/` = 整个 FS 根;**必须 `staticVolume: "true"` + `userID`/`userKey`(secret,不能用 adminID/adminKey)**;走现有 nodeplugin(kernel mount on node,**不动 rcoder 镜像、不需 privileged、不需 ceph-fuse、不需 hostPath**)。
- **专用 cephx**(推荐,最小权限): 新建 `client.rcoder-aggregator`,caps `mds allow rw path=/` + `mgr allow rw` + `osd allow rw tag cephfs data=*` + `mon allow r`;最简复用生产 `client.admin`(caps `allow *`,能工作但过权)。
- **落地 YAML**(chart 新增): Secret(`cephfs-root-secret`, userID/userKey)+ 静态 PV(`rootPath: /`, `staticVolume: true`, `fsName: myfs`, `clusterID: rook-ceph`, nodeStageSecretRef)+ PVC(`storageClassName: ""`, `volumeName: 静态PV`)+ rcoder pod volumeMount(`/app/cephfs-root`, 无 subPath)。rcoder 读 `/app/cephfs-root/volumes/csi/<agent-subvol>/<uuid>/...` 即聚合视图。
- **生产参数(实证)**: fsName=`myfs`, clusterID=`rook-ceph`, mon=`10.43.81.81:6789,10.43.206.69:6789,10.43.227.237:6789`;nodeplugin 在 rcoder 所在 node 运行(可 stage);`ceph fs subvolume ls myfs csi` 实证根下可见全部 subvolume。
- 限制: 静态 PVC 不支持 CSI resize/snapshot(无关,根无配额)。
- 参考: [ceph-csi 静态 PV 文档](https://github.com/ceph/ceph-csi/blob/devel/docs/static-pvc.md)。

### 3.4 subvolume 路径映射 + 配额管理(rcoder 经 PVC/CSI)

**subvolume 路径映射(projectId → subvolume 路径,方式 a)**:
- rcoder 确定式知道 PVC 名(`workspace_pvc_name`,`k8s_pvc.rs:79-89`)。
- 读 PVC `spec.volumeName`(Bound 后)→ 读 PV `csi.volumeAttributes.subvolumePath`(=`/volumes/csi/<uuid>/<random>`)→ rcoder 挂根后按此路径访问 agent 数据。
- 改动: `rbac.yaml` 加 `persistentvolumes get/list/watch`;`k8s_pvc.rs` 加 `Api::<PersistentVolume>::all(client)`(cluster-scoped)+ `resolve_subvolume_path()`;启用已存在的 `wait_for_pvc_bound`(现为 dead code)。

**配额管理(经 PVC → CSI,rcoder 不碰 Ceph admin)**:
- **查上限**: 读 PVC `spec.resources.requests.storage`(免调 mgr;`bytes_quota` 精确匹配 requests.storage,实证)。
- **改**: `patch pvc`(`spec.resources.requests.storage`)→ CSI external-resizer 自动 `ceph fs subvolume resize`(生产 SC `allowVolumeExpansion=true`,RBAC 已有 patch)。只能扩不能缩。
- **查用量**(可选,监控用): 走 Prometheus ceph_mgr 监控指标(非 rcoder 逐 agent 调 mgr),避免给 rcoder 引入 ceph admin 凭据。

**rcoder 与 CSI 分工边界**:
| 职责 | 归属 |
|---|---|
| PVC create/delete/patch | rcoder(K8s API,只碰 PVC 对象) |
| subvolume lifecycle(create/delete/resize) | ceph-csi provisioner(持 admin 凭据) |
| 配额设置 | CSI(服务端,provision/expand 时) |
| 配额上限查询 | rcoder 读 PVC |
| subvolumePath 定位 | rcoder 读 PV volumeAttributes |
| 聚合访问(tree/git/skills/build/dev) | rcoder 挂根,按 subvolumePath 进子目录 |
| per-subvolume 凭据隔离 | CSI provisioner 生成 path-restricted cephx |

**关键原则: rcoder 配额层面永远不直接碰 Ceph admin / subvolume CLI,一切经 PVC → CSI 翻译。** rcoder 镜像无需装 ceph-common(配额层面);挂根聚合只需 cephx 读凭据(静态 PV)。

### 3.5 file-server Rust 完整重写(留 rcoder 部署,不下沉)

- **完整功能**(对齐 nuwax-file-server 64 文件): HTTP 接口、目录管理(tree)、上传、build、dev server、git、static、CRUD、skills/hooks 分发、zip、模板缓存。
- **留 rcoder pod**(现状位置不变,与 rcoder 同容器/同 pod): 挂根聚合读 agent subvolume,提供 tree/git/skills/static/build/dev——**不启动 agent pod 也能服务**(核心业务保留)。
- **dev server 在 rcoder pod**(file-server spawn,读 agent subvolume 数据,经 rcoder 挂根): 前端直访 rcoder(dev server 端口池在 rcoder,现状不变),**不需要 dev server 反向代理**(之前 spec 的 §3.5① 作废)。
- **兼容现有 HTTP 接口**(rcoder/前端调用方零改动)。
- Node 依赖 Rust 替代: express→axum、fs-extra→tokio::fs、isomorphic-git→**gix**、archiver/yauzl→zip、pm2/cross-spawn/tree-kill→tokio::process、multer→axum multipart、node-cron→tokio::time、iconv-lite→encoding_rs。
- **git 选型(已评估,可行)**: 选 **gix**(纯 Rust)。调研确认 nuwax 用的全部 isomorphic-git API 在 gix 有对等或更强实现,无功能缺口:
  - `statusMatrix` → gix `Repository::status()` Platform→Item 流(强类型枚举,比数字编码清晰)
  - `diff`(nuwax 现 ~200 行手搓 patch)→ gix `diff_tree_to_tree` 原生(大幅简化)
  - 需封装 3 处薄层(gix 无高层): `git add`(~15 行)、`checkout 切分支`(~20 行)、`reset`(~30 行)
  - 备选 `git2-rs`(libgit2): 高层 API 更全但 C 依赖。不需混合,gix 单独覆盖。

### 3.6 skills/hooks 分发逻辑(外部推送模型,留 rcoder)

nuwax-file-server 现状(`AgentWorkspaceUtils.js` + `hookConfigUtils.js`):
- 主目录 `.claude`(primary): 建 `.claude/skills` + `.claude/agents`;`syncAgents` 同步到 `.agents`/`.opencode`/`.codex`(多 agent 类型兼容)。
- hooks: vendored 资产 `opencode-hooks-plugin` + `opencode-platform-env-plugin`(随镜像),配 Codex 生命周期事件。
- skills 来源: **外部系统(rcoder/平台)HTTP 上传文件或 url**,file-server 接收(upload-file/files)或 fetch url,落到 agent 工作区 `.claude/skills`。

Rust 重写要点(留 rcoder,挂根聚合写 agent subvolume): 上传接口兼容(multipart)、url 拉取(reqwest)、`.claude` 目录 + syncAgents(纯 fs)、hooks vendored 资产→Rust 静态资源。

### 3.7 per-agent PVC 创建/删除(rcoder 代码改动)

**创建(可行,改动小)**:
- `create_container`(`kubernetes_runtime.rs:665-668`)删 `if !matches!` 跳过 + 删共享 PVC match 分支(`L706-726`),所有 service_type 走 `ensure_workspace_pvc`。
- `ensure_workspace_pvc`(`k8s_pvc.rs:91-253`)**完全可复用**(access mode/SC/size/labels + Terminating 等待 + 409 重试);labels 已对齐 cleanup_all selector。

**删除(需新增)**:
- `delete_workspace_pvc` / `wait_for_pvc_removable`(`k8s_pvc.rs:42-43` trait 注释提到但**未实现**)需新增。
- 挂载点: `stop_container_by_identifier`(`L1326-1405`,现状保留 PVC,改删)+ `cleanup_all`(`L1526-1624`,现状跳过,改扫 label `delete_collection`)。**顺序**: 必须先等 Pod 真正消失(404),否则 PVC 被 `pvc-protection` finalizer 卡住。
- **CSI finalizer 两阶段**: 等 Pod 释放(pvc-protection 移除)→ 等 CSI subvolume 删除(MDS 端,可能慢);超时(60s)默认 warn + 异步后台清理,强制 patch finalizer 作 opt-in 兜底(防孤儿 subvolume)。

**rcoder 改动汇总**: rbac.yaml 加 PV 读 + k8s_pvc.rs 加 `resolve_subvolume_path`/`resize_workspace_pvc`/`delete_workspace_pvc`/`wait_for_pvc_removable` + create_container 去 Web/Computer 跳过 + stop/cleanup 改删 PVC。

## 4. 安全

- **rcoder 挂根凭据**: 静态 PV 用专用 restricted cephx user(收敛权限,非 admin),只读 agent subvolume(`/volumes`)。
- **路径防穿越**: rcoder file-server(tree/git/skills/static)按 subvolumePath 访问,强制 canonicalize + 限定 `/volumes/csi/` 下(防逃逸)。
- **agent pod 隔离**: per-subvolume path-restricted cephx 凭据(agent 只能 rw 自己 subvolume)是**目标**,但 ceph-csi 现用单一共享 `csi-cephfs-node` client 挂所有 agent PVC(**当前未实现 per-agent 受限 caps**)。真隔离需额外 cephx 配置(future enhancement,与本任务正交);当前 agent 间靠 subvolume 路径边界 + 配额(subvolume --size)兜底。

## 5. 阶段路径(增量、可回滚)

1. **阶段 1 file-server Rust 重写**: `crates/file-server` 完整重写(兼容接口),**留 rcoder 部署**(此时 rcoder 仍挂共享 PVC subPath,配额还没——先验证 Rust 服务)。
2. **阶段 2 per-agent PVC + rcoder 挂根**: agent 切 per-agent subvolume PVC(CSI 配额);rcoder 改静态 PV 挂 CephFS 根(聚合);加 subvolume 映射 + 配额管理;退役 xattr 配额/caps Job + 共享 subPath PVC。
3. **阶段 3 数据迁移 + cutover**: rsync 共享 PVC subPath 子树 → per-agent subvolume PVC(复用 `scripts/data-migrate/sync.sh`),停服滚动,reclaimPolicy Retain 兜底。

> 比之前"file-server 下沉"方案少 2 个大阶段(无 dev server 反向代理、无 WorkspaceService RPC),改动聚焦存储模型 + 配额 + Rust 重写。

## 6. 已决策(评审通过)
1. **架构方向**: per-agent subvolume(agent 配额)+ rcoder 静态 PV 挂 CephFS 根聚合(不限磁盘,提供 tree/git/skills 不启动 pod)。
2. **file-server**: Rust 完整重写,**留 rcoder 部署**(不下沉 agent pod);dev server 也在 rcoder(现状不变,不需反向代理)。
3. **PVC 粒度**: 按 `container_identifier`(共享容器 per pod_id / Computer per user_id / Web·UserApp per project_id)。
4. **git 实现**: **gix**(纯 Rust,GitButler 验证),git2-rs 备选。
5. **配额管理**: 经 PVC → CSI(查读 PVC,改 patch pvc);rcoder 不碰 Ceph admin(配额层面)。
6. **rcoder 挂根凭据**: ceph-csi 静态 PV(rootPath=/),建议专用 restricted cephx user。
7. **迁移策略**: 停服滚动。

## 7. 风险
- file-server Rust 重写功能对齐(isomorphic-git→gix、dev server 进程管理、模板缓存 symlink、skills/hooks syncAgents)——需对比测试。
- CephFS 配额 cooperative/imprecise: 写入短暂超配额(几十秒),阶段 2 上线前压测 EDQUOT 触发阈值;对抗性 client 靠 per-subvolume 凭据隔离兜底。
- 数据迁移双系统并存期一致性。
- rcoder 挂根的 cephx 凭据安全(restricted user,定期轮转)。

## 8. 调研依据
- **Ceph 官方文档**: [CephFS Quotas · Limitations](https://docs.ceph.com/en/latest/cephfs/quota/)(cooperative/imprecise)、[FS Volumes / Subvolumes](https://docs.ceph.com/en/latest/cephfs/fs-volumes/)(subvolume 多租户 + authorize 凭据 + admin 挂根聚合)。
- **ceph-csi**: [静态 PV 文档](https://github.com/ceph/ceph-csi/blob/devel/docs/static-pvc.md)(staticVolume + rootPath=/)。
- **生产实证(2026-07,ssh 只读)**: cephfs SC(subvolume-backed,allowVolumeExpansion=true);4 个 PVC `ceph fs subvolume info` bytes_quota 匹配 requests.storage;SubVolumeGroup `csi` Ready;client.admin caps `allow *`;rcoder 现挂 subvolume PVC(subvolumePath=/volumes/csi/...)。
- **代码**: rcoder create_container/ensure_workspace_pvc/k8s_pvc、agent_runner gRPC、nuwax-file-server 全量、build-agent-docker chart。
- **gitoxide/gix**: gitbutler 主力 gix 0.85 验证;nuwax isomorphic-git API 全覆盖(3 薄封装)。
