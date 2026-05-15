use super::config::{get_default_sacp_agent_config, get_default_sacp_agent_config_with_resolver};
use super::env::{apply_model_env_bindings, apply_sensitive_model_env_fallback};
use super::mcp::{
    ENV_MCP_PROXY_LOG_DIR, convert_context_servers_sacp, enhance_mcp_proxy_args,
    get_mcp_proxy_log_dir, has_convert_subcommand, has_log_dir_arg, is_mcp_proxy_command,
};
use super::types::SacpAgentLaunchConfig;
use crate::launcher::model_env;
use crate::launcher::model_env::ResolvedModelEnv;
use agent_client_protocol::schema::McpServer;
use agent_config::ContextServerConfig;
use shared_types::{ModelEnvBinding, ModelEnvBindingSource, ModelProviderConfig};
use std::collections::HashMap;

static ENV_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn test_resolved_model_env(override_existing_sensitive_env: bool) -> ResolvedModelEnv {
    ResolvedModelEnv {
        api_key: "resolved-api-key".to_string(),
        base_url: "https://resolved.example.com/v1".to_string(),
        default_model: "resolved-model".to_string(),
        provider_name: "resolved-provider".to_string(),
        override_existing_sensitive_env,
    }
}

#[test]
fn test_model_env_bindings_override_and_create_env_keys() {
    let resolved = test_resolved_model_env(false);
    let mut env = HashMap::from([
        ("CODEX_API_KEY".to_string(), "original-key".to_string()),
        ("CODEX_MODEL".to_string(), "original-model".to_string()),
    ]);
    let bindings = vec![
        ModelEnvBinding {
            env_key: "CODEX_API_KEY".to_string(),
            source: ModelEnvBindingSource::ApiKey,
        },
        ModelEnvBinding {
            env_key: "CODEX_BASE_URL".to_string(),
            source: ModelEnvBindingSource::BaseUrl,
        },
        ModelEnvBinding {
            env_key: "CODEX_MODEL".to_string(),
            source: ModelEnvBindingSource::DefaultModel,
        },
        ModelEnvBinding {
            env_key: "CODEX_PROVIDER".to_string(),
            source: ModelEnvBindingSource::ProviderName,
        },
    ];

    let bound_keys = apply_model_env_bindings(&mut env, &bindings, &resolved);

    assert_eq!(env.get("CODEX_API_KEY"), Some(&resolved.api_key));
    assert_eq!(env.get("CODEX_BASE_URL"), Some(&resolved.base_url));
    assert_eq!(env.get("CODEX_MODEL"), Some(&resolved.default_model));
    assert_eq!(env.get("CODEX_PROVIDER"), Some(&resolved.provider_name));
    assert!(bound_keys.contains("CODEX_API_KEY"));
    assert!(bound_keys.contains("CODEX_BASE_URL"));
    assert!(bound_keys.contains("CODEX_MODEL"));
    assert!(bound_keys.contains("CODEX_PROVIDER"));
}

#[test]
fn test_model_env_bindings_have_priority_over_known_key_fallback() {
    let resolved = test_resolved_model_env(true);
    let mut env = HashMap::from([
        ("CODEX_API_KEY".to_string(), "original-key".to_string()),
        (
            "CODEX_BASE_URL".to_string(),
            "original-base-url".to_string(),
        ),
        (
            "OPENAI_API_KEY".to_string(),
            "original-openai-key".to_string(),
        ),
        (
            "OPENAI_BASE_URL".to_string(),
            "original-openai-base-url".to_string(),
        ),
    ]);
    let bindings = vec![
        ModelEnvBinding {
            env_key: "CODEX_API_KEY".to_string(),
            source: ModelEnvBindingSource::DefaultModel,
        },
        ModelEnvBinding {
            env_key: "CODEX_BASE_URL".to_string(),
            source: ModelEnvBindingSource::ProviderName,
        },
    ];

    let bound_keys = apply_model_env_bindings(&mut env, &bindings, &resolved);
    apply_sensitive_model_env_fallback(&mut env, &resolved, &bound_keys);

    assert_eq!(env.get("CODEX_API_KEY"), Some(&resolved.default_model));
    assert_eq!(env.get("CODEX_BASE_URL"), Some(&resolved.provider_name));
    assert_eq!(env.get("OPENAI_API_KEY"), Some(&resolved.api_key));
    assert_eq!(env.get("OPENAI_BASE_URL"), Some(&resolved.base_url));
}

