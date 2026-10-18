//! Logical UserApp conversations share only the existing application route.
use crate::ProjectStoreBackend;
use anyhow::{Context, Result, ensure};
use chrono::Utc;
use shared_types::{UserAppAgentBinding as Binding, UserAppAgentSession as Session};
use std::collections::BTreeMap;

#[cfg(feature = "pg")]
#[derive(Debug, thiserror::Error)]
#[error("UserApp mapping request identity was reused with different input")]
pub(crate) struct MappingRequestConflict;

#[derive(Debug, Clone, serde::Serialize)]
pub(crate) enum MappingCommand {
    #[cfg(feature = "pg")]
    Accept {
        binding: Binding,
        app_project_generation: String,
    },
    Register {
        binding: Binding,
        session: Session,
        app_project_generation: String,
    },
    Clear {
        binding: Binding,
        sessions: Vec<Session>,
    },
    Invalidate {
        app_id: String,
        lifecycle_id: String,
        bindings: Vec<Binding>,
        sessions: Vec<Session>,
    },
}
impl MappingCommand {
    pub(crate) fn fingerprint_input(&self) -> Result<String> {
        // Timestamps and newly allocated physical generations are not request input.
        Ok(match self {
            #[cfg(feature = "pg")]
            Self::Accept {
                binding: b,
                app_project_generation,
            } => serde_json::to_string(&(
                "accept",
                &b.app_id,
                &b.lifecycle_id,
                &b.project_id,
                &b.user_id,
                app_project_generation,
            ))?,
            Self::Register {
                binding: b,
                session: s,
                app_project_generation,
            } => serde_json::to_string(&(
                "register",
                &b.app_id,
                &b.lifecycle_id,
                &b.project_id,
                &b.user_id,
                &b.binding_generation,
                b.revision,
                &s.session_id,
                app_project_generation,
            ))?,
            Self::Clear { binding: b, .. } => {
                serde_json::to_string(&("clear", &b.binding_generation, b.revision))?
            }
            Self::Invalidate {
                app_id,
                lifecycle_id,
                ..
            } => serde_json::to_string(&("invalidate", app_id, lifecycle_id))?,
        })
    }
}

#[derive(Default)]
pub(crate) struct MemoryBindings {
    pub(crate) bindings: BTreeMap<String, Binding>,
    pub(crate) sessions: BTreeMap<String, Session>,
    receipts: BTreeMap<String, (String, bool)>,
    invalidation_receipts: BTreeMap<String, (String, usize)>,
}

pub(crate) fn new_binding(
    app_id: &str,
    lifecycle_id: &str,
    project_id: &str,
    user_id: &str,
) -> Result<Binding> {
    for (name, value) in [
        ("app_id", app_id),
        ("lifecycle_id", lifecycle_id),
        ("project_id", project_id),
        ("user_id", user_id),
    ] {
        ensure!(
            !value.trim().is_empty(),
            "{name} is required for UserApp agent binding"
        );
    }
    Ok(Binding {
        app_id: app_id.into(),
        lifecycle_id: lifecycle_id.into(),
        project_id: project_id.into(),
        user_id: user_id.into(),
        binding_generation: uuid::Uuid::new_v4().to_string(),
        revision: 1,
        current_session_id: None,
        created_at: Utc::now(),
        updated_at: Utc::now(),
    })
}

fn confirmed_memory_registration(
    inner: &crate::ProjectAdapter,
    state: &MemoryBindings,
    captured: &Binding,
    session: &Session,
    expected_app_generation: &str,
) -> Option<Binding> {
    let current = state.bindings.get(&captured.binding_generation)?;
    if !current.owns(
        &captured.app_id,
        &captured.lifecycle_id,
        &captured.project_id,
        &captured.user_id,
    ) || current.revision < captured.revision
        || current.current_session_id.as_deref() != Some(session.session_id.as_str())
    {
        return None;
    }
    let member = state.sessions.get(&session.session_id)?;
    if member.binding_generation != captured.binding_generation
        || member.session_generation != session.session_generation
    {
        return None;
    }
    let route = inner.get_by_session_id(&session.session_id)?;
    let identity = route.persistence_identity();
    if route.project_id() != captured.app_id
        || route.service_type() != Some(shared_types::ServiceType::UserappBuilder)
        || identity.generation != expected_app_generation
        || identity.sessions.get(&session.session_id) != Some(&member.session_generation)
        || identity.retired_sessions.get(&session.session_id) == Some(&member.session_generation)
    {
        return None;
    }
    Some(current.clone())
}

