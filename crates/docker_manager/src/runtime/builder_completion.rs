//! Builder completion policy. Unknown outcomes retain the durable recovery fence.
use container_runtime_api::{ContainerRuntimeError as Error, ContainerRuntimeResult};
use shared_types::{AppOperationLease, RuntimeRequestRejection};

#[cfg(feature = "kubernetes")]
pub(crate) fn k8s_error(message: String, error: kube::Error) -> Error {
    if let kube::Error::Api(response) = &error
        && let Some(rejection) =
            RuntimeRequestRejection::from_status(response.code, message.clone())
    {
        return Error::RequestRejected(rejection);
    }
    Error::K8sError(message)
}

/// Docker→runtime 错误的集中转换（普通 Agent 创建与 Builder 创建共用）。
///
/// 按提交阶段分四类，不整体压平：
/// - 发写前配置/构建失败（ConfigurationError 等）→ 确定失败；
/// - Docker 明确 4xx 拒绝（408/499 除外）→ RequestRejected，确定失败；
/// - 传输断连/超时（请求已发出、无回复）→ ConnectionError/Timeout，
///   `runtime_mutation_outcome_unknown` 判真（mutation 结果未知）；
/// - create 已确认 CID 后续阶段失败（ContainerCreationIncomplete）→
///   保留 CID、Docker 阶段与类型化 source，整次创建保留恢复围栏。
pub(crate) fn docker_error(error: crate::DockerError) -> Error {
    use crate::DockerError as D;
    match error {
        D::BuilderCreationCancelled => Error::CreationCancelled,
        D::ConfigurationError(message) => Error::ConfigurationError(message),
        D::ConnectionError(message) => Error::ConnectionError(message),
        D::Timeout(message) => Error::Timeout(message),
        D::ContainerCreationIncomplete {
            container_id,
            phase,
            source,
        } => incomplete_creation_error(container_id, phase, *source),
        D::ContainerCreationError(message) => Error::ContainerCreationError(message),
        D::ContainerStartError(message) => Error::ContainerStartError(message),
        D::ContainerStopError(message) => Error::ContainerStopError(message),
        // 以下变体不在创建提交链路上；穷尽列出并语义等价映射，不静默兜底。
        D::ContainerRemoveError(message) => {
            Error::ContainerCreationError(format!("container removal failed: {message}"))
        }
        D::ImagePullError(message) => Error::ContainerCreationError(message),
        D::ImageInspectError(message) => Error::ContainerCreationError(message),
        D::IoError(source) => Error::ContainerCreationError(source.to_string()),
        D::SerializationError(source) => {
            Error::ContainerCreationError(format!("serialization failed: {source}"))
        }
        D::InvalidTimestamp(message) => Error::ContainerCreationError(message),
        D::BollardError(source) => bollard_error(source),
    }
}

/// 已确认 create CID 后的失败：当次 API 拒绝不能抹掉前面成功的 create。
fn incomplete_creation_error(
    container_id: String,
    phase: crate::ContainerCreationPhase,
    source: crate::DockerError,
) -> Error {
    Error::DockerCreationIncomplete {
        container_id,
        phase,
        source: Box::new(docker_error(source)),
        recording_failure: None,
    }
}

fn bollard_error(source: bollard::errors::Error) -> Error {
    match source {
        bollard::errors::Error::DockerResponseServerError {
            status_code,
            message,
        } => {
            if let Some(rejection) =
                RuntimeRequestRejection::from_status(status_code, message.clone())
            {
                Error::RequestRejected(rejection)
            } else {
                // 408/499/5xx 并不证明 mutation 未提交；保持结果未知。
                Error::DockerError(format!("docker server response {status_code}: {message}"))
            }
        }
        bollard::errors::Error::HttpClientError { err } => {
            // 请求已发出、HTTP 客户端传输失败（含超时）：mutation 结果未知
            Error::ConnectionError(format!("docker transport failure: {err}"))
        }
        // 响应已收到但不可解码等：无法证明写未发生，保守判未知
        other => Error::DockerError(other.to_string()),
    }
}

/// Receipt failure is secondary to the acknowledged create and original phase
/// error. It cannot erase that evidence or release the creation recovery fence.
pub(crate) async fn record_partial_creation<T>(
    mut result: ContainerRuntimeResult<T>,
    recording: impl Future<Output = ContainerRuntimeResult<()>>,
) -> ContainerRuntimeResult<T> {
    if let Err(Error::DockerCreationIncomplete {
        recording_failure, ..
    }) = &mut result
        && let Err(error) = recording.await
    {
        *recording_failure = Some(Box::new(error));
    }
    result
}

