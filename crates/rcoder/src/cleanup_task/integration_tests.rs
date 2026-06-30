//! 清理任务集成测试
//!
//! 测试核心业务逻辑：
//! 1. ComputerAgentRunner 引用计数检查（核心）
//! 2. 活跃窗口边界条件
//! 3. 容器标识符获取策略

#[cfg(test)]
mod tests {
    use super::super::*;
    use crate::cleanup_task::strategies::CleanupStrategy;
    use chrono::{Duration as ChronoDuration, Utc};
    use shared_types::{
        ContainerBasicInfo, ProjectAndContainerInfo, ProjectExtendedFields, ServiceType,
    };
    use std::sync::Arc;
    use std::time::Duration;

    /// 创建测试用的 ProjectAndContainerInfo
    fn create_test_project(
        project_id: &str,
        user_id: &str,
        service_type: ServiceType,
        last_activity_seconds_ago: i64,
    ) -> Arc<ProjectAndContainerInfo> {
        let last_activity = Utc::now() - ChronoDuration::seconds(last_activity_seconds_ago);
        let created_at = Utc::now() - ChronoDuration::hours(2);
        let container = ContainerBasicInfo {
            container_id: format!("container_{}", project_id),
            container_name: format!("container_{}", project_id),
            container_ip: "127.0.0.1".to_string(),
            internal_port: 8086,
            external_port: 0,
            project_id: project_id.to_string(),
            status: "running".to_string(),
            created_at,
            service_url: format!("http://{}", project_id),
        };

        Arc::new(ProjectAndContainerInfo::from_parts(
            project_id.to_string(),
            Some(user_id.to_string()),
            None,
            None,
            Some(container),
            ProjectExtendedFields {
                service_type: Some(service_type),
                last_activity: Some(last_activity),
                created_at: Some(created_at),
                ..Default::default()
            },
        ))
    }

    /// 创建带 pod_id 的测试用 ProjectAndContainerInfo（RCoder 共享容器模式）
    fn create_test_project_with_pod(
        project_id: &str,
        user_id: &str,
        pod_id: &str,
        service_type: ServiceType,
        last_activity_seconds_ago: i64,
    ) -> Arc<ProjectAndContainerInfo> {
        let last_activity = Utc::now() - ChronoDuration::seconds(last_activity_seconds_ago);
        let created_at = Utc::now() - ChronoDuration::hours(2);
        let container = ContainerBasicInfo {
            container_id: format!("container_{}", pod_id),
            container_name: format!("container_{}", pod_id),
            container_ip: "127.0.0.1".to_string(),
            internal_port: 8086,
            external_port: 0,
            project_id: project_id.to_string(),
            status: "running".to_string(),
            created_at,
            service_url: format!("http://{}", pod_id),
        };

        Arc::new(ProjectAndContainerInfo::from_parts(
            project_id.to_string(),
            Some(user_id.to_string()),
            Some(pod_id.to_string()),
            None,
            Some(container),
            ProjectExtendedFields {
                service_type: Some(service_type),
                last_activity: Some(last_activity),
                created_at: Some(created_at),
                ..Default::default()
            },
        ))
    }

    // ========================================================================
    // 核心测试：ComputerAgentRunner 引用计数逻辑
    // ========================================================================

    /// 测试场景：有活跃项目时，不应该销毁容器
    #[test]
    fn test_computer_runner_ref_count_with_active_projects() {
        let proj_a =
            create_test_project("proj_A", "user_1", ServiceType::ComputerAgentRunner, 1800);
        let proj_b = create_test_project("proj_B", "user_1", ServiceType::ComputerAgentRunner, 120);
        let proj_c =
            create_test_project("proj_C", "user_1", ServiceType::ComputerAgentRunner, 1800);

        let config = CleanupConfig {
            active_window: Duration::from_secs(300),
            ..Default::default()
        };

        assert!(
            strategies::computer_runner::is_project_active(&proj_b, &config),
            "proj_B (2分钟前活动) 应该被认为是活跃的"
        );
        assert!(
            !strategies::computer_runner::is_project_active(&proj_a, &config),
            "proj_A (30分钟前活动) 应该被认为是闲置的"
        );

        let related_projects = [proj_a.clone(), proj_b.clone(), proj_c.clone()];
        let has_active_refs = related_projects.iter().any(|p| {
            p.project_id() != "proj_A" && strategies::computer_runner::is_project_active(p, &config)
        });

        assert!(
            has_active_refs,
            "应该存在活跃的引用项目 (proj_B)，因此不应该销毁容器"
        );
    }

