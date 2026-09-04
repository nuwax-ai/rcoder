use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceManifest {
    pub schema_version: u32,
    pub workspace: WorkspaceMeta,
    #[serde(default)]
    pub pingap: PingapSection,
    /// workspace 级健康策略(默认空 = app-cli 自给自足,不强依赖任何后端)。
    #[serde(default)]
    pub health: WorkspaceHealthSection,
}

/// workspace 级健康配置。
///
/// `bridge_service`:显式指定用哪个 service 的 `[health].readiness_path` 代表整个 workspace
/// 的就绪状态。**不写(默认)** = app-cli 自身提供 `/ready`(后端 bug 不卡容器,用户可排查);
/// 写了 = app-cli 把 `/ready` 桥接到该后端,深检查(后端不 ready 摘流,但 liveness 仍 200 不杀容器)。
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceHealthSection {
    #[serde(default)]
    pub bridge_service: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceMeta {
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum PingapMode {
    #[default]
    Managed,
    Extend,
    Custom,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PingapSection {
    #[serde(default)]
    pub mode: PingapMode,
    #[serde(default)]
    pub config: Option<String>,
}

impl Default for PingapSection {
    fn default() -> Self {
        Self {
            mode: PingapMode::Managed,
            config: None,
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ProjectManifest {
    pub schema_version: u32,
    pub project: ProjectMeta,
    pub build: BuildSection,
    /// dev 阶段编译/准备命令（可选；仅源码态 dev 链路生效）。缺省三分派：未配
    /// `[devrun]` 的服务回落 [`Self::build`]（run.command 消费源码目录产物）；
    /// 配了 `[devrun]` 未配本段则跳过编译（devrun 自足，见 [`DevrunSection`]）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub devbuild: Option<DevbuildSection>,
    /// 运行段：进程态服务必填（`command` 非空由校验保证）；`type = "static"`
    /// 的服务省略本段（serde default 空段；校验层拒绝 static 配置启动命令）。
    #[serde(default)]
    pub run: RunSection,
    /// dev 阶段启动命令（可选，缺省回落 [`Self::run`]；配置即触发源码态 dev 链路）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub devrun: Option<DevrunSection>,
    #[serde(default)]
    pub health: HealthSection,
    #[serde(default)]
    pub proxy: Option<ProxySection>,
    #[serde(default)]
    pub logs: LogsSection,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ProjectMeta {
    pub service_id: String,
    pub name: String,
    pub r#type: ProjectType,
    #[serde(default)]
    pub kind: ProjectKind,
    #[serde(default = "default_true")]
    pub enabled: bool,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ProjectType {
    Node,
    Java,
    Python,
    Go,
    Rust,
    Static,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ProjectKind {
    #[default]
    Web,
    Worker,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct BuildSection {
    pub command: Vec<String>,
    pub artifact: String,
}

/// dev 阶段编译/准备命令（可选）。
///
/// 仅在源码态 dev 链路（任一服务配了 `[devrun]`）生效：`/dev/start`·`/dev/restart`
/// 对配置本段的服务执行本命令而非 `[build].command`——典型用法是轻量检查
/// （如 `pnpm run type-check`）或依赖准备（如 `npm install`），**不要求产出
/// artifact**。未配本段的缺省三分派：配了 `[devrun]` 的服务**跳过编译**
/// （devrun 命令自足跑源码，产物零消费者——依赖安装等前置准备即应放本段，
/// 否则首次 dev 启动会因依赖缺失失败）；未配 `[devrun]` 的服务回落
/// `[build].command`（刷新源码目录产物，run.command 依赖它）。产物态链路
/// （未配 `[devrun]` 的 app）编译恒用 `[build].command`（发布同核，zip 是
/// 部署物必需品，轻量命令不产 zip 不能替换）。
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct DevbuildSection {
    pub command: Vec<String>,
}

/// dev 阶段热加载启动命令（可选）。
///
/// 任一 enabled 服务配置本段即把该 app 的 dev 形态切为**源码态**：dev/start
/// ·restart 不再部署 `.run` 产物，app-cli 直接编排源码 workspace 并用本命令
/// 启动（典型：`vite`/`nodemon`/`spring-boot:run` 等热加载命令，跑源码改码即
/// 生效）；缺省回落 `[run].command`。端口注入（PORT env）、pingap 路由、健康
/// 检查、拓扑编排与产物态完全一致；生产（serve 形态）不读本段。**本段即声明
/// dev 态自足**：未配 `[devbuild]` 时该服务的 dev 编译被跳过（本命令不消费
/// 构建产物）；需要 type-check、依赖安装等前置，显式配 `[devbuild]`。
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct DevrunSection {
    pub command: Vec<String>,
}

/// 运行段。进程态服务 `command` 必填非空（校验保证）；`type = "static"` 服务
/// 省略整段（[`Default`]：空 command——由 app-cli 内置静态托管承载，无进程）。
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RunSection {
    /// 段内缺省容忍（校验层统一把关：进程态空 command 报错/static 配置报错，
    /// 比 serde missing field 的裸错更可操作）。
    #[serde(default)]
    pub command: Vec<String>,
    #[serde(default)]
    pub migrate: Vec<String>,
    #[serde(default)]
    pub depends_on: Vec<String>,
    #[serde(default = "default_shutdown_timeout")]
    pub shutdown_timeout_seconds: u64,
}

impl Default for RunSection {
    /// 手写而非 derive：`shutdown_timeout_seconds` 的 serde default 属性不参与
    /// `Default` trait（derive 会给 u64 零值，触发 >0 校验）——与反序列化缺省
    /// 值（30）保持一致。
    fn default() -> Self {
        Self {
            command: Vec::new(),
            migrate: Vec::new(),
            depends_on: Vec::new(),
            shutdown_timeout_seconds: default_shutdown_timeout(),
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct HealthSection {
    #[serde(default = "default_health_path")]
    pub startup_path: String,
    #[serde(default = "default_health_path")]
    pub readiness_path: String,
    #[serde(default = "default_health_path")]
    pub liveness_path: String,
    /// 启动就绪探测窗口（秒，dev/start·restart 逐服务并行探测 readiness_path
    /// 的上限；超时记 service_start_fail 不阻塞其余服务）。默认 25；慢启动
    /// 服务（如 Spring Boot 冷启动）按需调大。
    ///
    /// 序列化省略默认值（读侧 default 恢复）：release.lock 会被镜像内的
    /// app-cli `deny_unknown_fields` 解析——已发布的旧二进制不认识新字段，
    /// 默认值不写入才能保持前向兼容（显式非默认值仍写入，那些部署需新镜像）。
    #[serde(
        default = "default_startup_timeout",
        skip_serializing_if = "is_default_startup_timeout"
    )]
    pub startup_timeout_seconds: u64,
}

impl Default for HealthSection {
    fn default() -> Self {
        Self {
            startup_path: default_health_path(),
            readiness_path: default_health_path(),
            liveness_path: default_health_path(),
            startup_timeout_seconds: default_startup_timeout(),
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ProxySection {
    pub path: String,
    #[serde(default)]
    pub strip_prefix: bool,
    #[serde(default)]
    pub plugins: Vec<String>,
    #[serde(default)]
    pub upstream_includes: Vec<String>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct LogsSection {
    #[serde(default)]
    pub sources: Vec<LogSource>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct LogSource {
    pub id: String,
    pub glob: String,
    pub format: LogFormat,
    #[serde(default)]
    pub multiline_start_pattern: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum LogFormat {
    Jsonl,
    Text,
}

#[derive(Debug, Clone)]
pub struct DiscoveredProject {
    pub dir: String,
    pub manifest: ProjectManifest,
}

impl ProjectManifest {
    /// dev 编译命令三分派（按产物消费方判定；平台 dev 链路与 app-cli 本地 `build`
    /// 共用的单一事实源）：
    /// - 配了 `[devbuild]` → 用之（显式检查/准备意图：type-check、依赖安装等，
    ///   不要求产出 artifact）；
    /// - 未配 `[devbuild]` 但配了 `[devrun]` → `None`（**跳过编译**：devrun 命令
    ///   自足跑源码，构建产物零消费者；dev 命令确实要消费产物的非常规用法，
    ///   显式配 `[devbuild]` 强制构建）；
    /// - 都未配 → 回落 `[build].command`（run.command 在源码目录消费产物，需刷新）。
    pub fn devbuild_argv(&self) -> Option<&[String]> {
        if let Some(devbuild) = &self.devbuild {
            return Some(devbuild.command.as_slice());
        }
        if self.devrun.is_some() {
            return None;
        }
        Some(self.build.command.as_slice())
    }
}

impl DiscoveredProject {
    pub fn name(&self) -> &str {
        &self.manifest.project.name
    }

    pub fn service_id(&self) -> &str {
        &self.manifest.project.service_id
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ReleaseLock {
    pub schema_version: u32,
    pub release_id: String,
    pub workspace_name: String,
    pub pingap: LockedPingap,
    pub minimum_app_cli_version: String,
    /// Manifest v1 compatibility name. The value is the platform-versioned
    /// app-runtime image reference; it is not required to be an OCI digest.
    pub runtime_image_digest: String,
    pub services: Vec<LockedService>,
    /// workspace 级健康桥接策略(从 WorkspaceManifest 透传;None = app-cli 自给 /ready)。
    #[serde(default)]
    pub bridge_service: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct LockedPingap {
    pub mode: PingapMode,
    pub config: Option<String>,
    pub version: String,
    pub commit: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct LockedService {
    pub service_id: String,
    pub name: String,
    pub dir: String,
    pub r#type: ProjectType,
    pub kind: ProjectKind,
    pub enabled: bool,
    pub port: u16,
    /// dev 阶段编译命令（manifest 透传；编译执行在平台侧，app-cli 不消费，
    /// 携带以保持 lock 对 manifest 的完整投影与 gen-lock 预览可见）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub devbuild: Option<DevbuildSection>,
    pub run: RunSection,
    /// dev 阶段启动命令（manifest 透传；app-cli 仅在 `APP_CLI_RUN_PROFILE=dev`
    /// 的源码态编排下消费，生产 serve 形态不读）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub devrun: Option<DevrunSection>,
    pub health: HealthSection,
    pub proxy: Option<ProxySection>,
    pub logs: Vec<LogSource>,
    pub env: BTreeMap<String, String>,
    /// `type = static` 服务的托管内容目录（相对 `dir`，= manifest
    /// `[build].artifact` 目录语义）——app-cli 内置静态托管 serve
    /// `{dir}/{该目录}`。非 static 服务 None（不序列化，旧 lock 兼容）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub static_content_dir: Option<String>,
}

fn default_true() -> bool {
    true
}

fn default_shutdown_timeout() -> u64 {
    30
}

fn default_health_path() -> String {
    "/health".into()
}

fn default_startup_timeout() -> u64 {
    25
}

/// `startup_timeout_seconds` 序列化省略判定（= 默认值不写入，前向兼容旧
/// app-cli 二进制；见 [`HealthSection::startup_timeout_seconds`] 文档）。
fn is_default_startup_timeout(seconds: &u64) -> bool {
    *seconds == default_startup_timeout()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 最小合法 manifest（dev 段由各用例追加）。
    fn manifest(extra: &str) -> ProjectManifest {
        let toml_text = format!(
            "schema_version = 1\n\
             [project]\nservice_id = 'svc'\nname = 'svc'\ntype = 'node'\n\
             [build]\ncommand = ['cargo', 'build']\nartifact = 'artifact.zip'\n\
             [run]\ncommand = ['./server']\n{extra}"
        );
        toml::from_str(&toml_text).expect("parse manifest")
    }

    /// 三态之一：devrun + devbuild → 执行 devbuild（显式检查/准备意图优先）。
    #[test]
    fn devbuild_argv_prefers_explicit_devbuild() {
        let m = manifest(
            "[devbuild]\ncommand = ['pnpm', 'type-check']\n[devrun]\ncommand = ['vite']",
        );
        assert_eq!(
            m.devbuild_argv().map(Vec::from),
            Some(vec!["pnpm".to_string(), "type-check".to_string()])
        );
    }

    /// 三态之二：devrun 自足（未配 devbuild）→ 跳过编译。
    #[test]
    fn devrun_without_devbuild_skips_compile() {
        let m = manifest("[devrun]\ncommand = ['vite']");
        assert_eq!(
            m.devbuild_argv(),
            None,
            "devrun-only service must skip compile (devrun self-sufficient)"
        );
    }

    /// 三态之三：未配 devrun → 回落 [build].command（run.command 消费源码目录产物）。
    #[test]
    fn no_devrun_falls_back_to_build() {
        let m = manifest("");
        assert_eq!(
            m.devbuild_argv().map(Vec::from),
            Some(vec!["cargo".to_string(), "build".to_string()])
        );
    }

    /// 仅配 [devbuild] 未配 [devrun]（少见但合法）：显式准备意图仍生效。
    #[test]
    fn devbuild_without_devrun_still_wins() {
        let m = manifest("[devbuild]\ncommand = ['pnpm', 'install']");
        assert_eq!(
            m.devbuild_argv().map(Vec::from),
            Some(vec!["pnpm".to_string(), "install".to_string()])
        );
    }
}
