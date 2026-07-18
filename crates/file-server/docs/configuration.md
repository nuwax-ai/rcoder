# file-server 配置与嵌入

file-server 支持三种启动方式：无配置默认启动、环境变量启动、配置文件启动。设置
`FILE_SERVER_CONFIG=/path/file-server.yaml` 后读取 YAML、YML、TOML 或 JSON；未设置时读取现有环境变量。配置文件允许只覆盖部分字段，未知字段会导致启动失败。独立二进制的优先级为“默认值 → 配置文件 → 环境变量”；lib 调用方直接传入的 `Config` 不会隐式读取进程环境。

```yaml
listen_host: 0.0.0.0
port: 60000
project_source_dir: /app/project_workspace
computer_workspace_dir: /app/computer-project-workspace
request_body_max_bytes: 1073741824
upload_max_file_size_bytes: 1073741824
git_enabled: true
git_diff_max_file_size_bytes: 16777216
git_diff_max_total_bytes: 67108864
git_file_content_max_bytes: 67108864
service_log_dir: /app/logs/file-server
service_log_retention_days: 7
```

独立二进制按 UTC 日期滚动 `file-server.log`，最多保留 7 个每日日志文件。
`FILE_SERVER_LOG_DIR` 和 `FILE_SERVER_LOG_RETENTION_DAYS` 可覆盖目录和保留天数，
`RUST_LOG` 完整覆盖默认的 `file_server=info,tower_http=info` 日志过滤规则。

嵌入其他 Rust 服务时直接传入强类型配置，不会读取环境变量或初始化全局日志：

```rust,ignore
let config = file_server::Config::default();
let server = file_server::FileServer::builder(config).build()?;
let app = server.router()?;
```

调用方也可以通过 `with_workspace_resolver` 注入自己的 workspace 定位实现，并自行决定
TCP listener、tracing subscriber 和 shutdown future。
