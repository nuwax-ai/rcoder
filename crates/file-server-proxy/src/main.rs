//! file-server-proxy 独立二进制入口。两种构建形态、一套 CLI/env 契约：
//!
//! - **default feature（npm/Electron 独立分发形态）**：file-server 以 lib 方式集成，
//!   `--embed`（或 `EMBED_FILE_SERVER=1`）装配进程内直连——rust 域请求直接
//!   `router.oneshot()` 进 file-server 的 axum Router，**无内部监听端口**；
//!   单二进制 = 分流代理 + rust 上游。TS nuwax-file-server 由 npm CLI 托管于
//!   随机端口（`--ts-port` / `TS_UPSTREAM_PORT`）。
//! - **`--no-default-features`（容器 supervisord 纯转发形态）**：上游由外部进程
//!   提供（file-server 路由 merge 进 rcoder/agent_runner 的 8086），本代理只做
//!   60000 分流转发。
//!
//! 配置优先级：**CLI 参数 > 环境变量 > 默认值**。
//!
//! | 项 | CLI | env | 默认 |
//! |---|---|---|---|
//! | 监听端口 | `--port N` | `FILE_SERVER_PORT` | 60000 |
//! | rust 上游端口（仅纯转发形态使用） | `--rust-port N` | `RUST_UPSTREAM_PORT` | 8086 |
//! | TS 上游端口 | `--ts-port N` | `TS_UPSTREAM_PORT` | 60001 |
//! | 路由策略 | `--policy P` | `ROUTE_POLICY` | `all_rust` |
//! | 内嵌直连装配 | `--embed` / `--no-embed` | `EMBED_FILE_SERVER=1/0` | 不装配（纯转发，容器安全缺省） |
//!
//! 策略词汇（serde/CLI/env/helm 同源）：`userapp_split | all_rust | all_ts | ts_first`
//! （语义见 `RoutePolicy` 文档）。容器内切换模式：改 supervisord conf 的
//! command/env 后 `supervisorctl reread && supervisorctl update`，重启生效。
//!
//! 失败即退出非零（bind 冲突/非法配置等）——supervisord/守护 CLI 重试可见。

use file_server_proxy::{FileServerProxyConfig, RoutePolicy};
use shared_types::{AGENT_FILE_SERVER_PORT, NUWAX_FILE_SERVER_INTERNAL_PORT};

/// CLI 参数覆盖项（未指定的项为 None，交给 env/默认兜底）。
#[derive(Default, Debug, PartialEq, Eq)]
struct CliOverrides {
    listen_port: Option<u16>,
    rust_upstream_port: Option<u16>,
    ts_upstream_port: Option<u16>,
    policy: Option<RoutePolicy>,
    embed: Option<bool>,
}

/// 启动设置（CLI > env > 默认 归一后的终值）。
#[derive(Debug, PartialEq, Eq)]
struct Settings {
    listen_port: u16,
    rust_upstream_port: u16,
    ts_upstream_port: u16,
    policy: RoutePolicy,
    /// true = 装配进程内直连（feature 编译 + 显式启用）；false = 纯转发。
    embed: bool,
}

fn usage() -> String {
    format!(
        "file-server-proxy {}\n\n\
         Usage:\n  \
         file-server-proxy [--policy <userapp_split|all_rust|all_ts|ts_first>] \\\n \
         [--port <60000>] [--rust-port <8086>] [--ts-port <60001>] \\\n \
         [--embed|--no-embed] [--version]\n\n\
         CLI args take precedence over env (FILE_SERVER_PORT / RUST_UPSTREAM_PORT /\n\
         TS_UPSTREAM_PORT / ROUTE_POLICY / EMBED_FILE_SERVER). Default policy is\n\
         all_rust; embed is off by default (pure forward mode for containers).\n\n\
         Policies:\n  \
         userapp_split  /api/userapp* or x-service-type:userapp -> rust; rest -> ts\n  \
         all_rust       everything -> rust upstream\n  \
         all_ts         everything -> ts upstream\n  \
         ts_first       only /api/userapp* -> rust; legacy paths -> ts (even with\n\
         x-service-type header - ts handles service_type in-band)",
        env!("CARGO_PKG_VERSION")
    )
}

fn fail(message: String) -> ! {
    eprintln!("file-server-proxy: {message}");
    std::process::exit(1);
}

fn parse_port(flag: &str, value: &str) -> Result<u16, String> {
    value
        .trim()
        .parse()
        .map_err(|e| format!("invalid {flag} {value:?}: {e}"))
}

