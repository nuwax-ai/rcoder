use std::{
    path::{Component, PathBuf},
    sync::LazyLock,
};

use dashmap::DashMap;

use crate::{AgentStatus, ChatPrompt, ChatPromptResponse, ProjectAndAgentInfo};

/// 使用 OnceLock 和 DashMap 管理 ProjectAndAgentInfo
pub static PROJECT_AND_AGENT_INFO_MAP: LazyLock<DashMap<String, ProjectAndAgentInfo>> =
    LazyLock::new(DashMap::new);
