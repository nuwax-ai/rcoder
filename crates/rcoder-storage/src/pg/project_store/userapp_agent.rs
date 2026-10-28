//! Atomic logical ownership plus existing app-owned physical session routing.
use super::{database, persist_ops::PersistOp, repo, writer};
use crate::{
    db::models,
    pg::PgStore,
    userapp_agent::{MappingCommand, unique_binding},
};
use anyhow::{Context, Result, ensure};
use chrono::{DateTime, Utc};
use shared_types::persistence::{PersistenceOperationOutcome as Outcome, PersistenceWriteOutcome};
use shared_types::{UserAppAgentBinding as Binding, UserAppAgentSession as Session};
use toasty::Executor;

fn timestamp(value: i64) -> Result<DateTime<Utc>> {
    DateTime::from_timestamp_micros(value).context("Invalid UserApp agent timestamp")
}
fn binding(row: models::UserAppAgentBinding) -> Result<Binding> {
    ensure!(
        row.revision >= 1 && !row.binding_generation.is_empty(),
        "Invalid UserApp agent binding identity"
    );
    Ok(Binding {
        app_id: row.app_id,
        lifecycle_id: row.lifecycle_id,
        project_id: row.project_id,
        user_id: row.user_id,
        binding_generation: row.binding_generation,
        revision: row.revision,
        current_session_id: row.current_session_id,
        created_at: timestamp(row.created_at_us)?,
        updated_at: timestamp(row.updated_at_us)?,
    })
}
fn session(row: models::UserAppAgentSession) -> Result<Session> {
    ensure!(
        !row.session_generation.is_empty(),
        "Invalid UserApp session generation"
    );
    Ok(Session {
        session_id: row.session_id,
        session_generation: row.session_generation,
        binding_generation: row.binding_generation,
        created_at: timestamp(row.created_at_us)?,
    })
}
async fn bindings(tx: &mut dyn Executor) -> Result<Vec<Binding>> {
    models::UserAppAgentBinding::all()
        .exec(tx)
        .await?
        .into_iter()
        .map(binding)
        .collect()
}
async fn members(tx: &mut dyn Executor) -> Result<Vec<Session>> {
    models::UserAppAgentSession::all()
        .exec(tx)
        .await?
        .into_iter()
        .map(session)
        .collect()
}
async fn by_generation(tx: &mut dyn Executor, generation: &str) -> Result<Option<Binding>> {
    models::UserAppAgentBinding::filter_by_binding_generation(generation)
        .first()
        .exec(tx)
        .await?
        .map(binding)
        .transpose()
}
fn fingerprint(command: &MappingCommand) -> Result<String> {
    use sha2::{Digest, Sha256};
    Ok(Sha256::digest(command.fingerprint_input()?.as_bytes())
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect())
}
fn receipt_id(request_id: &str) -> String {
    format!("userapp-agent:{request_id}")
}

