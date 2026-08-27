//! 共享操作层（extractor 之后、service 之前的「workspace 无关实现」）。
//!
//! 这层的函数从各域 handler 壳抽出：壳负责 utoipa 注解、参数提取与 garde
//! 校验，本层收已解析的参数（路径/TemporaryFile/简单参数结构）执行操作并
//! 构造响应。file-server 自己的 handler 壳与 file-server-userapp（跨 crate）
//! 都从这里消费——**跨 crate 禁止引用 `crate::handlers`**，共享实现一律下沉
//! 到本层或 service/models（handlers→handlers 是被拆除的畸形形态）。
//!
//! 文件按 handlers/computer 域 1:1 镜像；multipart/static_share 为 HTTP 边界
//! 工具。impl 签名自 handler 抽出时保持不变（`&AppState` 依赖 config 与
//! skill_downloader）。

pub mod archive;
pub mod exec;
pub mod files;
pub mod files_read;
pub mod multipart;
pub mod packages;
pub mod process_capture;
pub mod static_share;
pub mod workspace;

pub use archive::{download_all_files_impl, zip_workspace_impl};
pub use exec::{execute_command_impl, get_logs_impl};
pub use files::{
    files_update_impl, generate_file_impl, import_project_impl, upload_file_impl, upload_files_impl,
};
pub use files_read::{
    FileListParams, SearchFilesParams, get_file_list_impl, resolve_file_impl, search_files_impl,
};
pub use multipart::{file_field, text_field, validate_zip_ext};
pub use packages::install_project_impl;
pub use process_capture::{CaptureResult, capture_command, run_capture};
pub use static_share::{
    COMPUTER_CORS, CorsConfig, add_cors_headers, cors_404, origin_value, serve_from_root,
};
pub use workspace::{init_project_template_impl, push_skills_impl};

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    /// 层纯度守卫：ops/ 内不得引用 `crate::handlers`（共享实现回流入 handlers
    /// 会重建畸形依赖；handler 语义的东西属于壳层）。按源码文本扫描；匹配串
    /// 拼接构造避免守卫自身命中。
    #[test]
    fn ops_never_reference_handlers() {
        let marker = ["crate::", "handlers"].concat();
        let ops = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src/ops");
        let mut offenders = Vec::new();
        let mut visited = 0usize;
        visit(&ops, &mut |file, content| {
            visited += 1;
            for line in content.lines() {
                let trimmed = line.trim_start();
                if trimmed.starts_with("//") || trimmed.starts_with("//!") {
                    continue;
                }
                if line.contains(&marker) {
                    offenders.push(format!("{}: {}", file.display(), line.trim()));
                }
            }
        });
        assert!(
            offenders.is_empty(),
            "ops/ 共享层不得引用 {}（实现回流即重建畸形依赖）：\n{}",
            marker,
            offenders.join("\n")
        );
        assert!(
            visited >= 10,
            "sanity: 至少扫描 10 个 ops 文件, 实际 {visited}"
        );
    }

    fn visit(dir: &Path, f: &mut dyn FnMut(&Path, &str)) {
        let entries = match std::fs::read_dir(dir) {
            Ok(entries) => entries,
            Err(_) => return,
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                visit(&path, f);
            } else if path.extension().is_some_and(|ext| ext == "rs")
                && let Ok(content) = std::fs::read_to_string(&path)
            {
                f(&path, &content);
            }
        }
    }
}
