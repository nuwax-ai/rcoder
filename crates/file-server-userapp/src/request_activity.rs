//! Container request observation through handler execution and body completion.
use std::{
    pin::Pin,
    task::{Context, Poll},
};

use axum::{
    body::Body,
    extract::Request,
    middleware::Next,
    response::{IntoResponse, Response},
};
use process_utils::request_activity::RequestActivity;

pub(crate) async fn track(request: Request, next: Next) -> Response {
    // The task owns the request through disconnect. Real writes are separately
    // owned by WorkerRegistry, including their unknown-result reconciliation.
    let activity = RequestActivity::begin();
    let task = tokio::spawn(async move {
        let response = next.run(request).await;
        response.map(|body| {
            Body::new(ActivityBody {
                inner: body,
                activity: Some(activity),
            })
        })
    });
    match task.await {
        Ok(response) => response,
        Err(error) => (
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            format!("container request worker failed: {error}"),
        )
            .into_response(),
    }
}

struct ActivityBody {
    inner: Body,
    activity: Option<RequestActivity>,
}

impl http_body::Body for ActivityBody {
    type Data = axum::body::Bytes;
    type Error = axum::Error;

    fn poll_frame(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<http_body::Frame<Self::Data>, Self::Error>>> {
        let this = self.get_mut();
        let frame = Pin::new(&mut this.inner).poll_frame(cx);
        if matches!(frame, Poll::Ready(None | Some(Err(_)))) || this.inner.is_end_stream() {
            this.activity.take();
        }
        frame
    }

    fn is_end_stream(&self) -> bool {
        self.inner.is_end_stream()
    }
    fn size_hint(&self) -> http_body::SizeHint {
        self.inner.size_hint()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::Request;
    use tower::ServiceExt;

    #[tokio::test]
    async fn actual_container_zip_download_stays_active_through_body_eof_and_drop() {
        let directory = tempfile::tempdir().unwrap();
        let workspace = directory.path().join("download-app");
        std::fs::create_dir_all(&workspace).unwrap();
        std::fs::write(workspace.join("preserved.txt"), "real downloaded content").unwrap();
        let server = file_server::FileServer::builder(file_server::Config {
            userapp_workspace_dir: directory.path().into(),
            log_base_dir: directory.path().join("logs"),
            upload_project_dir: directory.path().join("archives"),
            ..file_server::Config::default()
        })
        .build()
        .unwrap();
        let router = crate::container_router(&server).unwrap();
        let download = || {
            Request::builder()
            .uri("/api/computer/download-all-files?userId=fixture-user&cId=fixture-conversation&workspaceType=userApp&serviceType=userapp&appId=download-app")
            .body(Body::empty()).unwrap()
        };
        assert_eq!(process_utils::request_activity::active_in_process(), 0);
        let response = router.clone().oneshot(download()).await.unwrap();
        if response.status() != axum::http::StatusCode::OK {
            let status = response.status();
            let body = axum::body::to_bytes(response.into_body(), 1024 * 1024)
                .await
                .unwrap();
            panic!(
                "actual ZIP download returned {status}: {}",
                String::from_utf8_lossy(&body)
            );
        }
        assert_eq!(response.headers()["content-type"], "application/zip");
        assert!(
            process_utils::request_activity::active_in_process() > 0,
            "response headers are not completion of the actual file body"
        );
        let bytes = axum::body::to_bytes(response.into_body(), 1024 * 1024)
            .await
            .unwrap();
        assert_eq!(process_utils::request_activity::active_in_process(), 0);
        let mut archive = zip::ZipArchive::new(std::io::Cursor::new(bytes)).unwrap();
        let mut file = archive
            .by_name("fixture-user_fixture-conversation/preserved.txt")
            .unwrap();
        let mut contents = String::new();
        std::io::Read::read_to_string(&mut file, &mut contents).unwrap();
        assert_eq!(contents, "real downloaded content");
        let response = router.oneshot(download()).await.unwrap();
        assert!(process_utils::request_activity::active_in_process() > 0);
        drop(response);
        assert_eq!(
            process_utils::request_activity::active_in_process(),
            0,
            "dropping a read body ends its observation without poisoning write tracking"
        );
    }
}