/// CLI 参数解析（手写轻量解析，file-server bin 同款风格；纯函数供单测）。
fn parse_cli_args(args: &[String]) -> Result<CliOverrides, String> {
    let mut out = CliOverrides::default();
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        let mut value = |flag: &str| -> Result<&String, String> {
            iter.next()
                .ok_or_else(|| format!("missing value for {flag}"))
        };
        match arg.as_str() {
            "--policy" => {
                out.policy = Some(
                    file_server_proxy::parse_route_policy(value("--policy")?)
                        .map_err(|e| format!("--policy: {e}"))?,
                );
            }
            "--port" => out.listen_port = Some(parse_port("--port", value("--port")?)?),
            "--rust-port" => {
                out.rust_upstream_port = Some(parse_port("--rust-port", value("--rust-port")?)?);
            }
            "--ts-port" => {
                out.ts_upstream_port = Some(parse_port("--ts-port", value("--ts-port")?)?);
            }
            "--embed" => out.embed = Some(true),
            "--no-embed" => out.embed = Some(false),
            other => return Err(format!("unknown argument {other:?}\n\n{}", usage())),
        }
    }
    Ok(out)
}

fn env_port(key: &str, default: u16) -> Result<u16, String> {
    match std::env::var(key) {
        Ok(value) => value
            .trim()
            .parse()
            .map_err(|e| format!("invalid {key}: {e}")),
        Err(std::env::VarError::NotPresent) => Ok(default),
        Err(e) => Err(format!("read {key}: {e}")),
    }
}

/// env 布尔词汇解析（纯函数）：1/true 启用，0/false 禁用，其他值 None（报错）。
fn parse_env_bool_value(raw: &str) -> Option<bool> {
    match raw.trim() {
        "1" | "true" => Some(true),
        "0" | "false" => Some(false),
        _ => None,
    }
}

/// env 布尔（词汇：1/true 启用，0/false 禁用；其他值报错——消歧义）。
fn env_bool(key: &str) -> Result<Option<bool>, String> {
    match std::env::var(key) {
        Ok(value) => parse_env_bool_value(&value)
            .map(Some)
            .ok_or_else(|| format!("invalid {key} {value:?}: expected 1|0|true|false")),
        Err(std::env::VarError::NotPresent) => Ok(None),
        Err(e) => Err(format!("read {key}: {e}")),
    }
}

fn env_policy() -> Result<RoutePolicy, String> {
    match std::env::var("ROUTE_POLICY") {
        Ok(value) => {
            file_server_proxy::parse_route_policy(&value).map_err(|e| format!("ROUTE_POLICY: {e}"))
        }
        Err(std::env::VarError::NotPresent) => Ok(RoutePolicy::AllRust),
        Err(e) => Err(format!("read ROUTE_POLICY: {e}")),
    }
}

/// 归一：CLI > env > 默认。
fn resolve_settings(cli: CliOverrides) -> Result<Settings, String> {
    Ok(Settings {
        listen_port: match cli.listen_port {
            Some(port) => port,
            None => env_port("FILE_SERVER_PORT", AGENT_FILE_SERVER_PORT)?,
        },
        rust_upstream_port: match cli.rust_upstream_port {
            Some(port) => port,
            None => env_port("RUST_UPSTREAM_PORT", 8086)?,
        },
        ts_upstream_port: match cli.ts_upstream_port {
            Some(port) => port,
            None => env_port("TS_UPSTREAM_PORT", NUWAX_FILE_SERVER_INTERNAL_PORT)?,
        },
        policy: cli.policy.unwrap_or(env_policy()?),
        // 未显式指定（CLI/env 都没给）= 纯转发：容器形态安全缺省
        embed: cli
            .embed
            .or(env_bool("EMBED_FILE_SERVER")?)
            .unwrap_or(false),
    })
}

/// 纯代理形态（未装配直连）的日志：console fmt + `RUST_LOG`。
fn init_tracing_plain() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();
}