impl PgStore {
    pub(crate) async fn accept_userapp_agent_binding(
        &self,
        value: Binding,
        app_project_generation: String,
    ) -> Result<Binding> {
        let command = MappingCommand::Accept {
            binding: value.clone(),
            app_project_generation,
        };
        let id = format!("accept:{}", uuid::Uuid::new_v4());
        let pending = self.register_durable(vec![PersistOp::UserAppAgentMapping {
            request_id: receipt_id(&id),
            command: Box::new(command),
        }]);
        match self.execute_durable(pending, "accept_userapp_agent").await {
            PersistenceWriteOutcome::Committed => {}
            PersistenceWriteOutcome::Superseded => anyhow::bail!("UserApp project belongs to another application or user"),
            PersistenceWriteOutcome::Deferred { .. } => return Err(shared_types::OperationOutcomeUnknown::new("userapp_agent.accept", format!("Binding acceptance is unconfirmed; resolve the original app/project before Chat (request_id={id})")).into()),
        }
        self.get_userapp_agent_binding(&value.app_id, &value.lifecycle_id, &value.project_id)
            .await?
            .context("Committed UserApp binding is missing")
    }
    pub async fn get_userapp_agent_binding(
        &self,
        app_id: &str,
        lifecycle_id: &str,
        project_id: &str,
    ) -> Result<Option<Binding>> {
        let (app, life, project) = (
            app_id.to_owned(),
            lifecycle_id.to_owned(),
            project_id.to_owned(),
        );
        database::read(&self.database, move |tx| {
            Box::pin(async move {
                let fields = models::UserAppAgentBinding::fields();
                let row = models::UserAppAgentBinding::filter(
                    fields
                        .app_id()
                        .eq(&app)
                        .and(fields.lifecycle_id().eq(&life))
                        .and(fields.project_id().eq(&project)),
                )
                .first()
                .exec(tx)
                .await?;
                row.map(binding).transpose()
            })
        })
        .await
    }
    pub async fn find_userapp_agent_binding(
        &self,
        user_id: Option<&str>,
        project_id: &str,
    ) -> Result<Option<Binding>> {
        let (user, project) = (user_id.map(str::to_owned), project_id.to_owned());
        database::read(&self.database, move |tx| {
            Box::pin(async move {
                let fields = models::UserAppAgentBinding::fields();
                let rows = models::UserAppAgentBinding::filter(fields.project_id().eq(&project))
                    .exec(tx)
                    .await?;
                let values = rows.into_iter().map(binding).collect::<Result<Vec<_>>>()?;
                unique_binding(
                    values
                        .into_iter()
                        .filter(|b| user.as_ref().is_none_or(|u| &b.user_id == u)),
                )
            })
        })
        .await
    }
    pub async fn get_userapp_agent_session(
        &self,
        session_id: &str,
    ) -> Result<Option<(Binding, Session)>> {
        if shared_types::SessionHint::classify(Some(session_id))
            .as_session_id()
            .is_none()
        {
            return Ok(None);
        }
        let sid = session_id.to_owned();
        database::read(&self.database, move |tx| Box::pin(async move {
            let Some(row) = models::UserAppAgentSession::filter_by_session_id(&sid).first().exec(tx).await? else { return Ok(None); };
            let member = session(row)?;
            let owner = by_generation(tx, &member.binding_generation).await?.context("UserApp session binding is missing")?;
            if let Some(physical) = models::Session::filter_by_session_id(&sid).first().exec(tx).await?
                && (physical.project_id != owner.app_id || physical.generation != member.session_generation) {
                    tracing::warn!(%sid, "Unusable UserApp historical session: physical owner/generation changed");
                    return Ok(None);
            }
            let fields = models::SessionTombstone::fields();
            if models::SessionTombstone::filter(fields.session_id().eq(&sid).and(fields.generation().eq(&member.session_generation))).first().exec(tx).await?.is_some() {
                return Ok(None);
            }
            // Physical routing may be absent after idle compute cleanup. The logical
            // membership still proves historical ownership for a future session/load.
            Ok(Some((owner, member)))
        })).await
    }
    pub async fn register_userapp_agent_session_for_route(
        &self,
        owner: &Binding,
        session_id: &str,
        request_id: &str,
        deadline: tokio::time::Instant,
        expected_app_project_generation: &str,
    ) -> Result<Binding> {
        ensure!(
            shared_types::SessionHint::classify(Some(session_id))
                .as_session_id()
                .is_some(),
            "An established session identity is required"
        );
        let app_id = owner.app_id.clone();
        let sid = session_id.to_owned();
        let expected = expected_app_project_generation.to_owned();
        let (app_generation, existing_session_generation) = tokio::time::timeout_at(
            deadline,
            database::read(&self.database, move |tx| {
                Box::pin(async move {
                    let app = models::Project::filter_by_project_id(&app_id)
                        .first()
                        .exec(tx)
                        .await?
                        .context("UserApp builder route is not committed")?;
                    ensure!(
                        app.service_type.as_deref() == Some("user-app-builder"),
                        "UserApp binding requires a builder route"
                    );
                    ensure!(
                        app.generation == expected,
                        "UserApp builder route generation changed before session registration"
                    );
                    let generation = match models::Session::filter_by_session_id(&sid)
                        .first()
                        .exec(tx)
                        .await?
                    {
                        Some(row) => {
                            ensure!(
                                row.project_id == app_id
                                    && row.project_generation == app.generation,
                                "Session physical owner differs from UserApp builder"
                            );
                            Some(row.generation)
                        }
                        None => None,
                    };
                    Ok((app.generation, generation))
                })
            }),
        )
        .await
        .context("UserApp session registration deadline expired before mapping admission")??;
        let member = Session {
            session_id: session_id.into(),
            session_generation: existing_session_generation
                .clone()
                .unwrap_or_else(|| uuid::Uuid::new_v4().to_string()),
            binding_generation: owner.binding_generation.clone(),
            created_at: Utc::now(),
        };
        let command = MappingCommand::Register {
            binding: owner.clone(),
            session: member,
            app_project_generation: app_generation,
        };
        let committed = self
            .commit_agent_mapping(request_id, command, Some(deadline))
            .await?;
        let updated = if committed {
            self.get_userapp_agent_binding(&owner.app_id, &owner.lifecycle_id, &owner.project_id)
                .await?
                .context("Committed UserApp binding is missing")?
        } else {
            // The immutable receipt remains superseded. A read-only snapshot can
            // still confirm that a sibling request committed the exact desired
            // SID/route; never replay Chat or overwrite a later different SID.
            tokio::time::timeout_at(
                deadline,
                self.confirm_registered_userapp_session(
                    owner,
                    session_id,
                    expected_app_project_generation,
                    existing_session_generation,
                ),
            )
            .await
            .context("UserApp registered-session confirmation deadline expired")??
            .context("UserApp session registration was superseded; the logical binding changed")?
        };
        // Hydrate the unchanged application project and its new physical sessions,
        // never submit a whole application snapshot that races sibling conversations.
        let _physical = self.get_by_session_id_with_fetch(session_id).await;
        Ok(updated)
    }

