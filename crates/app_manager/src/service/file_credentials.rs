impl crate::service::AppService {
    pub fn set_file_credentials_provider(
        &self,
        provider: std::sync::Arc<dyn shared_types::FileServerCredentialsProvider>,
    ) -> crate::AppResult<()> {
        self.file_credentials.set(provider).map_err(|_| {
            crate::AppOperationError::Diagnostic(shared_types::WakeFailure::new(
                shared_types::ERR_RUNTIME_CONFIGURATION,
                "internal_http_configuration",
                "File credentials provider is already configured",
            ))
        })
    }
    pub(crate) async fn file_request_credentials(
        &self,
        stage: shared_types::UserappStage,
        app_id: &str,
        deadline: tokio::time::Instant,
    ) -> crate::AppResult<shared_types::FileServerRequestCredentials> {
        match self.file_credentials.get() {
            None => Ok(shared_types::FileServerRequestCredentials::default()),
            Some(provider) => {
                tokio::time::timeout_at(deadline, provider.for_target(stage, app_id, deadline))
                    .await
                    .map_err(|_| {
                        crate::AppOperationError::Diagnostic(shared_types::WakeFailure::timeout(
                            "file_credentials_configuration",
                            None,
                            true,
                        ))
                    })?
                    .map_err(crate::AppOperationError::Diagnostic)
            }
        }
    }
}