    /// 测试场景：所有项目都闲置时，应该销毁容器
    #[test]
    fn test_computer_runner_ref_count_all_idle() {
        let proj_d =
            create_test_project("proj_D", "user_2", ServiceType::ComputerAgentRunner, 3600);
        let proj_e =
            create_test_project("proj_E", "user_2", ServiceType::ComputerAgentRunner, 7200);
        let proj_f =
            create_test_project("proj_F", "user_2", ServiceType::ComputerAgentRunner, 1800);

        let config = CleanupConfig {
            active_window: Duration::from_secs(300),
            ..Default::default()
        };

        let related_projects = vec![proj_d.clone(), proj_e.clone(), proj_f.clone()];

        for proj in &related_projects {
            assert!(
                !strategies::computer_runner::is_project_active(proj, &config),
                "{} 应该被认为是闲置的",
                proj.project_id()
            );
        }

        let has_active_refs = related_projects.iter().any(|p| {
            p.project_id() != "proj_D" && strategies::computer_runner::is_project_active(p, &config)
        });

        assert!(!has_active_refs, "不存在活跃的引用项目，因此应该销毁容器");
    }

    // ========================================================================
    // 测试：活跃窗口边界条件
    // ========================================================================

    #[test]
    fn test_active_window_boundary_conditions() {
        let config = CleanupConfig {
            idle_timeout: Duration::from_secs(600),
            ..Default::default()
        };

        let test_cases = vec![
            ("刚刚活动", 0, true),
            ("1分钟前", 60, true),
            ("9分59秒前", 599, true),
            ("恰好在边界", 600, false),
            ("10分1秒前", 601, false),
            ("30分钟前", 1800, false),
        ];

        for (desc, seconds_ago, expected_active) in test_cases {
            let project = create_test_project(
                &format!("project_{}", desc.replace(' ', "_")),
                "test_user",
                ServiceType::ComputerAgentRunner,
                seconds_ago,
            );

            let is_active = strategies::computer_runner::is_project_active(&project, &config);

            assert_eq!(
                is_active, expected_active,
                "{}: {}秒前活动, 预期={}, 实际={}",
                desc, seconds_ago, expected_active, is_active
            );
        }
    }

    // ========================================================================
    // 测试：容器标识符获取策略
    // ========================================================================

    #[test]
    fn test_container_identifier_extraction() {
        let rcoder_strategy = strategies::rcoder::RCoderStrategy;
        let computer_runner_strategy = strategies::computer_runner::ComputerRunnerStrategy;

        let rcoder_info = strategies::ProjectInfo {
            project_id: "project_abc".to_string(),
            user_id: Some("user_xyz".to_string()),
            pod_id: None,
            last_activity: Utc::now(),
        };

        let rcoder_id = rcoder_strategy
            .get_container_identifier(&rcoder_info)
            .unwrap();
        assert_eq!(
            rcoder_id, "project_abc",
            "RCoder 无 pod_id 时应该使用 project_id 作为容器标识符"
        );

        let rcoder_pod_info = strategies::ProjectInfo {
            project_id: "project_jkl".to_string(),
            user_id: Some("user_xyz".to_string()),
            pod_id: Some("pod_123".to_string()),
            last_activity: Utc::now(),
        };

        let rcoder_pod_id = rcoder_strategy
            .get_container_identifier(&rcoder_pod_info)
            .unwrap();
        assert_eq!(
            rcoder_pod_id, "pod_123",
            "RCoder 有 pod_id 时应该使用 pod_id 作为容器标识符"
        );

        let runner_info = strategies::ProjectInfo {
            project_id: "project_def".to_string(),
            user_id: Some("user_123".to_string()),
            pod_id: None,
            last_activity: Utc::now(),
        };

        let runner_id = computer_runner_strategy
            .get_container_identifier(&runner_info)
            .unwrap();
        assert_eq!(
            runner_id, "user_123",
            "ComputerAgentRunner 应该使用 user_id 作为容器标识符"
        );

        let runner_info_missing_user = strategies::ProjectInfo {
            project_id: "project_ghi".to_string(),
            user_id: None,
            pod_id: None,
            last_activity: Utc::now(),
        };

        let result = computer_runner_strategy.get_container_identifier(&runner_info_missing_user);
        assert!(result.is_err(), "缺少 user_id 时应该返回错误");
    }

