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
    use duckdb_manager::ProjectRecord;
    use shared_types::ServiceType;
    use std::time::Duration;

    /// 创建测试用的 ProjectRecord
    fn create_test_project(
        project_id: &str,
        user_id: &str,
        service_type: ServiceType,
        last_activity_seconds_ago: i64,
    ) -> ProjectRecord {
        ProjectRecord {
            project_id: project_id.to_string(),
            session_id: None,
            service_type,
            container_id: format!("container_{}", project_id),
            user_id: Some(user_id.to_string()),
            pod_id: None,
            agent_status_code: None,
            agent_status_name: None,
            request_id: None,
            model_provider_json: None,
            created_at: Utc::now() - ChronoDuration::hours(2),
            last_activity: Utc::now() - ChronoDuration::seconds(last_activity_seconds_ago),
            session_created_at: None,
            session_last_activity: None,
        }
    }

    /// 创建带 pod_id 的测试用 ProjectRecord（RCoder 共享容器模式）
    fn create_test_project_with_pod(
        project_id: &str,
        user_id: &str,
        pod_id: &str,
        service_type: ServiceType,
        last_activity_seconds_ago: i64,
    ) -> ProjectRecord {
        ProjectRecord {
            project_id: project_id.to_string(),
            session_id: None,
            service_type,
            container_id: format!("container_{}", pod_id),
            user_id: Some(user_id.to_string()),
            pod_id: Some(pod_id.to_string()),
            agent_status_code: None,
            agent_status_name: None,
            request_id: None,
            model_provider_json: None,
            created_at: Utc::now() - ChronoDuration::hours(2),
            last_activity: Utc::now() - ChronoDuration::seconds(last_activity_seconds_ago),
            session_created_at: None,
            session_last_activity: None,
        }
    }

    // ========================================================================
    // 核心测试：ComputerAgentRunner 引用计数逻辑
    // ========================================================================

    /// 测试场景：有活跃项目时，不应该销毁容器
    #[test]
    fn test_computer_runner_ref_count_with_active_projects() {
        // 场景：user_1 有 3 个项目
        // - proj_A: 闲置30分钟（准备清理）
        // - proj_B: 活跃2分钟（仍在使用）
        // - proj_C: 闲置30分钟
        //
        // 预期：清理 proj_A 时，因为 proj_B 仍活跃，容器应该被保留

        let proj_a =
            create_test_project("proj_A", "user_1", ServiceType::ComputerAgentRunner, 1800);
        let proj_b = create_test_project("proj_B", "user_1", ServiceType::ComputerAgentRunner, 120);
        let proj_c =
            create_test_project("proj_C", "user_1", ServiceType::ComputerAgentRunner, 1800);

        let config = CleanupConfig {
            active_window: Duration::from_secs(300), // 5分钟窗口
            ..Default::default()
        };

        // proj_B 应该是活跃的
        assert!(
            strategies::computer_runner::is_project_active(&proj_b, &config),
            "proj_B (2分钟前活动) 应该被认为是活跃的"
        );

        // proj_A 和 proj_C 应该是闲置的
        assert!(
            !strategies::computer_runner::is_project_active(&proj_a, &config),
            "proj_A (30分钟前活动) 应该被认为是闲置的"
        );

        // 验证引用计数逻辑：存在活跃引用，不应该销毁容器
        let related_projects = [proj_a.clone(), proj_b.clone(), proj_c.clone()];
        let has_active_refs = related_projects.iter().any(|p| {
            p.project_id != "proj_A" && strategies::computer_runner::is_project_active(p, &config)
        });

        assert!(
            has_active_refs,
            "应该存在活跃的引用项目 (proj_B)，因此不应该销毁容器"
        );
    }

    /// 测试场景：所有项目都闲置时，应该销毁容器
    #[test]
    fn test_computer_runner_ref_count_all_idle() {
        // 场景：user_2 有 3 个项目，全部闲置
        // - proj_D: 闲置1小时
        // - proj_E: 闲置2小时
        // - proj_F: 闲置30分钟
        //
        // 预期：清理 proj_D 时，因为所有项目都闲置，应该销毁容器

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

        // 所有项目都应该是闲置的
        for proj in &related_projects {
            assert!(
                !strategies::computer_runner::is_project_active(proj, &config),
                "{} 应该被认为是闲置的",
                proj.project_id
            );
        }

        // 没有活跃引用，应该销毁容器
        let has_active_refs = related_projects.iter().any(|p| {
            p.project_id != "proj_D" && strategies::computer_runner::is_project_active(p, &config)
        });

        assert!(!has_active_refs, "不存在活跃的引用项目，因此应该销毁容器");
    }

    // ========================================================================
    // 测试：活跃窗口边界条件
    // ========================================================================

    #[test]
    fn test_active_window_boundary_conditions() {
        // is_project_active 使用 idle_timeout 作为判断标准（与 scanner 一致）
        let config = CleanupConfig {
            idle_timeout: Duration::from_secs(600), // 10分钟
            ..Default::default()
        };

        // 测试边界：(描述, 距离上次活动秒数, 是否应该活跃)
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

        // RCoder 无 pod_id: 使用 project_id
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

        // RCoder 有 pod_id: 使用 pod_id（共享容器模式）
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

        // ComputerAgentRunner: 使用 user_id
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

        // ComputerAgentRunner 缺少 user_id 应该返回错误
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

    /// 测试场景：RCoder 有 pod_id 时，有活跃项目不应该销毁容器
    #[test]
    fn test_rcoder_pod_id_ref_count_with_active_projects() {
        // 场景：pod_1 下有 3 个 RCoder 项目
        // - proj_A: 闲置30分钟（准备清理）
        // - proj_B: 活跃2分钟（仍在使用）
        // - proj_C: 闲置30分钟
        //
        // 预期：清理 proj_A 时，因为 proj_B 仍活跃，容器应该被保留

        let proj_a = create_test_project_with_pod(
            "proj_A",
            "user_1",
            "pod_1",
            ServiceType::RCoder,
            1800,
        );
        let proj_b = create_test_project_with_pod(
            "proj_B",
            "user_1",
            "pod_1",
            ServiceType::RCoder,
            120,
        );
        let proj_c = create_test_project_with_pod(
            "proj_C",
            "user_1",
            "pod_1",
            ServiceType::RCoder,
            1800,
        );

        let config = CleanupConfig {
            idle_timeout: Duration::from_secs(600), // 10分钟
            ..Default::default()
        };

        // proj_B 应该是活跃的
        assert!(
            strategies::computer_runner::is_project_active(&proj_b, &config),
            "proj_B (2分钟前活动) 应该被认为是活跃的"
        );

        // proj_A 和 proj_C 应该是闲置的
        assert!(
            !strategies::computer_runner::is_project_active(&proj_a, &config),
            "proj_A (30分钟前活动) 应该被认为是闲置的"
        );

        // 验证引用计数逻辑：存在活跃引用，不应该销毁容器
        let related_projects = [proj_a.clone(), proj_b.clone(), proj_c.clone()];
        let has_active_refs = related_projects.iter().any(|p| {
            p.project_id != "proj_A" && strategies::computer_runner::is_project_active(p, &config)
        });

        assert!(
            has_active_refs,
            "应该存在活跃的引用项目 (proj_B)，因此不应该销毁容器"
        );
    }

    /// 测试场景：RCoder 有 pod_id 时，所有项目都闲置应该销毁容器
    #[test]
    fn test_rcoder_pod_id_ref_count_all_idle() {
        // 场景：pod_2 下有 3 个 RCoder 项目，全部闲置
        // - proj_D: 闲置1小时
        // - proj_E: 闲置2小时
        // - proj_F: 闲置30分钟
        //
        // 预期：清理 proj_D 时，因为所有项目都闲置，应该销毁容器

        let proj_d = create_test_project_with_pod(
            "proj_D",
            "user_2",
            "pod_2",
            ServiceType::RCoder,
            3600,
        );
        let proj_e = create_test_project_with_pod(
            "proj_E",
            "user_2",
            "pod_2",
            ServiceType::RCoder,
            7200,
        );
        let proj_f = create_test_project_with_pod(
            "proj_F",
            "user_2",
            "pod_2",
            ServiceType::RCoder,
            1800,
        );

        let config = CleanupConfig {
            idle_timeout: Duration::from_secs(600),
            ..Default::default()
        };

        let related_projects = vec![proj_d.clone(), proj_e.clone(), proj_f.clone()];

        // 所有项目都应该是闲置的
        for proj in &related_projects {
            assert!(
                !strategies::computer_runner::is_project_active(proj, &config),
                "{} 应该被认为是闲置的",
                proj.project_id
            );
        }

        // 没有活跃引用，应该销毁容器
        let has_active_refs = related_projects.iter().any(|p| {
            p.project_id != "proj_D" && strategies::computer_runner::is_project_active(p, &config)
        });

        assert!(!has_active_refs, "不存在活跃的引用项目，因此应该销毁容器");
    }

    /// 测试场景：RCoder 无 pod_id 时，始终应该销毁容器（1:1 模式）
    #[test]
    fn test_rcoder_no_pod_id_always_destroy() {
        // 场景：无 pod_id 的 RCoder 项目，1容器=1项目
        // 预期：无论其他项目如何，清理时始终销毁容器

        let proj = create_test_project("proj_solo", "user_3", ServiceType::RCoder, 1800);

        let config = CleanupConfig {
            idle_timeout: Duration::from_secs(600),
            ..Default::default()
        };

        // 确认没有 pod_id
        assert!(
            proj.pod_id.is_none(),
            "无 pod_id 的项目应该直接销毁容器"
        );

        // 项目闲置
        assert!(
            !strategies::computer_runner::is_project_active(&proj, &config),
            "proj_solo (30分钟前活动) 应该被认为是闲置的"
        );
    }
}
