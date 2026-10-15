#[path = "../../../crates/rcoder-storage/src/preview_lifecycle/domain.rs"]
mod domain;
#[path = "../../../crates/rcoder-storage/src/preview_lifecycle/postgres.rs"]
mod postgres;
pub use postgres::PgPreviewStore;
#[path = "../../../crates/preview-coordinator/src/contract_suite.rs"]
pub mod contract_suite;

#[cfg(test)]
#[path = "../../../crates/preview-coordinator/src/in_process.rs"]
mod in_process;

#[cfg(test)]
#[tokio::test]
async fn in_process_preview_uses_the_same_identity_contract() {
    contract_suite::run(&in_process::InProcessPreviewStore::new()).await;
}
