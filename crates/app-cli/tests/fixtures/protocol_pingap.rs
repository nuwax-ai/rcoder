//! Controlled proxy protocol stand-in. Uses the real configuration parser and
//! digest encoding; it does not execute Pingap's routing/plugin engine.
use anyhow::{Context, Result, ensure};
use pingap_config::PingapConfig;
use serde::Serialize;
use serde_json::json;
use std::{
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    path::PathBuf,
    sync::Arc,
    time::Duration,
};

struct Configuration {
    config: PingapConfig,
    hash: String,
    publication: Option<String>,
    marker_status: u16,
}

#[derive(Serialize)]
struct BasicWire<'a> {
    pid: String,
    version: &'static str,
    git_hash: &'static str,
    config_hash: &'a str,
    supported_plugins: Vec<&'static str>,
    upstream_healthy_status: std::collections::BTreeMap<String, UpstreamWire>,
}

#[derive(Serialize)]
struct UpstreamWire {
    healthy: u32,
    total: u32,
}

fn load(path: &PathBuf) -> Result<Configuration> {
    let bytes = std::fs::read(path)?;
    let config = PingapConfig::new(&bytes, true)?;
    config.validate()?;
    let plugin = config.plugins.get("rcoder:publication");
    let publication = plugin
        .and_then(|plugin| plugin.get("data"))
        .and_then(toml::Value::as_str)
        .map(str::to_owned);
    let hash = config.hash()?;
    let marker_status = plugin
        .and_then(|plugin| plugin.get("status"))
        .and_then(toml::Value::as_integer)
        .unwrap_or(200) as u16;
    Ok(Configuration {
        config,
        hash,
        marker_status,
        publication,
    })
}

pub fn run(args: &[String]) -> Result<()> {
    ensure!(
        !args
            .iter()
            .any(|arg| arg == "--apply-protocol-version" || arg == "--autoreload"),
        "official fixture has no private protocol or automatic reload"
    );
    let index = args
        .iter()
        .position(|arg| arg == "-c")
        .context("fixture requires -c")?;
    let path = PathBuf::from(
        args.get(index + 1)
            .context("fixture requires config path")?,
    );
    if args.iter().any(|arg| arg == "-t") {
        let cfg = PingapConfig::new(&std::fs::read(&path)?, true)?;
        cfg.validate()?;
        return Ok(());
    }
    let instance = uuid::Uuid::new_v4().to_string();
    let configuration = load(&path)?;
    let listeners: Vec<String> = configuration
        .config
        .servers
        .values()
        .flat_map(|server| server.addr.split(',').map(|addr| addr.trim().to_string()))
        .collect();
    let state = Arc::new(configuration);
    let admin = TcpListener::bind(std::env::var("PINGAP_ADMIN_ADDR")?)?;
    let user = std::env::var("PINGAP_ADMIN_USER")?;
    let password = std::env::var("PINGAP_ADMIN_PASSWORD")?;
    let admin_state = state.clone();
    std::thread::spawn(move || {
        for stream in admin.incoming().flatten() {
            let state = admin_state.clone();
            let user = user.clone();
            let password = password.clone();
            std::thread::spawn(move || {
                let _ = respond(stream, state, Some((user, password)));
            });
        }
    });
    for address in listeners {
        let listener = TcpListener::bind(address)?;
        let state = state.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let state = state.clone();
                std::thread::spawn(move || {
                    let _ = respond(stream, state, None);
                });
            }
        });
    }
    if state.publication.is_some()
        && let Some(pid_file) = std::env::var_os("PROTOCOL_PROXY_PID_FILE")
    {
        let file = PathBuf::from(pid_file);
        let temporary = file.with_extension("pending");
        std::fs::write(
            &temporary,
            serde_json::to_vec(
                &json!({"pid":std::process::id(),"instance":instance,"config":path}),
            )?,
        )?;
        std::fs::rename(temporary, file)?;
    }
    loop {
        std::thread::sleep(Duration::from_secs(60));
    }
}

