//! 校验问题类型与渲染（agent 可按 file → 字段 → 建议直接修复）。

#[derive(Debug, Clone)]
pub struct ValidationIssue {
    /// 出错的 manifest 文件相对路径（如 `backend-java-b/project.manifest.toml`）。
    pub file: Option<String>,
    /// 涉及服务的 `service_id`（跨服务冲突时为首个声明方）。
    pub service: Option<String>,
    /// TOML 字段键路径（如 `proxy.path`、`run.depends_on`）。
    pub field: Option<String>,
    /// 问题陈述（含实际值）。
    pub message: String,
    /// 修复建议（可直接执行的动作或示例值）。
    pub hint: Option<String>,
}

impl ValidationIssue {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            file: None,
            service: None,
            field: None,
            message: message.into(),
            hint: None,
        }
    }

    pub fn at_file(mut self, file: impl Into<String>) -> Self {
        self.file = Some(file.into());
        self
    }

    pub fn at_service(mut self, service: impl Into<String>) -> Self {
        self.service = Some(service.into());
        self
    }

    pub fn at_field(mut self, field: impl Into<String>) -> Self {
        self.field = Some(field.into());
        self
    }

    pub fn with_hint(mut self, hint: impl Into<String>) -> Self {
        self.hint = Some(hint.into());
        self
    }
}

impl std::fmt::Display for ValidationIssue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut location = Vec::with_capacity(3);
        if let Some(file) = &self.file {
            location.push(file.clone());
        }
        if let Some(service) = &self.service {
            location.push(format!("service \"{service}\""));
        }
        write!(f, "{}", self.message)?;
        if !location.is_empty() {
            write!(f, " [{}]", location.join(" · "))?;
        }
        if let Some(field) = &self.field {
            write!(f, "\n     field: {field}")?;
        }
        if let Some(hint) = &self.hint {
            write!(f, "\n     fix:   {hint}")?;
        }
        Ok(())
    }
}

/// 模块目录内 manifest 文件的规范相对路径（issue 定位用）。
pub fn manifest_file_of(dir: &str) -> String {
    format!("{dir}/project.manifest.toml")
}