#[test]
fn test_default_config() {
    let config = get_default_sacp_agent_config(None, &shared_types::ServiceType::RCoder);
    assert!(config.is_ok());
    let config = config.unwrap();

    // 命令应该是 "claude-code-acp-ts" 或其绝对路径（如果 which crate 能找到）
    // 两种情况都是正确的
    let cmd = &config.command;
    assert!(
        cmd == "claude-code-acp-ts" || cmd.ends_with("claude-code-acp-ts"),
        "Expected command to be 'claude-code-acp-ts' or an absolute path ending with 'claude-code-acp-ts', got: {}",
        cmd
    );
}

#[test]
fn test_default_config_with_model_provider() {
    let provider = ModelProviderConfig {
        id: "test-id".to_string(),
        name: "test-provider".to_string(),
        api_key: "sk-test-key".to_string(),
        base_url: "https://api.test.com".to_string(),
        default_model: "test-model".to_string(),
        requires_openai_auth: false,
        api_protocol: None,
        wire_api: None,
    };

    let config = get_default_sacp_agent_config(Some(&provider), &shared_types::ServiceType::RCoder);
    assert!(config.is_ok());
    let config = config.unwrap();

    // 验证 API Key：默认 helper 使用 direct 模式
    assert!(config.env.contains_key("ANTHROPIC_API_KEY"));
    assert_eq!(
        config.env.get("ANTHROPIC_API_KEY"),
        Some(&"sk-test-key".to_string())
    );

    // 应该包含模型设置
    assert!(config.env.contains_key("ANTHROPIC_MODEL"));
    assert_eq!(
        config.env.get("ANTHROPIC_MODEL"),
        Some(&"test-model".to_string())
    );
}

#[test]
fn test_default_config_disables_nonessential_traffic() {
    let config = get_default_sacp_agent_config(None, &shared_types::ServiceType::RCoder);
    assert!(config.is_ok());
    let config = config.unwrap();

    assert_eq!(
        config.env.get("CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC"),
        Some(&"1".to_string())
    );
}

#[test]
fn test_default_config_with_openai_provider() {
    let provider = ModelProviderConfig {
        id: "test-openai".to_string(),
        name: "openai".to_string(),
        api_key: "sk-test-openai-key".to_string(),
        base_url: "https://api.openai.com/v1".to_string(),
        default_model: "openai-compatible/gpt-4".to_string(), // model_name 已包含前缀
        requires_openai_auth: true,
        api_protocol: Some("openai".to_string()),
        wire_api: None,
    };

    let config = get_default_sacp_agent_config(Some(&provider), &shared_types::ServiceType::RCoder);
    assert!(config.is_ok());
    let config = config.unwrap();

    // 验证 OpenAI 环境变量
    assert!(config.env.contains_key("OPENAI_API_KEY"));
    assert!(config.env.contains_key("OPENAI_BASE_URL"));

    assert_eq!(
        config.env.get("OPENAI_API_KEY"),
        Some(&"sk-test-openai-key".to_string())
    );
    assert_eq!(
        config.env.get("OPENAI_BASE_URL"),
        Some(&"https://api.openai.com/v1".to_string())
    );

    // nuwaxcode 使用 OPENCODE_MODEL，直接使用 model_name（已包含 openai-compatible/ 前缀）
    assert!(config.env.contains_key("OPENCODE_MODEL"));
    assert_eq!(
        config.env.get("OPENCODE_MODEL"),
        Some(&"openai-compatible/gpt-4".to_string())
    );

    // 同时验证 Anthropic 环境变量也存在 (兼容性)
    assert!(config.env.contains_key("ANTHROPIC_API_KEY"));
    assert!(config.env.contains_key("ANTHROPIC_BASE_URL"));
}

