# Docker Compose 形态

本地开发与单机部署的推荐形态：rcoder 主服务、Pingora 代理与可观测性栈全部以 Docker Compose 编排。

## 快速开始

```bash
make dev-build    # 首次：构建镜像
make dev-up       # 启动（主服务 8090 / Pingora 8089）

make dev-hot      # 日常：改 Rust 源码后秒级热编译（容器内增量编译 + 替换二进制 + 重启）
make dev-restart  # 全量重建（仅 Dockerfile / Cargo.toml 依赖 / 非 Rust 文件变化时需要）

make dev-logs     # 日志
make dev-down     # 停止
```

Compose 定义在 `docker/docker-compose.yml`；配置经 `docker/config.yml` 挂载（配置优先级：CLI 参数 > 环境变量 > 配置文件 > 默认值）。

## 组件拓扑

| 组件 | 端口 | 说明 |
|------|------|------|
| rcoder 主服务 | 8090 | HTTP API + 内嵌 file-server |
| Pingora 代理 | 8089 | 端口路由 / UserApp 应用代理 / VNC·音频·IME 等数据面 |
| 可观测栈 | — | otel-collector / Tempo / Loki / fluent-bit / Grafana（compose 常开，与生产同拓扑），见[可观测性指南](../observability.md) |

动态创建的 agent 容器与 UserApp 容器由 rcoder 经 Docker socket 管理，不在 compose 清单内。

## 镜像构建

```bash
make docker-build                  # 全量
make docker-build-master           # 主服务镜像
make docker-build-agent-runner     # Agent Runner 镜像
make docker-build-agent-production # 生产镜像（无调试工具）
make docker-build-app-runtime      # UserApp 运行时镜像（多语言统一）
```

## 验证

```bash
curl http://localhost:8090/health      # 主服务健康
curl http://localhost:8089/…           # 经 Pingora 的数据面
```

接口文档：`http://localhost:8090/api/docs`（Swagger UI / Scalar）。

## 相关形态

- [Kubernetes 形态](kubernetes.md)：STS + PVC 的生产形态
- [宿主机形态（deploy-host）](host.md)：控制平面直接跑宿主机的单机形态