    async fn confirm_registered_userapp_session(
        &self,
        captured: &Binding,
        session_id: &str,
        expected_app_generation: &str,
        existing_session_generation: Option<String>,
    ) -> Result<Option<Binding>> {
        let captured = captured.clone();
        let sid = session_id.to_owned();
        let app_generation = expected_app_generation.to_owned();
        database::read(&self.database, move |tx| {
            Box::pin(async move {
                let Some(current) = by_generation(tx, &captured.binding_generation).await? else {
                    return Ok(None);
                };
                if !current.owns(
                    &captured.app_id,
                    &captured.lifecycle_id,
                    &captured.project_id,
                    &captured.user_id,
                ) || current.revision < captured.revision
                    || current.current_session_id.as_deref() != Some(sid.as_str())
                {
                    return Ok(None);
                }
                let Some(app) = models::Project::filter_by_project_id(&captured.app_id)
                    .first()
                    .exec(tx)
                    .await?
                else {
                    return Ok(None);
                };
                if app.generation != app_generation
                    || app.service_type.as_deref() != Some("user-app-builder")
                {
                    return Ok(None);
                }
                let Some(member) = models::UserAppAgentSession::filter_by_session_id(&sid)
                    .first()
                    .exec(tx)
                    .await?
                else {
                    return Ok(None);
                };
                let Some(physical) = models::Session::filter_by_session_id(&sid)
                    .first()
                    .exec(tx)
                    .await?
                else {
                    return Ok(None);
                };
                if member.binding_generation != captured.binding_generation
                    || member.session_generation.is_empty()
                    || physical.project_id != captured.app_id
                    || physical.project_generation != app_generation
                    || physical.generation != member.session_generation
                    || existing_session_generation
                        .is_some_and(|generation| generation != physical.generation)
                {
                    return Ok(None);
                }
                let fields = models::SessionTombstone::fields();
                if models::SessionTombstone::filter(
                    fields
                        .session_id()
                        .eq(&sid)
                        .and(fields.generation().eq(&physical.generation)),
                )
                .first()
                .exec(tx)
                .await?
                .is_some()
                {
                    return Ok(None);
                }
                Ok(Some(current))
            })
        })
        .await
    }
    pub async fn clear_userapp_agent_sessions(
        &self,
        owner: &Binding,
        request_id: &str,
        deadline: tokio::time::Instant,
    ) -> Result<bool> {
        let generation = owner.binding_generation.clone();
        let captured = database::read(&self.database, move |tx| {
            Box::pin(async move {
                Ok(members(tx)
                    .await?
                    .into_iter()
                    .filter(|s| s.binding_generation == generation)
                    .collect::<Vec<_>>())
            })
        })
        .await?;
        let committed = self
            .commit_agent_mapping(
                request_id,
                MappingCommand::Clear {
                    binding: owner.clone(),
                    sessions: captured.clone(),
                },
                Some(deadline),
            )
            .await?;
        // The periodic project snapshot synchronizes the physical mirror. Do not
        // publish a replay's newly captured members as deleted: the receipt may
        // describe an earlier clear followed by a newer conversation turn.
        Ok(committed)
    }
    pub async fn invalidate_userapp_agent_bindings(
        &self,
        app_id: &str,
        lifecycle_id: &str,
        request_id: &str,
        deadline: tokio::time::Instant,
    ) -> Result<usize> {
        let (app, life) = (app_id.to_owned(), lifecycle_id.to_owned());
        let (captured, sessions) = database::read(&self.database, move |tx| {
            Box::pin(async move {
                let captured = bindings(tx)
                    .await?
                    .into_iter()
                    .filter(|b| b.app_id == app && b.lifecycle_id == life)
                    .collect::<Vec<_>>();
                let sessions = members(tx)
                    .await?
                    .into_iter()
                    .filter(|s| {
                        captured
                            .iter()
                            .any(|b| b.binding_generation == s.binding_generation)
                    })
                    .collect::<Vec<_>>();
                Ok((captured, sessions))
            })
        })
        .await?;
        let count = captured.len();
        ensure!(
            self.commit_agent_mapping(
                request_id,
                MappingCommand::Invalidate {
                    app_id: app_id.into(),
                    lifecycle_id: lifecycle_id.into(),
                    bindings: captured,
                    sessions: sessions.clone()
                },
                Some(deadline)
            )
            .await?,
            "UserApp lifecycle invalidation was superseded"
        );
        Ok(count)
    }
    /// Lifecycle cleanup uses the existing owned writer and its confirmation
    /// result; its caller's established operation budget controls waiting.
    pub async fn invalidate_userapp_agent_bindings_durable(
        &self,
        app_id: &str,
        lifecycle_id: &str,
        request_id: &str,
    ) -> Result<usize> {
        let (app, life) = (app_id.to_owned(), lifecycle_id.to_owned());
        let captured = database::read(&self.database, move |tx| {
            Box::pin(async move {
                let captured = bindings(tx)
                    .await?
                    .into_iter()
                    .filter(|b| b.app_id == app && b.lifecycle_id == life)
                    .collect::<Vec<_>>();
                let sessions = members(tx)
                    .await?
                    .into_iter()
                    .filter(|s| {
                        captured
                            .iter()
                            .any(|b| b.binding_generation == s.binding_generation)
                    })
                    .collect::<Vec<_>>();
                Ok((captured, sessions))
            })
        })
        .await?;
        let count = captured.0.len();
        ensure!(
            self.commit_agent_mapping(
                request_id,
                MappingCommand::Invalidate {
                    app_id: app_id.into(),
                    lifecycle_id: lifecycle_id.into(),
                    bindings: captured.0,
                    sessions: captured.1
                },
                None
            )
            .await?,
            "UserApp lifecycle cleanup was superseded"
        );
        Ok(count)
    }

