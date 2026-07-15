# rcoder 待办清单(交接文档)

> 最近更新: 2026-07-15
> 来源: 近期线上排查/迁移收口整理,供接手 agent 开发。
> 仓库: `/Users/soddy/Documents/git-workspace/rcoder`

## 通用约定(接手前必读)

- **验证命令**:改动后统一跑 `cargo check --features kubernetes --workspace` + `cargo clippy --features kubernetes --workspace`。K8s runtime 全在 `#[cfg(feature = "kubernetes")]` 后,不带 feature 会漏判。
- **不要碰** `code/` 目录(若有)——是 build-agent-docker 侧的同步产物。
- **生产环境**: nuwax-k8s-prod / nuwax-k8s-test (K3s + Cilium + Rook Ceph)。本批改动**默认只改代码 + 本地编译验证,不部署**;部署由人工触发 `make` + helm。
- **失败优先 / 不用 unwrap()**: 生产代码禁 `unwrap()`/`expect()`(测试可用);异常加 `.context()`。

---

## ✅ 待办 1: 清理 Envoy Backend CRD 残留代码(✅ 已完成 commit `1a52f64`)

### 背景(为什么)
集群已从 Envoy Gateway 迁移到 **Cilium Gateway**(见 build-agent-docker 的 GW①~GW⑥ 与 commit `1cc3296`)。Envoy 的 `backends.gateway.envoyproxy.io` CRD 已从集群卸载(`kubectl get crd backends.gateway.envoyproxy.io` → NotFound),rcoder SA 也丢了对应 RBAC。

但 **rcoder 运行时仍每次 pod_ensure / keepalive / self-heal 尝试创建+删除这个 Envoy Backend CRD**,每次必失败 403,日志刷大量噪声(单次排查就看到 83+ 行)。功能上**非致命**(VNC 流量走 rcoder 内存代理 pingora,不依赖该 CRD),但:
1. 日志噪声掩盖真实错误(排查 "请求没反应" 时被它干扰)。
2. Envoy→Cilium 迁移没收干净的代码残留。

### 当前状态
`crates/docker_manager/src/runtime/k8s_backend_crd.rs` 整文件 + 4 处调用点仍在生效,每次创建/销毁 agent pod 都触发失败的 CRD 操作。

### 涉及文件 / 改动点(5 处)
| # | 位置 | 改动 |
|---|------|------|
| 1 | `crates/docker_manager/src/runtime/k8s_backend_crd.rs` | **整文件删除**(154 行,含 `backend_api_resource()` / `K8sBackendCRDOps` trait / impl) |
| 2 | `crates/docker_manager/src/runtime/mod.rs:11` | 删 `pub(crate) mod k8s_backend_crd;` |
| 3 | `crates/docker_manager/src/runtime/kubernetes_runtime.rs:48` | 删 import `k8s_backend_crd::K8sBackendCRDOps` |
| 4 | `kubernetes_runtime.rs:1129-1135`(create_container) | 删 `if let Err(e) = self.create_backend_crd(...)` 块 |
| 4b | `kubernetes_runtime.rs:1244-1249`(get_container_info_by_identifier self-heal) | 删 `if let Err(e) = self.create_backend_crd(...)` 块 |
| 5 | `kubernetes_runtime.rs:1368-1384`(stop_container Step 0) | 删 `delete_backend_crd` 调用块(**保留** `delete_agent_service` 那块) |
| 5b | `kubernetes_runtime.rs:1573-1590`(cleanup_all Step 0) | 删 Backend CRD `delete_collection` 整块(**保留** 后面的 Service delete_collection) |

### 收尾注意(否则编译/ clippy 挂)
- 删 5b 后,`kubernetes_runtime.rs` 顶部的 `use kube::api::{... DynamicObject ...}` 和 `use kube::discovery::ApiResource` 可能变成**未使用 import** → 需一并清理(clippy 会报)。
- 全局搜一遍 `backend_crd` / `create_backend_crd` / `delete_backend_crd` / `backend_api_resource` / `K8sBackendCRDOps` 确认无残留引用。
- 检查有无针对该模块的单测(`grep -rn k8s_backend_crd crates/` / `tests/`)。

