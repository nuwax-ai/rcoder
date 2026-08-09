# 修复:容器销毁时主动终止 SSE 进度流(`/computer/progress/{session_id}`)

> 状态:待修复(交接说明)。类型:可靠性 / UX bug。
> 影响环境:全部(K8s + Docker Compose)。优先级:中高。

## 一、问题现象

容器(agent-runner)被回收/重启/重建后,前端订阅的 SSE 进度流
`GET /computer/progress/{session_id}` **不会主动断开**,前端对着一条已死的连接"空等":
后端 agent_runner pod 已经没了,但浏览器侧 `EventSource` 一直处于 open,用户看不到结束、
也收不到"该重连/重建"的信号,直到 gRPC 链路超时(可能几十秒~数分钟)才被动断开。

## 二、背景

- 产品形态:每个用户一个虚拟电脑容器(agent-runner),跑 XFCE + Xvnc 桌面 + agent。
  前端经 `/computer/progress/{session_id}`(SSE)订阅该容器内 agent 的实时进度事件。
- 闲置回收:服务器资源有限,空闲超时的容器由 idle cleaner 自动销毁(`reason=Idle timeout`)。
  生产曾出现用户正在用、但容器被误回收的事故(00:54 发起任务 → 01:07 被 idle cleaner 销毁)。
- 现有 SSE 关闭机制只在"部分销毁路径"上接线(见根因),其余路径漏接 → SSE 不随容器死亡而断。

## 三、SSE 流的内部架构(必读,决定怎么修)

`/computer/progress/{session_id}` 不是直接转发,而是经 **`SessionStreamRegistry`** 中转:

```
HTTP SSE 客户端 ──recv──► SharedStream(broadcast fan-out + 历史 ring)
                                 ▲
                                 │ broadcast
                          后台 tokio task(持有 agent_runner 的 SubscribeProgress gRPC 流)
                                 │
                          agent_runner 容器(已死 → gRPC 流应报错)
```

- 每个 session 一个 `SharedStream`(`crates/rcoder/src/grpc/session_stream_registry.rs:170`):
  一个后台 task 持有到 agent_runner 的 `SubscribeProgress` gRPC 流,把事件 fan-out 给 N 个 HTTP SSE
  客户端(broadcast channel),并维护历史 ring(断线重连补齐)。
- 关闭语义:`shutdown_session(sid)`(`session_stream_registry.rs:76`)= 从 registry 摘除 + `removed.shutdown()`
  (abort 后台 task + cancel)→ broadcast channel 关闭 → 所有 SSE 客户端 `recv()` 收到 `Closed` →
  HTTP SSE 响应结束 → 前端 `EventSource` 收到 close。
- 现成的项目级关闭:`AppState::shutdown_sse_streams_for_project(project_id)`
  (`crates/rcoder/src/router.rs:216`):枚举该 project 的 `sessions()`,逐个 `shutdown_session`。
  ⚠️ **必须在 `remove_project`/`delete_container_with_projects` 之前调**——后者会清空 sessions 集合
  (见 router.rs:214 的 doc 注释),之后无法再据此枚举。

## 四、根因(两层叠加)

### 层 1:SSE 关闭的调用面有缺口(主因)

`shutdown_sse_streams_for_project` 全仓库**只有 3 处**调用:

| 路径 | 文件:行 | 是否关 SSE |
|---|---|---|
| `/agent/stop`(用户主动停) | `handler/agent_stop_handler.rs:75` | ✅ |
| idle cleaner 销毁后(destroyed 分支) | `cleanup_task/cleaner.rs:320` | ✅ |
| idle cleaner 仅删记录(else 分支) | `cleanup_task/cleaner.rs:333` | ✅ |
| **RAII `ResourceReaper`**(refcount=0 触发) | `storage/resource_reaper.rs:116`(`process_cleanup`→`stop_container_by_identifier`) | ❌ |
| **`/computer/pod/restart`** 的 destroy+recreate 回落 | `handler/pod_handler/restart.rs:165` | ❌ |
| **`/computer/pod/ensure`** 的"容器坏了/停了"重建 | `handler/pod_handler/ensure.rs:251` | ❌ |
| **物理销毁层 `ContainerDestroyer`** | `cleanup_task/container/destroyer.rs:67`(`destroy_with_reason`) | ❌(只 `remove_vnc_backend`,见 :120) |

**只要容器经未关 SSE 的路径死亡,SSE 流和后台 gRPC task 就继续指向已死的 agent_runner → 前端空等。**

