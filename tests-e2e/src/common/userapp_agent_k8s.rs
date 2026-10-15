//! Read-only K8s identity evidence and API-controlled cleanup for an owned test builder.
use std::{collections::BTreeMap, time::Duration};

use anyhow::{Context, Result, anyhow, ensure};
use serde_json::{Value, json};
use tokio::time::Instant;

use super::{Backend, Env, report::JsonlReporter};

pub fn scoped_app_id(case_id: &str, backend: Backend) -> Result<String> {
    let prefix = case_id
        .get(..10)
        .context("case identity needs at least ten ASCII characters")?;
    ensure!(
        prefix
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit()),
        "case prefix must be lowercase alphanumeric"
    );
    let suffix = if backend == Backend::Openai { 'o' } else { 'a' };
    Ok(format!("e2epar{prefix}{suffix}"))
}

fn check_scope(env: &Env, app: &str) -> Result<()> {
    let case = std::env::var("E2E_CASE_ID").context("strict case identity is required")?;
    let prefix = case.get(..10).context("invalid case identity")?;
    ensure!(
        app.len() <= 22
            && app.contains(prefix)
            && app
                .bytes()
                .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit()),
        "builder is outside this case's app namespace"
    );
    ensure!(
        !env.k8s_ssh.is_empty()
            && !env.k8s_ssh.starts_with('-')
            && !env.k8s_ssh.chars().any(char::is_whitespace),
        "TEST_K8S_SSH must identify one SSH destination"
    );
    ensure!(
        !env.k8s_ns.is_empty()
            && env.k8s_ns != "nuwax-k8s-prod"
            && !env.k8s_ns.ends_with("-prod")
            && env.k8s_ns != "default",
        "an owned non-production test namespace is required"
    );
    Ok(())
}

fn quoted(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

fn inventory_command(namespace: &str, context: Option<&str>) -> String {
    let context = context
        .filter(|value| !value.is_empty())
        .map(|value| format!(" --context {}", quoted(value)))
        .unwrap_or_default();
    format!(
        "KUBECONFIG=\"$HOME/.kube/config\" kubectl{context} -n {} get statefulsets,pods,pvc -o json",
        quoted(namespace)
    )
}

async fn inventory(env: &Env) -> Result<Value> {
    let context = std::env::var("TEST_K8S_CONTEXT").ok();
    let command = inventory_command(&env.k8s_ns, context.as_deref());
    let output = tokio::time::timeout(
        Duration::from_secs(45),
        tokio::process::Command::new("ssh")
            .args([
                "-o",
                "BatchMode=yes",
                "-o",
                "ConnectTimeout=10",
                "--",
                &env.k8s_ssh,
                &command,
            ])
            .kill_on_drop(true)
            .output(),
    )
    .await
    .context("K8s identity query timed out")??;
    ensure!(
        output.status.success(),
        "K8s identity query failed ({})",
        output.status
    );
    serde_json::from_slice(&output.stdout).context("decode K8s identity inventory")
}

fn text<'a>(value: &'a Value, path: &[&str]) -> Result<&'a str> {
    let mut value = value;
    for key in path {
        value = &value[*key];
    }
    value
        .as_str()
        .filter(|value| !value.is_empty())
        .ok_or_else(|| anyhow!("missing K8s identity field {}", path.join(".")))
}

fn compact(resource: &Value) -> Value {
    json!({"kind":resource["kind"], "name":resource["metadata"]["name"], "namespace":resource["metadata"]["namespace"],
        "uid":resource["metadata"]["uid"], "resource_version":resource["metadata"]["resourceVersion"],
        "replicas":resource["spec"]["replicas"], "phase":resource["status"]["phase"],
        "volume_name":resource["spec"]["volumeName"],
        "containers":resource["status"]["containerStatuses"].as_array().map(|containers| containers.iter().map(|container|
            json!({"name":container["name"],"image":container["image"],"image_id":container["imageID"]})).collect::<Vec<_>>())})
}