#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|a| a == "--version" || a == "-V") {
        println!("file-server-proxy {}", env!("CARGO_PKG_VERSION"));
        return;
    }
    if args.iter().any(|a| a == "-h" || a == "--help") {
        eprintln!("{}", usage());
        return;
    }
    let settings = match parse_cli_args(&args).and_then(resolve_settings) {
        Ok(settings) => settings,
        Err(e) => fail(e),
    };

    // 内嵌直连装配（feature 编译 + 显式启用）。日志与 file-server 的 file layer
    // 必须一次组装（全局 subscriber 只能 init 一次）。
    #[cfg(feature = "embed-file-server")]
    let _log_guard = if settings.embed {
        match prepare_embed() {
            Ok(guard) => guard,
            Err(e) => fail(e),
        }
    } else {
        init_tracing_plain();
        None
    };
    #[cfg(not(feature = "embed-file-server"))]
    {
        init_tracing_plain();
        if settings.embed {
            tracing::warn!(
                "--embed/EMBED_FILE_SERVER=1 被忽略: 二进制未编译 embed-file-server \
                 feature (--no-default-features 纯转发形态)"
            );
        }
    }

    file_server_proxy::init(FileServerProxyConfig {
        listen_port: settings.listen_port,
        rust_upstream_port: settings.rust_upstream_port,
        ts_upstream_port: settings.ts_upstream_port,
        policy: settings.policy,
    });
    match file_server_proxy::try_start().await {
        Ok(address) => {
            #[cfg(feature = "embed-file-server")]
            let embed_note = if settings.embed {
                "直连内嵌"
            } else {
                "转发 127.0.0.1"
            };
            #[cfg(not(feature = "embed-file-server"))]
            let embed_note = "转发 127.0.0.1";
            tracing::info!(
                "file-server-proxy (standalone) 运行中: {address} → rust[{embed_note} \
                 :{}], ts=127.0.0.1:{} ({})",
                settings.rust_upstream_port,
                settings.ts_upstream_port,
                settings.policy.as_str()
            );
        }
        Err(e) => fail(e),
    }
    // 前台挂起：serve task 在后台持有监听口；supervisord/守护 CLI SIGTERM 杀进程
    // 即整体退出。直连形态无独立上游进程（router 在本进程内）。
    std::future::pending::<()>().await;
}

/// 直连装配：加载 file-server 配置（工作目录/日志）+ 一次组装全局 subscriber
/// （console + **双文件日志**：file-server.log 收 `file_server` target、
/// file-server-proxy.log 收 `file_server_proxy` target——代理自身的分流决策/
/// 上游错误/生命周期独立成文件，排查文件服务问题互不淹没）+ 独立全量 Router
/// 注册进直连通道。
///
/// 日志目录不可用（npm 本机默认 /app/... 建不了）时回退系统临时目录——对齐
/// TS 源工程 appConfig 的 LOG_BASE_DIR 回退行为（本机人体工学优先，容器内
/// 配置错误仍经 warn 留痕可见）。
///
/// 返回的 (file-server guard, proxy guard) 必须持有到 main 结束（文件日志完整
/// 刷盘——guard 仅 Drop 语义，tuple 解构成 `_` 绑定持有，与 file-server bin 同款）。
#[cfg(feature = "embed-file-server")]
fn prepare_embed() -> Result<
    Option<(
        file_server::logging::WorkerGuard,
        file_server::logging::WorkerGuard,
    )>,
    String,
