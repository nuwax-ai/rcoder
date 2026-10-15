# Docker Compose 部署与开发

Docker Compose 用于本地开发和单机验证。主服务管理动态创建的 Agent 与 UserApp 容器，Compose 同时提供代理和可观测性服务。

## 启动与更新

在仓库根目录操作。需要 Docker、Docker Compose、Make 和 Python 3；Make 入口使用 `docker-compose` 命令。宿主机编译 Rust 时使用当前 Rust stable，并保留仓库锁文件。

```bash
make dev-build    # 构建主服务与 Agent Runner 镜像
make dev-up       # 启动 Compose 服务
make dev-logs     # 跟随服务日志

make dev-hot      # 容器内增量编译 rcoder、替换二进制并重启主容器
make dev-restart  # 重新构建镜像并重建 Compose 服务
make dev-down    # 停止 Compose 服务，保留数据卷
```

`dev-hot` 需要主容器运行，源码和 Cargo 缓存已按 Compose 配置挂载。它只更新 rcoder 二进制；修改 Agent Runner、app-cli、系统依赖、Dockerfile 或启动脚本后，应重建相应镜像并更新被测容器。已有动态容器不会仅因主服务镜像更新而自动更换镜像。

Compose 配置见 [docker-compose.yml](../../docker/docker-compose.yml)，主服务配置见 [config.yml](../../docker/config.yml)。配置优先级为环境变量 > CLI 参数 > 配置文件 > 默认值。镜像与运行环境配置见[配置指南](../configuration.md)。本地真实地址和凭据写入不提交的环境文件。

## 服务与入口

| 服务 | 默认宿主机入口 | 用途 |
|---|---|---|
| RCoder | `http://localhost:8090` | HTTP API、SSE、内嵌文件服务 |
| Pingora | `http://localhost:8089` | Computer 桌面、音频、IME 和应用数据面代理；容器内监听 8088 |
| file-server 分流代理 | `http://localhost:60000` | 按配置转发 Rust 或 TypeScript 文件服务 |
| Grafana | `http://localhost:3300` | Tempo、Prometheus、Loki 数据查询 |
| Prometheus | `http://localhost:9091` | 指标查询 |
| Tempo / Loki | `http://localhost:3200` / `http://localhost:3100` | 追踪与日志后端 |

`RCODER_PORT` 可调整主服务端口。最终端口、挂载和镜像身份以实际 Compose 展开结果为准。Compose 中的默认 Grafana 账户仅用于本地开发；对外使用前应配置访问控制。

```bash
docker compose -f docker/docker-compose.yml -f docker/docker-compose.turso-volume.yml ps
curl --fail http://localhost:8090/health
```

API 文档位于主服务的 `/api/docs/` 和 `/api/docs/scalar`。健康检查只能证明服务基本可用，业务验证入口见下文。

动态 Agent 容器、UserApp builder 与应用运行容器不在静态 Compose 服务清单中，由 RCoder 通过 Docker socket 管理。Computer 数据面必须使用代理入口，详见 [Computer Agent 指南](../concepts/computer-agent.md)。

## 镜像构建

```bash
make docker-build-master           # 主服务
make docker-build-agent-runner     # Agent Runner
make docker-build-agent-production # 不附加开发观测 features 的 Agent 镜像
make docker-build-app-runtime      # 多语言 UserApp 运行时
```

构建入口会准备所需资产并校验 Pingap、工具链及基础镜像契约。按错误提示修正依赖或镜像配对后重试，不跳过版本门禁。UserApp 使用统一运行时与 app-cli 编排，使用方法见 [app-cli 指南](../development/app-cli.md)。

## UserApp 元数据持久化

Docker Compose 的 UserApp 元数据使用本地 Turso，Agent 存储独立配置。二进制需要包含默认启用的 `userapp-turso` feature。Kubernetes 的 UserApp 元数据使用 PostgreSQL；本地 Turso 目录只能由一个 rcoder 实例持有。

| 启动方式 | `/app/data` 的来源 |
|---|---|
| `make dev-up`、`make dev-restart`、`make dev-hot` 配套环境 | 自动叠加 `docker-compose.turso-volume.yml`，使用项目级 named volume `rcoder-userapp-data` |
| 仅使用 `docker/docker-compose.yml` | bind mount，默认宿主目录为 `docker/data/rcoder`；可通过 `RCODER_DATA_DIR` 指定专用绝对目录 |