    async fn commit_agent_mapping(
        &self,
        request_id: &str,
        command: MappingCommand,
        deadline: Option<tokio::time::Instant>,
    ) -> Result<bool> {
        ensure!(
            !request_id.trim().is_empty(),
            "UserApp mapping request identity is required"
        );
        let request_id = receipt_id(request_id);
        let expected = fingerprint(&command)?;
        let id = request_id.clone();
        let existing = database::read(&self.database, move |tx| {
            Box::pin(async move {
                models::ProjectWriteReceipt::filter_by_request_id(&id)
                    .first()
                    .exec(tx)
                    .await
                    .map_err(Into::into)
            })
        });
        // Detect a known invalid replay before admitting a poisoned writer item.
        // Unavailable reads remain unknown and must retain the immutable command.
        let existing = match deadline {
            Some(deadline) => tokio::time::timeout_at(deadline, existing)
                .await
                .ok()
                .and_then(Result::ok),
            None => existing.await.ok(),
        };
        if let Some(Some(receipt)) = existing {
            if receipt.fingerprint != expected {
                return Err(crate::userapp_agent::MappingRequestConflict.into());
            }
            return match receipt.outcome.as_str() {
                "committed" => Ok(true),
                "superseded" => Ok(false),
                _ => anyhow::bail!("Invalid UserApp mapping receipt outcome"),
            };
        }
        let op = PersistOp::UserAppAgentMapping {
            request_id: request_id.clone(),
            command: Box::new(command),
        };
        let pending = self.register_durable(vec![op.clone()]);
        let Some(deadline) = deadline else {
            return match self.execute_durable(pending, "userapp_agent_cleanup").await {
                PersistenceWriteOutcome::Committed => Ok(true),
                PersistenceWriteOutcome::Superseded => Ok(false),
                PersistenceWriteOutcome::Deferred { reason } => {
                    Err(shared_types::OperationOutcomeUnknown::new(
                        "userapp_agent.cleanup",
                        format!("Cleanup mapping receipt is unconfirmed: {request_id}; {reason}"),
                    )
                    .into())
                }
            };
        };
        match tokio::time::timeout_at(
            deadline,
            self.execute_durable(pending, "userapp_agent_mapping"),
        )
        .await
        {
            Ok(PersistenceWriteOutcome::Committed) => return Ok(true),
            Ok(PersistenceWriteOutcome::Superseded) => return Ok(false),
            Ok(PersistenceWriteOutcome::Deferred { reason }) => {
                tracing::warn!(%request_id, %reason, "UserApp mapping outcome unknown; reconciling the same storage receipt")
            }
            Err(_) => {
                tracing::warn!(%request_id, "UserApp mapping deadline reached; original owner/writer retains the receipt")
            }
        }
        loop {
            let id = request_id.clone();
            let read = database::read(&self.database, move |tx| {
                Box::pin(async move {
                    models::ProjectWriteReceipt::filter_by_request_id(&id)
                        .first()
                        .exec(tx)
                        .await
                        .map_err(Into::into)
                })
            });
            if let Ok(Ok(Some(receipt))) = tokio::time::timeout_at(deadline, read).await {
                ensure!(
                    receipt.fingerprint == expected,
                    "UserApp mapping request identity was reused with different input"
                );
                return match receipt.outcome.as_str() {
                    "committed" => Ok(true),
                    "superseded" => Ok(false),
                    _ => anyhow::bail!("Invalid UserApp mapping receipt outcome"),
                };
            }
            if tokio::time::Instant::now() >= deadline {
                return Err(shared_types::OperationOutcomeUnknown::new("userapp_agent.mapping", format!("Session mapping commit is unconfirmed; reconcile request_id={request_id}; do not resend Chat")).into());
            }
            // Only storage is replayed. The existing receipt makes a lost COMMIT
            // reply idempotent, while PendingWrite also retains the writer fallback.
            let _attempt = tokio::time::timeout_at(
                deadline,
                writer::execute_registered(&self.database, vec![op.clone()]),
            )
            .await;
            tokio::time::sleep_until(
                (tokio::time::Instant::now() + std::time::Duration::from_millis(50)).min(deadline),
            )
            .await;
        }
    }
}

