# Pingap 配置

## managed

平台根据每个 web 服务的 `[proxy]` 自动生成 server、location 和 upstream。

**入口默认页（index.html 兜底）**：workspace 根放 `index.html` 且无服务声明
`[proxy].path = "/"` 时，额外注入 `workspaceIndex` upstream（app-cli 内置静态服务
:9081，serve workspace 根一级文件）与无 path 兜底 location——根路径与未匹配路径
展示该页面，用户改完刷新即生效（每次请求实时读文件）。服务路由恒优先；catch-all
服务独占 `/` 时不注入。`template-cli init` 会按所选服务自动生成导航页。

## extend

`[pingap].config` 指向单文件或目录。只允许定义 `plugins` 和 `storages`，项目通过
`plugins`、`upstream_includes` 引用。与平台生成名称冲突时失败，不做字段级覆盖。

## custom

用户提供完整原生 Pingap TOML，可使用单文件或 Pingap 多文件布局。workspace 服务写为：

```toml
[upstreams.backend]
addrs = ["rcoder://backend-go"]
```

app-cli 根据 release lock 转成 `127.0.0.1:<locked_port>`。引用不存在或 disabled 服务时失败。

## 生效和护栏

app-cli 加载配置、解析逻辑地址、检查冲突、调用 `PingapConfig::validate()`，再执行
`pingap -t`。通过后写入 `/run/app-cli/pingap/<release_id>/pingap.toml`，权限 `0600`。

- 统一 listener 只能是 `0.0.0.0:9080`，接受公网或内网来源；额外 listener 只能 loopback。
- 不要求公网域名或 HTTPS；内网 IP、内网域名和 HTTP 都是受支持的部署方式。
- Admin 始终关闭，运行时修改不作为权威配置。
- 每类对象默认最多 256 个，总配置最多 2 MiB。
- binary 和 `pingap-config` 必须锁到相同 commit；release lock 记录版本、commit 和平台
  版本化 app-runtime 镜像引用。
- TOML 明文会进入源码和保留的 Release 包，app-cli 不应主动打印完整配置。