impl ProjectStoreBackend {
    pub async fn accept_userapp_agent_binding(
        &self,
        app_id: &str,
        lifecycle_id: &str,
        project_id: &str,
        user_id: &str,
    ) -> Result<Binding> {
        let binding = new_binding(app_id, lifecycle_id, project_id, user_id)?;
        match self {
            Self::Memory(inner) => {
                let mut state = inner
                    .userapp_agents
                    .lock()
                    .map_err(|_| anyhow::anyhow!("UserApp agent binding lock poisoned"))?;
                // Admission carries the current lifecycle read by the HTTP owner.
                // Old metadata for this same application cannot permanently block
                // a new lifecycle; unrelated applications are never cleanup targets.
                let stale: Vec<_> = state
                    .bindings
                    .values()
                    .filter(|b| b.app_id == app_id && b.lifecycle_id != lifecycle_id)
                    .map(|b| b.binding_generation.clone())
                    .collect();
                state
                    .sessions
                    .retain(|_, member| !stale.contains(&member.binding_generation));
                for generation in stale {
                    state.bindings.remove(&generation);
                }
                if let Some(found) = state.bindings.values().find(|b| {
                    b.app_id == app_id
                        && b.lifecycle_id == lifecycle_id
                        && b.project_id == project_id
                }) {
                    ensure!(
                        found.user_id == user_id,
                        "UserApp project belongs to another user"
                    );
                    return Ok(found.clone());
                }
                ensure!(
                    !state.bindings.values().any(|b| b.user_id == user_id
                        && b.project_id == project_id
                        && (b.app_id != app_id || b.lifecycle_id != lifecycle_id)),
                    "UserApp project is already bound to another application or lifecycle"
                );
                state
                    .bindings
                    .insert(binding.binding_generation.clone(), binding.clone());
                Ok(binding)
            }
            #[cfg(feature = "pg")]
            Self::Postgres(store) => {
                let expected = store
                    .inner()
                    .get(app_id)
                    .context("UserApp builder route is missing")?
                    .persistence_identity()
                    .generation
                    .clone();
                store.accept_userapp_agent_binding(binding, expected).await
            }
        }
    }

    pub async fn get_userapp_agent_binding(
        &self,
        app_id: &str,
        lifecycle_id: &str,
        project_id: &str,
    ) -> Result<Option<Binding>> {
        match self {
            Self::Memory(inner) => Ok(inner
                .userapp_agents
                .lock()
                .map_err(|_| anyhow::anyhow!("UserApp agent binding lock poisoned"))?
                .bindings
                .values()
                .find(|b| {
                    b.app_id == app_id
                        && b.lifecycle_id == lifecycle_id
                        && b.project_id == project_id
                })
                .cloned()),
            #[cfg(feature = "pg")]
            Self::Postgres(store) => {
                store
                    .get_userapp_agent_binding(app_id, lifecycle_id, project_id)
                    .await
            }
        }
    }

    pub async fn find_userapp_agent_binding(
        &self,
        user_id: Option<&str>,
        project_id: &str,
    ) -> Result<Option<Binding>> {
        match self {
            Self::Memory(inner) => {
                let state = inner
                    .userapp_agents
                    .lock()
                    .map_err(|_| anyhow::anyhow!("UserApp agent binding lock poisoned"))?;
                unique_binding(
                    state
                        .bindings
                        .values()
                        .filter(|b| {
                            b.project_id == project_id && user_id.is_none_or(|u| b.user_id == u)
                        })
                        .cloned(),
                )
            }
            #[cfg(feature = "pg")]
            Self::Postgres(store) => store.find_userapp_agent_binding(user_id, project_id).await,
        }
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
        match self {
            Self::Memory(inner) => {
                let state = inner
                    .userapp_agents
                    .lock()
                    .map_err(|_| anyhow::anyhow!("UserApp agent binding lock poisoned"))?;
                let Some(member) = state.sessions.get(session_id) else {
                    return Ok(None);
                };
                let Some(binding) = state.bindings.get(&member.binding_generation) else {
                    return Ok(None);
                };
                if inner.get(&binding.app_id).is_some_and(|app| {
                    app.persistence_identity().retired_sessions.get(session_id)
                        == Some(&member.session_generation)
                }) {
                    return Ok(None);
                }
                if let Some(physical) = inner.get_by_session_id(session_id)
                    && (physical.project_id() != binding.app_id
                        || physical.persistence_identity().sessions.get(session_id)
                            != Some(&member.session_generation))
                {
                    return Ok(None);
                }
                Ok(Some((binding.clone(), member.clone())))
            }
            #[cfg(feature = "pg")]
            Self::Postgres(store) => store.get_userapp_agent_session(session_id).await,
        }
    }