pub(in crate::pg) fn command_locks(command: &MappingCommand) -> Vec<String> {
    let mut locks = Vec::new();
    match command {
        MappingCommand::Accept { binding: b, .. } => {
            locks.push(format!("project:{}", b.app_id));
            locks.push(format!(
                "userapp-agent-user-project:{}:{}",
                b.user_id, b.project_id
            ));
            locks.push(format!(
                "userapp-agent-project:{}:{}:{}",
                b.app_id, b.lifecycle_id, b.project_id
            ));
        }
        MappingCommand::Register {
            binding: b,
            session: s,
            ..
        } => {
            locks.push(format!("project:{}", b.app_id));
            locks.push(format!("session:{}", s.session_id));
            locks.push(format!("userapp-agent-binding:{}", b.binding_generation));
        }
        MappingCommand::Clear {
            binding: b,
            sessions,
        } => {
            locks.push(format!("project:{}", b.app_id));
            locks.push(format!("userapp-agent-binding:{}", b.binding_generation));
            locks.extend(sessions.iter().map(|s| format!("session:{}", s.session_id)));
        }
        MappingCommand::Invalidate {
            app_id,
            bindings,
            sessions,
            ..
        } => {
            locks.push(format!("project:{app_id}"));
            locks.extend(
                bindings
                    .iter()
                    .map(|b| format!("userapp-agent-binding:{}", b.binding_generation)),
            );
            locks.extend(sessions.iter().map(|s| format!("session:{}", s.session_id)));
        }
    }
    locks
}

