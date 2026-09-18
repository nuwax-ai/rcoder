//! Offline Turso userApp database observer (e2e tool; specs/userapp-turso-local-storage T4).
//! Takes the instance directory lock and reads lifecycle/operation records with
//! the same Turso engine. Runs migrations never and the restart quarantine
//! never. Use only while the rcoder instance is stopped — the lock is the
//! mutual-exclusion proof. Output: {"section":"lifecycles"} marker, one record
//! JSON per line, then {"section":"operations"} marker, one record per line.
use std::path::PathBuf;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let mut arguments = std::env::args().skip(1);
    let path = PathBuf::from(
        arguments
            .next()
            .ok_or_else(|| anyhow::anyhow!("usage: userapp-db-observer <database-path>"))?,
    );
    if !path.is_absolute() || arguments.next().is_some() {
        anyhow::bail!("observer requires exactly one absolute database path");
    }
    let (lifecycles, operations) =
        rcoder_storage::userapp_lifecycle::turso::offline_snapshot(&path).await?;
    println!("{}", serde_json::json!({"section": "lifecycles"}));
    for record in lifecycles {
        println!("{record}");
    }
    println!("{}", serde_json::json!({"section": "operations"}));
    for record in operations {
        println!("{record}");
    }
    Ok(())
}
