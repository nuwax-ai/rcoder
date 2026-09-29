//! DBX 只读观察：捕获实例、选择实际可达通道、查询 4224、复核实例。
//! 定位、HTTP/固定 exec 和复核共用调用方剩余预算；不唤醒、不登记，
//! 不申请操作租约、不刷新闲置时间。观察结果不授予任何变更权限。

use std::sync::{Arc, Weak};
use std::time::Duration;

use container_runtime_api::{
    ContainerRuntimeError, ExecResult, UserAppDeploymentRuntime, UserAppReadinessTarget,
    UserAppRuntimeReadiness,
};
use shared_types::{
    DbxReadinessObservation as Observation, DbxReadinessProber, DbxReadinessReason as Reason,
    DbxReadinessStatus as Status, UserAppNoComputeState, UserAppReadinessChannel, UserappStage,
};
use tokio::time::Instant;

use crate::app_state::AppState;
use crate::userapp_readiness::{Access, observation_transport};

const PROBE_TIMEOUT: Duration = Duration::from_millis(1500);

pub struct DbxReadinessProberImpl {
    state: Weak<AppState>,
    http: reqwest::Client,
}

impl DbxReadinessProberImpl {
    pub(crate) fn new(state: Weak<AppState>) -> Self {
        Self {
            state,
            http: crate::http_client::probe_client().clone(),
        }
    }

    fn state(&self) -> Result<Arc<AppState>, String> {
        self.state
            .upgrade()
            .ok_or_else(|| "app state already dropped".to_string())
    }
}

#[async_trait::async_trait]
impl DbxReadinessProber for DbxReadinessProberImpl {
    async fn probe(
        &self,
        app_id: &str,
        stage: UserappStage,
        budget: Duration,
    ) -> Result<Observation, String> {
        let state = self.state()?;
        Ok(probe_runtime(
            state.runtime().as_ref(),
            &self.http,
            app_id,
            stage,
            Access::current(),
            budget,
        )
        .await)
    }
}

async fn probe_runtime(
    runtime: &dyn UserAppDeploymentRuntime,
    http: &reqwest::Client,
    app_id: &str,
    stage: UserappStage,
    access: Access,
    budget: Duration,
) -> Observation {
    if budget.is_zero() {
        return unknown(Reason::ObserveIncomplete);
    }
    let deadline = Instant::now() + budget;
    let observe = async {
        for _ in 0..2 {
            let located = match runtime.observe_userapp_readiness(app_id, stage).await {
                Ok(located) => located,
                Err(error) => {
                    return unknown(Reason::ObservationFailed).with_message(format!(
                        "Locate {stage:?} instance (app {app_id}): {error}"
                    ));
                }
            };
            let target = match located {
                UserAppRuntimeReadiness::NotRunning(state) => return no_compute(state),
                UserAppRuntimeReadiness::Running(target) => target,
            };
            let fetched = fetch(runtime, http, &target, access, deadline).await;
            let recheck = match runtime.observe_userapp_readiness(app_id, stage).await {
                Ok(recheck) => recheck,
                Err(error) => {
                    return unknown(Reason::ObservationFailed).with_message(format!(
                        "Recheck {stage:?} instance (app {app_id}): {error}"
                    ));
                }
            };
            // In-place restarts (started_at/container ID), replaced Pods and
            // changed host bindings all invalidate the response just fetched.
            if recheck != UserAppRuntimeReadiness::Running(target) {
                continue;
            }
            if let Some(observation) = fetched {
                return observation;
            }
        }
        unknown(Reason::InstanceChanged)
    };
    tokio::time::timeout_at(deadline, observe)
        .await
        .unwrap_or_else(|_| unknown(Reason::ObserveIncomplete))
}

fn unknown(reason: Reason) -> Observation {
    Observation::new(Status::Unknown, Some(reason))
}

