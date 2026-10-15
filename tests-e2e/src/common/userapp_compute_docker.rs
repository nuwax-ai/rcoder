//! Docker physical evidence for the shared UserApp dev compute scenario.

use super::userapp_compute::DevComputeProbe;
use async_trait::async_trait;
use serde_json::Value;
use std::process::Command;
use std::time::{Duration, Instant};

pub struct DockerDevProbe {
    pub expect_published: bool,
    mount_source: Option<String>,
    before_id: Option<String>,
}

impl DockerDevProbe {
    pub fn new(expect_published: bool) -> Self {
        Self {
            expect_published,
            mount_source: None,
            before_id: None,
        }
    }
}

fn docker(args: &[&str]) -> Result<String, String> {
    let output = Command::new("docker")
        .args(args)
        .output()
        .map_err(|e| e.to_string())?;
    if !output.status.success() {
        return Err(format!(
            "docker {}: {}",
            args.first().copied().unwrap_or(""),
            String::from_utf8_lossy(&output.stderr)
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

fn inspect(name: &str) -> Result<Value, String> {
    let raw = docker(&["inspect", "--format", "{{json .}}", name])?;
    serde_json::from_str(&raw).map_err(|e| format!("decode Docker inspect: {e}"))
}

fn workspace_mount<'a>(info: &'a Value, app_id: &str) -> Option<&'a str> {
    let destination = format!("/home/user/{app_id}");
    info["Mounts"].as_array()?.iter().find_map(|mount| {
        (mount["Destination"].as_str() == Some(destination.as_str()))
            .then(|| mount["Source"].as_str())
            .flatten()
    })
}

fn required_ports_published(info: &Value) -> bool {
    // Same builder port set as deploy_host_ports::published_ports_for(UserappBuilder).
    // The assertion catches the former restart path that omitted port bindings.
    [
        8086, 50051, 6080, 17681, 60000, 4224, 6089, 6090, 3010, 9080, 6091,
    ]
    .iter()
    .all(|port| {
        info["NetworkSettings"]["Ports"][format!("{port}/tcp")]
            .as_array()
            .is_some_and(|bindings| {
                bindings.iter().any(|binding| {
                    binding["HostPort"]
                        .as_str()
                        .is_some_and(|port| !port.is_empty())
                })
            })
    })
}

#[async_trait]
impl DevComputeProbe for DockerDevProbe {
    async fn prepare(&mut self, app_id: &str, marker: &str) -> Result<(), String> {
        let name = format!("rcoder-app-builder-{app_id}");
        let info = inspect(&name)?;
        if info["State"]["Running"] != true {
            return Err("builder is not running after workspace creation".into());
        }
        if self.expect_published && !required_ports_published(&info) {
            return Err("Published builder is missing a required host port".into());
        }
        self.mount_source = Some(
            workspace_mount(&info, app_id)
                .ok_or("builder workspace mount missing")?
                .to_owned(),
        );
        self.before_id = info["Id"].as_str().map(str::to_owned);
        let path = format!("/home/user/{app_id}/.e2e-compute-marker");
        docker(&[
            "exec",
            &name,
            "sh",
            "-c",
            "printf '%s' \"$1\" > \"$2\"",
            "e2e",
            marker,
            &path,
        ])?;
        Ok(())
    }

    async fn stopped(&mut self, app_id: &str) -> Result<(), String> {
        let name = format!("rcoder-app-builder-{app_id}");
        let ids = docker(&[
            "ps",
            "-aq",
            "--no-trunc",
            "--filter",
            &format!("name=^/{name}$"),
        ])?;
        let running = if ids.is_empty() {
            false
        } else {
            inspect(&name)?["State"]["Running"] == true
        };
        if running {
            return Err("builder still running after completed Stop".into());
        }
        let source = self
            .mount_source
            .as_deref()
            .ok_or("initial mount was not captured")?;
        if !std::path::Path::new(source).exists() {
            return Err("workspace source disappeared after Stop".into());
        }
        Ok(())
    }

    async fn restarted(&mut self, app_id: &str, marker: &str) -> Result<(), String> {
        let name = format!("rcoder-app-builder-{app_id}");
        let deadline = Instant::now() + Duration::from_secs(120);
        loop {
            if let Ok(info) = inspect(&name)
                && info["State"]["Running"] == true
            {
                let mount = workspace_mount(&info, app_id)
                    .ok_or("restarted builder workspace mount missing")?;
                if Some(mount) != self.mount_source.as_deref() {
                    return Err("builder workspace mount source changed".into());
                }
                if self.expect_published && !required_ports_published(&info) {
                    return Err("restarted Published builder lost a required host port".into());
                }
                let path = format!("/home/user/{app_id}/.e2e-compute-marker");
                let observed = docker(&["exec", &name, "cat", &path])?;
                if observed != marker {
                    return Err(format!(
                        "workspace marker changed after Restart: {observed:?}"
                    ));
                }
                if self.before_id.as_deref().is_none_or(str::is_empty)
                    || info["Id"].as_str().is_none_or(str::is_empty)
                {
                    return Err("Docker physical identity missing".into());
                }
                return Ok(());
            }
            if Instant::now() >= deadline {
                return Err("builder did not become running after Restart".into());
            }
            tokio::time::sleep(Duration::from_secs(2)).await;
        }
    }
}
