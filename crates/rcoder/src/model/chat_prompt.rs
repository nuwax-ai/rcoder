use std::path::PathBuf;

#[derive(Debug, Clone)]
pub struct ChatPrompt {
    /// 项目ID, 再 ./project_workspace/{project_id} 对应
    pub project_id: String,
    /// 项目路径, 再 ./project_workspace/{project_id}
    pub project_path: PathBuf,
    /// agent 的会话ID ,可能没有,如果没有,agent使用自动创建会话,返回会话id
    pub session_id: Option<String>,
    /// 提示内容 prompt
    pub prompt: String,
}

/// 返回用户 prompt 的提示,一定有project_id ,session_id ,否则报错
#[derive(Debug, Clone)]
pub struct ChatPromptResponse {
    /// 项目ID, 再 ./project_workspace/{project_id} 对应
    pub project_id: String,
    /// agent 的会话ID ,可能没有,如果没有,agent使用自动创建会话,返回会话id
    pub session_id: String,
}