### 完成标准
- [ ] `cargo check --features kubernetes --workspace` 通过
- [ ] `cargo clippy --features kubernetes --workspace` 无新增 warning
- [ ] `grep -rn "gateway.envoyproxy\|backend_crd\|BackendCrd" crates/` 无结果
- [ ] (部署后验证,非本次)rcoder 日志不再出现 `backends.gateway.envoyproxy.io ... forbidden` 噪声

---

## 🔧 待办 2: CephFS 工作区磁盘配额根治(需设计,中风险)

### 背景(为什么)
rcoder 创建 agent 时,本应按接口入参 `storage_size` 给每个用户/agent 的 CephFS 工作区子目录设磁盘配额,防单用户写爆共享卷。

**原实现(②-b,已禁用)**: rcoder 主 pod 挂共享 CephFS 根,用 `xattr::set(..., "ceph.quota.max_bytes", ...)` 给 agent 子目录设配额。

**为何禁用**(2026-07-15 线上事故):
- CephFS 是 **kernel mount**(`type ceph`)。kernel client **不允许 setfattr 设 `ceph.quota.*` 虚拟 xattr**(这是 kernel client 限制,非 caps;连 `client.admin` `allow *` 也 denied)。
- 旧版 cephx caps `allow rw` 时**意外设成功**了 per-user 10Gi 配额 → 用户写超 10Gi 触发 **EDQUOT (errno 122)**,nuwax-file-server 报 "Unknown system error -122" 500,"请求没反应"。
- kernel client 的 `getfattr` 又**不显示** `ceph.quota.*`(误导排查以为"没配额")。
- 已止血: node admin `setfattr -x ceph.quota.max_bytes` 清掉所有 userId 旧配额 + 代码注释禁用。

### 当前状态
`kubernetes_runtime.rs:735-775` 整个配额块**已注释禁用**,带详细背景注释。配额功能**完全关闭**(用户写入不限)。可复用的 helper 仍在:
- `agent_workspace_quota_dir(service_type, isolation, tenant, space, project, user)` — 算配额目录路径(含 web 三级 tenant/space/project 规则)
- `parse_quantity_to_bytes(qty)` — "150Gi" → 字节数

### 约束(设计时必须考虑)
- 共享卷模型: **一个 CephFS PVC**(`nuwax-k8s-prod-rcoder-computer-workspace` / `-rcoder-workspace`)+ per-user **子目录**,不是 per-user subvolume。
- kernel cephfs mount 无法 setfattr 虚拟 xattr(已证伪,别再走这条路)。
- `ephemeral-storage` limit **管不到** CephFS 挂载(只管可写层 + emptyDir),不能当配额替代。

### 候选实现方向(需评审择一)
1. **per-user CephFS subvolume + `ceph fs subvolume resize`**: 每个 user/project 一个 subvolume,各自原生 quota。**最干净但改动大**(卷模型从"共享卷+子目录"→"每用户独立卷",涉及 PVC/StorageClass/挂载逻辑/已有数据迁移)。代码注释里提的就是这个方向。
2. **rcoder pod 改用 ceph-fuse 挂工作区**: ceph-fuse(userspace)**允许** setfattr 虚拟 xattr,原 ②-b 逻辑可直接复用。改动集中在 chart 挂载方式 + StorageClass mounter,rcoder 代码几乎不动。代价: ceph-fuse 性能略低于 kernel,且需测 rcoder pod 内 fuse 挂载的稳定性。
3. **rcoder 调 Ceph admin 设目录配额**: rcoder 持 admin/合适 caps 的 cephx key,通过 mgr/MDS 命令设子目录 quota(绕过 kernel client)。需评估有无对应 mgr 命令 + 密钥管理复杂度。