#[derive(Clone)]
struct Snapshot {
    sts: Option<Value>,
    sts_uid: Option<String>,
    lifecycle: Option<String>,
    pods: Vec<Value>,
    claims: BTreeMap<String, (String, String)>,
    pvc_records: Vec<Value>,
}

fn snapshot(
    raw: &Value,
    namespace: &str,
    app: &str,
    retained_claims: &[String],
    captured: Option<&Snapshot>,
) -> Result<Snapshot> {
    let items = raw["items"]
        .as_array()
        .context("K8s inventory has no items")?;
    let name = format!("rcoder-app-builder-{app}");
    let states: Vec<_> = items
        .iter()
        .filter(|resource| {
            resource["kind"] == "StatefulSet" && resource["metadata"]["name"] == name
        })
        .collect();
    ensure!(states.len() <= 1, "ambiguous builder StatefulSet identity");
    let sts = states.first().copied();
    let mut claim_names: Vec<String> = retained_claims.to_vec();
    let mut lifecycle = None;
    let mut sts_uid = None;
    if let Some(sts) = sts {
        ensure!(
            text(sts, &["metadata", "namespace"])? == namespace,
            "builder belongs to a different namespace"
        );
        sts_uid = Some(text(sts, &["metadata", "uid"])?.to_owned());
        let identity = |key: &str| {
            sts["metadata"]["annotations"][key]
                .as_str()
                .or_else(|| sts["metadata"]["labels"][key].as_str())
        };
        ensure!(
            identity("rcoder.io/application-id") == Some(app),
            "builder application identity does not match"
        );
        lifecycle = Some(
            identity("rcoder.io/lifecycle-id")
                .filter(|value| !value.is_empty())
                .context("builder has no physical lifecycle identity")?
                .to_owned(),
        );
        if let Some(volumes) = sts["spec"]["template"]["spec"]["volumes"].as_array() {
            claim_names.extend(volumes.iter().filter_map(|volume| {
                volume["persistentVolumeClaim"]["claimName"]
                    .as_str()
                    .map(str::to_owned)
            }));
        }
        if let Some(templates) = sts["spec"]["volumeClaimTemplates"].as_array() {
            for template in templates {
                let claim = text(template, &["metadata", "name"])?;
                let prefix = format!("{claim}-{name}-");
                claim_names.extend(
                    items
                        .iter()
                        .filter(|resource| resource["kind"] == "PersistentVolumeClaim")
                        .filter_map(|resource| resource["metadata"]["name"].as_str())
                        .filter(|claim| {
                            claim.strip_prefix(&prefix).is_some_and(|ordinal| {
                                !ordinal.is_empty()
                                    && ordinal.bytes().all(|byte| byte.is_ascii_digit())
                            })
                        })
                        .map(str::to_owned),
                );
            }
        }
    }
    claim_names.sort();
    claim_names.dedup();
    let owned_pods: Vec<_> = items
        .iter()
        .filter(|resource| resource["kind"] == "Pod")
        .filter(|pod| {
            let named_for_app = pod["metadata"]["name"].as_str().is_some_and(|pod_name| {
                pod_name
                    .strip_prefix(&format!("{name}-"))
                    .is_some_and(|ordinal| {
                        !ordinal.is_empty() && ordinal.bytes().all(|byte| byte.is_ascii_digit())
                    })
            });
            named_for_app
                || pod["metadata"]["ownerReferences"]
                    .as_array()
                    .is_some_and(|owners| {
                        owners.iter().any(|owner| {
                            owner["kind"] == "StatefulSet"
                                && sts_uid
                                    .as_deref()
                                    .or_else(|| captured.and_then(|proof| proof.sts_uid.as_deref()))
                                    .is_some_and(|uid| owner["uid"] == uid)
                        })
                    })
        })
        .collect();
    for pod in &owned_pods {
        if let Some(volumes) = pod["spec"]["volumes"].as_array() {
            claim_names.extend(volumes.iter().filter_map(|volume| {
                volume["persistentVolumeClaim"]["claimName"]
                    .as_str()
                    .map(str::to_owned)
            }));
        }
    }
    claim_names.sort();
    claim_names.dedup();
    let mut claims = BTreeMap::new();
    let mut pvc_records = Vec::new();
    for claim in &claim_names {
        let matches: Vec<_> = items
            .iter()
            .filter(|resource| {
                resource["kind"] == "PersistentVolumeClaim"
                    && resource["metadata"]["name"] == *claim
            })
            .collect();
        ensure!(
            matches.len() == 1,
            "captured workspace PVC {claim} is missing or ambiguous"
        );
        let pvc = matches[0];
        ensure!(
            text(pvc, &["metadata", "namespace"])? == namespace
                && pvc["status"]["phase"] == "Bound",
            "workspace PVC {claim} is not Bound in this namespace"
        );
        claims.insert(
            claim.clone(),
            (
                text(pvc, &["metadata", "uid"])?.into(),
                text(pvc, &["spec", "volumeName"])?.into(),
            ),
        );
        pvc_records.push(compact(pvc));
    }
    for pod in &owned_pods {
        text(pod, &["metadata", "uid"])?;
        ensure!(
            text(pod, &["metadata", "namespace"])? == namespace,
            "builder Pod namespace mismatch"
        );
    }
    Ok(Snapshot {
        sts: sts.map(compact),
        sts_uid: sts_uid.clone(),
        lifecycle,
        pods: owned_pods
            .into_iter()
            .map(|pod| {
                let owner_uid = sts_uid
                    .as_deref()
                    .or_else(|| captured.and_then(|proof| proof.sts_uid.as_deref()));
                let by_owner =
                    pod["metadata"]["ownerReferences"]
                        .as_array()
                        .is_some_and(|owners| {
                            owners.iter().any(|owner| {
                                owner["kind"] == "StatefulSet"
                                    && owner_uid.is_some_and(|uid| owner["uid"] == uid)
                            })
                        });
                let by_captured_pod = captured.is_some_and(|proof| {
                    proof
                        .pods
                        .iter()
                        .any(|old| old["uid"] == pod["metadata"]["uid"])
                });
                let mut value = compact(pod);
                value["ownership_verified"] = (by_owner || by_captured_pod).into();
                value
            })
            .collect(),
        claims,
        pvc_records,
    })
}

