//! UserApp agent identity is independent of the application-owned builder route.
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UserAppAgentBinding {
    pub app_id: String,
    pub lifecycle_id: String,
    /// Original caller project/conversation identity; never replaced by app_id.
    pub project_id: String,
    pub user_id: String,
    pub binding_generation: String,
    pub revision: i64,
    pub current_session_id: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UserAppAgentSession {
    pub session_id: String,
    /// Physical sessions-table generation, preventing stale SID reuse.
    pub session_generation: String,
    pub binding_generation: String,
    pub created_at: DateTime<Utc>,
}

impl UserAppAgentBinding {
    pub fn owns(&self, app_id: &str, lifecycle_id: &str, project_id: &str, user_id: &str) -> bool {
        self.app_id == app_id
            && self.lifecycle_id == lifecycle_id
            && self.project_id == project_id
            && self.user_id == user_id
    }
}