#[test]
fn test_sensitive_env_vars_protection() {
    // 测试默认配置中的环境变量值：默认 helper 使用 direct 模式
    let provider = ModelProviderConfig {
        id: "test".to_string(),
        name: "test".to_string(),
        api_key: "sk-real-key-should-be-replaced".to_string(),
        base_url: "https://real-url-should-be-replaced.com".to_string(),
        default_model: "openai-compatible/gpt-4".to_string(),
        requires_openai_auth: true,
        api_protocol: Some("openai".to_string()),
        wire_api: None,
    };

    let config = get_default_sacp_agent_config(Some(&provider), &shared_types::ServiceType::RCoder);
    assert!(config.is_ok());
    let config = config.unwrap();

    assert_eq!(
        config.env.get("ANTHROPIC_API_KEY"),
        Some(&"sk-real-key-should-be-replaced".to_string())
    );
    assert_eq!(
        config.env.get("OPENAI_API_KEY"),
        Some(&"sk-real-key-should-be-replaced".to_string())
    );
    assert_eq!(
        config.env.get("ANTHROPIC_BASE_URL"),
        Some(&"https://real-url-should-be-replaced.com".to_string())
    );
    assert_eq!(
        config.env.get("OPENAI_BASE_URL"),
        Some(&"https://real-url-should-be-replaced.com".to_string())
    );
}

#[test]
fn test_default_config_with_proxy_model_env_resolver() {
    let provider = ModelProviderConfig {
        id: "test".to_string(),
        name: "test".to_string(),
        api_key: "sk-real-key".to_string(),
        base_url: "https://real-url.example.com".to_string(),
        default_model: "openai-compatible/gpt-4".to_string(),
        requires_openai_auth: true,
        api_protocol: Some("openai".to_string()),
        wire_api: None,
    };
    let resolver =
        model_env::ProxyModelRuntimeEnvResolver::new("http://localhost:8088/api/{SERVICE_UUID}");

    let config = get_default_sacp_agent_config_with_resolver(
        Some(&provider),
        &shared_types::ServiceType::RCoder,
        &resolver,
        Some("svc-123"),
    )
    .unwrap();

    assert_eq!(
        config.env.get("ANTHROPIC_API_KEY"),
        Some(&model_env::API_KEY_PLACEHOLDER.to_string())
    );
    assert_eq!(
        config.env.get("OPENAI_BASE_URL"),
        Some(&"http://localhost:8088/api/svc-123".to_string())
    );
}

#[test]
fn test_proxy_model_env_resolver_requires_service_uuid() {
    let provider = ModelProviderConfig {
        id: "test".to_string(),
        name: "test".to_string(),
        api_key: "sk-real-key".to_string(),
        base_url: "https://real-url.example.com".to_string(),
        default_model: "test-model".to_string(),
        requires_openai_auth: true,
        api_protocol: None,
        wire_api: None,
    };
    let resolver =
        model_env::ProxyModelRuntimeEnvResolver::new("http://localhost:8088/api/{SERVICE_UUID}");

    let err = get_default_sacp_agent_config_with_resolver(
        Some(&provider),
        &shared_types::ServiceType::RCoder,
        &resolver,
        None,
    )
    .unwrap_err();

    assert!(err.to_string().contains("service_uuid"));
}

#[test]
fn test_convert_context_servers_empty() {
    let configs: HashMap<String, ContextServerConfig> = HashMap::new();
    let servers = convert_context_servers_sacp(&configs);
    assert!(servers.is_empty());
}

#[test]
fn test_convert_context_servers_disabled() {
    let mut configs = HashMap::new();
    configs.insert(
        "disabled-server".to_string(),
        ContextServerConfig {
            source: "local".to_string(),
            enabled: false,
            command: Some("node".to_string()),
            args: None,
            env: None,
        },
    );

    let servers = convert_context_servers_sacp(&configs);
    assert!(servers.is_empty()); // disabled 的服务器应该被过滤
}

#[test]
fn test_convert_context_servers_no_command() {
    let mut configs = HashMap::new();
    configs.insert(
        "no-command-server".to_string(),
        ContextServerConfig {
            source: "local".to_string(),
            enabled: true,
            command: None, // 没有命令
            args: None,
            env: None,
        },
    );

    let servers = convert_context_servers_sacp(&configs);
    assert!(servers.is_empty()); // 没有命令的服务器应该被过滤
}

