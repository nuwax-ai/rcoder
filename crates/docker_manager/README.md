# Docker 与 Kubernetes 运行时

`docker_manager` 实现 RCoder 的 Docker 与 Kubernetes 容器运行时，管理 Agent、UserApp builder 和生产应用的计算资源。跨 crate 接口定义在 [container-runtime-api](../container-runtime-api/src/runtime_trait.rs)，业务生命周期和持久化协调由上层处理。

## 主要入口

| 入口 | 职责 |
|---|---|
| [RuntimeManager](src/runtime/manager.rs) | 初始化选中的后端，完成健康检查，提供 `Arc<dyn ContainerRuntime>` |
| [DockerRuntime](src/runtime/docker_runtime/mod.rs) | 将统一运行时接口适配到 Docker |
| [KubernetesRuntime](src/runtime/kubernetes_runtime/mod.rs) | 将统一接口适配到 K8s；需要 `kubernetes` feature |
| [DockerManager](src/manager.rs) | Docker 连接、容器创建、查询、停止及底层资源操作 |
| [ContainerConfigBuilder](src/container_builder/config_builder.rs) | 构造 Docker 容器配置 |

Agent 执行通过容器内的 agent_runner 完成；本 crate 负责运行资源，不直接实现 ACP 会话。

## 配置与初始化

后端由 `CONTAINER_RUNTIME` 选择：`docker` 或未设置时使用 Docker，`kubernetes` / `k8s` 使用 Kubernetes。选择 Kubernetes 而二进制未编入对应 feature 时，初始化返回配置错误。

RCoder 从应用配置构造 [DockerManagerConfig](src/types/config.rs)，再调用 `RuntimeManager::init`。配置装配见 [docker_init.rs](../rcoder-engine/src/docker_init.rs)：Docker 的镜像及服务配置读取 `multi_image_config`，K8s 读取 `kubernetes_config`。具体镜像、网络、资源限制和工作目录由所选部署配置提供，不能假定所有服务使用同一镜像或固定的 host 网络。

Docker 连接优先使用显式 `docker_host`；未设置时，非默认的 `DOCKER_SOCKET_PATH` 用于指定本地 socket，否则使用 Docker 本地默认连接。初始化会验证 daemon 连接和所需网络，失败直接返回原因。K8s 的连接和资源配置见 [Kubernetes 部署说明](../../docs/deployment/kubernetes.md)。

业务层通过统一运行时接口调用后端。只有需要 Docker 专有能力的底层代码才直接使用 `DockerManager`；完整字段以源码类型为准，配置说明见 [项目配置](../../docs/configuration.md)。

## 生命周期与数据保护

- 创建、停止、重启和删除使用捕获的应用、生命周期及物理资源身份，不能以同名资源或端口连通授权接管。
- UserApp builder 的复用、配置更新和恢复由上层协调器与运行时共同完成。停止未确认或已发写入结果未知时，继续核验原操作，不能另起第二套业务。
- Agent 停止保留 PVC；UserApp 的计算停止与数据销毁分开，数据销毁必须通过对应的显式业务入口。
- 状态缓存和健康检查提供观察结果，不代替资源归属、操作结果或业务就绪证明。

应用行为见 [UserApp 指南](../../docs/concepts/userapp.md)，错误分类和重试规则见 [运行时错误契约](../../docs/runtime-error-contract.md)。

## 验证与排障

从仓库根执行受影响的组件检查：

```bash
cargo nextest run -p docker_manager --no-fail-fast
cargo nextest run -p docker_manager --no-fail-fast --all-features
```

组件测试不能替代真实 Docker/K8s 资源验证。Docker 场景使用 [Compose 部署与验证](../../docs/deployment/docker.md)，K8s 场景使用 [远端工作流](../../docs/deployment/kubernetes.md#remote-workflow)；只操作本轮拥有的资源并保留要求保护的数据卷。

连接或创建失败时，核对当前后端、socket/kubeconfig 权限、镜像拉取凭据、实际挂载及资源状态。结合原 operation ID 和物理资源 ID 查询日志，不把查询失败解释为资源不存在。日志与追踪入口见 [可观测性指南](../../docs/observability.md)。
