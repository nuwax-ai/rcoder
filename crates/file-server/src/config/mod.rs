//! 配置模块 (对齐 nuwax `appConfig/index.js`)。
//!
//! 按职责拆分:
//! - [`mod@self`] (本文件): [`Config`] 结构定义、默认值与常量;
//! - [`env`][]: 环境变量解析工具与限额校验;
//! - `load`: 配置文件/环境变量加载入口与 [`Config::validate`]。
//!
//! 仅含业务路径与开关; 工作区根 (project/computer) 由 [`crate::workspace::WorkspaceResolver`] 负责。

mod env;
mod load;

use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use shared_types::AGENT_FILE_SERVER_PORT;
use shared_types::paths::{COMPUTER_WORKSPACE_ROOT, USERAPP_WORKSPACE_ROOT, WORKSPACE_ROOT};

pub use self::env::MAX_UPLOAD_FILE_SIZE_BYTES;
use self::env::{default_attachment_extensions, split_default};

/// 部署模式 (区分 K8s 容器部署 vs 宿主机/Electron 部署)。
///
/// file-server 自身保持 kube-free：K8s 重逻辑 (per-agent PVC / CephFS subvolume 解析) 全经
/// `WorkspaceResolver` trait 由 rcoder 注入。本枚举只给 file-server 自身一个运行时区分手段
/// (如启动日志、未来 K8s 独有的轻量行为分支)。
///
/// `Default` 随编译期 `kubernetes` Cargo feature 联动 (由 rcoder 转发开启)：
/// 嵌入 rcoder (k8s 构建) → [`DeploymentMode::Kubernetes`]；独立/Electron 构建 →
/// [`DeploymentMode::DockerCompose`]。运行时可用 `DEPLOYMENT_MODE` env 覆盖 (Fail Fast：未知值报错)。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum DeploymentMode {
    /// 宿主机 / Electron 客户端 (无容器编排、无 PV/PVC)。
    #[serde(alias = "host")]
    Standalone,
    /// docker-compose 容器 (有容器、无 k8s) — TS 原版现状默认。
    #[serde(alias = "docker_compose", alias = "compose")]
    DockerCompose,
    /// K8s + PVC (随 rcoder 集成)。
    #[serde(alias = "k8s")]
    Kubernetes,
}

impl Default for DeploymentMode {
    fn default() -> Self {
        // cfg! 编译期求值：与 rcoder 的 kubernetes feature 对齐 (本 marker 由 rcoder 转发开启)。
        if cfg!(feature = "kubernetes") {
            Self::Kubernetes
        } else {
            Self::DockerCompose
        }
    }
}

impl DeploymentMode {
    pub fn is_kubernetes(&self) -> bool {
        matches!(self, Self::Kubernetes)
    }

    pub fn is_standalone(&self) -> bool {
        matches!(self, Self::Standalone)
    }
}

impl std::str::FromStr for DeploymentMode {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim().to_ascii_lowercase().as_str() {
            "standalone" => Ok(Self::Standalone),
            // 兼容 docker_compose / compose 写法
            "docker-compose" | "docker_compose" | "compose" => Ok(Self::DockerCompose),
            "kubernetes" | "k8s" => Ok(Self::Kubernetes),
            other => Err(format!(
                "unknown DEPLOYMENT_MODE '{other}'; expected one of: standalone, docker-compose, kubernetes"
            )),
        }
    }
}

// ── 默认值常量 (Default impl 与 load.rs env 回退共享, 单一事实源) ────────────────

/// 遍历时排除的目录 (对齐 TS env.development TRAVERSE_EXCLUDE_DIRS)。
pub(crate) const DEFAULT_TRAVERSE_EXCLUDE_DIRS: &str = "dist,node_modules,.pnpm-store,__MACOSX,.attachments,.git,.agents,.codex,.opencode,.grok,.pi,.logs";
/// 备份时排除的文件。
pub(crate) const DEFAULT_BACKUP_TRAVERSE_EXCLUDE_FILES: &str =
    "pnpm-lock.yaml,yarn.lock,package-lock.json";
/// 内容遍历时排除的文件 (对齐 TS env.development CONTENT_TRAVERSE_EXCLUDE_FILES)。
pub(crate) const DEFAULT_CONTENT_TRAVERSE_EXCLUDE_FILES: &str =
    "AGENT.md,AGENTS.md,CLAUDE.md,pnpm-lock.yaml,yarn.lock,package-lock.json";
/// 内联图片扩展名。
pub(crate) const DEFAULT_INLINE_IMAGE_EXTENSIONS: &str =
    ".png,.jpg,.jpeg,.gif,.bmp,.svg,.ico,.webp,.avif";
/// zip-workspace 排除列表（含 dev 部署运行目录 .run/.previous/.staging——平台
/// 生成的解压产物与轮换现场，不进下载包）。
pub(crate) const DEFAULT_ZIP_WORKSPACE_EXCLUDE: &str = ".git,.tmp,.claude,.agents,.codex,.opencode,.grok,.pi,.logs,.npmrc,__pycache__,node_modules,dist,pnpm-lock.yaml,yarn.lock,package-lock.json,.run,.previous,.staging";

