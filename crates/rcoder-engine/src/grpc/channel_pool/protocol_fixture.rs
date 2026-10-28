//! Transport seam for controlled protocol tests, never an HTTP routing bypass.
use super::{Channel, GrpcChannelPool};

impl GrpcChannelPool {
    /// Bind an already connected real tonic channel to one exact authority.
    /// Production images do not enable this explicitly requested test feature.
    #[doc(hidden)]
    pub async fn bind_protocol_fixture_channel(&self, authority: String, channel: Channel) {
        self.channels.insert(authority, channel).await;
    }
}
