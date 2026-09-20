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

pub(crate) fn docker_error(error: crate::DockerError) -> Error {
    if matches!(error, crate::DockerError::BuilderCreationCancelled) {
        return Error::CreationCancelled;
    }
    if let crate::DockerError::BollardError(bollard::errors::Error::DockerResponseServerError {
        status_code,
        ..
    }) = &error
        && let Some(rejection) =
            RuntimeRequestRejection::from_status(*status_code, error.to_string())
    {
        return Error::RequestRejected(rejection);
    }
    Error::ContainerCreationError(error.to_string())
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
    async fn release_failure_preserves_original_error_and_rejects_success() {
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
            if was_ok {
                assert!(matches!(error, Error::ConnectionError(_)));
            } else {
                assert!(matches!(error, Error::RequestRejected(_)));
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
