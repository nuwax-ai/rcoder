//! Pure, bounded UserApp log reading. Catalogs describe files, never ownership.
pub mod catalog;
pub mod filter;
pub mod model;
pub mod read;
pub mod service;
pub mod sources;
pub mod stream;

pub use catalog::{LogCatalog, catalog_path};
pub use service::{LogLayout, LogService};
pub use stream::{CancelOnDrop, LogProvider, LogStreamEvent, stream};