### 层 2:靠 gRPC 报错自断也不可靠

即便没人显式关,后台 `SubscribeProgress` gRPC 流在 pod 死后理应报错 → 关 broadcast → SSE 自断。但:

- gRPC channel **发现 pod 死亡可能很慢**:连接池(moka,`time_to_idle=300s`)+ 无激进 keepalive,
  死链可能几十秒~数分钟才感知。
- 后台 task 报错时**可能重试/续命而非关闭**:registry 里有"grpc_addr 变化(容器重建)→ 移除旧 task
  再按新地址重建"的逻辑(`session_stream_registry.rs:105-137` 附近),有续命倾向。

两者叠加 → 显式关闭之前的空等窗口很长。

### 关于 01:07 那次(idle cleaner)

cleaner **其实调了** `shutdown_sse_streams_for_project`,但仍可能空等:(a) 它按 project 的
`sessions()` 枚举——若当前 SSE 的 session_id 没登记进该集合(映射缺/时序)就漏了;
(b) 它在"销毁 pod 之后"才关,中间有窗口;(c) 层 2 的 gRPC 感知慢照样适用。

## 五、修复方案

### 推荐:把 SSE 关闭下沉到物理销毁统一点(覆盖所有 reaper)

核心思路——**一处改,全兜住**:让 `ContainerDestroyer::destroy_with_reason` 在物理销毁的同时
关掉该容器的 SSE 流。因为 cleaner / ResourceReaper / pod_restart / pod_ensure **全部**最终经
`ContainerDestroyer`(或 `runtime.stop_container_by_identifier`)销毁,统一点关一次即可。

#### 步骤

1. **新增容器级 SSE 关闭辅助**(`crates/rcoder/src/router.rs`,挨着 `shutdown_sse_streams_for_project`):
   ```rust
   /// 关闭挂在该容器下的所有 SSE 进度流。必须在 delete_container_with_projects 之前调。
   pub fn shutdown_sse_streams_for_container(&self, container_id: &str) {
       for p in self.projects.get_projects_by_container_id(container_id) {
           self.shutdown_sse_streams_for_project(p.project_id());
       }
   }
   ```
   (`get_projects_by_container_id` 已存在,cleaner.rs:321 现用。)

2. **让 `ContainerDestroyer` 能访问 registry/projects**。当前构造(`cleanup_task/cleaner.rs:56`):
   ```rust
   ContainerDestroyer::new(runtime, grpc_pool, pingora_service, namespace, cluster_domain, is_kubernetes)
   ```
   没有注入 SSE 关闭能力。两种接法(任选其一):
   - **接法 A(耦合小)**:给 `ContainerDestroyer` 多注入一个回调
     `shutdown_sse: Arc<dyn Fn(&str) + Send + Sync>`(参数是 container_id),
     构造时由 cleaner 传入 `Arc::new({ let s = state.clone(); move |cid| s.shutdown_sse_streams_for_container(cid) })`。
   - **接法 B(直接)**:给 `ContainerDestroyer` 注入 `session_stream_registry` + projects 句柄
     (或直接 `state: Arc<AppState>`),destroyer 内部调 `shutdown_sse_streams_for_container`。
   接法 A 更解耦(destroyer 不依赖 AppState 全貌),推荐。

3. **在 `destroy_with_reason`(`cleanup_task/container/destroyer.rs:67`)里调用**:
   物理销毁前(或后)调一次 `shutdown_sse(container_id)`。
   destroyer.rs 已有 `container_id`(`:91-93` 附近,`destroy_with_reason` 的参数链里有)。
   注意:`remove_vnc_backend`(`:120`)同样是从 pingora 摘流量,SSE 关闭与之并列加一行即可。

4. **(可选,推荐)防御性:后台 gRPC task 在 upstream stream `End`/`Error` 时直接关 broadcast**
   而非重试(`session_stream_registry.rs:170-260` 的后台 task 循环)。这样即便某条路径漏了显式关,
   agent_runner 一死 SSE 也能自断,不依赖 gRPC 感知速度。需先读后台 task 的 recv 循环确认当前是
   "关"还是"重试",再改。

5. **(可选)给 cleaner 里的两处显式调用做去重**——destroyer 接管后,cleaner.rs:320/333 那两处
   `shutdown_sse_streams_for_project` 可保留(无害,幂等)也可删(避免重复)。幂等即可保留。

### 备选:只补缺口(改动小、不够集中)

