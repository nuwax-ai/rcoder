//! UserApp Manifest v1 shared contract.
//!
//! Build-time (`file-server`) and runtime (`app-cli`) both consume this crate.

mod discovery;
mod error;
mod release_lock;
mod types;
mod validation;

pub use discovery::discover_projects;
pub use error::{DiscoverError, ManifestError};
pub use release_lock::{ReleaseMetadata, build_release_lock};
pub use types::*;
pub use validation::{
    parse_project, parse_workspace, validate_project, validate_topology, validate_workspace,
};

pub const SCHEMA_VERSION: u32 = 1;
pub const INTERNAL_PORT_MIN: u16 = 4000;
pub const INTERNAL_PORT_MAX: u16 = 7999;
