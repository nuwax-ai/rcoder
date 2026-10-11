//! Observe retained stopped compute without treating an absent Pod as a stop.
use super::*;

impl KubernetesRuntime {
    pub(crate) async fn observe_retained_builder_stopped(
        &self,
        target: &BuilderControlTarget,
    ) -> Result<bool> {
        target.validate().map_err(Error::ConfigurationError)?;
        let Some(expected) = target.workload.as_ref() else {
            return Ok(false);
        };
        if expected.kind != AppResourceKind::StatefulSet {
            return Ok(false);
        }
        let controllers: Api<StatefulSet> = Api::namespaced(self.client.clone(), &self.namespace);
        let current = controllers
            .get_opt(&expected.name)
            .await
            .map_err(|error| api_error("Observe retained builder controller", error))?;
        let Some(current) = current else {
            return Ok(false);
        };
        let stopped_controller = |controller: &StatefulSet| -> Result<bool> {
            let observed = workload_identity_with_binding(
                controller,
                &target.context,
                target.resource_binding.as_ref(),
                false,
            )?;
            if observed.uid != expected.uid || observed.name != expected.name {
                return Err(Error::Conflict(
                    "Captured builder controller changed during stop observation".into(),
                ));
            }
            Ok(controller.spec.as_ref().and_then(|spec| spec.replicas) == Some(0))
        };
        if !stopped_controller(&current)? {
            return Ok(false);
        }
        let pods: Api<Pod> = Api::namespaced(self.client.clone(), &self.namespace);
        // Scan ownerReferences, including Pods whose labels were removed or
        // that are already terminating. No remaining owner is guessed gone.
        let listed = pods
            .list(&kube::api::ListParams::default())
            .await
            .map_err(|error| api_error("Observe all retained builder owned Pods", error))?;
        if listed.items.iter().any(|pod| {
            pod.metadata
                .owner_references
                .as_ref()
                .is_some_and(|owners| {
                    owners.iter().any(|owner| {
                        owner.uid == expected.uid
                            && owner.kind == "StatefulSet"
                            && owner.api_version == "apps/v1"
                    })
                })
        }) {
            return Ok(false);
        }
        let canonical =
            self.agent_pod_name(&target.context.app_id, &ServiceType::UserappBuilder)?;
        if pods
            .get_opt(&canonical)
            .await
            .map_err(|error| api_error("Recheck canonical stopped builder Pod", error))?
            .is_some()
        {
            return Ok(false);
        }
        let after = controllers
            .get_opt(&expected.name)
            .await
            .map_err(|error| api_error("Recheck retained stopped controller", error))?;
        match after {
            Some(after) => stopped_controller(&after),
            None => Ok(false),
        }
    }
}