    /// Commit physical membership and logical ownership before publishing success.
    /// Retrying this method only retries the immutable storage command, never Chat.
    pub async fn register_userapp_agent_session(
        &self,
        binding: &Binding,
        session_id: &str,
        request_id: &str,
        deadline: tokio::time::Instant,
    ) -> Result<Binding> {
        let expected = self
            .memory_mirror()
            .get(&binding.app_id)
            .context("UserApp builder route is missing")?
            .persistence_identity()
            .generation
            .clone();
        self.register_userapp_agent_session_for_route(
            binding, session_id, request_id, deadline, &expected,
        )
        .await
    }

    pub async fn register_userapp_agent_session_for_route(
        &self,
        binding: &Binding,
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
        ensure!(
            !session_id.trim().is_empty() && !request_id.trim().is_empty(),
            "UserApp session and registration request identity are required"
        );
        match self {
            Self::Memory(inner) => {
                let mut state = inner
                    .userapp_agents
                    .lock()
                    .map_err(|_| anyhow::anyhow!("UserApp agent binding lock poisoned"))?;
                let app = inner
                    .get(&binding.app_id)
                    .context("UserApp builder route is missing")?;
                ensure!(
                    app.service_type() == Some(shared_types::ServiceType::UserappBuilder),
                    "UserApp binding requires a builder route"
                );
                ensure!(
                    app.persistence_identity().generation == expected_app_project_generation,
                    "UserApp builder route generation changed before session registration"
                );
                let session = Session {
                    session_id: session_id.into(),
                    session_generation: state
                        .sessions
                        .get(session_id)
                        .map(|s| s.session_generation.clone())
                        .unwrap_or_else(|| uuid::Uuid::new_v4().to_string()),
                    binding_generation: binding.binding_generation.clone(),
                    created_at: Utc::now(),
                };
                let command = MappingCommand::Register {
                    binding: binding.clone(),
                    session: session.clone(),
                    app_project_generation: app.persistence_identity().generation.clone(),
                };
                let fingerprint = command.fingerprint_input()?;
                if let Some((old, committed)) = state.receipts.get(request_id) {
                    ensure!(
                        old == &fingerprint,
                        "UserApp mapping request identity was reused"
                    );
                    if !*committed {
                        return confirmed_memory_registration(
                            inner,
                            &state,
                            binding,
                            &session,
                            expected_app_project_generation,
                        )
                        .context("UserApp mapping registration was superseded");
                    }
                    return state
                        .bindings
                        .get(&binding.binding_generation)
                        .cloned()
                        .context("UserApp binding was retired");
                }
                ensure!(
                    state
                        .sessions
                        .get(session_id)
                        .is_none_or(|s| s.binding_generation == binding.binding_generation),
                    "UserApp session belongs to another conversation"
                );
                let current = state
                    .bindings
                    .get(&binding.binding_generation)
                    .cloned()
                    .context("UserApp binding was retired")?;
                if current != *binding {
                    // Preserve the failed CAS receipt. Another concurrently accepted
                    // prompt can already have committed this exact SID and route;
                    // confirming that fact does not perform another registration.
                    state
                        .receipts
                        .insert(request_id.into(), (fingerprint, false));
                    return confirmed_memory_registration(
                        inner,
                        &state,
                        binding,
                        &session,
                        expected_app_project_generation,
                    )
                    .context("UserApp binding changed before session registration");
                }
                ensure!(
                    inner.add_userapp_session_if_generation(
                        &binding.app_id,
                        expected_app_project_generation,
                        session_id,
                        &session.session_generation
                    )?,
                    "UserApp builder route generation changed before session registration"
                );
                state.sessions.insert(session_id.into(), session);
                let current = state
                    .bindings
                    .get_mut(&binding.binding_generation)
                    .context("UserApp binding was retired")?;
                current.revision = current
                    .revision
                    .checked_add(1)
                    .context("UserApp binding revision exhausted")?;
                current.current_session_id = Some(session_id.into());
                current.updated_at = Utc::now();
                let result = current.clone();
                state
                    .receipts
                    .insert(request_id.into(), (fingerprint, true));
                let _ = deadline;
                Ok(result)
            }
            #[cfg(feature = "pg")]
            Self::Postgres(store) => {
                store
                    .register_userapp_agent_session_for_route(
                        binding,
                        session_id,
                        request_id,
                        deadline,
                        expected_app_project_generation,
                    )
                    .await
            }
        }
    }