pub(in crate::pg) async fn execute_command(
    tx: &mut dyn Executor,
    request_id: &str,
    command: &MappingCommand,
) -> Result<Outcome> {
    let hash = fingerprint(command)?;
    if let Some(receipt) = models::ProjectWriteReceipt::filter_by_request_id(request_id)
        .first()
        .exec(tx)
        .await?
    {
        if receipt.fingerprint != hash {
            return Err(crate::userapp_agent::MappingRequestConflict.into());
        }
        return match receipt.outcome.as_str() {
            "committed" => Ok(Outcome::Committed),
            "superseded" => Ok(Outcome::Superseded),
            _ => anyhow::bail!("Invalid UserApp mapping receipt outcome"),
        };
    }
    toasty::sql::statement("SAVEPOINT userapp_agent_mapping")
        .exec(tx)
        .await?;
    let outcome = apply_command(tx, command).await?;
    if outcome == Outcome::Superseded {
        toasty::sql::statement("ROLLBACK TO SAVEPOINT userapp_agent_mapping")
            .exec(tx)
            .await?;
    }
    toasty::sql::statement("RELEASE SAVEPOINT userapp_agent_mapping")
        .exec(tx)
        .await?;
    toasty::sql::statement("INSERT INTO project_write_receipts(request_id,fingerprint,outcome,recorded_at_us) VALUES($1,$2,$3,$4)")
        .bind(request_id).bind(hash).bind(if outcome == Outcome::Committed { "committed" } else { "superseded" }).bind(Utc::now().timestamp_micros()).exec(tx).await?;
    Ok(outcome)
}
async fn apply_command(tx: &mut dyn Executor, command: &MappingCommand) -> Result<Outcome> {
    match command {
        MappingCommand::Accept {
            binding: b,
            app_project_generation,
        } => {
            let Some(route) = models::Project::filter_by_project_id(&b.app_id)
                .first()
                .exec(tx)
                .await?
            else {
                return Ok(Outcome::Superseded);
            };
            if route.generation != *app_project_generation
                || route.service_type.as_deref() != Some("user-app-builder")
            {
                return Ok(Outcome::Superseded);
            }
            let stale = bindings(tx)
                .await?
                .into_iter()
                .filter(|old| old.app_id == b.app_id && old.lifecycle_id != b.lifecycle_id)
                .collect::<Vec<_>>();
            for old in stale {
                erase_binding_rows(tx, &old).await?;
            }
            let fields = models::UserAppAgentBinding::fields();
            let found = models::UserAppAgentBinding::filter(
                fields
                    .app_id()
                    .eq(&b.app_id)
                    .and(fields.lifecycle_id().eq(&b.lifecycle_id))
                    .and(fields.project_id().eq(&b.project_id)),
            )
            .first()
            .exec(tx)
            .await?;
            if let Some(found) = found {
                return Ok(if found.user_id == b.user_id {
                    Outcome::Committed
                } else {
                    Outcome::Superseded
                });
            }
            let other = models::UserAppAgentBinding::filter(
                fields
                    .user_id()
                    .eq(&b.user_id)
                    .and(fields.project_id().eq(&b.project_id)),
            )
            .first()
            .exec(tx)
            .await?;
            if other.is_some() {
                return Ok(Outcome::Superseded);
            }
            toasty::sql::statement("INSERT INTO userapp_agent_bindings(binding_generation,app_id,lifecycle_id,project_id,user_id,revision,current_session_id,created_at_us,updated_at_us) VALUES($1,$2,$3,$4,$5,1,NULL,$6,$6)")
                .bind(&b.binding_generation).bind(&b.app_id).bind(&b.lifecycle_id).bind(&b.project_id).bind(&b.user_id).bind(b.created_at.timestamp_micros()).exec(tx).await?;
            Ok(Outcome::Committed)
        }
        MappingCommand::Register {
            binding: b,
            session: s,
            app_project_generation,
        } => {
            let Some(current) = by_generation(tx, &b.binding_generation).await? else {
                return Ok(Outcome::Superseded);
            };
            if current != *b {
                return Ok(Outcome::Superseded);
            }
            if let Some(member) = models::UserAppAgentSession::filter_by_session_id(&s.session_id)
                .first()
                .exec(tx)
                .await?
                && member.binding_generation != b.binding_generation
            {
                return Ok(Outcome::Superseded);
            }
            if repo::add_session(
                tx,
                &b.app_id,
                &s.session_id,
                app_project_generation,
                &s.session_generation,
                None,
            )
            .await?
                != Outcome::Committed
            {
                return Ok(Outcome::Superseded);
            }
            toasty::sql::statement("INSERT INTO userapp_agent_sessions(session_id,session_generation,binding_generation,created_at_us) VALUES($1,$2,$3,$4) ON CONFLICT(session_id) DO UPDATE SET session_generation=EXCLUDED.session_generation WHERE userapp_agent_sessions.binding_generation=EXCLUDED.binding_generation")
                .bind(&s.session_id).bind(&s.session_generation).bind(&s.binding_generation).bind(s.created_at.timestamp_micros()).exec(tx).await?;
            let revision = b
                .revision
                .checked_add(1)
                .context("UserApp binding revision exhausted")?;
            let rows = toasty::sql::statement("UPDATE userapp_agent_bindings SET current_session_id=$1,revision=$2,updated_at_us=$3 WHERE binding_generation=$4 AND revision=$5")
                .bind(&s.session_id).bind(revision).bind(Utc::now().timestamp_micros()).bind(&b.binding_generation).bind(b.revision).exec(tx).await?;
            Ok(if rows == 1 {
                Outcome::Committed
            } else {
                Outcome::Superseded
            })
        }
        MappingCommand::Clear {
            binding: b,
            sessions,
        } => {
            let Some(current) = by_generation(tx, &b.binding_generation).await? else {
                return Ok(Outcome::Superseded);
            };
            if current != *b {
                return Ok(Outcome::Superseded);
            }
            let revision = b
                .revision
                .checked_add(1)
                .context("UserApp binding revision exhausted")?;
            toasty::sql::statement("UPDATE userapp_agent_bindings SET current_session_id=NULL,revision=$2,updated_at_us=$3 WHERE binding_generation=$1 AND revision=$4")
                .bind(&b.binding_generation).bind(revision).bind(Utc::now().timestamp_micros()).bind(b.revision).exec(tx).await?;
            clear_members(tx, &b.app_id, sessions).await?;
            Ok(Outcome::Committed)
        }
        MappingCommand::Invalidate {
            app_id,
            lifecycle_id,
            bindings,
            sessions: _,
        } => {
            // Only captured generations from the requested lifecycle are targets.
            for b in bindings {
                ensure!(
                    b.app_id == *app_id && b.lifecycle_id == *lifecycle_id,
                    "Invalid UserApp lifecycle invalidation capture"
                );
                toasty::sql::statement("UPDATE userapp_agent_bindings SET current_session_id=NULL WHERE binding_generation=$1 AND app_id=$2 AND lifecycle_id=$3")
                    .bind(&b.binding_generation).bind(app_id).bind(lifecycle_id).exec(tx).await?;
            }
            // The application + captured binding locks exclude registration.
            // A late response may have appended a member after the initial read;
            // retire it only inside these exact captured binding generations.
            let actual = members(tx)
                .await?
                .into_iter()
                .filter(|member| {
                    bindings
                        .iter()
                        .any(|owner| owner.binding_generation == member.binding_generation)
                })
                .collect::<Vec<_>>();
            clear_members(tx, app_id, &actual).await?;
            for b in bindings {
                toasty::sql::statement("DELETE FROM userapp_agent_bindings WHERE binding_generation=$1 AND app_id=$2 AND lifecycle_id=$3")
                    .bind(&b.binding_generation).bind(app_id).bind(lifecycle_id).exec(tx).await?;
            }
            Ok(Outcome::Committed)
        }
    }
}
async fn clear_members(tx: &mut dyn Executor, app_id: &str, sessions: &[Session]) -> Result<()> {
    for s in sessions {
        let Some(current) = models::UserAppAgentSession::filter_by_session_id(&s.session_id)
            .first()
            .exec(tx)
            .await?
        else {
            continue;
        };
        if current.binding_generation != s.binding_generation
            || current.session_generation != s.session_generation
        {
            continue;
        }
        if let Some(physical) = models::Session::filter_by_session_id(&s.session_id)
            .first()
            .exec(tx)
            .await?
            && physical.project_id == app_id
            && physical.generation == s.session_generation
        {
            repo::remove_session(tx, &s.session_id, &s.session_generation).await?;
        }
        toasty::sql::statement("DELETE FROM userapp_agent_sessions WHERE session_id=$1 AND binding_generation=$2 AND session_generation=$3")
            .bind(&s.session_id).bind(&s.binding_generation).bind(&s.session_generation).exec(tx).await?;
    }
    Ok(())
}

