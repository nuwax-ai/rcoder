//! Docker API 时间戳解析 + 需本地 Docker 的集成测试。

use chrono::{DateTime, Utc};

use crate::DockerManager;

impl DockerManager {
    /// 解析 RFC3339 时间戳字符串
    ///
    /// 内部辅助函数，统一处理 Docker API 返回的 RFC3339 时间戳解析
    ///
    /// # 参数
    /// * `timestamp_str` - RFC3339 格式的时间戳字符串
    /// * `context` - 上下文描述（用于日志）
    ///
    /// # 返回
    /// * `Ok(DateTime<Utc>)` - 解析成功
    /// * `Err(String)` - 解析失败，返回错误描述
    pub(crate) fn parse_rfc3339_timestamp(
        timestamp_str: &str,
        context: &str,
    ) -> Result<DateTime<Utc>, String> {
        DateTime::parse_from_rfc3339(timestamp_str)
            .map(|dt| dt.with_timezone(&Utc))
            .map_err(|e| {
                format!(
                    "Failed to parse RFC3339 timestamp for {}: '{}', error: {}",
                    context, timestamp_str, e
                )
            })
    }

    /// 解析 Unix 秒时间戳
    ///
    /// 内部辅助函数，统一处理 Docker API 返回的 Unix 秒时间戳解析
    /// 用于 `list_containers` API 返回的 created 字段
    ///
    /// # 参数
    /// * `timestamp_secs` - Unix 秒时间戳
    /// * `context` - 上下文描述（用于日志）
    ///
    /// # 返回
    /// * `Ok(DateTime<Utc>)` - 解析成功
    /// * `Err(String)` - 解析失败，返回错误描述
    ///
    /// # 注意
    /// Docker 的 list_containers API 返回的是 Unix **秒**时间戳，不是毫秒
    #[cfg(test)]
    pub(crate) fn parse_unix_timestamp(
        timestamp_secs: i64,
        context: &str,
    ) -> Result<DateTime<Utc>, String> {
        DateTime::from_timestamp(timestamp_secs, 0).ok_or_else(|| {
            format!(
                "Failed to parse Unix timestamp for {}: {} (out of range)",
                context, timestamp_secs
            )
        })
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::time::Duration;

    use super::*;
    use bollard::Docker;
    use bollard::query_parameters::InspectContainerOptions;
    use chrono::{DateTime, Utc};

    /// 测试通过容器名称获取创建时间
    ///
    /// 使用真实容器 `rcoder-rcoder-1` 验证时间戳解析
    #[tokio::test]
    #[ignore] // 需要本地环境有 Docker 和容器，默认忽略
    #[allow(deprecated)] // 测试代码使用 deprecated API 是可接受的
    async fn test_get_container_creation_time_by_name_real() {
        // 直接使用 Bollard 创建 Docker 客户端
        let docker = Docker::connect_with_local_defaults().expect("Failed to connect to Docker");

        // 测试容器名称
        let container_name = "rcoder-rcoder-1";

        println!("\n🔍 checking container: {}", container_name);
        println!("─────────────────────────────────────────");

        // 直接调用 Docker API 获取容器信息
        match docker
            .inspect_container(container_name, None::<InspectContainerOptions>)
            .await
        {
            Ok(details) => {
                println!("✅ succeeded getcontainer");

                // 获取创建时间字符串
                if let Some(ref created_str) = details.created {
                    println!(" Docker API created: {}", created_str);

                    // 解析时间戳
                    match DateTime::parse_from_rfc3339(created_str) {
                        Ok(created_time) => {
                            let created_time_utc = created_time.with_timezone(&Utc);
                            println!(" created UTC: {}", created_time_utc);

                            // 计算容器年龄
                            let age = Utc::now().signed_duration_since(created_time_utc);
                            println!(" container age (seconds): {}", age.num_seconds());
                            println!(" container age (minutes): {}", age.num_minutes());
                            println!(" container age (hours): {}", age.num_hours());
                            println!(" container age (days): {}", age.num_days());

                            // 验证时间是否合理
                            assert!(created_time_utc < Utc::now(), "创建时间应该在过去");
                            assert!(age.num_days() < 365, "创建时间不应该超过 1 年");

                            println!("\n✅ timestamp test passed!");
                        }
                        Err(e) => {
                            panic!("❌ RFC3339 时间戳解析失败: {}", e);
                        }
                    }
                } else {
                    panic!("❌ 容器没有 created 字段");
                }

                // 使用 Docker CLI 对比验证
                println!("\n🔍 checking Docker CLI:");
                println!("─────────────────────────────────────────");

                use std::process::Command;
                let output = Command::new("docker")
                    .args(["inspect", container_name, "--format", "{{.Created}}"])
                    .output()
                    .expect("Failed to run docker inspect");

                let docker_cli_time = String::from_utf8_lossy(&output.stdout);
                println!(" Docker CLI created: {}", docker_cli_time.trim());

                // 解析 Docker CLI 返回的时间
                if let Ok(docker_time) = DateTime::parse_from_rfc3339(docker_cli_time.trim()) {
                    let docker_time_utc = docker_time.with_timezone(&Utc);
                    println!("   Docker CLI UTC: {}", docker_time_utc);

                    // 从 Docker API 获取的时间
                    if let Some(ref created_str) = details.created
                        && let Ok(api_time) = DateTime::parse_from_rfc3339(created_str)
                    {
                        let api_time_utc = api_time.with_timezone(&Utc);
                        println!(" API created UTC: {}", api_time_utc);

                        // 时间差应该为 0（应该完全一致）
                        let diff = (docker_time_utc.timestamp() - api_time_utc.timestamp()).abs();
                        println!(" time diff: {} seconds", diff);

                        assert_eq!(diff, 0, "API 和 CLI 返回的时间应该完全一致");
                        println!("\n✅ Docker CLI check passed!");
                    }
                }
            }
            Err(e) => {
                panic!("❌ 获取容器信息失败: {}", e);
            }
        }
    }

    /// 测试 Unix 时间戳解析（验证 bug 修复）
    #[tokio::test]
    #[ignore]
    #[allow(deprecated)] // 测试代码使用 deprecated API 是可接受的
    async fn test_unix_timestamp_parsing() {
        use chrono::TimeZone;

        println!("\n🔍 testing Unix timestamp ( old bug )");
        println!("─────────────────────────────────────────");

        // 容器实际创建时间: 2026-01-19T07:35:53Z
        let expected_time = Utc.with_ymd_and_hms(2026, 1, 19, 7, 35, 53).unwrap();
        let unix_timestamp = expected_time.timestamp(); // 1768808153 秒

        println!(" expected time: {}", expected_time);
        println!(" unix timestamp: {}", unix_timestamp);

        // 使用我们的解析函数
        match DockerManager::parse_unix_timestamp(unix_timestamp, "test") {
            Ok(parsed_time) => {
                println!(" parsed time: {}", parsed_time);

                let diff = (parsed_time.timestamp() - expected_time.timestamp()).abs();
                println!(" time diff: {} seconds", diff);

                assert_eq!(diff, 0, "时间戳解析应该完全准确");
                println!("\n✅ Unix timestamp test passed!");
            }
            Err(e) => {
                panic!("❌ 解析失败: {}", e);
            }
        }

        // 验证旧代码的错误
        println!("\n🔍 verifying bug:");
        let wrong_seconds = unix_timestamp / 1000; // 旧代码的错误处理
        let wrong_time = Utc.timestamp_opt(wrong_seconds, 0).single().unwrap();
        println!(" wrong time: {} (error!)", wrong_time);
        println!(
            "   与正确时间相差: {} 天",
            (expected_time.timestamp() - wrong_time.timestamp()) / 86400
        );
    }

    /// 测试时间戳解析的完整流程
    ///
    /// 主动创建一个测试容器，同时使用 list_containers 和 inspect_container API
    /// 验证 parse_unix_timestamp 和 parse_rfc3339_timestamp 的正确性
    #[tokio::test]
    #[ignore] // 需要本地 Docker 环境
    async fn test_timestamp_parsing_with_real_container() {
        use bollard::models::ContainerCreateBody;
        use bollard::query_parameters::{
            CreateContainerOptionsBuilder, CreateImageOptionsBuilder, ListContainersOptionsBuilder,
            RemoveContainerOptionsBuilder,
        };
        use futures_util::TryStreamExt;

        // 连接 Docker
        let docker = Docker::connect_with_local_defaults().expect("Failed to connect to Docker");

        // 测试容器名称（使用时间戳避免冲突）
        let container_name = format!("test-timestamp-{}", chrono::Utc::now().timestamp());

        println!("\n🔍 testing timestamp parsing");
        println!("─────────────────────────────────────────");
        println!(" testing container: {}", container_name);

        // 拉取 alpine 镜像（如果不存在）
        println!("\n📥 pulling image: alpine:latest");
        let create_image_options = CreateImageOptionsBuilder::default()
            .from_image("alpine:latest")
            .build();

        drop(
            docker
                .create_image(Some(create_image_options), None, None)
                .try_collect::<Vec<_>>()
                .await,
        );

        // 1. 创建测试容器（使用 alpine 镜像）
        let config = ContainerCreateBody {
            image: Some("alpine:latest".to_string()),
            cmd: Some(vec!["sleep".to_string(), "3600".to_string()]),
            host_config: Some(bollard::models::HostConfig {
                auto_remove: Some(false),
                ..Default::default()
            }),
            ..Default::default()
        };

        let create_options = CreateContainerOptionsBuilder::default()
            .name(&container_name)
            .build();

        let create_result = docker
            .create_container(Some(create_options), config)
            .await
            .expect("Failed to create test container");

        println!("✅ container already created: {}", create_result.id);

        // 2. 启动容器
        docker
            .start_container(
                &container_name,
                None::<bollard::query_parameters::StartContainerOptions>,
            )
            .await
            .expect("Failed to start test container");

        println!("✅ container already started");

        // 等待容器完全启动
        tokio::time::sleep(Duration::from_millis(500)).await;

        // 3. 使用 list_containers API 获取 Unix 时间戳
        println!("\n📋 testing list_containers API (Unix timestamp):");
        println!("─────────────────────────────────────────");

        let mut filters = HashMap::new();
        filters.insert("name".to_string(), vec![container_name.clone()]);

        let list_options = ListContainersOptionsBuilder::default()
            .all(true)
            .filters(&filters)
            .build();

        let containers = docker
            .list_containers(Some(list_options))
            .await
            .expect("Failed to list containers");

        assert_eq!(containers.len(), 1, "应该只找到一个测试容器");
        let container = &containers[0];

        let unix_timestamp = container.created.expect("容器应该有 created 字段");
        println!(" unix timestamp: {} seconds", unix_timestamp);

        // 使用 parse_unix_timestamp 解析
        let parsed_unix_time = DockerManager::parse_unix_timestamp(
            unix_timestamp,
            &format!("container {}", container_name),
        )
        .expect("parse_unix_timestamp 应该成功");

        println!(" parsed (UTC): {}", parsed_unix_time);

        // 4. 使用 inspect_container API 获取 RFC3339 时间戳
        println!("\n📋 testing inspect_container API (RFC3339 timestamp):");
        println!("─────────────────────────────────────────");

        let details = docker
            .inspect_container(&container_name, None::<InspectContainerOptions>)
            .await
            .expect("Failed to inspect container");

        let rfc3339_str = details.created.expect("容器应该有 created 字段");
        println!(" RFC3339 timestamp: {}", rfc3339_str);

        // 使用 parse_rfc3339_timestamp 解析
        let parsed_rfc3339_time = DockerManager::parse_rfc3339_timestamp(
            &rfc3339_str,
            &format!("container {}", container_name),
        )
        .expect("parse_rfc3339_timestamp 应该成功");

        println!(" parsed (UTC): {}", parsed_rfc3339_time);

        // 5. 验证两个解析结果的一致性
        println!("\n🔍 comparing API results:");
        println!("─────────────────────────────────────────");

        let time_diff = (parsed_unix_time.timestamp() - parsed_rfc3339_time.timestamp()).abs();
        println!(" list_containers parsed: {}", parsed_unix_time);
        println!(" inspect_container parsed: {}", parsed_rfc3339_time);
        println!(" time diff: {} seconds", time_diff);

        // 两个 API 应该返回相同的时间（允许 1 秒误差，因为精度不同）
        assert!(
            time_diff <= 1,
            "两个 API 的时间差应该在 1 秒以内，实际差异: {} 秒",
            time_diff
        );

        // 6. 验证时间合理性
        println!("\n🔍 verifying timestamps:");
        println!("─────────────────────────────────────────");

        let now = Utc::now();
        let age = now.signed_duration_since(parsed_unix_time);

        println!(" current time: {}", now);
        println!(" container age (seconds): {}", age.num_seconds());

        assert!(age.num_seconds() >= 0, "容器创建时间应该在过去");
        assert!(age.num_seconds() < 60, "容器应该是刚创建的（< 60 秒）");

        println!("\n✅ timestamp test passed!");

        // 7. 清理测试容器
        println!("\n🧹 cleaning up test container...");

        let remove_options = RemoveContainerOptionsBuilder::default().force(true).build();

        docker
            .remove_container(&container_name, Some(remove_options))
            .await
            .expect("Failed to cleanup test container");

        println!("✅ container already cleaned up: {}", container_name);
    }
}
