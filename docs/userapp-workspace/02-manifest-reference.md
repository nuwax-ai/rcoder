# Manifest v1 字段参考

## workspace.manifest.toml

```toml
schema_version = 1

[workspace]
name = "my-app"
description = "optional"

[pingap]
mode = "managed" # managed / extend / custom
# config = "pingap/" # extend/custom 必填
```

workspace manifest 不保存镜像、资源、Secret、端口暴露和版本保留数；这些属于平台策略。

## project.manifest.toml

```toml
schema_version = 1

[project]
service_id = "backend-go"
name = "Go Backend"
type = "go"       # node/java/python/go/rust/static
kind = "web"      # web/worker
enabled = true

[build]
command = ["sh", "scripts/build-standalone.sh"]
artifact = "artifact.zip"

# 可选：dev 阶段编译命令，缺省回落 [build].command。仅配了 [devrun] 的源码态
# dev 链路生效；不要求产出 artifact（纯检查命令如 type-check 可用）。
[devbuild]
command = ["pnpm", "run", "type-check"]

[run]
command = ["./server"]
migrate = []
depends_on = []
shutdown_timeout_seconds = 30

# 可选：dev 阶段热加载启动命令，缺省回落 [run].command。任一 enabled 服务配置
# 即把该 app 的 dev 链路切为源码态：dev/start·restart 编译（[devbuild] 优先）
# 后 app-cli 直接编排源码 workspace（跑源码，改码即生效），不再部署 .run 产物。
# 须监听 0.0.0.0:$PORT（注入同 [run]）；生产部署不读本段。
[devrun]
command = ["pnpm", "exec", "vite"]

[health]
startup_path = "/health"
readiness_path = "/health"
liveness_path = "/health"

[proxy]
path = "/api/go/"
strip_prefix = true
plugins = []
upstream_includes = []

[[logs.sources]]
id = "application"
glob = "application*.log"
format = "jsonl"
```

`service_id` 是稳定身份，必须符合 DNS-1123，并在 workspace 内唯一。重命名目录不会改变服务
身份。构建生成的 `release.lock.toml` 锁定依赖顺序、确定性端口、路由、日志源和运行时版本；
app-cli 不再从源 manifest 重复推导。

以下情况立即失败：未知字段或 enum、缺少/非 1 schema、重复 ID/路由、多个 `/`、
worker 声明代理、依赖缺失/环、危险相对路径、保留环境变量、空 argv。
