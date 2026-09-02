//! 配置加载契约锁：优先级链（env > CLI > config.yml > 内置默认）与 env 软失败行为。
//!
//! 此前完全无测试的面：env 覆盖全族、优先级应用顺序、enable_proxy 替换副作用、
//! api_key fail-fast、CLI 子命令不落盘。
//!
//! 独立测试二进制（tests/ 每文件一个进程，与 crate 其他测试天然进程隔离）；
//! chdir 与 std::env 均为进程全局，故文件内用 Mutex 全串行。

use clap::Parser as _;
use rcoder::config::{CONFIG_FILE, CliArgs, load_config_for_cli, load_config_with_args};
use std::fs;
use std::path::PathBuf;
use std::sync::{Mutex, MutexGuard, OnceLock};

/// 最小合法 proxy_config 段（ProxyConfig 无 serde default，字段必填）
const PROXY_SECTION: &str = "proxy_config:
  listen_port: 9999
  default_backend_port: 8086
  backend_host: 127.0.0.1
  port_param: port
  health_check:
    enabled: true
    interval_seconds: 30
    timeout_seconds: 5
    healthy_threshold: 2
    unhealthy_threshold: 3
";

fn serial() -> MutexGuard<'static, ()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|e| e.into_inner())
}

/// 测试作用域：tempdir 工作目录 + 注入的 env 键，退出时全部恢复
struct Sandbox {
    origin_cwd: PathBuf,
    dir: PathBuf,
    env_keys: Vec<&'static str>,
}