fn no_compute(state: UserAppNoComputeState) -> Observation {
    let (status, reason) = match state {
        UserAppNoComputeState::Missing => (Status::Stopped, Reason::ComputeMissing),
        UserAppNoComputeState::Stopped => (Status::Stopped, Reason::ComputeStopped),
        UserAppNoComputeState::Starting => (Status::Starting, Reason::ComputeStarting),
        UserAppNoComputeState::Stopping => (Status::Unknown, Reason::ComputeStopping),
        UserAppNoComputeState::Failed => (Status::Unknown, Reason::ComputeFailed),
        UserAppNoComputeState::Unknown => (Status::Unknown, Reason::ComputeUnknown),
    };
    Observation::new(status, Some(reason))
}

async fn fetch(
    runtime: &dyn UserAppDeploymentRuntime,
    http: &reqwest::Client,
    target: &UserAppReadinessTarget,
    access: Access,
    deadline: Instant,
) -> Option<Observation> {
    let address = target
        .address
        .map(|address| std::net::SocketAddr::new(address.ip(), shared_types::DBX_PORT));
    let (channel, address) = observation_transport(
        &target.instance,
        access,
        address,
        target.dbx_published_address,
    );
    match channel {
        UserAppReadinessChannel::Direct => {
            let Some(address) = address else {
                return Some(
                    unknown(Reason::ProbeUnsupported).with_message(
                        "Captured instance has no address for direct DBX observation",
                    ),
                );
            };
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Some(unknown(Reason::ObserveIncomplete));
            }
            // SocketAddr's Display preserves IPv6 brackets and the actual
            // published port. Any response means the DBX HTTP listener exists.
            let response = http
                .get(format!("http://{address}/"))
                .timeout(remaining.min(PROBE_TIMEOUT))
                .send()
                .await;
            Some(match response {
                Ok(_) => Observation::new(Status::Ready, None),
                Err(error) if error.is_timeout() => {
                    unknown(Reason::ObserveIncomplete).with_message(error.to_string())
                }
                Err(error) if error.is_connect() => {
                    Observation::new(Status::Starting, Some(Reason::DbxUnreachable))
                        .with_message(error.to_string())
                }
                Err(error) => unknown(Reason::ObservationFailed).with_message(error.to_string()),
            })
        }
        UserAppReadinessChannel::Exec => match runtime.exec_userapp_dbx_readiness(target).await {
            Ok(Some(exec)) => Some(classify_exec(&exec)),
            Ok(None) => None,
            Err(ContainerRuntimeError::ConfigurationError(detail)) => {
                Some(unknown(Reason::ProbeUnsupported).with_message(detail))
            }
            Err(error) => Some(unknown(Reason::ObservationFailed).with_message(error.to_string())),
        },
    }
}

