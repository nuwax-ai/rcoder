//! 清理结果与容器过滤器（含通配符匹配）。

use serde::{Deserialize, Serialize};

use super::ContainerStatus;

/// 容器清理结果统计
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct CleanupResult {
    /// 找到的容器数量
    pub total_found: usize,
    /// 成功删除的容器数量
    pub successfully_removed: usize,
    /// 删除失败的容器数量
    pub failed_removals: usize,
    /// 跳过的运行中容器数量（仅在非强制删除时）
    pub skipped_running: usize,
    /// 被删除的容器ID列表
    pub removed_container_ids: Vec<String>,
    /// 失败的容器及错误信息
    pub failed_removals_details: Vec<ContainerRemovalFailure>,
    /// 清理操作耗时（毫秒）
    pub duration_ms: u64,
}

impl CleanupResult {
    /// 是否完全成功（没有失败）
    pub fn is_complete_success(&self) -> bool {
        self.failed_removals == 0
    }

    /// 是否有任何成功删除的容器
    pub fn has_removals(&self) -> bool {
        self.successfully_removed > 0
    }

    /// 获取成功率百分比
    pub fn success_rate(&self) -> f64 {
        if self.total_found == 0 {
            100.0
        } else {
            (self.successfully_removed as f64 / self.total_found as f64) * 100.0
        }
    }
}

/// 容器删除失败信息
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContainerRemovalFailure {
    /// 容器ID
    pub container_id: String,
    /// 容器名称
    pub container_name: String,
    /// 失败原因
    pub error_message: String,
}

/// 容器过滤条件
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ContainerFilter {
    /// 按名称模式过滤
    NamePattern(String),
    /// 按状态过滤
    Status(Vec<ContainerStatus>),
    /// 按标签过滤
    Label(String, String),
    /// 组合过滤条件（AND逻辑）
    And(Vec<ContainerFilter>),
    /// 组合过滤条件（OR逻辑）
    Or(Vec<ContainerFilter>),
}

impl ContainerFilter {
    /// 创建名称模式过滤器
    pub fn name_pattern(pattern: impl Into<String>) -> Self {
        ContainerFilter::NamePattern(pattern.into())
    }

    /// 创建状态过滤器
    pub fn status(statuses: Vec<ContainerStatus>) -> Self {
        ContainerFilter::Status(statuses)
    }

    /// 创建标签过滤器
    pub fn label(key: impl Into<String>, value: impl Into<String>) -> Self {
        ContainerFilter::Label(key.into(), value.into())
    }

    /// 创建AND组合过滤器
    pub fn and(filters: Vec<ContainerFilter>) -> Self {
        ContainerFilter::And(filters)
    }

    /// 创建OR组合过滤器
    pub fn or(filters: Vec<ContainerFilter>) -> Self {
        ContainerFilter::Or(filters)
    }

    /// 检查容器是否匹配过滤条件
    pub fn matches(&self, container: &bollard::models::ContainerSummary) -> bool {
        match self {
            ContainerFilter::NamePattern(pattern) => {
                if let Some(names) = &container.names {
                    for name in names {
                        // Docker 容器名称通常以 '/' 开头，需要去掉
                        let clean_name = name.trim_start_matches('/');
                        if Self::matches_pattern(clean_name, pattern) {
                            return true;
                        }
                    }
                }
                false
            }
            ContainerFilter::Status(statuses) => {
                if let Some(state) = &container.state {
                    // 转换为字符串进行比较
                    let state_str = match state {
                        bollard::models::ContainerSummaryStateEnum::RUNNING => "running",
                        bollard::models::ContainerSummaryStateEnum::EXITED => "exited",
                        bollard::models::ContainerSummaryStateEnum::CREATED => "created",
                        bollard::models::ContainerSummaryStateEnum::PAUSED => "paused",
                        bollard::models::ContainerSummaryStateEnum::RESTARTING => "restarting",
                        bollard::models::ContainerSummaryStateEnum::REMOVING => "removing",
                        bollard::models::ContainerSummaryStateEnum::DEAD => "dead",
                        bollard::models::ContainerSummaryStateEnum::STOPPING => "stopping",
                        bollard::models::ContainerSummaryStateEnum::EMPTY => "unknown",
                    };
                    statuses.iter().any(|s| s.to_string() == state_str)
                } else {
                    false
                }
            }
            ContainerFilter::Label(key, value) => {
                if let Some(labels) = &container.labels {
                    labels.get(key.as_str()) == Some(value)
                } else {
                    false
                }
            }
            ContainerFilter::And(filters) => filters.iter().all(|f| f.matches(container)),
            ContainerFilter::Or(filters) => filters.iter().any(|f| f.matches(container)),
        }
    }

    /// 简单的模式匹配（支持 * 通配符）
    fn matches_pattern(text: &str, pattern: &str) -> bool {
        // 如果模式不包含通配符，直接比较
        if !pattern.contains('*') {
            return text == pattern;
        }

        // 简单的通配符匹配实现
        Self::wildcard_match(text, pattern)
    }

    /// 通配符匹配实现
    fn wildcard_match(text: &str, pattern: &str) -> bool {
        let pattern_chars: Vec<char> = pattern.chars().collect();
        let text_chars: Vec<char> = text.chars().collect();

        let mut text_idx = 0;
        let mut pattern_idx = 0;
        let mut star_idx = -1isize;
        let mut match_idx = 0;

        while text_idx < text_chars.len() {
            if pattern_idx < pattern_chars.len()
                && (pattern_chars[pattern_idx] == text_chars[text_idx]
                    || pattern_chars[pattern_idx] == '?')
            {
                text_idx += 1;
                pattern_idx += 1;
            } else if pattern_idx < pattern_chars.len() && pattern_chars[pattern_idx] == '*' {
                star_idx = pattern_idx as isize;
                match_idx = text_idx as isize;
                pattern_idx += 1;
            } else if star_idx != -1 {
                pattern_idx = (star_idx + 1) as usize;
                match_idx += 1;
                text_idx = match_idx as usize;
            } else {
                return false;
            }
        }

        // 处理模式末尾的通配符
        while pattern_idx < pattern_chars.len() && pattern_chars[pattern_idx] == '*' {
            pattern_idx += 1;
        }

        pattern_idx == pattern_chars.len() && text_idx == text_chars.len()
    }
}

/// 容器清理选项
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CleanupOptions {
    /// 是否强制删除运行中的容器
    pub force_remove_running: bool,
    /// 是否等待容器优雅停止
    pub wait_for_graceful_stop: bool,
    /// 优雅停止超时时间（秒）
    pub stop_timeout_seconds: u64,
    /// 删除容器后是否同时清理相关卷
    pub remove_associated_volumes: bool,
}

impl Default for CleanupOptions {
    fn default() -> Self {
        Self {
            force_remove_running: false,
            wait_for_graceful_stop: true,
            stop_timeout_seconds: 30,
            remove_associated_volumes: false,
        }
    }
}
