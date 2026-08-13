# custom-page build pod 设计:vite/build 下沉 WebAgentRunner + node_modules 本地化

> 状态:方案已定,待 .20 验证
> 日期:2026-08-13
> 关联:`docs/storage-optimization-roadmap.md`(#7 的细化)、`specs/per-agent-storage-quota/`
> 范围:改动控制在 rcoder 仓库 + 镜像构建(build-agent-docker/build_config),不扩散 java 项目

---

## 1. 背景与目标

### 1.1 问题
custom-page(前端项目开发)场景,vite dev server / build 的 `node_modules`(典型 18672 个小文件)存在 CephFS workspace,触发 CephFS 小文件元数据风暴:install 慢、HMR/读取慢(实测每文件写 CephFS ~7ms,200 文件 ~1.7s)。根因是 per-file 数据对象写,与 CephFS 调优无关(见 `cephfs-performance-diagnosis.md`)。

### 1.2 目标
把 `node_modules` 从 CephFS 挪到**容器本地**(emptyDir),根治 custom-page 的 install/build/读写慢。

### 1.3 前提(必须满足)
1. **file-server 文件服务不丢**:rcoder 主 Pod 的 file-server 仍能访问源码(tree/git/查看/下载),提供文件服务
2. **对外 HTTP 接口不变**:`/api/build/*`、`/api/project/*` 等接口签名不变(内部实现可改)
3. **改动不扩散 java**:逻辑改造控制在 rcoder 项目 + 镜像构建
4. **build 接口自动拉起容器**:不要求用户手动起 pod,build/restart-dev 接口自动 ensure WebAgentRunner pod

### 1.4 非目标
- 不下沉源码(源码留 CephFS 共享,vite HMR 在 CephFS 上正常,只下沉 node_modules)
- 不新增 ServiceType 枚举(复用 WebAgentRunner,避免扩散 java)
- 不改 custom-page 的 WebAgentRunner 与对话 pod 的关系(本来就是同一个)

---

## 2. 现状(调研实证)

| 项 | 现状 | 来源 |
|----|------|------|
| vite 在哪跑 | **rcoder 主 Pod 的 file-server 进程内**(`tokio::process` spawn `npx vite`) | `handlers/build.rs` + `service/dev_server/process/mod.rs:179` |
| node_modules 在哪 | CephFS workspace PVC(`/app/project_workspace/{project_id}`) | 慢的根因 |
| WebAgentRunner pod 干什么 | 只跑 agent 对话(gRPC 50051),**不参与 build/dev** | `chat_handler.rs` 创建,route_table 路由 |
| dev server 状态 | `DevServerManager.processes`(内存 Map)+ `PortPool` + `is_project_alive`(打 127.0.0.1) | 全在控制面本机 |
| dist 产物 | 写 `/app/project_nginx`(CephFS),供 file-server `ServeFile` 发布 | `config/dist_target_dir` |
| build handler 接 ensure? | **不接**(直接本地 spawn,不碰 pod ensure) | `handlers/build.rs` |

**关键**:custom-page 的 build/dev 链路当前**完全没有 WebAgentRunner pod 参与**,vite 是控制面本地子进程。

---

## 3. 核心洞察:file-server 整体下沉,状态跟着走

最大的设计简化:**不是"控制面远程管 vite",而是"file-server 整体搬到 WebAgentRunner pod 内"**。

- file-server(含 vite 进程管理、端口池、dev server 状态)整体在 WebAgentRunner pod 内运行
- 控制面只做 HTTP 转发(`/api/build/*` → pod:60000),**不管 vite 进程状态**
- → dev server 状态管理**不需要改造成分布式**,状态跟着 file-server 走

这化解了"跨 Pod dev server 状态"的最大风险。

---

## 4. 最终架构

```
┌─ rcoder 主 Pod (控制面) ──────────────────────────────────┐
│  file-server (保留,职责收窄):                             │
│    /api/project/* /api/git/* (文件服务) → 读 CephFS 源码 ✅│
│    /api/computer/* (文件操作)            → 读 CephFS ✅    │
│    /api/build/* → HTTP 转发到 WebAgentRunner pod:60000    │
│  (不再 spawn vite,不管 dev server 状态)                   │
└───────────────────────────────────────────────────────────┘
        │ /api/build 转发(HTTP)         │ 源码共享(CephFS RWX)
        ▼                               │
┌─ WebAgentRunner pod (rcoder 镜像, 改造后) ────────────────┐
│  agent_runner 进程 (内嵌 file-server):                    │
│    ├─ agent 对话 (gRPC :50051)         ← /chat 打这里     │
│    └─ file-server (:60000):           ← /api/build 打这里 │
│         └─ vite/build (读本地 node_modules)               │
│         └─ dev server 状态在这里 (内存 Map/端口池, 本地)   │
│  volumes:                                                  │
│    ├─ CephFS: 源码 (共享, vite 读 src, HMR)               │
│    ├─ emptyDir: node_modules (本地, pnpm install 写本地)   │
│    └─ dist → 写回 CephFS (供 nginx/file-server 发布)      │
└───────────────────────────────────────────────────────────┘
```

**单 pod 两职责**:WebAgentRunner pod 既是 agent 对话 pod,又是 build/dev pod。custom-page 业务本来就用 WebAgentRunner,这里只是让它多内嵌一个 file-server。

---

## 5. 数据分布

| 数据 | 位置 | 共享? | 理由 |
|------|------|------|------|
| 源码(`.vue/.tsx/.py` 等) | CephFS | ✅ 跨 Pod | rcoder file-server 管理 + agent 改 + vite 读(HMR 正常) |
| `.git` | CephFS | ✅ | git 操作 |
| **node_modules** | **emptyDir(本地)** | ❌ Pod 内 | 可重建产物,小文件重灾区,本地根治 |
| pnpm store / npm cache | emptyDir(本地,配合 `/cache` 方案) | ❌ | 缓存 |
| dist(build 产物) | CephFS | ✅ | 供 nginx/file-server 发布 |
| skills | CephFS(tar,见 P1b) | ✅ | rcoder 写 / agent 读,跨 Pod |

**只下沉 node_modules + 缓存**(可重建、Pod 内闭环);源码/dist/skills 留 CephFS(需共享)。

---

## 6. 改动清单(都在 rcoder + 镜像范围)

### 6.1 镜像 / 启动配置
| # | 改动 | 文件 | 说明 |
|---|------|------|------|
| 1 | WebAgentRunner 启动时内嵌 file-server | `build_config/rcoder-agent-runner/` 启动脚本 + `_kubernetes-config.tpl` 的 WebAgentRunner env | 设 `RCODER_EMBED_FILE_SERVER=true`,agent_runner 进程内起 file-server(:60000)。UserAppBuilder 已验证此模式(`k8s_agent_create.rs:289-308`) |

### 6.2 Helm(volume)
| # | 改动 | 文件 | 说明 |
|---|------|------|------|
| 2 | WebAgentRunner pod 加 emptyDir(node_modules + /cache) | `templates/rcoder/_kubernetes-config.tpl`(services.web-agent-runner.volumes) | `build_agent_pod_spec` 的 volumes/sidecars 是 configmap 驱动,声明即可(`k8s_agent_create.rs:194-213`) |
| 3 | 项目 node_modules 软链 → emptyDir | WebAgentRunner 启动脚本 | 启动时把 workspace 的 node_modules 软链到本地 emptyDir;pnpm install 写本地 |

### 6.3 rcoder 代码(file-server)
| # | 改动 | 文件 | 说明 |
|---|------|------|------|
| 4 | `/api/build/*` 改 HTTP 转发到 WebAgentRunner pod:60000 | `crates/file-server/src/handlers/build.rs`(start_dev/restart_dev/build_project/keep_alive/stop_dev/list_dev/get_dev_log) | **接口签名不变**,内部从本地 spawn vite 改为 HTTP 转发到 pod:60000 的内嵌 file-server |
| 5 | build handler 加 ensure WebAgentRunner | `handlers/build.rs` + 复用 `pod_ensure`(`handler/pod_handler/ensure.rs`)/`ComputerContainerManager::get_or_create_container_for_user_with_type` | build 前 ensure pod 存在(幂等,复用现成 ensure 零件);ensure 后拿 pod 地址再转发 |

### 6.4 端口路由
| # | 改动 | 文件 | 说明 |
|---|------|------|------|
| 6 | vite 端口经 WebAgentRunner pod Service 暴露 | `crates/rcoder-proxy/src/service/handlers/port_proxy.rs`(`handle_port_proxy_upstream` L114) + WebAgentRunner Service | 现假设 vite 在 127.0.0.1(同 Pod);改成动态端口 → WebAgentRunner pod 地址(注册进 `backends: HashMap<u16,String>`,或 build pod 暴露端口范围经 Service) |

### 6.5 dist 产物
| # | 改动 | 文件 | 说明 |
|---|------|------|------|
| 7 | dist 写回 CephFS | WebAgentRunner 内嵌 file-server 的 `DIST_TARGET_DIR` 配置 | 指向 CephFS 的 `/app/project_nginx`(现状不变,只是 build 在 pod 内执行后产物落 CephFS) |

---

## 7. 数据流(验证前提满足)

### 7.1 文件服务(源码)—— 前提保证
```
用户查看/下载源码 → rcoder 主 Pod file-server → 读 CephFS 源码 → 返回
```
源码在 CephFS,控制面 file-server 直接读。**WebAgentRunner 没启动也能服务**(源码持久在 CephFS)。✅ 前提 1 满足。

### 7.2 build / vite —— 下沉到 pod,本地 node_modules
```
用户 restart-dev → rcoder 主 Pod /api/build/restart-dev
  → ensure WebAgentRunner pod(不存在则创建)
  → HTTP 转发 → WebAgentRunner pod:60000 内嵌 file-server
  → pod 内:pnpm install(写本地 emptyDir) + vite 启动(读本地 node_modules)
```
node_modules 全程在 pod 本地,**不写 CephFS**。✅ build 自动拉起容器(前提 4)。

### 7.3 agent 对话 —— 同一个 pod
```
用户 /chat → WebAgentRunner pod gRPC:50051 → agent 对话
```
和 build 同一个 pod(WebAgentRunner),不冲突。

### 7.4 源码修改同步
```
agent 在 pod 改 .vue → 写 CephFS(共享)
rcoder 主 Pod file-server 读 CephFS → 立即看到(tree/git)
vite HMR 监视 CephFS src → 热重载(现状正常)
```

### 7.5 vite 预览访问
```
浏览器 → backend → rcoder-proxy /proxy/{port}/ → WebAgentRunner pod:vite 端口
```
port_proxy 把动态端口路由到 WebAgentRunner pod(改 6.6)。

---

## 8. 关键设计决策(为什么这样选)

| 决策 | 理由 |
|------|------|
| **单 pod(对话 + build 合一)** | custom-page 业务本就用 WebAgentRunner;合并避免双 pod 协调;agent 和 build 共享 node_modules(同 pod emptyDir) |
| **复用 WebAgentRunner ServiceType** | 不新增枚举 → 不扩散 java;ensure/创建逻辑现成 |
| **file-server 整体下沉(非远程管)** | dev server 状态跟着 file-server 走,**不用分布式重构**(化解最大风险) |
| **只下沉 node_modules,不下沉 src** | src 文件少(用户代码)、HMR 在 CephFS 正常;node_modules 文件多(上万)、是慢的大头 |
| **HTTP 转发(非新 gRPC)** | 复用内嵌 file-server 的 60000 端口,不用新 proto;UserAppBuilder 已验证 |
| **dist 写回 CephFS** | 供 nginx/file-server 发布(需共享) |

---

## 9. 风险与验证(.20)

### 9.1 验证矩阵
| # | 验证项 | 方法 | 验收 |
|---|--------|------|------|
| 1 | WebAgentRunner 内嵌 file-server 跑 vite | .20 配置 `RCODER_EMBED_FILE_SERVER=true`,起 WebAgentRunner pod,curl :60000 起 vite | vite 正常起,HMR 工作 |
| 2 | node_modules 软链 + pnpm 兼容 | 项目 node_modules → emptyDir 软链,pnpm install 写本地,vite/esbuild 读本地 | install 写本地(不写 CephFS),工具链兼容 |
| 3 | build 转发链路 | rcoder 主 Pod /api/build/* → 转发 WebAgentRunner:60000 | 接口不变,build 在 pod 内执行 |
| 4 | ensure 时机 | build handler ensure WebAgentRunner(没起则拉起) | build 自动拉起 pod,不报错 |
| 5 | port_proxy 路由 | vite 动态端口 → WebAgentRunner pod 地址 | 预览可访问 |
| 6 | dist 回传 | pod 内 build,dist 写 CephFS | nginx/file-server 能发布 |

### 9.2 风险点
| 风险 | 缓解 |
|------|------|
| WebAgentRunner 内嵌 file-server 跑 dev server(非 build)未验证 | UserAppBuilder 验证过 build;dev server + HMR 要重点测(验证 1) |
| node_modules 软链的 pnpm/npm/esbuild 兼容 edge case | .20 充分测 react/vue3 模板 |
| emptyDir 生命周期(pod 重建丢 node_modules) | pod 重建重装(pnpm store 在 /cache emptyDir,缓存命中快);若要持久缓存用 per-agent PVC |
| port_proxy 改动影响其他场景 | port_proxy 只改 WebAgentRunner 的动态端口路由,不影响其他 |
| 两条 build 路径(UserAppBuilder + custom-page) | 对齐实现,避免重复;custom-page 复用 UserAppBuilder 的内嵌模式 |

---

## 10. 与其他优化方案的关系

| 方案 | 关系 |
|------|------|
| **#2 缓存本地化**(`/cache` emptyDir) | **互补,先做**。pnpm store/npm cache 走本地,和本方案的 node_modules 本地化叠加。本方案的 WebAgentRunner emptyDir 可包含 /cache |
| **#9 per-agent PVC** | **完美互补**。源码 per-agent PVC(CephFS 共享)+ node_modules pod 内 emptyDir(本地)。rcoder 管理聚合面只管源码,不管 node_modules |
| **#8 skills tar** | 独立,可并行。skills 跨 Pod 共享(CephFS tar) |

---

## 11. 推进计划

```
1. 先做 #2 缓存本地化(零风险,马上见效,和本方案不冲突)
2. 本方案 .20 验证(按 9.1 矩阵):
   a. WebAgentRunner 内嵌 file-server 跑 vite + HMR
   b. node_modules 软链 + pnpm 兼容
   c. build 转发 + ensure + port_proxy
3. 验证通过 → cutover(回滚开关:env 切回控制面 spawn vite)
```

**回滚**:保留控制面 spawn vite 的旧路径,env 开关切换。出问题切回旧路径(慢但可用)。

---

## 12. 关键文件索引(实现时参考)

| 职责 | 文件 |
|------|------|
| build handler(转发 + ensure 改这里) | `crates/file-server/src/handlers/build.rs` |
| dev server 管理(下沉到 pod,本机逻辑保留作回滚) | `crates/file-server/src/service/dev_server/{mod,process,port_pool}.rs` |
| WebAgentRunner pod spec(volumes/sidecars,config 驱动) | `crates/docker_manager/src/runtime/k8s_agent_create.rs:113 build_agent_pod_spec` |
| ensure 零件(复用) | `crates/rcoder/src/handler/pod_handler/ensure.rs` + `service/computer_container_manager.rs` |
| 端口代理(改 127.0.0.1 假设) | `crates/rcoder-proxy/src/service/handlers/port_proxy.rs:114` |
| 路由(DataPlane vs ControlPlane) | `crates/rcoder-gateway/src/route_table.rs` |
| 内嵌 file-server(UserAppBuilder 范例) | `crates/rcoder/src/file_server_embed.rs` + `k8s_agent_create.rs:289-308` |
| Helm volume 配置 | `build-agent-docker/k8s/helm/nuwax-platform/templates/rcoder/_kubernetes-config.tpl` |
| 镜像启动 | `build-agent-docker/build_config/rcoder-agent-runner/` |
