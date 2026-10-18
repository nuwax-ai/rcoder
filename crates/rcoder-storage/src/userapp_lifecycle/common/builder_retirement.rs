//! A registry retirement shares the original destructive operation's root CAS.
use super::{compute, ops};
use crate::db::schema::Backend;
use anyhow::{Result, ensure};
use shared_types::{BuilderControlTarget, ServiceType};
use toasty::Executor;

pub(crate) async fn authorize_builder_registration_retirement(
    tx: &mut dyn Executor,
    target: &BuilderControlTarget,
) -> Result<Option<shared_types::BuilderRegistryIdentity>> {
    let context = &target.context;
    context
        .validate_identity(&context.app_id)
        .map_err(anyhow::Error::msg)?;
    let (_, operation) = ops::current(
        tx,
        Backend::Postgres,
        &context.app_id,
        &context.operation_id,
        &context.lifecycle_id,
    )
    .await?;
    let receipt = shared_types::captured_builder_deletion_for_execution(&operation, context)
        .map_err(anyhow::Error::msg)?;
    let captured = receipt.runtime;
    compute::check_business_authority(tx, &operation).await?;
    ensure!(
        captured.app_id == context.app_id && captured.resource_binding == target.resource_binding,
        "Captured builder deletion ownership changed"
    );
    let original_compute = captured
        .resources
        .iter()
        .filter(|resource| match resource.kind {
            shared_types::AppResourceKind::Container
            | shared_types::AppResourceKind::StatefulSet => true,
            shared_types::AppResourceKind::Deployment
            | shared_types::AppResourceKind::Service
            | shared_types::AppResourceKind::ConfigMap
            | shared_types::AppResourceKind::Secret
            | shared_types::AppResourceKind::HttpRoute
            | shared_types::AppResourceKind::PersistentVolumeClaim
            | shared_types::AppResourceKind::HostPath
            | shared_types::AppResourceKind::File => false,
        })
        .collect::<Vec<_>>();
    match (&target.workload, original_compute.as_slice()) {
        (None, []) => {}
        (Some(actual), [original])
            if actual.kind == original.kind
                && actual.name == original.name
                && actual.uid == original.uid => {}
        _ => anyhow::bail!("Builder retirement target differs from the original deletion capture"),
    }
    if let Some(workload) = &target.workload {
        let binding = ops::get_resource_binding(
            tx,
            Backend::Postgres,
            &ServiceType::UserappBuilder,
            &workload.uid,
        )
        .await?;
        if let Some(binding) = &binding {
            binding
                .validate(context, &workload.uid)
                .map_err(anyhow::Error::msg)?;
        }
        if let Some(captured) = &target.resource_binding {
            ensure!(
                binding.as_ref() == Some(captured),
                "Captured builder resource binding changed during cleanup"
            );
        }
    }
    Ok(receipt.registry)
}