若不想动 destroyer 构造,就在 3 个漏接处各加一行:
- `storage/resource_reaper.rs:116`(`process_cleanup`,在 `stop_container_by_identifier` 前)
- `handler/pod_handler/restart.rs:165`(destroy+recreate 回落,destroy 前)
- `handler/pod_handler/ensure.rs:251`(stop 重建前)

每处:`state.shutdown_sse_streams_for_container(&container_id);`(需要拿到 container_id;
restart/ensure 上下文里有 container_info,ResourceReaper 里需要从 CleanupRequest 带/查)。

**缺点**:以后新增任何 reaper 都得记着加,容易再漏。推荐方案更稳。

## 六、关键文件速查(均在 rcoder 仓库根下)

| 关注点 | 位置 |
|---|---|
| `/computer/progress` 路由 | `crates/rcoder/src/router.rs:360` |
| SSE handler(用 registry) | `crates/rcoder/src/handler/agent_session_notification.rs:662`、`:704` |
| `session_stream_registry` 字段 | `crates/rcoder/src/router.rs:74` |
| `shutdown_sse_streams_for_project` | `crates/rcoder/src/router.rs:216` |
| `shutdown_session`(abort 后台 task) | `crates/rcoder/src/grpc/session_stream_registry.rs:76` |
| `SharedStream`(bg gRPC task + broadcast + cancel_token) | `crates/rcoder/src/grpc/session_stream_registry.rs:170` |
| 后台 task 创建/重建/grpc_addr 变化逻辑 | `crates/rcoder/src/grpc/session_stream_registry.rs:88-137` |
| `ContainerDestroyer::new`(唯一构造点) | `crates/rcoder/src/cleanup_task/cleaner.rs:56` |
| `ContainerDestroyer::destroy_with_reason` | `crates/rcoder/src/cleanup_task/container/destroyer.rs:67` |
| destroyer 现在只摘 VNC backend | `crates/rcoder/src/cleanup_task/container/destroyer.rs:120` |
| cleaner 现有 SSE 关闭(destroyed/else) | `crates/rcoder/src/cleanup_task/cleaner.rs:320`、`:333` |
| RAII ResourceReaper(漏接) | `crates/rcoder/src/storage/resource_reaper.rs:116` |
| pod_restart destroy 回落(漏接) | `crates/rcoder/src/handler/pod_handler/restart.rs:165` |
| pod_ensure stop 重建(漏接) | `crates/rcoder/src/handler/pod_handler/ensure.rs:251` |
| 物理 stop(K8s/Docker) | `crates/docker_manager/src/runtime/k8s_agent_pod.rs` / `docker_runtime.rs` |

## 七、验证

1. **编译/测试**:`cargo check -p rcoder --tests`、`cargo clippy -p rcoder`、相关单测。
2. **端到端(K8s dev 环境)**:
   - 开一台虚拟电脑,前端订阅 `/computer/progress/{session_id}`(`EventSource` 处于 open)。
   - 触发容器销毁的各种路径,分别验证前端 SSE **立即收到 close**(不再空等):
     - idle cleaner 闲置回收(超 idle_timeout);
     - 调 `/computer/pod/restart` 重启;
     - 调 `/computer/pod/ensure` 触发重建;
     - 制造 RAII refcount=0(若有现成手段)或直接 `kubectl delete pod` 模拟容器消失。
   - 期望:容器死的瞬间,前端 SSE close → 走既定的重连/重建逻辑。
3. **回归**:正常对话期间 SSE 事件仍正常推送(关闭逻辑只在销毁时触发,不影响活跃流)。
   幂等:`shutdown_session` 对已关闭 session 返回 false,重复调用安全。

## 八、注意事项

- **顺序**:任何 `shutdown_sse_streams_*` 必须在 `remove_project` / `delete_container_with_projects`
  之前(它们清空 sessions 集合)。cleaner.rs 现有顺序已是"先关 SSE 再 delete",保持。
- **幂等**:多处关同一个 session 安全(`shutdown_session` 对不存在的 sid 返回 false)。
- **container_id 可得性**:destroyer/destroy_with_reason 的参数链里已有 container_id;
  RAII `process_cleanup` 的 `CleanupRequest` 若没带 container_id,需要从 `stop_container_by_identifier`
  的 identifier 反查或让 CleanupRequest 带上(见 `resource_reaper.rs:43` 字段注释)。
- **不要依赖层 2(gRPC 自断)作为唯一手段**——它是兜底,感知慢;显式关闭才是正解。
