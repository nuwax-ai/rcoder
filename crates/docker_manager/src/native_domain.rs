//! Docker-daemon evidence for a CLI generation whose whole container exited.
//! The private container exec only publishes a receipt; business journals and
//! application desired state are left to their existing coordinators.
use anyhow::{Context, Result, ensure};
use bollard::{Docker, models::ContainerCreateBody, query_parameters::ListContainersOptions};
use runtime_supervisor::domain::{DOMAIN_ENV, DOMAIN_LABEL, PhysicalDomain, Retirement};
use sha2::{Digest, Sha256};
use std::collections::HashMap;

pub(crate) async fn stamp(client: &Docker, body: &mut ContainerCreateBody) -> Result<()> {
    let authority = client
        .info()
        .await?
        .id
        .context("Docker daemon identity missing")?;
    let host = body
        .host_config
        .as_ref()
        .context("builder host config missing")?;
    let mut mounts = Vec::new();
    for mount in host.mounts.iter().flatten() {
        if let (Some(source), Some(target)) = (&mount.source, &mount.target)
            && target.starts_with("/home/user/")
        {
            mounts.push((source.clone(), target.clone()));
        }
    }
    for bind in host.binds.iter().flatten() {
        let mut parts = bind.split(':');
        if let (Some(source), Some(target)) = (parts.next(), parts.next())
            && target.starts_with("/home/user/")
        {
            mounts.push((source.to_owned(), target.to_owned()));
        }
    }
    mounts.sort();
    mounts.dedup();
    ensure!(!mounts.is_empty(), "builder data mounts missing");
    let volume: String = Sha256::digest(serde_json::to_vec(&mounts)?)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    let domain = PhysicalDomain {
        authority,
        volume,
        instance: uuid::Uuid::new_v4().to_string(),
    };
    body.labels
        .get_or_insert_with(HashMap::new)
        .insert(DOMAIN_LABEL.into(), domain.instance.clone());
    let env = body.env.get_or_insert_with(Vec::new);
    env.retain(|s| !s.starts_with(&format!("{DOMAIN_ENV}=")));
    env.push(format!("{DOMAIN_ENV}={}", serde_json::to_string(&domain)?));
    Ok(())
}

pub(crate) async fn reconcile(client: &Docker, physical_id: &str, app: &str) -> Result<()> {
    shared_types::validate_identifier(app, "app_id").map_err(|e| anyhow::anyhow!("{e}"))?;
    let inspect = client.inspect_container(physical_id, None).await?;
    ensure!(
        inspect.id.as_deref() == Some(physical_id),
        "builder physical identity changed"
    );
    let Some(domain) = inspect
        .config
        .as_ref()
        .and_then(|c| c.env.as_ref())
        .and_then(|env| {
            env.iter()
                .find_map(|s| s.strip_prefix(&format!("{DOMAIN_ENV}=")))
        })
    else {
        return Ok(()); // Older containers have no domain proof; do not invent one.
    };
    let domain: PhysicalDomain = serde_json::from_str(domain)?;
    ensure!(
        client.info().await?.id.as_deref() == Some(&domain.authority),
        "Docker daemon identity changed"
    );
    let workspace = format!("/home/user/{app}");
    let exec = |args: Vec<String>| {
        crate::runtime::docker_app_runtime::execute_container_command(client, physical_id, args)
    };
    let result = exec(vec![
        "app-cli".into(),
        "--app-cli-domain-recovery".into(),
        "inspect".into(),
        workspace.clone(),
    ])
    .await?;
    ensure!(
        result.exit_code == 0,
        "inspect owner recovery: {}",
        result.stderr
    );
    let pending: Vec<Retirement> = serde_json::from_str(&result.stdout)?;
    for proof in pending {
        if proof.domain == domain {
            continue;
        } // A same-container process failure uses guardians.
        ensure!(
            proof.binding.component == "app-cli"
                && proof.binding.resource == std::path::Path::new(&workspace)
                && proof.domain.authority == domain.authority
                && proof.domain.volume == domain.volume,
            "previous owner physical/volume binding differs"
        );
        // all=true is essential: a stopped container can be started again and
        // does not grant permanent retirement. Labels were stamped before create.
        let previous = client
            .list_containers(Some(ListContainersOptions {
                all: true,
                filters: Some(HashMap::from([(
                    "label".into(),
                    vec![format!("{DOMAIN_LABEL}={}", proof.domain.instance)],
                )])),
                ..Default::default()
            }))
            .await?;
        ensure!(
            previous.is_empty(),
            "previous owner container still exists; retirement is not authorized"
        );
        let result = exec(vec![
            "app-cli".into(),
            "--app-cli-domain-recovery".into(),
            "confirm".into(),
            workspace.clone(),
            serde_json::to_string(&proof)?,
        ])
        .await?;
        ensure!(
            result.exit_code == 0,
            "confirm owner physical exit: {}",
            result.stderr
        );
    }
    Ok(())
}

pub(crate) async fn reconcile_bounded(client: &Docker, physical_id: &str, app: &str) {
    match tokio::time::timeout(
        std::time::Duration::from_secs(15),
        reconcile(client, physical_id, app),
    )
    .await
    {
        Ok(Ok(())) => {}
        result => tracing::warn!(%app, %physical_id, ?result,
            "Management process recovery could not be confirmed; physical container remains available"),
    }
}