#[test]
fn test_convert_context_servers_stdio() {
    let mut configs = HashMap::new();
    configs.insert(
        "test-mcp".to_string(),
        ContextServerConfig {
            source: "local".to_string(),
            enabled: true,
            command: Some("node".to_string()),
            args: Some(vec![
                "server.js".to_string(),
                "--port".to_string(),
                "3000".to_string(),
            ]),
            env: Some({
                let mut env = HashMap::new();
                env.insert("NODE_ENV".to_string(), "production".to_string());
                env
            }),
        },
    );

    let servers = convert_context_servers_sacp(&configs);
    assert_eq!(servers.len(), 1);

    // 验证是 Stdio 类型
    match &servers[0] {
        McpServer::Stdio(stdio) => {
            assert_eq!(stdio.name, "test-mcp");
        }
        _ => panic!("Expected Stdio variant"),
    }
}

#[test]
fn test_convert_context_servers_multiple() {
    let mut configs = HashMap::new();
    configs.insert(
        "server1".to_string(),
        ContextServerConfig {
            source: "local".to_string(),
            enabled: true,
            command: Some("node".to_string()),
            args: Some(vec!["server1.js".to_string()]),
            env: None,
        },
    );
    configs.insert(
        "server2".to_string(),
        ContextServerConfig {
            source: "local".to_string(),
            enabled: true,
            command: Some("python".to_string()),
            args: Some(vec!["server2.py".to_string()]),
            env: None,
        },
    );
    configs.insert(
        "disabled".to_string(),
        ContextServerConfig {
            source: "local".to_string(),
            enabled: false,
            command: Some("ruby".to_string()),
            args: None,
            env: None,
        },
    );

    let servers = convert_context_servers_sacp(&configs);
    // 应该只有 2 个 enabled 的服务器
    assert_eq!(servers.len(), 2);
}

#[test]
fn test_sacp_agent_launch_config_fields() {
    let config = SacpAgentLaunchConfig {
        command: "test-cmd".to_string(),
        args: vec!["arg1".to_string(), "arg2".to_string()],
        env: {
            let mut env = HashMap::new();
            env.insert("KEY".to_string(), "VALUE".to_string());
            env
        },
        context_servers: HashMap::new(),
    };

    assert_eq!(config.command, "test-cmd");
    assert_eq!(config.args.len(), 2);
    assert_eq!(config.env.get("KEY"), Some(&"VALUE".to_string()));
    assert!(config.context_servers.is_empty());
}

#[test]
fn test_sacp_agent_launch_config_debug() {
    let config = SacpAgentLaunchConfig {
        command: "test".to_string(),
        args: vec![],
        env: HashMap::new(),
        context_servers: HashMap::new(),
    };

    let debug_str = format!("{:?}", config);
    assert!(debug_str.contains("SacpAgentLaunchConfig"));
    assert!(debug_str.contains("test"));
}

// === mcp-proxy convert 诊断参数测试 ===

#[test]
fn test_is_mcp_proxy_command_simple() {
    // 简化版只检测精确的命令名
    assert!(is_mcp_proxy_command("mcp-proxy"));
    // 不再检测大小写变体和路径
    assert!(!is_mcp_proxy_command("MCP-PROXY"));
    assert!(!is_mcp_proxy_command("Mcp-Proxy"));
}

#[test]
fn test_is_mcp_proxy_command_not_mcp_proxy() {
    assert!(!is_mcp_proxy_command("node"));
    assert!(!is_mcp_proxy_command("bunx"));
    assert!(!is_mcp_proxy_command("/usr/bin/uvx"));
    assert!(!is_mcp_proxy_command("mcp-proxy-other"));
    // 路径形式不再匹配（简化版）
    assert!(!is_mcp_proxy_command("/usr/local/bin/mcp-proxy"));
    assert!(!is_mcp_proxy_command("C:\\Users\\test\\mcp-proxy.exe"));
}

#[test]
fn test_has_convert_subcommand() {
    assert!(has_convert_subcommand(&["convert".to_string()]));
    assert!(has_convert_subcommand(&[
        "convert".to_string(),
        "http://example.com".to_string()
    ]));
    assert!(has_convert_subcommand(&[
        "--config".to_string(),
        "config.json".to_string(),
        "convert".to_string()
    ]));
}

