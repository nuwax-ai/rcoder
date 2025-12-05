//! 网络工具函数

/// 根据项目名称构建网络名称
///
/// # Arguments
/// * `project_name` - Docker Compose 项目名称
///
/// # Returns
/// * `String` - 完整的网络名称
///
/// # Examples
/// ```
/// use docker_manager::network::build_network_name;
/// let network = build_network_name("rcoder");
/// // 返回: "rcoder_agent-network"
/// ```
pub fn build_network_name(project_name: &str) -> String {
    format!("{}_{}", project_name, crate::RCODER_NETWORK_BASE_NAME)
}

/// 解析网络名称获取项目名称
///
/// # Arguments
/// * `network_name` - 完整的网络名称
///
/// # Returns
/// * `Option<String>` - 项目名称，如果无法解析返回 None
pub fn parse_project_from_network(network_name: &str) -> Option<String> {
    if let Some(pos) = network_name.rfind('_') {
        let suffix = &network_name[pos + 1..];
        if suffix == crate::RCODER_NETWORK_BASE_NAME {
            return Some(network_name[..pos].to_string());
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_build_network_name() {
        let network = build_network_name("rcoder");
        assert_eq!(network, "rcoder_agent-network");

        let network = build_network_name("myproject");
        assert_eq!(network, "myproject_agent-network");
    }

    #[test]
    fn test_parse_project_from_network() {
        assert_eq!(
            parse_project_from_network("rcoder_agent-network"),
            Some("rcoder".to_string())
        );

        assert_eq!(
            parse_project_from_network("myproject_agent-network"),
            Some("myproject".to_string())
        );

        assert_eq!(parse_project_from_network("invalid_network"), None);
    }
}