fn respond(
    mut stream: TcpStream,
    state: Arc<Configuration>,
    credentials: Option<(String, String)>,
) -> Result<()> {
    stream.set_read_timeout(Some(Duration::from_secs(2)))?;
    let mut bytes = vec![0; 16384];
    let size = stream.read(&mut bytes)?;
    let request = String::from_utf8_lossy(&bytes[..size]);
    let path = request
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .unwrap_or("/");
    if let Some((user, password)) = credentials {
        let authorized = request
            .lines()
            .find_map(|line| {
                line.split_once(':')
                    .filter(|(name, _)| name.eq_ignore_ascii_case("authorization"))
            })
            .and_then(|(_, header)| {
                header
                    .trim()
                    .rsplit_once(':')
                    .and_then(|(_, ts)| ts.parse::<u64>().ok().map(|ts| (header.trim(), ts)))
            })
            .is_some_and(|(header, ts)| {
                header == app_cli::proxy::admin_probe::authorization_header(&user, &password, ts)
            });
        if !authorized {
            return send(&mut stream, 401, "denied", None);
        }
        // Capability probes contain no publication and must ignore faults
        // targeted at the actual serving instance.
        let mut rejected_hash = false;
        if state.publication.is_some()
            && let Some(fault) = std::env::var_os("PROTOCOL_PROXY_ADMIN_FAULT")
        {
            match std::fs::read_to_string(fault).unwrap_or_default().trim() {
                "failed" => rejected_hash = true,
                "401" => return send(&mut stream, 401, "fault", None),
                "invalid" => return send(&mut stream, 200, "{broken", None),
                "timeout" => {
                    std::thread::sleep(Duration::from_secs(30));
                    return Ok(());
                }
                _ => {}
            }
        }
        if path != "/api/basic" {
            return send(&mut stream, 404, "unsupported official endpoint", None);
        }
        let body = serde_json::to_string(&BasicWire {
            pid: std::process::id().to_string(),
            version: "0.15.0",
            git_hash: "8270a1ebb7a238ea86fa220215714613410378bb",
            config_hash: if rejected_hash {
                "fixture-rejected-config"
            } else {
                &state.hash
            },
            supported_plugins: vec![
                "mock",
                "directory",
                "response_headers",
                "request_id",
                "compression",
            ],
            upstream_healthy_status: state
                .config
                .upstreams
                .keys()
                .map(|name| {
                    (
                        name.clone(),
                        UpstreamWire {
                            healthy: 1,
                            total: 1,
                        },
                    )
                })
                .collect(),
        })?;
        return send(&mut stream, 200, &body, None);
    }
    let publication = state
        .publication
        .as_ref()
        .context("fixture has no business listener publication")?;
    let marker_status = state.marker_status;
    let config = &state.config;
    if path.starts_with("/_pub/") {
        ensure!(
            path == format!("/_pub/{publication}"),
            "late marker publication"
        );
        return send(&mut stream, marker_status, publication, Some(publication));
    }
    if config.locations.contains_key("standby") {
        return send(
            &mut stream,
            503,
            "service stopped or restarting",
            Some(publication),
        );
    }
    // Component fixture follows a real backend for an ordinary request, enough
    // to expose stale-generation restoration after a precise proxy kill.
    let upstream = config
        .upstreams
        .values()
        .flat_map(|upstream| &upstream.addrs)
        .next()
        .context("fixture backend missing")?;
    let address = upstream
        .split_whitespace()
        .next()
        .context("fixture backend address missing")?;
    let mut backend = TcpStream::connect(address)?;
    backend.set_read_timeout(Some(Duration::from_secs(2)))?;
    backend.write_all(
        format!("GET {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n").as_bytes(),
    )?;
    let mut response = Vec::new();
    backend.read_to_end(&mut response)?;
    stream.write_all(&response)?;
    Ok(())
}

fn send(stream: &mut TcpStream, status: u16, body: &str, publication: Option<&str>) -> Result<()> {
    let header = publication
        .map(|publication| format!("X-Rcoder-Publication: {publication}\r\n"))
        .unwrap_or_default();
    stream.write_all(format!("HTTP/1.1 {status} Fixture\r\n{header}Content-Length: {}\r\nContent-Type: application/json\r\nConnection: close\r\n\r\n{body}", body.len()).as_bytes())?;
    Ok(())
}