fn record(report: &JsonlReporter, phase: &str, app: &str, evidence: &Snapshot) {
    report.diagnostic(phase, "observed", &json!({"app_id":app,"statefulset":evidence.sts,"lifecycle_id":evidence.lifecycle,"pods":evidence.pods,"pvcs":evidence.pvc_records}).to_string());
}

fn retained(before: &Snapshot, after: &Snapshot) -> Result<()> {
    ensure!(
        after.sts_uid.is_none() || after.sts_uid == before.sts_uid,
        "builder physical identity changed during cleanup; refusing a replacement"
    );
    ensure!(
        after.lifecycle.is_none() || after.lifecycle == before.lifecycle,
        "builder lifecycle changed during cleanup; refusing a different application execution"
    );
    ensure!(
        before.claims == after.claims,
        "cleanup changed or removed captured PVC UID/volume identity"
    );
    Ok(())
}

pub struct BuilderProbe {
    app: String,
    captured: Option<Snapshot>,
    attempted: bool,
}

impl BuilderProbe {
    pub async fn prepare(env: &Env, report: &JsonlReporter, app: &str) -> Result<Self> {
        check_scope(env, app)?;
        let evidence = snapshot(&inventory(env).await?, &env.k8s_ns, app, &[], None)?;
        record(report, "K8s admission baseline", app, &evidence);
        ensure!(
            evidence.sts.is_none() && evidence.pods.is_empty(),
            "builder existed before this case; refusing admission or cleanup"
        );
        Ok(Self {
            app: app.into(),
            captured: None,
            attempted: false,
        })
    }