/// Retire only logical routing from an obsolete lifecycle, never another app's
/// runtime or the current builder's physical session rows.
async fn erase_binding_rows(tx: &mut dyn Executor, owner: &Binding) -> Result<()> {
    toasty::sql::statement("UPDATE userapp_agent_bindings SET current_session_id=NULL WHERE binding_generation=$1 AND app_id=$2 AND lifecycle_id=$3")
        .bind(&owner.binding_generation).bind(&owner.app_id).bind(&owner.lifecycle_id).exec(tx).await?;
    toasty::sql::statement("DELETE FROM userapp_agent_sessions WHERE binding_generation=$1")
        .bind(&owner.binding_generation)
        .exec(tx)
        .await?;
    toasty::sql::statement("DELETE FROM userapp_agent_bindings WHERE binding_generation=$1 AND app_id=$2 AND lifecycle_id=$3")
        .bind(&owner.binding_generation).bind(&owner.app_id).bind(&owner.lifecycle_id).exec(tx).await?;
    Ok(())
}

/// New tables are validated at boot; no pre-upgrade SID is imported or claimed.
pub(in crate::pg) async fn validate_snapshot(
    owner: &crate::db::owner::DatabaseOwner,
) -> Result<()> {
    database::read(owner, |tx| {
        Box::pin(async move {
            let owners = bindings(tx).await?;
            for member in members(tx).await? {
                ensure!(
                    owners
                        .iter()
                        .any(|b| b.binding_generation == member.binding_generation),
                    "Persistent UserApp session owner is missing"
                );
            }
            Ok(())
        })
    })
    .await
}