### 完成标准
- [ ] 选定方案 + 写 spec(放 `specs/` 下,遵循项目 spec 流程)
- [ ] 配额可设、可查、可改、可清,**且 kernel mount 下能生效**(或明确改 fuse/subvolume)
- [ ] 不重蹈覆辙: 配额设失败要显式可观测(不静默退化为"不限"还以为设上了)
- [ ] 灰度验证: 单用户写超配额 → 正确返回 EDQUOT 且不波及其他用户

### 相关记忆
- 配额排查全过程见 memory `cephfs-csi-stale-session-mgr-caps`(同源 CephFS 排查)、以及 EDQUOT 止血记录。

---

## ⏸️ 暂缓(勿动): 异步停止 + 并行启动(原 P2)

**状态: 有意暂缓,不是活跃待办。** 接手者**不要**在没有充分测试前提下贸然做。

### 暂缓理由(来自原 task #11)
- 目标: pod 销毁/启动并行化,把销毁耗时从 ~4.5s 再降。
- 风险: 实现方式是 **pod 名版本化**(新旧 pod 并存过渡),属架构级改动:
  - `pod_name` 5 处调用 + `pod_cache` + Service/FQDN/vnc_backends 都会被"传染"。
  - `find_container` / label 查询在并行期(新旧 pod 共存)有数据竞争/歧义。
- 收益不对等: P1(缩短 grace/timeout)+ P3(agent_runner SIGTERM 根治)已把销毁从 60s 降到 ~4.5s,P2 边际收益 ~4.5s,与风险不匹配。
- 若要做: 需单独专项,先出 spec + 并行期数据竞争的测试方案,再动。

---

## 🔧 待办 3: K8s self-heal service_type 一致性(需设计,中风险)

### 背景(为什么)
`get_container_info_by_identifier`(`crates/docker_manager/src/runtime/kubernetes_runtime.rs:1212-1239`) 的 self-heal 用【入参 service_type】调 `create_agent_service`,而 service_url 用【真实 Pod 名】。入参 ≠ 真实 Pod service_type 时,造出名字错位 + selector 不命中的**孤儿 Service**(`delete_agent_service` 同样推不出名字 → 删不掉,持续泄漏)。

### 触发条件(罕见)
- identifier 跨类型碰撞:Computer 用 user_id、Web 用 project_id、pod_id 通吃(`ServiceType::container_identifier`)
- 硬编码调用点:`sse_stream.rs:249` / `agent_session_notification.rs:1293` / `agent_mgmt_handler.rs:185` 被错类型容器用

### 修复方向(方案 A,推荐)
self-heal 分支改读 Pod label `rcoder.io/service-type`(`ServiceType: FromStr` 可反解析),读不到时 fallback 入参 service_type(兼容旧无 label Pod)。
- 现成先例:`create_container` 409 冲突路径(kubernetes_runtime.rs:1068-1076)已做完全相同的 label 读取。
- P0(Backend CRD 清理,commit `1a52f64`)后,self-heal 只剩 `create_agent_service`,修复面小。

### 不做(P3a identifier trait 方法)
规划时曾考虑加 `K8sServiceOps::agent_service_name_from_pod_name` trait 方法区分"业务 id 入口"vs"Pod 名入口"。但 P1(地址构建统一到 shared_types,commit `3b79c0f`)后,`get_container_access_address` 已改用 `shared_types::build_k8s_service_fqdn`,`agent_service_name` 只剩 create/delete 业务 id 路径在用——双前缀误用风险已根除,加方法是纯整洁无实际收益。**跳过**。

### 完成标准
- [ ] self-heal 用 Pod label 真实 service_type,fallback 入参
- [ ] 测试:模拟 service_type 不一致场景,验证不造孤儿 Service

---

## 🟡 待办 4: 低优先代码 TODO(2 处占位,可顺手)