    pub fn mark_admission_attempt(&mut self) {
        self.attempted = true;
    }

    pub async fn capture(
        &mut self,
        env: &Env,
        report: &JsonlReporter,
        required: bool,
    ) -> Result<()> {
        let evidence = snapshot(
            &inventory(env).await?,
            &env.k8s_ns,
            &self.app,
            &[],
            self.captured.as_ref(),
        )?;
        record(report, "K8s builder identity", &self.app, &evidence);
        if required {
            ensure!(
                evidence.sts.is_some() && !evidence.pods.is_empty() && !evidence.claims.is_empty(),
                "successful workspace admission lacks physical builder/Pod/PVC evidence"
            );
        }
        for pod in &evidence.pods {
            ensure!(
                pod["containers"]
                    .as_array()
                    .is_some_and(
                        |containers| containers.iter().any(|container| container["name"]
                            == "agent"
                            && container["image_id"]
                                .as_str()
                                .is_some_and(|id| !id.is_empty()))
                    ),
                "builder agent has no actual imageID evidence"
            );
        }
        if evidence.sts.is_some() {
            self.captured = Some(evidence);
        }
        Ok(())
    }

    pub async fn cleanup(
        &mut self,
        env: &Env,
        report: &JsonlReporter,
        user: &str,
        entry: &str,
    ) -> Result<()> {
        check_scope(env, &self.app)?;
        ensure!(
            self.attempted,
            "no admitted request from this case can authorize cleanup"
        );
        let deadline = Instant::now() + Duration::from_secs(300);
        tokio::time::timeout_at(
            deadline,
            self.cleanup_inner(env, report, user, entry, deadline),
        )
        .await
        .context("owned builder cleanup total deadline expired")?
    }