/// 全局配置 (启动时构造一次, 经 AppState 共享)。
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    // —— 服务运行参数 ——
    pub listen_host: String,
    pub port: u16,
    pub project_source_dir: PathBuf,
    pub computer_workspace_dir: PathBuf,
    /// UserApp 开发卷根 (env `USERAPP_WORKSPACE_DIR`; 沙箱挂载点, 见 paths::USERAPP_WORKSPACE_ROOT)。
    pub userapp_workspace_dir: PathBuf,
    /// 单 app 模式归属 (env `USERAPP_SINGLE_APP_ID`)。
    ///
    /// Some 时本 file-server 只服务该 app——**生产运行容器形态**：卷根即 app 根
    /// (`resolve_userapp_dev` 不 join app_id、拒绝不匹配 appId 与 customTargetDir)。
    /// None = 开发容器形态（多 app 共享开发卷，现状）。
    pub userapp_single_app_id: Option<String>,
    pub service_log_dir: PathBuf,
    pub service_log_retention_days: usize,
    /// workspace 构建产物（`builds/workspace-package-*.zip`）与构建 temp 日志
    /// （`dev-temp-*.log`）保留的最近构建数（按文件名字典序=uuid v7 时间序）。
    pub build_artifact_retain_count: usize,
    /// workspace 构建与 dev server 的 main 日志（`dev-YYYY-MM-DD.log`）保留天数。
    pub build_log_retention_days: usize,

    // —— 业务目录 ——
    pub init_project_dir: PathBuf,
    pub upload_project_dir: PathBuf,
    pub dist_target_dir: PathBuf,
    pub log_base_dir: PathBuf,

    // —— 文件大小上限 ——
    pub max_inline_file_size_bytes: u64,
    pub upload_max_file_size_bytes: u64,
    /// 单个 skill URL 响应上限；远小于通用项目上传上限，避免累计磁盘压力。
    pub skill_download_max_bytes: u64,
    pub skill_download_connect_timeout_secs: u64,
    pub skill_download_timeout_secs: u64,
    pub skill_download_max_redirects: usize,
    pub skill_url_max_count: usize,
    pub skill_url_allow_http: bool,
    pub skill_url_allow_private_networks: bool,
    pub skill_url_allowed_hosts: Vec<String>,
    pub download_max_file_size_bytes: u64,
    /// Axum 全局请求体上限；默认且硬上限为 1 GiB。
    pub request_body_max_bytes: u64,

    // —— 日志缓存 (对齐 nuwax logCacheManager) ——
    pub log_cache_enabled: bool,
    pub log_cache_duration_ms: u64,
    pub log_cache_max_entries: usize,
    pub log_cache_max_file_size_bytes: u64,
    /// 单次读取日志响应允许载入内存的最大字节数。
    pub log_read_max_bytes: u64,

    // —— 扩展名白名单 ——
    pub upload_allowed_extensions: Vec<String>,
    pub attachment_allowed_extensions: Vec<String>,

    // —— 遍历排除规则 ——
    pub traverse_exclude_dirs: Vec<String>,
    pub backup_traverse_exclude_files: Vec<String>,
    pub content_traverse_exclude_files: Vec<String>,
    pub inline_image_extensions: Vec<String>,
    pub zip_workspace_exclude: Vec<String>,

    // —— Git ——
    pub git_enabled: bool,
    pub git_default_author_name: String,
    pub git_default_author_email: String,
    pub git_diff_max_file_size_bytes: u64,
    pub git_diff_max_total_bytes: u64,
    pub git_diff_max_output_bytes: u64,
    pub git_file_content_max_bytes: u64,

    // —— 模板 ——
    pub init_project_name_react: String,
    pub init_project_name_vue3: String,

    // —— Dev server 进程管理 (对齐 nuwax processManager) ——
    /// 部署模式 (区分 K8s vs 宿主机/Electron)；见 [`DeploymentMode`]。
    pub deployment_mode: DeploymentMode,
    pub fast_restart_enabled: bool,
    pub computer_log_dir: PathBuf,
    pub bash_path: String,
    /// 端口池范围 [start, end] (对齐 nuwax portPool 硬编码 4000-55000)。
    pub dev_port_range_start: u16,
    pub dev_port_range_end: u16,
    /// 端口池保留区 (跳过, 对齐 nuwax 8000-9000)。
    pub dev_port_reserved_start: u16,
    pub dev_port_reserved_end: u16,
    /// 存活探活单次超时 ms (对齐 nuwax 1500)。
    pub dev_alive_check_timeout_ms: u64,
    /// 存活轮询上限 ms (对齐 nuwax 30000; 超时仍返回成功)。
    pub dev_alive_max_wait_ms: u64,
    /// 存活轮询间隔 ms (默认 300：vite 冷启动 ~300-500ms 即 ready，原 1s 粒度会白等；
    /// 测试可设极小值如 10 以消除调度噪声、并使就绪探测定时断言确定化)。
    pub dev_alive_poll_interval_ms: u64,
    /// stop 后等进程退出轮询间隔 ms (对齐 nuwax 100)。
    pub dev_stop_check_interval_ms: u64,
    /// stop 后最大轮询次数 (对齐 nuwax 50, 合计 5s)。
    pub dev_stop_max_attempts: u32,
    /// build/install 命令超时秒 (对齐 nuwax 10min)。
    pub dev_command_timeout_secs: u64,
    /// build 全局并发上限 (对齐 nuwax MAX_BUILD_CONCURRENCY, 默认 20)。
    pub max_build_concurrency: usize,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            listen_host: "0.0.0.0".to_string(),
            port: AGENT_FILE_SERVER_PORT,
            project_source_dir: PathBuf::from(WORKSPACE_ROOT),
            computer_workspace_dir: PathBuf::from(COMPUTER_WORKSPACE_ROOT),
            userapp_workspace_dir: PathBuf::from(USERAPP_WORKSPACE_ROOT),
            userapp_single_app_id: None,
            service_log_dir: PathBuf::from("/app/logs/file-server"),
            service_log_retention_days: 7,
            build_artifact_retain_count: 10,
            build_log_retention_days: 7,
            init_project_dir: PathBuf::from("/app/project_init"),
            upload_project_dir: PathBuf::from("/app/project_zips"),
            dist_target_dir: PathBuf::from("/app/project_nginx"),
            log_base_dir: PathBuf::from("/app/logs/project_logs"),
            max_inline_file_size_bytes: 1_048_576,
            upload_max_file_size_bytes: MAX_UPLOAD_FILE_SIZE_BYTES,
            skill_download_max_bytes: 104_857_600,
            skill_download_connect_timeout_secs: 10,
            skill_download_timeout_secs: 60,
            skill_download_max_redirects: 3,
            skill_url_max_count: 20,
            skill_url_allow_http: true,
            // 私有化部署默认放行私网/保留地址: skill 通常托管在内网 (10.x/192.168.x/容器域名)。
            // 公网/SaaS 部署可用 SKILL_URL_ALLOW_PRIVATE_NETWORKS=false 重新锁紧。
            skill_url_allow_private_networks: true,
            skill_url_allowed_hosts: Vec::new(),
            download_max_file_size_bytes: 104_857_600,
            request_body_max_bytes: MAX_UPLOAD_FILE_SIZE_BYTES,
            log_cache_enabled: true,
            log_cache_duration_ms: 180_000,
            log_cache_max_entries: 100,
            log_cache_max_file_size_bytes: 2_097_152,
            log_read_max_bytes: 64 * 1024 * 1024,
            upload_allowed_extensions: vec![".zip".to_string()],
            attachment_allowed_extensions: default_attachment_extensions(),
            traverse_exclude_dirs: split_default(DEFAULT_TRAVERSE_EXCLUDE_DIRS),
            backup_traverse_exclude_files: split_default(DEFAULT_BACKUP_TRAVERSE_EXCLUDE_FILES),
            content_traverse_exclude_files: split_default(DEFAULT_CONTENT_TRAVERSE_EXCLUDE_FILES),
            inline_image_extensions: split_default(DEFAULT_INLINE_IMAGE_EXTENSIONS),
            zip_workspace_exclude: split_default(DEFAULT_ZIP_WORKSPACE_EXCLUDE),
            git_enabled: false,
            git_default_author_name: "Nuwax File Server".to_string(),
            git_default_author_email: "git@nuwax.com".to_string(),
            git_diff_max_file_size_bytes: 16 * 1024 * 1024,
            git_diff_max_total_bytes: 64 * 1024 * 1024,
            git_diff_max_output_bytes: 64 * 1024 * 1024,
            git_file_content_max_bytes: 64 * 1024 * 1024,
            init_project_name_react: "react-vite-template".to_string(),
            init_project_name_vue3: "vue3-vite-template".to_string(),
            deployment_mode: DeploymentMode::default(),
            fast_restart_enabled: false,
            computer_log_dir: PathBuf::from("/app/logs/computer_logs"),
            bash_path: String::new(),
            dev_port_range_start: 4000,
            dev_port_range_end: 55_000,
            dev_port_reserved_start: 8000,
            dev_port_reserved_end: 9000,
            dev_alive_check_timeout_ms: 1500,
            dev_alive_max_wait_ms: 30_000,
            dev_alive_poll_interval_ms: 300,
            dev_stop_check_interval_ms: 100,
            dev_stop_max_attempts: 50,
            dev_command_timeout_secs: 600,
            max_build_concurrency: 20,
        }
    }
}

impl Config {
    /// 扩展名是否在白名单 (大小写不敏感)。
    pub fn ext_allowed(&self, list: &[String], ext: &str) -> bool {
        list.iter()
            .any(|candidate| candidate.eq_ignore_ascii_case(ext))
    }
}