> {
    let mut config = file_server::Config::load()
        .map_err(|e| format!("load embedded file-server config: {e:#}"))?;
    // 日志目录回退：默认 /app/... 是容器路径，npm/mac 本机不可写——回退 tmpdir
    let (file_layer, fs_guard) = match file_server::logging::build_file_layer(&config) {
        Ok(pair) => pair,
        Err(e) => {
            eprintln!(
                "file-server-proxy: 日志目录 {} 不可用 ({e:#}), 回退系统临时目录",
                config.service_log_dir.display()
            );
            config.service_log_dir = std::env::temp_dir().join("file-server-proxy").join("logs");
            file_server::logging::build_file_layer(&config)
                .map_err(|e| format!("build embedded file-server log layer: {e:#}"))?
        }
    };

    // proxy 独立文件日志（与 file-server 同目录、独立文件名、同款按日滚动与保留数）
    use tracing_subscriber::Layer as _;
    use tracing_subscriber::filter::{LevelFilter, Targets};
    let proxy_appender = tracing_appender::rolling::Builder::new()
        .rotation(tracing_appender::rolling::Rotation::DAILY)
        .filename_prefix("file-server-proxy.log")
        .max_log_files(config.service_log_retention_days)
        .build(&config.service_log_dir)
        .map_err(|e| format!("build proxy log appender: {e}"))?;
    let (proxy_writer, proxy_guard) = tracing_appender::non_blocking(proxy_appender);
    let proxy_file_layer = tracing_subscriber::fmt::layer()
        .with_ansi(false)
        .with_target(true)
        .with_writer(proxy_writer)
        .with_filter(
            Targets::new()
                .with_target("file_server_proxy", tracing::Level::INFO)
                .with_default(LevelFilter::OFF),
        );

    let filter = tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| {
        tracing_subscriber::EnvFilter::new(
            "file_server=info,file_server_proxy=info,tower_http=info",
        )
    });
    let console = tracing_subscriber::fmt::layer().with_target(true);
    use tracing_subscriber::layer::SubscriberExt;
    use tracing_subscriber::util::SubscriberInitExt;
    tracing_subscriber::registry()
        .with(file_layer)
        .with(proxy_file_layer)
        .with(filter)
        .with(console)
        .init();
    tracing::info!(
        log_dir = %config.service_log_dir.display(),
        "双文件日志已启用: file-server.log(内嵌 file-server) + file-server-proxy.log(代理)"
    );
    let server = file_server::FileServer::builder(config)
        .build()
        .map_err(|e| format!("build embedded file-server: {e:#}"))?;
    // 独立全量集（含 /、/health、swagger 与完整中间件栈 + /api/userapp 子树——
    // userApp 域拆至 file-server-userapp crate，组装经其 full_router）——直连行为
    // 与原独立 file-server bin 完全同构
    let router = file_server_userapp::full_router(&server)
        .map_err(|e| format!("build embedded file-server router: {e:#}"))?;
    file_server_proxy::set_in_process_router(router);
    tracing::info!("内嵌 file-server 直连已装配（进程内 router，无内部监听端口）");
    Ok(Some((fs_guard, proxy_guard)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn arg(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    /// CLI 参数解析：四值 policy、端口、embed 开关、非法/缺值/未知参数报错。
    #[test]
    fn cli_args_parse_full_surface() {
        let cli = parse_cli_args(&arg(&[
            "--policy",
            "ts_first",
            "--port",
            "61000",
            "--rust-port",
            "18086",
            "--ts-port",
            "16001",
            "--embed",
        ]))
        .unwrap();
        assert_eq!(cli.policy, Some(RoutePolicy::TsFirst));
        assert_eq!(cli.listen_port, Some(61000));
        assert_eq!(cli.rust_upstream_port, Some(18086));
        assert_eq!(cli.ts_upstream_port, Some(16001));
        assert_eq!(cli.embed, Some(true));

        assert_eq!(
            parse_cli_args(&arg(&["--no-embed"])).unwrap().embed,
            Some(false)
        );
        assert_eq!(parse_cli_args(&arg(&[])).unwrap(), CliOverrides::default());

        assert!(parse_cli_args(&arg(&["--policy", "bogus"])).is_err());
        assert!(parse_cli_args(&arg(&["--policy"])).is_err());
        assert!(parse_cli_args(&arg(&["--port", "notnum"])).is_err());
        assert!(parse_cli_args(&arg(&["--bogus"])).is_err());
    }

    /// 归一优先级：CLI 覆盖 env/默认；默认 policy=all_rust、embed=false（纯转发
    /// 安全缺省）。env 变体路径由本地冒烟覆盖（测试进程 env 不可独占设置）。
    #[test]
    fn resolve_cli_overrides_default_policy_all_rust() {
        let settings = resolve_settings(parse_cli_args(&arg(&[])).unwrap()).unwrap();
        assert_eq!(settings.policy, RoutePolicy::AllRust);
        assert!(!settings.embed);
        assert_eq!(settings.listen_port, AGENT_FILE_SERVER_PORT);

        let settings = resolve_settings(
            parse_cli_args(&arg(&["--policy", "ts_first", "--embed", "--port", "7"])).unwrap(),
        )
        .unwrap();
        assert_eq!(settings.policy, RoutePolicy::TsFirst);
        assert!(settings.embed);
        assert_eq!(settings.listen_port, 7);
    }

    /// env 布尔词汇：1/true/0/false 归一，非法值 None，trim 容差。
    /// （set_var 在 Rust 2024 是 unsafe 且项目禁 unsafe——测纯函数层。）
    #[test]
    fn env_bool_vocabulary() {
        assert_eq!(parse_env_bool_value("1"), Some(true));
        assert_eq!(parse_env_bool_value("true"), Some(true));
        assert_eq!(parse_env_bool_value("0"), Some(false));
        assert_eq!(parse_env_bool_value("false"), Some(false));
        assert_eq!(parse_env_bool_value(" 1\n"), Some(true));
        assert_eq!(parse_env_bool_value("yes"), None);
        assert_eq!(parse_env_bool_value(""), None);
        assert_eq!(parse_env_bool_value("TRUE"), None);
    }
}
