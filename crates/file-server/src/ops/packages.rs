//! 依赖安装共享实现：install-project。
//!
//! 壳在 handlers/computer/packages.rs；子进程捕获经
//! [`super::process_capture`]。

use std::path::PathBuf;

use crate::extract::AppJson as Json;
use serde_json::{Value, json};

use crate::AppState;
use crate::error::AppError;
use crate::service::package_build;
use crate::service::pnpm::{self, InstallOptions};
use crate::service::pnpm_config;

use super::process_capture::run_capture;

/// install-project 的 workspace 无关实现 (typescript→pnpm / python→pip, 按语言找 manifest)。
pub async fn install_project_impl(
    state: &AppState,
    ws: PathBuf,
    programming_language: &str,
) -> Result<Json<Value>, AppError> {
    if !ws.exists() {
        return Err(AppError::resource("workspace does not exist"));
    }
    let lang = programming_language.to_ascii_lowercase();
    let skip = package_build::package_search_skip_dirs(&state.config.zip_workspace_exclude);
    let (program, args, project_dir): (&str, Vec<&str>, Option<PathBuf>) = match lang.as_str() {
        "typescript" | "ts" => {
            // projectDir = findPackageScript || findNodeProjectDir (对齐 nuwax)
            let dir = match package_build::find_package_script(&ws, &skip).await {
                Some(d) => Some(d),
                None => package_build::find_first(&ws, "package.json").await,
            };
            ("pnpm", Vec::new(), dir)
        }
        "python" | "py" => {
            // 优先 pyproject.toml (pip install -e .), 否则 requirements.txt
            let (dir, args) =
                if let Some(d) = package_build::find_first(&ws, "pyproject.toml").await {
                    (Some(d), vec!["install", "-e", "."])
                } else {
                    (
                        package_build::find_first(&ws, "requirements.txt").await,
                        vec!["install", "-r", "requirements.txt"],
                    )
                };
            ("pip", args, dir)
        }
        other => {
            return Err(AppError::validation(format!(
                "unsupported programmingLanguage: {other}"
            )));
        }
    };
    let project_dir = project_dir.ok_or_else(|| {
        AppError::business(
            "package manifest (package.json / pyproject.toml / requirements.txt) not found",
        )
    })?;
    let timeout = state.config.dev_command_timeout_secs;
    // pnpm install 前准备 .npmrc (package-import-method=copy + built-deps sanitize + 3 行),
    // best-effort (失败仅 warn, 不阻断 install; 对齐 nuwax ensurePnpmInstallConfig)
    if program == "pnpm" {
        pnpm_config::ensure_pnpm_install_config(&project_dir).await;
    }
    if program == "pnpm" {
        let options = InstallOptions {
            prefer_offline: true,
            extra_args: vec![
                "--config.production=false".to_string(),
                "--config.confirmModulesPurge=false".to_string(),
                "--config.dangerouslyAllowAllBuilds=true".to_string(),
            ],
        };
        pnpm::install(&project_dir, &options, None, timeout)
            .await
            .map_err(|error| {
                AppError::system(format!("Project dependencies install failed: {error}"))
            })?;
    } else {
        let (stdout, stderr, code) = run_capture(program, &args, &project_dir, timeout).await?;
        if code != 0 {
            let detail = if stderr.trim().is_empty() {
                stdout
            } else {
                stderr
            };
            return Err(AppError::system(format!(
                "Project dependencies install failed: {detail}"
            )));
        }
    }
    Ok(Json(json!({
        "success": true,
        "message": "Project dependencies installed successfully",
        "projectDir": project_dir.display().to_string(),
        "programmingLanguage": lang,
    })))
}