    // ========================================================================
    // 测试：RCoder pod_id 共享容器引用计数逻辑
    // ========================================================================

    #[test]
    fn test_rcoder_pod_id_ref_count_with_active_projects() {
        let proj_a = create_test_project_with_pod(
            "proj_A",
            "user_1",
            "pod_1",
            ServiceType::WebAgentRunner,
            1800,
        );
        let proj_b = create_test_project_with_pod(
            "proj_B",
            "user_1",
            "pod_1",
            ServiceType::WebAgentRunner,
            120,
        );
        let proj_c = create_test_project_with_pod(
            "proj_C",
            "user_1",
            "pod_1",
            ServiceType::WebAgentRunner,
            1800,
        );

        let config = CleanupConfig {
            idle_timeout: Duration::from_secs(600),
            ..Default::default()
        };

        assert!(
            strategies::computer_runner::is_project_active(&proj_b, &config),
            "proj_B (2分钟前活动) 应该被认为是活跃的"
        );
        assert!(
            !strategies::computer_runner::is_project_active(&proj_a, &config),
            "proj_A (30分钟前活动) 应该被认为是闲置的"
        );

        let related_projects = [proj_a.clone(), proj_b.clone(), proj_c.clone()];
        let has_active_refs = related_projects.iter().any(|p| {
            p.project_id() != "proj_A" && strategies::computer_runner::is_project_active(p, &config)
        });

        assert!(
            has_active_refs,
            "应该存在活跃的引用项目 (proj_B)，因此不应该销毁容器"
        );
    }

    #[test]
    fn test_rcoder_pod_id_ref_count_all_idle() {
        let proj_d = create_test_project_with_pod(
            "proj_D",
            "user_2",
            "pod_2",
            ServiceType::WebAgentRunner,
            3600,
        );
        let proj_e = create_test_project_with_pod(
            "proj_E",
            "user_2",
            "pod_2",
            ServiceType::WebAgentRunner,
            7200,
        );
        let proj_f = create_test_project_with_pod(
            "proj_F",
            "user_2",
            "pod_2",
            ServiceType::WebAgentRunner,
            1800,
        );

        let config = CleanupConfig {
            idle_timeout: Duration::from_secs(600),
            ..Default::default()
        };

        let related_projects = vec![proj_d.clone(), proj_e.clone(), proj_f.clone()];

        for proj in &related_projects {
            assert!(
                !strategies::computer_runner::is_project_active(proj, &config),
                "{} 应该被认为是闲置的",
                proj.project_id()
            );
        }

        let has_active_refs = related_projects.iter().any(|p| {
            p.project_id() != "proj_D" && strategies::computer_runner::is_project_active(p, &config)
        });

        assert!(!has_active_refs, "不存在活跃的引用项目，因此应该销毁容器");
    }

    #[test]
    fn test_rcoder_no_pod_id_always_destroy() {
        let proj = create_test_project("proj_solo", "user_3", ServiceType::WebAgentRunner, 1800);

        let config = CleanupConfig {
            idle_timeout: Duration::from_secs(600),
            ..Default::default()
        };

        assert!(proj.pod_id().is_none(), "无 pod_id 的项目应该直接销毁容器");
        assert!(
            !strategies::computer_runner::is_project_active(&proj, &config),
            "proj_solo (30分钟前活动) 应该被认为是闲置的"
        );
    }
}
