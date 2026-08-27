//! wire 契约类型集中地（对齐 app_manager/models 范式）。
//!
//! 规则：凡派生 `utoipa::ToSchema` / `utoipa::IntoParams` 的结构体/枚举，
//! 以及被 HTTP 提取器反序列化的 wire 契约结构（无 utoipa 派生、参数在
//! path 注解里逐项声明的 GitLogQuery/CustomTargetQuery 形态），一律定义在
//! `models/` 下——models 是最底层共享层，handlers 与 service 都从这里引用。
//! 类型名、serde 属性、schema 名是 wire 契约，改动须同批核查守卫测试。
//!
//! 分组：[`commons`]（公共信封）/ [`code`]（跨域文件操作契约）/ [`forms`]
//! （OpenAPI-only multipart 占位）/ [`request`]（build+project 域请求）/
//! [`computer`]（computer 域请求）/ [`git`]（git 域请求）/ [`response`]
//! （响应载荷）。非 wire 契约的领域内部类型（TaskState、DevServerManager
//! 等）不在此处。子模块经 glob 展平再导出，消费方一律 `crate::models::X`。

pub mod code;
pub mod commons;
pub mod computer;
pub mod forms;
pub mod git;
pub mod request;
pub mod response;

pub use code::*;
pub use commons::*;
pub use computer::*;
pub use forms::*;
pub use git::*;
pub use request::*;
pub use response::*;

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    /// DTO 集中纪律守卫：`ToSchema` / `IntoParams` 派生只允许出现在 `models/`
    /// 目录（新增 DTO 散落回 handlers/service 会在此报红）。扫描按源码文本
    /// 匹配 token——models 内部的引用与导入不受影响。
    #[test]
    fn wire_contract_derives_live_only_in_models() {
        let src = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut offenders = Vec::new();
        let mut visited = 0usize;
        visit(
            &src,
            &mut |file, content| {
                let relative = file.strip_prefix(&src).unwrap_or(file);
                if relative.components().any(|c| c.as_os_str() == "models") {
                    return;
                }
                for line in content.lines() {
                    let trimmed = line.trim_start();
                    if trimmed.starts_with("//") || trimmed.starts_with("//!") {
                        continue;
                    }
                    if line.contains("ToSchema") || line.contains("IntoParams") {
                        offenders.push(format!("{}: {}", relative.display(), line.trim()));
                    }
                }
            },
            &mut visited,
        );
        assert!(
            offenders.is_empty(),
            "wire 契约派生（ToSchema/IntoParams）只允许定义在 src/models/ 下：\n{}",
            offenders.join("\n")
        );
        assert!(visited > 80, "sanity: 至少扫描 80 个源文件, 实际 {visited}");
    }

    fn visit(dir: &Path, f: &mut dyn FnMut(&Path, &str), visited: &mut usize) {
        let entries = match std::fs::read_dir(dir) {
            Ok(entries) => entries,
            Err(_) => return,
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                visit(&path, f, visited);
                continue;
            }
            if path.extension().is_some_and(|ext| ext == "rs")
                && let Ok(content) = std::fs::read_to_string(&path)
            {
                *visited += 1;
                f(&path, &content);
            }
        }
    }
}