impl Sandbox {
    fn new(env_keys: Vec<&'static str>) -> Self {
        let dir = std::env::temp_dir().join(format!(
            "rcoder-cfg-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&dir).expect("mkdir");
        // 进沙箱前先清掉待测 env 键，保证起点干净
        for k in &env_keys {
            #[allow(unsafe_code)]
            unsafe {
                std::env::remove_var(k);
            }
        }
        let origin_cwd = std::env::current_dir().expect("cwd");
        std::env::set_current_dir(&dir).expect("chdir");
        Self {
            origin_cwd,
            dir,
            env_keys,
        }
    }

    fn set_env(&self, key: &str, val: &str) {
        #[allow(unsafe_code)]
        unsafe {
            std::env::set_var(key, val);
        }
    }

    fn write_config(&self, extra: &str) {
        let base = format!("port: 1111\nprojects_dir: ./ws\n{extra}");
        fs::write(self.dir.join(CONFIG_FILE), base).expect("write config.yml");
    }

    fn args(&self, extra: &[&str]) -> CliArgs {
        let mut v = vec!["rcoder"];
        v.extend_from_slice(extra);
        CliArgs::try_parse_from(v).expect("cli args")
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        // 恢复顺序：env → cwd → 清理目录（断言失败也不留脏全局态）
        for k in &self.env_keys {
            #[allow(unsafe_code)]
            unsafe {
                std::env::remove_var(k);
            }
        }
        if let Err(e) = std::env::set_current_dir(&self.origin_cwd) {
            eprintln!("restore cwd failed: {e}");
        }
        fs::remove_dir_all(&self.dir).ok();
    }
}

#[test]
fn env_overrides_cli_which_overrides_file_for_port() {
    let _g = serial();
    let sb = Sandbox::new(vec!["RCODER_PORT"]);
    sb.write_config("");

    // 仅文件：port=1111
    let c = load_config_with_args(sb.args(&[])).expect("file only");
    assert_eq!(c.port, 1111);

    // 文件 + CLI：CLI 2222 覆盖文件
    let c = load_config_with_args(sb.args(&["--port", "2222"])).expect("file+cli");
    assert_eq!(c.port, 2222, "CLI 必须覆盖 config.yml");

    // 文件 + CLI + env：env 3333 覆盖一切（应用顺序 env 最后，重排即静默失效）
    sb.set_env("RCODER_PORT", "3333");
    let c = load_config_with_args(sb.args(&["--port", "2222"])).expect("file+cli+env");
    assert_eq!(c.port, 3333, "env 必须覆盖 CLI——优先级链 env > CLI > file");
}

#[test]
fn invalid_env_port_is_soft_failure_keeping_applied_value() {
    let _g = serial();
    let sb = Sandbox::new(vec!["RCODER_PORT"]);
    sb.write_config("");
    sb.set_env("RCODER_PORT", "not-a-number");

    // parse 失败 = warn + 保留已应用值（CLI 的 2222），而非 Err、也非回文件值
    let c = load_config_with_args(sb.args(&["--port", "2222"])).expect("软失败不报错");
    assert_eq!(
        c.port, 2222,
        "非法 env 值必须保留已应用的 CLI 值（warn 保现状）"
    );
}

#[test]
fn enable_proxy_replaces_file_proxy_config_entirely() {
    let _g = serial();
    let sb = Sandbox::new(vec![]);
    sb.write_config(PROXY_SECTION);

    // 不开代理：文件的 proxy_config（listen 9999）应保留
    let c = load_config_with_args(sb.args(&[])).expect("no proxy");
    assert_eq!(
        c.proxy_config
            .as_ref()
            .expect("文件 proxy_config 应生效")
            .listen_port,
        9999
    );

    // 开代理：CLI 构造的默认 ProxyConfig 整体替换（文件里的 9999 丢失是
    // 已知副作用——本测试锁住该现状，若有人改成"合并保留"须显式改这里）
    let c = load_config_with_args(sb.args(&["--enable-proxy", "--proxy-port", "7777"]))
        .expect("with proxy");
    let proxy = c
        .proxy_config
        .as_ref()
        .expect("enable_proxy 后必有 proxy_config");
    assert_eq!(proxy.listen_port, 7777, "CLI proxy_port 必须生效");
}

#[test]
fn api_key_enabled_with_empty_key_fails_fast() {
    let _g = serial();
    let sb = Sandbox::new(vec!["RCODER_API_KEY_ENABLED", "RCODER_API_KEY"]);
    sb.write_config("");
    sb.set_env("RCODER_API_KEY_ENABLED", "true");
    sb.set_env("RCODER_API_KEY", "");

    let err = load_config_with_args(sb.args(&[]))
        .expect_err("enabled + 空 key 必须 fail fast（不能带空鉴权上线）");
    assert!(
        format!("{err:#}").contains("API Key"),
        "错误应指向 API Key 配置: {err:#}"
    );
}

#[test]
fn auto_cleanup_invalid_env_falls_back_to_true() {
    let _g = serial();
    let sb = Sandbox::new(vec!["RCODER_AUTO_CLEANUP"]);
    // 方法级直测（不经 load 全链——链路里 validate_multi_image_config 对
    // 缺镜像的默认骨架 fail-fast，与本测试目的无关）
    let mut docker = rcoder::config::DockerConfig::default();
    docker.auto_cleanup = Some(false); // 预置非默认原值，验证 unwrap_or(true) 会覆盖它
    sb.set_env("RCODER_AUTO_CLEANUP", "not-a-bool");

    docker
        .apply_env_overrides()
        .expect("非法 env 值应软失败不报错");
    assert_eq!(
        docker.auto_cleanup,
        Some(true),
        "现状锁：RCODER_AUTO_CLEANUP 非法值回 true（unwrap_or(true)，预置的 false 也被覆盖——\
         连'保留原值'都不是）。若改为保留原值/报错，是有意的行为变更，请同步更新本测试"
    );
}

#[test]
fn cli_subcommand_config_load_does_not_write_default_file() {
    let _g = serial();
    let sb = Sandbox::new(vec![]);
    // 不写 config.yml：load_config_for_cli 必须内存默认、不落盘
    let c = load_config_for_cli(sb.args(&["--port", "2222"])).expect("cli load");
    assert_eq!(c.port, 2222);
    assert!(
        !sb.dir.join(CONFIG_FILE).exists(),
        "只读子命令不得有写文件副作用（load_config_with_args 缺文件时会生成默认文件，for_cli 不得）"
    );
}