fn classify_exec(exec: &ExecResult) -> Observation {
    match exec.exit_code {
        0 if exec
            .stdout
            .trim()
            .parse::<u16>()
            .is_ok_and(|code| (100..=599).contains(&code)) =>
        {
            Observation::new(Status::Ready, None)
        }
        0 => unknown(Reason::ProbeProtocolInvalid),
        // curl 7 is a failed connection to the fixed loopback listener.
        7 => Observation::new(Status::Starting, Some(Reason::DbxUnreachable)),
        28 => unknown(Reason::ObserveIncomplete),
        126 | 127 => unknown(Reason::ProbeUnsupported),
        _ => unknown(Reason::ObservationFailed),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use container_runtime_api::{ContainerRuntimeResult, UserAppReadinessInstance};
    use std::collections::VecDeque;
    use std::sync::{
        Mutex,
        atomic::{AtomicUsize, Ordering},
    };
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    struct Runtime {
        observations: Mutex<VecDeque<UserAppRuntimeReadiness>>,
        reads: AtomicUsize,
        pending_read: Option<usize>,
        pending_exec: bool,
        commands: Mutex<Vec<UserAppReadinessTarget>>,
        exec: ExecResult,
    }

    #[async_trait::async_trait]
    impl UserAppDeploymentRuntime for Runtime {
        async fn observe_userapp_readiness(
            &self,
            app_id: &str,
            stage: UserappStage,
        ) -> ContainerRuntimeResult<UserAppRuntimeReadiness> {
            let read = self.reads.fetch_add(1, Ordering::SeqCst);
            if self.pending_read == Some(read) {
                return std::future::pending().await;
            }
            let mut observations = self.observations.lock().unwrap();
            let observation = if observations.len() > 1 {
                observations.pop_front().unwrap()
            } else {
                observations.front().unwrap().clone()
            };
            if let UserAppRuntimeReadiness::Running(target) = &observation {
                assert_eq!(target.app_id, app_id);
                assert_eq!(target.stage, stage);
            }
            Ok(observation)
        }

        async fn exec_userapp_dbx_readiness(
            &self,
            target: &UserAppReadinessTarget,
        ) -> ContainerRuntimeResult<Option<ExecResult>> {
            self.commands.lock().unwrap().push(target.clone());
            if self.pending_exec {
                return std::future::pending().await;
            }
            Ok(Some(self.exec.clone()))
        }
    }

    fn runtime(observations: Vec<UserAppRuntimeReadiness>) -> Runtime {
        Runtime {
            observations: Mutex::new(observations.into()),
            reads: AtomicUsize::new(0),
            pending_read: None,
            pending_exec: false,
            commands: Mutex::default(),
            exec: ExecResult {
                exit_code: 0,
                stdout: "503".into(),
                stderr: String::new(),
            },
        }
    }

    fn target() -> UserAppReadinessTarget {
        UserAppReadinessTarget {
            app_id: "dbx1".into(),
            stage: UserappStage::Dev,
            instance: UserAppReadinessInstance::Docker {
                container_id: "captured-container".into(),
                started_at: Some("original-start".into()),
            },
            address: Some("192.0.2.1:3010".parse().unwrap()),
            published_address: Some("127.0.0.1:33010".parse().unwrap()),
            dbx_published_address: None,
        }
    }

    fn running(target: &UserAppReadinessTarget) -> UserAppRuntimeReadiness {
        UserAppRuntimeReadiness::Running(Box::new(target.clone()))
    }

    fn client() -> reqwest::Client {
        drop(rustls::crypto::ring::default_provider().install_default());
        crate::http_client::probe_client().clone()
    }

    async fn query(runtime: &Runtime, access: Access, budget: Duration) -> Observation {
        probe_runtime(
            runtime,
            &client(),
            "dbx1",
            UserappStage::Dev,
            access,
            budget,
        )
        .await
    }

    #[tokio::test]
    async fn dbx_published_probe_uses_4224_binding_instead_of_internal_or_admin_address() {
        // IPv6 is intentional: formatting addr.ip() would lose the required brackets.
        let listener = tokio::net::TcpListener::bind("[::1]:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = [0; 2048];
            let read = socket.read(&mut request).await.unwrap();
            assert!(String::from_utf8_lossy(&request[..read]).starts_with("GET / HTTP/1.1\r\n"));
            socket.write_all(b"HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").await.unwrap();
        });
        let mut target = target();
        target.dbx_published_address = Some(address);
        let runtime = runtime(vec![running(&target)]);
        let observation = query(&runtime, Access::HostPublished, Duration::from_secs(2)).await;
        if observation.status != Status::Ready {
            server.abort();
        }
        assert_eq!(observation.status, Status::Ready, "{observation:?}");
        server
            .await
            .expect("the controlled DBX HTTP server must complete");
        assert_eq!(
            runtime.reads.load(Ordering::SeqCst),
            2,
            "a successful HTTP probe must recheck its physical target"
        );
        assert!(runtime.commands.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn dbx_host_k8s_exec_requires_an_http_reply_and_preserves_physical_target() {
        let mut target = target();
        target.instance = UserAppReadinessInstance::Kubernetes {
            namespace: "test".into(),
            pod_name: "builder-0".into(),
            pod_uid: "pod-uid".into(),
            container_name: "agent".into(),
            container_id: Some("container-id".into()),
            owner_uid: "sts-uid".into(),
        };
        // Neither this host mapping nor the Pod IP is the host K8s observation channel.
        target.dbx_published_address = Some("127.0.0.1:34224".parse().unwrap());
        for (exit_code, stdout, status, reason) in [
            (0, "503", Status::Ready, None),
            (0, "", Status::Unknown, Some(Reason::ProbeProtocolInvalid)),
            (
                0,
                "000",
                Status::Unknown,
                Some(Reason::ProbeProtocolInvalid),
            ),
            (127, "", Status::Unknown, Some(Reason::ProbeUnsupported)),
            (7, "000", Status::Starting, Some(Reason::DbxUnreachable)),
            (28, "000", Status::Unknown, Some(Reason::ObserveIncomplete)),
        ] {
            let mut runtime = runtime(vec![running(&target)]);
            runtime.exec = ExecResult {
                exit_code,
                stdout: stdout.into(),
                stderr: String::new(),
            };
            let observation = query(&runtime, Access::HostDirect, Duration::from_secs(1)).await;
            assert_eq!(
                (observation.status, observation.reason_code),
                (status, reason)
            );
            assert_eq!(*runtime.commands.lock().unwrap(), vec![target.clone()]);
            assert_eq!(runtime.reads.load(Ordering::SeqCst), 2);
        }
    }

    #[tokio::test]
    async fn dbx_prober_preserves_all_runtime_no_compute_states() {
        for (state, status, reason) in [
            (
                UserAppNoComputeState::Missing,
                Status::Stopped,
                Reason::ComputeMissing,
            ),
            (
                UserAppNoComputeState::Stopped,
                Status::Stopped,
                Reason::ComputeStopped,
            ),
            (
                UserAppNoComputeState::Starting,
                Status::Starting,
                Reason::ComputeStarting,
            ),
            (
                UserAppNoComputeState::Stopping,
                Status::Unknown,
                Reason::ComputeStopping,
            ),
            (
                UserAppNoComputeState::Failed,
                Status::Unknown,
                Reason::ComputeFailed,
            ),
            (
                UserAppNoComputeState::Unknown,
                Status::Unknown,
                Reason::ComputeUnknown,
            ),
        ] {
            let runtime = runtime(vec![UserAppRuntimeReadiness::NotRunning(state)]);
            let observation = query(&runtime, Access::HostPublished, Duration::from_secs(1)).await;
            assert_eq!(
                (observation.status, observation.reason_code),
                (status, Some(reason))
            );
            assert!(runtime.commands.lock().unwrap().is_empty());
        }
    }

    #[tokio::test]
    async fn dbx_discards_ready_from_restarted_or_replaced_instances() {
        let original = target();
        let mut restarted = original.clone();
        if let UserAppReadinessInstance::Docker { started_at, .. } = &mut restarted.instance {
            *started_at = Some("second-start".into());
        }
        let mut replaced = restarted.clone();
        if let UserAppReadinessInstance::Docker { container_id, .. } = &mut replaced.instance {
            *container_id = "replacement-container".into();
        }
        let runtime = runtime(vec![
            running(&original),
            running(&restarted),
            running(&restarted),
            running(&replaced),
        ]);
        let observation = query(&runtime, Access::HostPublished, Duration::from_secs(1)).await;
        assert_eq!(
            (observation.status, observation.reason_code),
            (Status::Unknown, Some(Reason::InstanceChanged))
        );
        assert_eq!(*runtime.commands.lock().unwrap(), vec![original, restarted]);
    }

    #[tokio::test(start_paused = true)]
    async fn dbx_total_deadline_covers_discovery_exec_and_identity_recheck() {
        for (pending_read, pending_exec) in [(Some(0), false), (None, true), (Some(1), false)] {
            let target = target();
            let mut runtime = runtime(vec![running(&target)]);
            runtime.pending_read = pending_read;
            runtime.pending_exec = pending_exec;
            let started = Instant::now();
            let observation = query(&runtime, Access::HostPublished, Duration::from_secs(1)).await;
            assert_eq!(started.elapsed(), Duration::from_secs(1));
            assert_eq!(
                (observation.status, observation.reason_code),
                (Status::Unknown, Some(Reason::ObserveIncomplete))
            );
        }
    }
}