    async fn cleanup_inner(
        &self,
        env: &Env,
        report: &JsonlReporter,
        user: &str,
        entry: &str,
        deadline: Instant,
    ) -> Result<()> {
        let retained_claims: Vec<_> = self
            .captured
            .as_ref()
            .map(|captured| captured.claims.keys().cloned().collect())
            .unwrap_or_default();
        let before = snapshot(
            &inventory(env).await?,
            &env.k8s_ns,
            &self.app,
            &retained_claims,
            self.captured.as_ref(),
        )?;
        if let Some(captured) = &self.captured {
            retained(captured, &before)?;
        }
        record(report, "K8s before controlled cleanup", &self.app, &before);
        ensure!(
            before
                .pods
                .iter()
                .all(|pod| pod["ownership_verified"] == true),
            "orphaned case Pod lacks captured physical ownership; cleanup outcome is unconfirmed"
        );
        let lifecycle = api(
            env,
            entry,
            reqwest::Method::GET,
            &format!("/api/v1/userapp/{}/lifecycle", self.app),
            None,
            deadline,
        )
        .await?;
        let lifecycle_id = text(&lifecycle, &["data", "lifecycle_id"])?;
        let physical_lifecycle = before.lifecycle.as_deref().or_else(|| {
            self.captured
                .as_ref()
                .and_then(|captured| captured.lifecycle.as_deref())
        });
        ensure!(
            physical_lifecycle.is_none_or(|physical| physical == lifecycle_id),
            "physical builder and API lifecycle differ"
        );
        let body = json!({"user_id":user,"project_id":self.app,"service_type":"userapp","app_id":self.app,"app_stage":"dev",
            "lifecycle_id":lifecycle_id,"request_id":format!("{}-parallel-cleanup",env.run_tag)});
        let accepted = api(
            env,
            entry,
            reqwest::Method::POST,
            "/computer/pod/stop",
            Some(&body),
            deadline,
        )
        .await?;
        let operation_id = text(&accepted, &["data", "operation_id"])?;
        let status_url = format!("/computer/pod/operations/{}/{operation_id}", self.app);
        ensure!(
            accepted["data"]["status_url"] == status_url,
            "stop operation returned a different target URL"
        );
        loop {
            ensure!(
                Instant::now() < deadline,
                "owned builder stop operation exceeded its total deadline"
            );
            let view = api(
                env,
                entry,
                reqwest::Method::GET,
                &status_url,
                None,
                deadline,
            )
            .await?;
            ensure!(
                view["data"]["operation_id"] == operation_id
                    && view["data"]["app_id"] == self.app
                    && view["data"]["lifecycle_id"] == lifecycle_id
                    && view["data"]["scope"] == "Dev"
                    && view["data"]["action"] == "stop",
                "cleanup operation identity changed"
            );
            match view["data"]["state"].as_str() {
                Some("succeeded") => break,
                Some("failed" | "superseded") => {
                    return Err(anyhow!(
                        "owned builder stop failed: {}",
                        view["data"]["error_message"]
                    ));
                }
                Some("pending" | "running" | "recovery_required") => {
                    tokio::time::sleep(Duration::from_secs(1)).await
                }
                _ => return Err(anyhow!("unexpected builder stop state")),
            }
        }
        let captured_claims: Vec<_> = before.claims.keys().cloned().collect();
        loop {
            ensure!(
                Instant::now() < deadline,
                "builder Pods did not exit after confirmed stop"
            );
            let after = snapshot(
                &inventory(env).await?,
                &env.k8s_ns,
                &self.app,
                &captured_claims,
                Some(&before),
            )?;
            retained(&before, &after)?;
            if after.pods.is_empty() {
                record(report, "K8s after controlled cleanup", &self.app, &after);
                report.assert_hard("K8s builder 停止已确认且原 PVC UID 保留", true, json!({"app_id":self.app,"statefulset_uid":after.sts_uid,"pvcs":after.pvc_records}).to_string());
                return Ok(());
            }
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
    }
}

async fn api(
    env: &Env,
    entry: &str,
    method: reqwest::Method,
    path: &str,
    body: Option<&Value>,
    deadline: Instant,
) -> Result<Value> {
    let remaining = deadline.saturating_duration_since(Instant::now());
    ensure!(!remaining.is_zero(), "cleanup HTTP deadline expired");
    let request = env
        .http
        .request(method, format!("{entry}{path}"))
        .timeout(remaining.min(Duration::from_secs(90)));
    let request = match body {
        Some(body) => request.json(body),
        None => request,
    };
    let response = request.send().await?;
    let status = response.status();
    let value: Value = response.json().await?;
    ensure!(
        status.is_success() && value["code"] == "0000",
        "cleanup API {path} failed (HTTP {status}): {}",
        value["message"]
    );
    Ok(value)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn inventory_fixture(uid: &str, pvc_uid: &str, pods: bool) -> Value {
        let mut items = vec![
            json!({"kind":"StatefulSet","metadata":{"name":"rcoder-app-builder-app1","namespace":"test-ns","uid":uid,"annotations":{"rcoder.io/application-id":"app1","rcoder.io/lifecycle-id":"life1"}},"spec":{"replicas":1,"template":{"spec":{"volumes":[{"persistentVolumeClaim":{"claimName":"workspace"}}]}}}}),
            json!({"kind":"PersistentVolumeClaim","metadata":{"name":"workspace","namespace":"test-ns","uid":pvc_uid},"spec":{"volumeName":"retained-volume"},"status":{"phase":"Bound"}}),
        ];
        if pods {
            items.push(json!({"kind":"Pod","metadata":{"name":"rcoder-app-builder-app1-0","namespace":"test-ns","uid":"pod-one","ownerReferences":[{"kind":"StatefulSet","uid":uid}]},"status":{"phase":"Running","containerStatuses":[{"name":"agent","image":"runner@sha256:abc","imageID":"containerd://sha256:abc"}]}}));
        }
        items.push(json!({"kind":"Pod","metadata":{"name":"unrelated","namespace":"test-ns","uid":"foreign-pod","ownerReferences":[{"kind":"StatefulSet","uid":"foreign-sts"}]}}));
        json!({"items":items})
    }
    #[test]
    fn scoped_app_retains_case_prefix_and_fits_the_builder_name_budget() {
        let app = scoped_app_id("0123456789abcdef0123456789abcdef", Backend::Openai).unwrap();
        assert!(app.contains("0123456789") && app.len() <= 22);
        assert!(shared_types::validate_identifier(&app, "app_id").is_ok());
        assert!(scoped_app_id("short", Backend::Openai).is_err());
    }
    #[test]
    fn evidence_uses_physical_owner_and_rejects_replaced_sts_or_pvc() {
        let before = snapshot(
            &inventory_fixture("sts-one", "pvc-one", true),
            "test-ns",
            "app1",
            &[],
            None,
        )
        .unwrap();
        assert_eq!(before.pods.len(), 1);
        assert_eq!(before.pods[0]["uid"], "pod-one");
        assert_eq!(
            before.pods[0]["containers"][0]["image_id"],
            "containerd://sha256:abc"
        );
        let stopped = snapshot(
            &inventory_fixture("sts-one", "pvc-one", false),
            "test-ns",
            "app1",
            &[],
            None,
        )
        .unwrap();
        assert!(retained(&before, &stopped).is_ok());
        let replaced = snapshot(
            &inventory_fixture("replacement-sts", "pvc-one", false),
            "test-ns",
            "app1",
            &[],
            None,
        )
        .unwrap();
        assert!(retained(&before, &replaced).is_err());
        let replaced_volume = snapshot(
            &inventory_fixture("sts-one", "replacement-pvc", false),
            "test-ns",
            "app1",
            &[],
            None,
        )
        .unwrap();
        assert!(retained(&before, &replaced_volume).is_err());
        let mut different_lifecycle = inventory_fixture("sts-one", "pvc-one", false);
        different_lifecycle["items"][0]["metadata"]["annotations"]["rcoder.io/lifecycle-id"] =
            "life-two".into();
        let different_lifecycle =
            snapshot(&different_lifecycle, "test-ns", "app1", &[], None).unwrap();
        assert!(retained(&before, &different_lifecycle).is_err());
    }

    #[test]
    fn orphaned_case_pod_is_not_mistaken_for_absent_compute() {
        let before = snapshot(
            &inventory_fixture("old-sts", "pvc-one", true),
            "test-ns",
            "app1",
            &[],
            None,
        )
        .unwrap();
        let mut raw = inventory_fixture("old-sts", "pvc-one", true);
        raw["items"]
            .as_array_mut()
            .unwrap()
            .retain(|resource| resource["kind"] != "StatefulSet");
        let observed = snapshot(
            &raw,
            "test-ns",
            "app1",
            &["workspace".into()],
            Some(&before),
        )
        .unwrap();
        assert!(observed.sts.is_none());
        assert_eq!(observed.pods.len(), 1);
        assert_eq!(observed.pods[0]["uid"], "pod-one");
        assert_eq!(observed.claims["workspace"].0, "pvc-one");
    }
    #[test]
    fn remote_query_uses_user_kubeconfig_and_quotes_context_without_delete() {
        let command = inventory_command("test-ns", Some("context'with-space"));
        assert!(command.starts_with("KUBECONFIG=\"$HOME/.kube/config\" kubectl"));
        assert!(command.contains("'context'\\''with-space'"));
        assert!(!command.contains(" delete "));
        assert!(command.ends_with(" get statefulsets,pods,pvc -o json"));
    }
}
