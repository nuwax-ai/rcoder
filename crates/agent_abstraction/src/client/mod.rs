//! Client module — convenience API for single-session ACP consumers.
//!
//! Provides [`AcpClientBuilder`] and [`AcpClient`] to simplify launching
//! an ACP agent from short-lived consumers (e.g. CLI tools, tests).
//!
//! # Example
//!
//! ```ignore
//! let client = AcpClientBuilder::new(notifier, registry)
//!     .command("python")
//!     .args(vec!["./my-agent.py".into()])
//!     .working_dir("/workspace")
//!     .start()
//!     .await?;
//!
//! client.send_prompt("hello").await?;
//! // ... response flows through notifier callbacks ...
//! client.stop().await?;
//! ```

mod acp_client;
mod builder;

pub use acp_client::{AcpClient, PromptCompletionSignal};
pub use builder::AcpClientBuilder;
