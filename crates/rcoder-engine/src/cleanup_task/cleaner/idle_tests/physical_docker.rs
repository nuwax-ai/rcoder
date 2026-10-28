//! Explicit opt-in physical Docker witness for the real cleaner/gRPC protocol.
//! Uses a controlled runtime hook, not a full deployed Main or an AI E2E test.
use super::*;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::process::{Command, Output};

const OWNER_LABEL: &str = "rcoder.validation.idle-sweep";

#[derive(Clone)]
pub(super) struct Target {
    pub(super) id: String,
    pub(super) name: String,
    owner: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct Inspection {
    id: String,
    name: String,
    state: State,
    config: Config,
}
#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct State {
    running: bool,
}
#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct Config {
    labels: HashMap<String, String>,
}

#[derive(Serialize)]
struct Witness<'a> {
    phase: &'static str,
    container_id: &'a str,
    container_name: &'a str,
    stops: usize,
    original_gone: bool,
    workspace_sha256: String,
}

fn witness(
    target: &Target,
    marker: &std::path::Path,
    phase: &'static str,
    stops: usize,
    original_gone: bool,
) {
    use sha2::Digest as _;
    let evidence = Witness {
        phase,
        container_id: &target.id,
        container_name: &target.name,
        stops,
        original_gone,
        workspace_sha256: hex::encode(sha2::Sha256::digest(std::fs::read(marker).unwrap())),
    };
    println!(
        "DOCKER_IDLE_WITNESS {}",
        serde_json::to_string(&evidence).unwrap()
    );
}

fn docker(args: &[&str]) -> Output {
    Command::new("docker")
        .args(args)
        .output()
        .expect("Docker CLI")
}

impl Target {
    fn inspection(&self) -> Inspection {
        let result = docker(&["inspect", &self.id]);
        assert!(
            result.status.success(),
            "inspect failed: {}",
            String::from_utf8_lossy(&result.stderr)
        );
        let mut rows: Vec<Inspection> = serde_json::from_slice(&result.stdout).unwrap();
        assert_eq!(rows.len(), 1);
        let actual = rows.pop().unwrap();
        assert_eq!(actual.id, self.id);
        assert_eq!(actual.name, format!("/{}", self.name));
        assert_eq!(actual.config.labels.get(OWNER_LABEL), Some(&self.owner));
        actual
    }

    fn gone(&self) -> bool {
        let result = docker(&[
            "ps",
            "-aq",
            "--no-trunc",
            "--filter",
            &format!("id={}", self.id),
        ]);
        assert!(result.status.success(), "absence observation failed");
        result.stdout.iter().all(u8::is_ascii_whitespace)
    }

    pub(super) fn stop_and_confirm_gone(&self) {
        self.inspection();
        let result = docker(&["stop", "--time", "2", &self.id]);
        assert!(result.status.success(), "original captured Stop failed");
        let result = docker(&["rm", &self.id]); // Never remove volumes.
        assert!(result.status.success(), "original captured remove failed");
        assert!(self.gone());
    }
}

struct OwnedDocker(Target);
impl Drop for OwnedDocker {
    fn drop(&mut self) {
        // Failure cleanup is bounded to this run's captured CID AND owner
        // label. Unknown observation cannot authorize a broader cleanup.
        let ids = docker(&[
            "ps",
            "-aq",
            "--no-trunc",
            "--filter",
            &format!("id={}", self.0.id),
            "--filter",
            &format!("label={OWNER_LABEL}={}", self.0.owner),
        ]);
        if ids.status.success() && String::from_utf8_lossy(&ids.stdout).trim() == self.0.id {
            let result = docker(&["rm", "--force", &self.0.id]);
            if !result.status.success() {
                eprintln!("Owned Docker fixture cleanup remains unconfirmed");
            }
        }
    }
}

#[tokio::test]
#[ignore = "requires Docker and RCODER_CLEANER_DOCKER_IMAGE set to a local Alpine image ID; never pulls images"]
async fn physical_docker_survives_first_shared_sweep_and_keeps_bind_data_after_second() {
    let image = std::env::var("RCODER_CLEANER_DOCKER_IMAGE").expect("explicit local fixture image");
    assert!(
        image.starts_with("sha256:") && image.len() == 71,
        "use an exact existing local image ID"
    );
    let mut fixture = Fixture::new(2).await;
    let owner = uuid::Uuid::new_v4().simple().to_string();
    let name = format!("rcoder-idle-sweep-{owner}");
    let data = fixture._directory.path().join("workspace");
    std::fs::create_dir(&data).unwrap();
    let marker = data.join("data-witness");
    std::fs::write(&marker, &owner).unwrap();
    let result = docker(&[
        "run",
        "--pull=never",
        "--detach",
        "--network",
        "none",
        "--name",
        &name,
        "--label",
        &format!("{OWNER_LABEL}={owner}"),
        "--mount",
        &format!("type=bind,src={},dst=/workspace", data.display()),
        "--entrypoint",
        "/bin/sh",
        &image,
        "-c",
        "trap 'exit 0' TERM; while :; do sleep 1 & wait $!; done",
    ]);
    assert!(
        result.status.success(),
        "owned fixture creation failed: {}",
        String::from_utf8_lossy(&result.stderr)
    );
    let id = String::from_utf8(result.stdout).unwrap().trim().to_owned();
    assert!(id.len() == 64 && id.bytes().all(|b| b.is_ascii_hexdigit()));
    let owned = OwnedDocker(Target {
        id,
        name,
        owner: owner.clone(),
    });
    *fixture.runtime.docker.lock().unwrap() = Some(owned.0.clone());
    fixture.replace_physical(&owned.0.id).await;

    fixture.cleaner.cleanup_once().await.unwrap();
    assert_eq!(fixture.runtime.stops.load(Ordering::SeqCst), 0);
    assert!(
        owned.0.inspection().state.running,
        "first complete sweep must retain the actual process"
    );
    assert_eq!(fixture.cleaner.state.projects.iter().len(), 2);
    assert_eq!(std::fs::read_to_string(&marker).unwrap(), owner);
    witness(&owned.0, &marker, "first_complete_sweep", 0, false);

    fixture.cleaner.cleanup_once().await.unwrap();
    assert_eq!(fixture.runtime.stops.load(Ordering::SeqCst), 1);
    assert!(
        owned.0.gone(),
        "second independent sweep must retire the original exact CID"
    );
    assert!(fixture.cleaner.state.projects.iter().is_empty());
    assert_eq!(
        std::fs::read_to_string(&marker).unwrap(),
        owner,
        "workspace data must remain after compute retirement"
    );
    witness(&owned.0, &marker, "second_complete_sweep", 1, true);
    fixture.close().await;
}