`grep` 出的代码内 TODO,均为功能占位,非阻塞:
1. `crates/docker_manager/src/health/service_health.rs:35` — `TODO: 用于后续自动重启功能`(健康检查挂自动重启)
2. `crates/rcoder/src/app_manager/handlers.rs:396` — `获取应用事件(best-effort:当前返回空,TODO 接 K8s events)`

接手者可按需排期,无紧迫性。

---

## 🔧 待办 5: 路径拼接改用 PathBuf::join(需设计,中风险)

### 背景(为什么)
`container_path_template` 路径生成全链路是**纯字符串操作**,没用 `PathBuf`:
- default 模板(`crates/shared_types/src/service_config.rs:10-12`): `format!("{}{{project_id}}", WORKSPACE_ROOT)` → 缺 `/`,产出 `/app/project_workspace{project_id}`(同理 `{user_id}` 前也缺 `/`)
- `resolve_container_path`(`service_config.rs:462`): `resolved.replace("{key}", value)` 纯字符串替换,返回裸 `String`

症状: 4 个 `container_path_test` 预存失败(default 模板缺 `/`)。**补 `/` 只是补字符串,根本问题(用字符串操作路径)没解决**——跨平台分隔符、重复 `/`、下游继续拼等问题都挡不住。

### 正确方向(PathBuf::join,不是补 /)
路径段用 `PathBuf::join`(系统规则):跨平台分隔符、不缺/不重复 `/`、下游可继续 `.join()`。

两条路(需评审择一):
- **A(彻底)**: 废弃字符串模板,改结构化配置(root + segments)+ `PathBuf::join`。最干净,但 config.yml 的 `container_path_template` 字段属 breaking change。
- **B(兼容)**: 保留模板配置(用户自定义),`resolve_container_path` 把替换结果喂 `PathBuf` 规范化(`PathBuf::from(resolved)` + 去重复 `/`),default 用 `join` 生成。保留灵活性,路径正确性交 `PathBuf`。

### 涉及
- `crates/shared_types/src/service_config.rs`: `default_container_path_template` / `resolve_container_path`
- `crates/shared_types/src/paths.rs`: `WORKSPACE_ROOT` / `COMPUTER_WORKSPACE_ROOT` 常量
- 下游所有用 `resolve_container_path` 结果处(评估 `String` → `PathBuf` 的影响面)

### 完成标准
- [ ] 选定 A 或 B + 写 spec
- [ ] resolve 用 `PathBuf`,跨平台 + 不缺/不重复分隔符
- [ ] `container_path_test` 4 个预存失败转绿
- [ ] 评估 config.yml 自定义模板的兼容性

---

## 附:近期已完成(勿重复做)
- **service_url 双前缀 bug 修复**(commit `c840f05`): K8s permission/cancel/stop transport error 根因;service_url 复用 shared_types::build_k8s_service_fqdn
- **P0 Backend CRD 残留清理**(commit `1a52f64`): 删 k8s_backend_crd.rs + 4 处调用点,消除 Envoy CRD 403 噪声
- **P1 地址构建统一**(commit `3b79c0f`): 14 处内联 if/else → shared_types::build_backend_addr/build_grpc_addr,净减 171 行
- **P2 UserApp service_url 统一 + 魔数 8086**(commit `2101255`): UserApp 用 build_k8s_service_fqdn(app_deployment_name);魔数 → HTTP_DEFAULT_PORT
- ②-b setfattr 集中配额 → **已禁用**(见待办 2)
- Envoy→Cilium Gateway 迁移(GW①~GW⑥,build-agent-docker 侧)+ chart 内 envoy/streaming 死配置清理(commit `1cc3296` / `85`)
- rcoder 启动/grace 优化 P1/P3/P4/P5(销毁 60s→~4.5s)
- shared_types paths 模块统一路径常量
- FAST_RESTART_ENABLED 快路径(helm env 可关)