    pub async fn clear_userapp_agent_sessions(
        &self,
        binding: &Binding,
        request_id: &str,
        deadline: tokio::time::Instant,
    ) -> Result<bool> {
        ensure!(
            !request_id.trim().is_empty(),
            "UserApp clear request identity is required"
        );
        match self {
            Self::Memory(inner) => {
                let mut state = inner
                    .userapp_agents
                    .lock()
                    .map_err(|_| anyhow::anyhow!("UserApp agent binding lock poisoned"))?;
                let command = MappingCommand::Clear {
                    binding: binding.clone(),
                    sessions: Vec::new(),
                };
                let fingerprint = command.fingerprint_input()?;
                if let Some((old, result)) = state.receipts.get(request_id) {
                    ensure!(
                        old == &fingerprint,
                        "UserApp mapping request identity was reused"
                    );
                    return Ok(*result);
                }
                let matches = state
                    .bindings
                    .get(&binding.binding_generation)
                    .is_some_and(|current| current == binding);
                if matches {
                    let sessions: Vec<_> = state
                        .sessions
                        .values()
                        .filter(|s| s.binding_generation == binding.binding_generation)
                        .cloned()
                        .collect();
                    for member in sessions {
                        state.sessions.remove(&member.session_id);
                        inner.clear_session_one_if_generation(
                            &binding.app_id,
                            &member.session_id,
                            &member.session_generation,
                        );
                    }
                    if let Some(current) = state.bindings.get_mut(&binding.binding_generation) {
                        current.current_session_id = None;
                        current.revision = current
                            .revision
                            .checked_add(1)
                            .context("UserApp binding revision exhausted")?;
                        current.updated_at = Utc::now();
                    }
                }
                state
                    .receipts
                    .insert(request_id.into(), (fingerprint, matches));
                let _ = deadline;
                Ok(matches)
            }
            #[cfg(feature = "pg")]
            Self::Postgres(store) => {
                store
                    .clear_userapp_agent_sessions(binding, request_id, deadline)
                    .await
            }
        }
    }

    /// Cleanup invoked under the existing lifecycle operation's owned budget.
    /// An unknown PG commit is retained by the writer and reported honestly.
    pub async fn invalidate_userapp_agent_bindings_durable(
        &self,
        app_id: &str,
        lifecycle_id: &str,
        request_id: &str,
    ) -> Result<usize> {
        match self {
            Self::Memory(_) => {
                self.invalidate_userapp_agent_bindings(
                    app_id,
                    lifecycle_id,
                    request_id,
                    tokio::time::Instant::now(),
                )
                .await
            }
            #[cfg(feature = "pg")]
            Self::Postgres(store) => {
                store
                    .invalidate_userapp_agent_bindings_durable(app_id, lifecycle_id, request_id)
                    .await
            }
        }
    }

    /// Exact lifecycle cleanup; this API never touches compute or PVCs.
    pub async fn invalidate_userapp_agent_bindings(
        &self,
        app_id: &str,
        lifecycle_id: &str,
        request_id: &str,
        deadline: tokio::time::Instant,
    ) -> Result<usize> {
        ensure!(
            !request_id.trim().is_empty(),
            "UserApp invalidation request identity is required"
        );
        match self {
            Self::Memory(inner) => {
                let mut state = inner
                    .userapp_agents
                    .lock()
                    .map_err(|_| anyhow::anyhow!("UserApp agent binding lock poisoned"))?;
                let fingerprint = MappingCommand::Invalidate {
                    app_id: app_id.into(),
                    lifecycle_id: lifecycle_id.into(),
                    bindings: Vec::new(),
                    sessions: Vec::new(),
                }
                .fingerprint_input()?;
                if let Some((old, _count)) = state.invalidation_receipts.get(request_id) {
                    ensure!(
                        old == &fingerprint,
                        "UserApp invalidation request identity was reused"
                    );
                    return Ok(0);
                }
                let generations: Vec<_> = state
                    .bindings
                    .values()
                    .filter(|b| b.app_id == app_id && b.lifecycle_id == lifecycle_id)
                    .map(|b| b.binding_generation.clone())
                    .collect();
                let sessions: Vec<_> = state
                    .sessions
                    .values()
                    .filter(|s| generations.contains(&s.binding_generation))
                    .cloned()
                    .collect();
                for member in sessions {
                    state.sessions.remove(&member.session_id);
                    inner.clear_session_one_if_generation(
                        app_id,
                        &member.session_id,
                        &member.session_generation,
                    );
                }
                for generation in &generations {
                    state.bindings.remove(generation);
                }
                state
                    .invalidation_receipts
                    .insert(request_id.into(), (fingerprint, generations.len()));
                let _ = deadline;
                Ok(generations.len())
            }
            #[cfg(feature = "pg")]
            Self::Postgres(store) => {
                store
                    .invalidate_userapp_agent_bindings(app_id, lifecycle_id, request_id, deadline)
                    .await
            }
        }
    }
}

pub(crate) fn unique_binding(
    mut bindings: impl Iterator<Item = Binding>,
) -> Result<Option<Binding>> {
    let first = bindings.next();
    ensure!(
        bindings.next().is_none(),
        "UserApp project identity is ambiguous; user_id and app_id are required"
    );
    Ok(first)
}

#[cfg(test)]
mod tests;
