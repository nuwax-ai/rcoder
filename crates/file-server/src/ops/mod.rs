//! 共享操作层（extractor 之后、service 之前的「workspace 无关实现」）。
//!
//! 分层契约：壳（handlers）负责 utoipa 注解、参数提取与 garde 校验；本层
//! 收已解析的参数（路径/TemporaryFile/简单参数结构）执行操作并构造响应。
//! file-server 自己的 handler 壳与 file-server-userapp（跨 crate）都从这层
//! 消费——跨 crate 引用 `crate::handlers` 被编译器禁止（handlers 为
//! pub(crate)），共享实现一律在本层或 service/models。
//!
//! 文件与 handlers/computer 域 1:1 镜像；multipart/static_share 为 HTTP 边界
//! 工具。impl 签名收 `&AppState`（实际依赖 config 与 skill_downloader）。
//! 引用风格统一走子模块路径（`ops::files::xxx_impl`），不做平铺 re-export。

pub mod archive;
pub mod exec;
pub mod files;
pub mod files_read;
pub mod multipart;
pub mod packages;
pub mod process_capture;
pub mod static_share;
pub mod workspace;

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