#[test]
fn test_has_convert_subcommand_no_convert() {
    assert!(!has_convert_subcommand(&[]));
    assert!(!has_convert_subcommand(&["serve".to_string()]));
    assert!(!has_convert_subcommand(&[
        "--config".to_string(),
        "config.json".to_string()
    ]));
}

#[test]
fn test_enhance_mcp_proxy_args_non_mcp_proxy() {
    // 非 mcp-proxy 命令，应该原样返回
    let args = vec!["arg1".to_string(), "arg2".to_string()];
    let result = enhance_mcp_proxy_args("node", args.clone());
    assert_eq!(result, args);
}

#[test]
fn test_enhance_mcp_proxy_args_no_convert() {
    // mcp-proxy 但没有 convert 子命令，应该原样返回
    let args = vec!["serve".to_string()];
    let result = enhance_mcp_proxy_args("mcp-proxy", args.clone());
    assert_eq!(result, args);
}

#[test]
fn test_enhance_mcp_proxy_args_already_has_diagnostic() {
    // 已有 --diagnostic 参数
    let args = vec![
        "convert".to_string(),
        "--diagnostic".to_string(),
        "--log-dir".to_string(),
        "/tmp/logs".to_string(),
    ];
    let result = enhance_mcp_proxy_args("mcp-proxy", args.clone());
    // 应该原样返回，不重复添加
    assert_eq!(result, args);
}

#[test]
fn test_get_mcp_proxy_log_dir_none_when_unset() {
    let _guard = ENV_TEST_LOCK.lock().unwrap();
    // 清除环境变量以测试返回 None
    // SAFETY: 测试环境中修改环境变量是安全的
    unsafe {
        std::env::remove_var(ENV_MCP_PROXY_LOG_DIR);
    }
    let log_dir = get_mcp_proxy_log_dir();
    assert_eq!(log_dir, None);
}

#[test]
fn test_get_mcp_proxy_log_dir_from_env() {
    let _guard = ENV_TEST_LOCK.lock().unwrap();
    let custom_dir = "/custom/mcp-proxy-logs";
    // SAFETY: 测试环境中修改环境变量是安全的
    unsafe {
        std::env::set_var(ENV_MCP_PROXY_LOG_DIR, custom_dir);
    }
    let log_dir = get_mcp_proxy_log_dir();
    assert_eq!(log_dir, Some(custom_dir.to_string()));
    // 清理环境变量
    // SAFETY: 测试环境中修改环境变量是安全的
    unsafe {
        std::env::remove_var(ENV_MCP_PROXY_LOG_DIR);
    }
}

#[test]
fn test_has_log_dir_arg() {
    // 检测 --log-dir 参数
    assert!(has_log_dir_arg(&[
        "--log-dir".to_string(),
        "/tmp".to_string()
    ]));
    assert!(has_log_dir_arg(&["--log-dir=/tmp".to_string()]));
    assert!(has_log_dir_arg(&[
        "convert".to_string(),
        "--log-dir".to_string()
    ]));

    // 检测 --log-file 参数
    assert!(has_log_dir_arg(&[
        "--log-file".to_string(),
        "/tmp/log.txt".to_string()
    ]));
    assert!(has_log_dir_arg(&["--log-file=/tmp/log.txt".to_string()]));
}

#[test]
fn test_has_log_dir_arg_no_log_args() {
    assert!(!has_log_dir_arg(&[]));
    assert!(!has_log_dir_arg(&["convert".to_string()]));
    assert!(!has_log_dir_arg(&["--diagnostic".to_string()]));
    assert!(!has_log_dir_arg(&[
        "--config".to_string(),
        "config.json".to_string()
    ]));
}

