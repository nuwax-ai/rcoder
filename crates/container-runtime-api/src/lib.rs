//! Container Runtime API
//!
//! This crate provides the `ContainerRuntime` trait abstraction for different
//! container runtimes (Docker, Kubernetes, etc.).

pub mod runtime_trait;

pub use runtime_trait::*;