pub(crate) async fn finish<T>(
    lease: Box<dyn AppOperationLease>,
    result: ContainerRuntimeResult<T>,
) -> ContainerRuntimeResult<T> {
    if result.is_ok()
        || matches!(
            &result,
            Err(Error::RequestRejected(_) | Error::CreationCancelled)
        )
    {
        if let Err(release_error) = lease.release().await {
            if result.is_ok() {
                return Err(Error::ConnectionError(release_error));
            }
            let original = match &result {
                Err(error) => error.to_string(),
                Ok(_) => "Builder creation completed".into(),
            };
            // A rejection describes the request, not successful lease cleanup.
            // Do not let the caller finalize and hide the retained runtime lock.
            return Err(Error::ConnectionError(format!(
                "{original}; release builder operation lease failed: {release_error}"
            )));
        }
    } else {
        if let Err(error) = &result {
            tracing::error!(error = %error, "Builder operation outcome is unconfirmed; retaining recovery fence");
        }
        drop(lease);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    struct Lease {
        releases: Arc<AtomicUsize>,
        fail: bool,
    }
    #[async_trait::async_trait]
    impl AppOperationLease for Lease {
        async fn release(self: Box<Self>) -> Result<(), String> {
            self.releases.fetch_add(1, Ordering::SeqCst);
            if self.fail {
                Err("release denied".into())
            } else {
                Ok(())
            }
        }
    }
    fn rejected() -> Error {
        Error::RequestRejected(
            RuntimeRequestRejection::from_status(403, "claim denied".into()).unwrap(),
        )
    }
    #[tokio::test]
    async fn completion_releases_only_success_or_explicit_rejection() {
        for (result, expected) in [
            (Ok(()), 1),
            (Err(rejected()), 1),
            (Err(Error::ConnectionError("response lost".into())), 0),
            (Err(Error::Timeout("request expired".into())), 0),
            (Err(Error::K8sError("unclassified".into())), 0),
        ] {
            let releases = Arc::new(AtomicUsize::new(0));
            let _result = finish(
                Box::new(Lease {
                    releases: releases.clone(),
                    fail: false,
                }),
                result,
            )
            .await;
            assert_eq!(releases.load(Ordering::SeqCst), expected);
        }
    }
    #[tokio::test]
    async fn release_failure_reports_unconfirmed_connection_error_with_context() {
        for result in [Ok(()), Err(rejected())] {
            let was_ok = result.is_ok();
            let error = finish(
                Box::new(Lease {
                    releases: Arc::new(AtomicUsize::new(0)),
                    fail: true,
                }),
                result,
            )
            .await
            .unwrap_err();
            // release 失败=清理未确认：无论原结果是成功还是显式拒绝，都不允许
            // 调用方按终态收束（租约仍持有，恢复围栏必须保留）——一律
            // ConnectionError；消息保留 release 失败原因，拒绝场景额外保留
            // 原拒绝文本以便归因。
            let Error::ConnectionError(message) = &error else {
                panic!("release failure must stay unconfirmed, got: {error:?}");
            };
            assert!(message.contains("release denied"), "message: {message}");
            if !was_ok {
                assert!(message.contains("claim denied"), "message: {message}");
            }
        }
    }
    #[test]
    fn docker_status_classification_does_not_guess_from_messages() {
        for status in [400, 401, 403, 404, 409, 422, 429, 408, 499, 500, 503] {
            let error = docker_error(crate::DockerError::BollardError(
                bollard::errors::Error::DockerResponseServerError {
                    status_code: status,
                    message: "rejected".into(),
                },
            ));
            assert_eq!(
                matches!(error, Error::RequestRejected(_)),
                status < 500 && !matches!(status, 408 | 499)
            );
        }
        assert!(!matches!(
            docker_error(crate::DockerError::ContainerCreationError(
                "HTTP 403".into()
            )),
            Error::RequestRejected(_)
        ));
    }
    #[cfg(feature = "kubernetes")]
    #[test]
    fn k8s_status_classification_retains_timeouts_and_server_errors() {
        for code in [403, 404, 409, 422, 429, 408, 499, 500, 503, 504] {
            let error = k8s_error(
                "request failed".into(),
                kube::Error::Api(Box::new(
                    serde_json::from_value(serde_json::json!({
                        "status":"Failure", "message":"failed", "reason":"test", "code":code
                    }))
                    .unwrap(),
                )),
            );
            assert_eq!(
                matches!(error, Error::RequestRejected(_)),
                code < 500 && !matches!(code, 408 | 499)
            );
        }
    }
}

#[cfg(test)]
mod docker_error_classification_tests {
    use super::docker_error;
    use crate::ContainerCreationPhase;
    use container_runtime_api::ContainerRuntimeError as Error;

    /// R03 反例: create 发出后传输断连/超时属于 mutation 结果未知, 压平成
    /// ContainerCreationError 会被 runtime_mutation_outcome_unknown 判为确定失败。
    #[test]
    fn transport_loss_during_create_keeps_unknown_outcome() {
        for error in [
            crate::DockerError::ConnectionError("create reply lost".into()),
            crate::DockerError::Timeout("create timed out".into()),
        ] {
            let runtime_error = docker_error(error);
            assert!(
                container_runtime_api::runtime_mutation_outcome_unknown(&runtime_error),
                "{runtime_error}"
            );
        }
    }

    #[test]
    fn ambiguous_server_response_during_create_keeps_unknown_outcome() {
        for status in [408, 499, 500, 503, 504] {
            let runtime_error = docker_error(crate::DockerError::BollardError(
                bollard::errors::Error::DockerResponseServerError {
                    status_code: status,
                    message: "daemon failed after accepting request".into(),
                },
            ));
            assert!(!matches!(runtime_error, Error::RequestRejected(_)));
            assert!(
                container_runtime_api::runtime_mutation_outcome_unknown(&runtime_error),
                "{status}: {runtime_error}"
            );
            assert!(runtime_error.to_string().contains(&status.to_string()));
        }
    }

    /// R03 反例: create 已确认 CID 后 start/observe 断连——结果未知, 且 CID
    /// 证据不得在 to_string 压平中丢失。
    #[test]
    fn incomplete_creation_keeps_cid_evidence_and_unknown_outcome() {
        for (phase, source) in [
            (
                ContainerCreationPhase::Start,
                crate::DockerError::ConnectionError("start reply lost".into()),
            ),
            (
                ContainerCreationPhase::Observe,
                crate::DockerError::Timeout("observe timed out".into()),
            ),
        ] {
            let runtime_error = docker_error(crate::DockerError::ContainerCreationIncomplete {
                container_id: "cid-evidence-9".into(),
                phase,
                source: Box::new(source),
            });
            assert!(
                container_runtime_api::runtime_mutation_outcome_unknown(&runtime_error),
                "{runtime_error}"
            );
            assert!(
                runtime_error.to_string().contains("cid-evidence-9"),
                "confirmed container id must survive conversion: {runtime_error}"
            );
            let Error::DockerCreationIncomplete {
                container_id,
                phase: actual_phase,
                source,
                ..
            } = runtime_error
            else {
                panic!("confirmed creation must retain typed evidence");
            };
            assert_eq!(container_id, "cid-evidence-9");
            assert_eq!(actual_phase, phase);
            match phase {
                ContainerCreationPhase::Start => {
                    assert!(matches!(*source, Error::ConnectionError(_)));
                }
                ContainerCreationPhase::Observe => {
                    assert!(matches!(*source, Error::Timeout(_)));
                }
            }
        }
    }

    #[test]
    fn rejected_start_preserves_confirmed_creation_and_request_rejection() {
        for status in [403, 409] {
            let error = docker_error(crate::DockerError::ContainerCreationIncomplete {
                container_id: "created-before-start".into(),
                phase: ContainerCreationPhase::Start,
                source: Box::new(crate::DockerError::BollardError(
                    bollard::errors::Error::DockerResponseServerError {
                        status_code: status,
                        message: "start denied".into(),
                    },
                )),
            });
            assert!(!error.is_definitive_rejection());
            assert!(container_runtime_api::runtime_mutation_outcome_unknown(
                &error
            ));
            let Error::DockerCreationIncomplete {
                container_id,
                phase,
                source,
                ..
            } = error
            else {
                panic!("start rejection cannot erase acknowledged create");
            };
            assert_eq!(container_id, "created-before-start");
            assert_eq!(phase, ContainerCreationPhase::Start);
            let Error::RequestRejected(rejection) = *source else {
                panic!("the individual request rejection must remain typed");
            };
            assert_eq!(rejection.status, status);
        }
    }

    /// R03: 明确 4xx 拒绝与发写前配置错误保持确定失败, 不得整体判 unknown。
    #[test]
    fn definite_rejections_and_pre_write_failures_stay_definite() {
        for status in [403u16, 409] {
            let runtime_error = docker_error(crate::DockerError::BollardError(
                bollard::errors::Error::DockerResponseServerError {
                    status_code: status,
                    message: "conflict".into(),
                },
            ));
            assert!(
                matches!(runtime_error, Error::RequestRejected(_)),
                "{status}: {runtime_error}"
            );
            assert!(!container_runtime_api::runtime_mutation_outcome_unknown(
                &runtime_error
            ));
        }
        let runtime_error = docker_error(crate::DockerError::ConfigurationError(
            "invalid mount config".into(),
        ));
        assert!(matches!(runtime_error, Error::ConfigurationError(_)));
        assert!(!container_runtime_api::runtime_mutation_outcome_unknown(
            &runtime_error
        ));
        // start 阶段已被 starter 分类的确定失败: 保持确定
        let runtime_error = docker_error(crate::DockerError::ContainerStartError(
            "health check failed".into(),
        ));
        assert!(matches!(runtime_error, Error::ContainerStartError(_)));
        assert!(!container_runtime_api::runtime_mutation_outcome_unknown(
            &runtime_error
        ));
    }
}