#[test]
fn test_enhance_args_respects_existing_log_dir() {
    let _guard = ENV_TEST_LOCK.lock().unwrap();
    // 模拟 debug 日志级别
    // SAFETY: 测试环境中修改环境变量是安全的
    unsafe {
        std::env::set_var("RUST_LOG", "debug");
        std::env::set_var(ENV_MCP_PROXY_LOG_DIR, "/env/path");
    }

    // 用户已配置 --log-dir，不应覆盖
    let args = vec![
        "convert".to_string(),
        "--log-dir".to_string(),
        "/custom/path".to_string(),
    ];
    let result = enhance_mcp_proxy_args("mcp-proxy", args);

    // 应该只追加 --diagnostic，不重复追加 --log-dir
    assert!(result.contains(&"--diagnostic".to_string()));
    // 只应有一个 --log-dir
    assert_eq!(result.iter().filter(|a| *a == "--log-dir").count(), 1);
    // --log-dir 的值应该是用户配置的 /custom/path
    let log_dir_idx = result.iter().position(|a| a == "--log-dir").unwrap();
    assert_eq!(
        result.get(log_dir_idx + 1),
        Some(&"/custom/path".to_string())
    );

    // 清理环境变量
    // SAFETY: 测试环境中修改环境变量是安全的
    unsafe {
        std::env::remove_var("RUST_LOG");
        std::env::remove_var(ENV_MCP_PROXY_LOG_DIR);
    }
}

#[test]
fn test_enhance_args_respects_existing_log_file() {
    let _guard = ENV_TEST_LOCK.lock().unwrap();
    // 模拟 debug 日志级别
    // SAFETY: 测试环境中修改环境变量是安全的
    unsafe {
        std::env::set_var("RUST_LOG", "debug");
        std::env::set_var(ENV_MCP_PROXY_LOG_DIR, "/env/path");
    }

    // 用户已配置 --log-file，不应追加 --log-dir
    let args = vec![
        "convert".to_string(),
        "--log-file=/custom/file.log".to_string(),
    ];
    let result = enhance_mcp_proxy_args("mcp-proxy", args);

    // 应该只追加 --diagnostic
    assert!(result.contains(&"--diagnostic".to_string()));
    // 不应有 --log-dir
    assert!(!result.iter().any(|a| a == "--log-dir"));

    // 清理环境变量
    // SAFETY: 测试环境中修改环境变量是安全的
    unsafe {
        std::env::remove_var("RUST_LOG");
        std::env::remove_var(ENV_MCP_PROXY_LOG_DIR);
    }
}

#[test]
fn test_enhance_args_adds_log_dir_when_env_set() {
    let _guard = ENV_TEST_LOCK.lock().unwrap();
    // 模拟 debug 日志级别和配置了 MCP_PROXY_LOG_DIR
    // SAFETY: 测试环境中修改环境变量是安全的
    unsafe {
        std::env::set_var("RUST_LOG", "debug");
        std::env::set_var(ENV_MCP_PROXY_LOG_DIR, "/var/log/mcp");
    }

    let args = vec!["convert".to_string()];
    let result = enhance_mcp_proxy_args("mcp-proxy", args);

    // 应该追加 --diagnostic 和 --log-dir
    assert!(result.contains(&"--diagnostic".to_string()));
    assert!(result.contains(&"--log-dir".to_string()));
    assert!(result.contains(&"/var/log/mcp".to_string()));

    // 清理环境变量
    // SAFETY: 测试环境中修改环境变量是安全的
    unsafe {
        std::env::remove_var("RUST_LOG");
        std::env::remove_var(ENV_MCP_PROXY_LOG_DIR);
    }
}

#[test]
fn test_enhance_args_no_log_dir_when_env_unset() {
    let _guard = ENV_TEST_LOCK.lock().unwrap();
    // 模拟 debug 日志级别但没有配置 MCP_PROXY_LOG_DIR
    // SAFETY: 测试环境中修改环境变量是安全的
    unsafe {
        std::env::set_var("RUST_LOG", "debug");
        std::env::remove_var(ENV_MCP_PROXY_LOG_DIR);
    }

    let args = vec!["convert".to_string()];
    let result = enhance_mcp_proxy_args("mcp-proxy", args);

    // 应该只追加 --diagnostic，不应有 --log-dir
    assert!(result.contains(&"--diagnostic".to_string()));
    assert!(!result.iter().any(|a| a == "--log-dir"));

    // 清理环境变量
    // SAFETY: 测试环境中修改环境变量是安全的
    unsafe {
        std::env::remove_var("RUST_LOG");
    }
}