named volume 方式可避免高频本地数据库写入穿过 macOS bind mount。仅使用基础 Compose 文件时，数据路径相对 Compose 文件目录解析；自定义目录必须对实际服务用户可写。确认挂载时只检查所需字段，避免公开完整环境变量和 Docker inspect 输出：

```bash
RCODER_CONTAINER_ID=$(docker compose -f docker/docker-compose.yml -f docker/docker-compose.turso-volume.yml ps -q rcoder)
docker inspect "$RCODER_CONTAINER_ID" --format '{{range .Mounts}}{{if eq .Destination "/app/data"}}{{.Type}} {{.Source}} -> {{.Destination}}{{end}}{{end}}'
```

数据库位于 `/app/data/userapp.turso.db`，journal sidecar 与 `.userapp-instance.lock` 位于同一目录。启动会校验目录访问、实例独占、本地文件系统、迁移与恢复条件；失败会阻止启动并返回错误。普通应用 purge 不包含这个控制面数据目录。

- 使用本地文件系统，NFS/SMB 不受支持；bind mount 被拒绝时改用上述 named volume overlay。
- 数据库、sidecar 与锁文件应为普通文件，禁止软链接或硬链接别名。数据目录的软链接会先解析再加锁，不能通过另一路径启动第二实例。
- 保持每个部署的数据目录独立；运行中不替换文件、不增加链接，不通过删除锁或清库处理启动失败。
- `down` 和服务重建保留 named volume；`down -v` 会删除数据卷，不用于需要保留数据的环境。停止 Agent 计算资源也应保留其工作区数据。

备份时先停止 rcoder，再复制整个数据目录或对应卷内容，完成后再启动。恢复时停服，恢复到独立目录或卷并核对健康与应用身份，保留原始备份。不要只复制运行中的主数据库文件。

旧 SQLite 后端配置已移除，`RCODER_USERAPP_STORAGE_BACKEND=sqlite` 和 `RCODER_USERAPP_SQLITE_PATH` 会明确报错；旧 `userapp.sqlite3` 不能直接当作 Turso 数据库使用。测试持久化时使用本轮专用数据目录，验证仅重建主容器后身份仍保留，不擦除现有开发数据。

## 工作区与终端

主要挂载包括项目工作区、Computer 工作区、UserApp 工作区、`docker/logs/` 和 Cargo/包管理器缓存。具体源目录以 Compose 配置为准，不手动替换活跃工作区或清理其他应用的数据。

主容器默认禁用 ttyd。动态容器的 ttyd 默认以 `root` 运行，未指定工作目录时进入 `/home/user`；账户的 `HOME=/root` 与当前工作目录不同。需要普通用户时显式设置 `TTYD_USER=user`，重建镜像并更新目标容器后核对实际行为。

## 观测与排障

追踪链为 RCoder/Agent → otel-collector → Tempo；日志经 fluent-bit → Loki，Grafana 聚合展示。指标、dial9 与 hotpath 的配置和查询方法见[可观测性指南](../observability.md)。

排障先检查当前环境的容器状态、镜像身份、挂载和对应日志：

```bash
docker compose -f docker/docker-compose.yml -f docker/docker-compose.turso-volume.yml ps
docker logs --tail 100 "$RCODER_CONTAINER_ID"
docker exec "$RCODER_CONTAINER_ID" sh -c 'ls -lt /app/logs | head'
docker stats --no-stream "$RCODER_CONTAINER_ID"
```

RCoder 还有 `/app/logs/` 下按天滚动的 JSON 文件日志；`docker logs` 为空不代表没有错误。动态 Agent 的 stdout、桌面日志与容器状态需要分别检查。Grafana 无数据时核对后端健康、采集器和查询时间窗，再用真实请求关联 trace_id。配置失败应检查挂载及覆盖优先级，避免回显密钥。

## 验证入口

| 目的 | 命令或工具 |
|---|---|
| Compose chat/SSE | `make test-e2e-compose` |
| UserApp、生命周期、构建部署及持久化 | `make test-e2e` |
| Compose 制品部署 | `make test-e2e-compose-deploy` |
| 手动检查 VNC、音频、IME | [桌面测试页](../../docker/test-page/README.md) |
| 手动检查文件服务 API | [文件服务请求集](../../docker/tests/README.md) |

严格套件要求与筛选方法见 [E2E 说明](../../tests-e2e/tools/README.md)。手动页面和请求集用于定位问题，不能代替完整 E2E 报告。Kubernetes 验证入口见 [Kubernetes 指南](kubernetes.md)；控制平面直接运行在宿主机时见[宿主机部署指南](host.md)。
