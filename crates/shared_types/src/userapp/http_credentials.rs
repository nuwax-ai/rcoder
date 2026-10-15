//! File peers use their existing proxy token, never the RCoder control key.
use crate::{UserappStage, WakeFailure};

#[derive(Clone, Default)]
pub struct FileServerRequestCredentials {
    pub proxy_token: Option<String>,
}
impl std::fmt::Debug for FileServerRequestCredentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FileServerRequestCredentials")
            .field(
                "proxy_token",
                &self.proxy_token.as_ref().map(|_| "[redacted]"),
            )
            .finish()
    }
}
impl FileServerRequestCredentials {
    pub fn apply(&self, request: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        match &self.proxy_token {
            Some(token) => request.header("x-proxy-token", token),
            None => request,
        }
    }
}
/// A narrow host-injected dependency. Resolving credentials never starts compute
/// or generates credentials. Existing unconfigured hosts retain optional auth.
#[async_trait::async_trait]
pub trait FileServerCredentialsProvider: Send + Sync {
    async fn for_target(
        &self,
        stage: UserappStage,
        app_id: &str,
        deadline: tokio::time::Instant,
    ) -> Result<FileServerRequestCredentials, WakeFailure>;
}
