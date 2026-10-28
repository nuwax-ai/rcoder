//! Offline Turso userApp database observer (e2e tool; specs/userapp-turso-local-storage T4).
//! Takes the instance directory lock and reads lifecycle/operation records with
//! the same Turso engine. Runs migrations never and the restart quarantine
//! never. Use only while the rcoder instance is stopped — the lock is the
//! mutual-exclusion proof. Output: {"section":"lifecycles"} marker, one record
//! JSON per line, then {"section":"operations"} marker, one record per line.
//! With `--compute-operation <app> <operation>`, output exactly one private
//! compute record JSON (including admission/lease fields) or `null`.
use std::path::PathBuf;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let mut arguments = std::env::args().skip(1);
    let path = PathBuf::from(
        arguments
            .next()
            .ok_or_else(|| anyhow::anyhow!("usage: userapp-db-observer <database-path>"))?,
    );
    if !path.is_absolute() {
        anyhow::bail!("observer requires an absolute database path");
    }
    if let Some(mode) = arguments.next() {
        if mode != "--compute-operation" {
            anyhow::bail!(
                "usage: userapp-db-observer <absolute-path> [--compute-operation <app> <operation>]"
            );
        }
        let app_id = arguments
            .next()
            .filter(|value| !value.is_empty())
            .ok_or_else(|| anyhow::anyhow!("compute observer requires an app ID"))?;
        let operation_id = arguments
            .next()
            .filter(|value| !value.is_empty())
            .ok_or_else(|| anyhow::anyhow!("compute observer requires an operation ID"))?;
        if arguments.next().is_some() {
            anyhow::bail!("compute observer accepts exactly an app ID and operation ID");
        }
        let record = rcoder_storage::userapp_lifecycle::offline_compute_snapshot(
            &path,
            &app_id,
            &operation_id,
        )
        .await?;
        match record {
            Some(record) => println!("{record}"),
            None => println!("null"),
        }
        return Ok(());
    }
    let (lifecycles, operations) =
        rcoder_storage::userapp_lifecycle::offline_snapshot(&path).await?;
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
