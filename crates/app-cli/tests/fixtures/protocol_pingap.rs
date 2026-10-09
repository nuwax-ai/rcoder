//! Controlled proxy protocol stand-in. Uses the real configuration parser and
//! digest encoding; it does not execute Pingap's routing/plugin engine.
use anyhow::{Context, Result, ensure};
use pingap_config::PingapConfig;
use serde_json::{Value, json};
use std::{
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    path::PathBuf,
    sync::{Arc, RwLock},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

struct Applied {
    config: PingapConfig,
    status: Value,
    publication: String,
    marker_status: u16,
}

fn load(path: &PathBuf, instance: &str, attempt: u64) -> Result<Applied> {
    let bytes = std::fs::read(path)?;
    let config = PingapConfig::new(&bytes, true)?;
    config.validate()?;
    let plugin = serde_json::to_value(
        config
            .plugins
            .get("rcoder:publication")
            .context("fixture publication missing")?,
    )?;
    let publication = plugin["data"]
        .as_str()
        .context("fixture publication UUID missing")?
        .to_owned();
    let hash = config.hash()?;
    let digest = app_cli::proxy::compiler::configuration_digest(&config)?;
    let ms = SystemTime::now().duration_since(UNIX_EPOCH)?.as_millis() as u64;
    let applied = json!({"attempt_id": attempt, "operation_id": publication, "config_hash": hash, "config_digest": digest, "applied_at_unix_ms": ms});
    Ok(Applied {
        config,
        marker_status: plugin["status"].as_u64().unwrap_or(200) as u16,
        publication: publication.clone(),
        status: json!({"schema_version":1,"process_id":std::process::id(),"process_instance_id":instance,"applied":applied,
            "last_attempt":{"attempt_id":attempt,"operation_id":publication,"config_hash":hash,"config_digest":digest,"state":"applied","failure":null}}),
    })
}

pub fn run(args: &[String]) -> Result<()> {
    if args.iter().any(|arg| arg == "--apply-protocol-version") {
        println!("1");
        return Ok(());
    }
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
    let applied = load(&path, &instance, 1)?;
    let listeners: Vec<String> = applied
        .config
        .servers
        .values()
        .flat_map(|server| server.addr.split(',').map(|addr| addr.trim().to_string()))
        .collect();
    let state = Arc::new(RwLock::new(applied));
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
    if let Some(pid_file) = std::env::var_os("PROTOCOL_PROXY_PID_FILE") {
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
    let mut previous = std::fs::read(&path)?;
    let mut attempt = 1;
    loop {
        std::thread::sleep(Duration::from_millis(50));
        let Ok(bytes) = std::fs::read(&path) else {
            continue;
        };
        if bytes == previous {
            continue;
        }
        attempt += 1;
        if let Ok(applied) = load(&path, &instance, attempt) {
            *state
                .write()
                .map_err(|_| anyhow::anyhow!("fixture state poisoned"))? = applied;
            previous = bytes;
        }
    }
}

fn respond(
    mut stream: TcpStream,
    state: Arc<RwLock<Applied>>,
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
        if let Some(fault) = std::env::var_os("PROTOCOL_PROXY_ADMIN_FAULT") {
            match std::fs::read_to_string(fault).unwrap_or_default().trim() {
                "failed" if path.starts_with("/api/apply-status") => {
                    let mut observation = state
                        .read()
                        .map_err(|_| anyhow::anyhow!("fixture state poisoned"))?
                        .status
                        .clone();
                    observation["applied"] = Value::Null;
                    observation["last_attempt"]["state"] = json!("failed");
                    observation["last_attempt"]["failure"] = json!({"category":"fixture","message":"controlled post-spawn application failure"});
                    return send(&mut stream, 200, &observation.to_string(), None);
                }
                "401" => return send(&mut stream, 401, "fault", None),
                "invalid" => return send(&mut stream, 200, "{broken", None),
                "timeout" => {
                    std::thread::sleep(Duration::from_secs(30));
                    return Ok(());
                }
                _ => {}
            }
        }
        let state = state
            .read()
            .map_err(|_| anyhow::anyhow!("fixture state poisoned"))?;
        let body = if path.starts_with("/api/apply-status") {
            state.status.to_string()
        } else {
            let upstreams: serde_json::Map<String, Value> = state
                .config
                .upstreams
                .keys()
                .map(|name| (name.clone(), json!({"healthy":1,"total":1})))
                .collect();
            json!({"config_hash":state.status["applied"]["config_hash"],"upstreams":upstreams})
                .to_string()
        };
        return send(&mut stream, 200, &body, None);
    }
    let (publication, marker_status, config) = {
        let state = state
            .read()
            .map_err(|_| anyhow::anyhow!("fixture state poisoned"))?;
        (
            state.publication.clone(),
            state.marker_status,
            state.config.clone(),
        )
    };
    if path.starts_with("/_pub/") {
        ensure!(
            path == format!("/_pub/{publication}"),
            "late marker publication"
        );
        return send(&mut stream, marker_status, &publication, Some(&publication));
    }
    if config.locations.contains_key("standby") {
        return send(
            &mut stream,
            503,
            "service stopped or restarting",
            Some(&publication),
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
