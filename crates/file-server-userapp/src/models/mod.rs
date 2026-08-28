//! wire 契约类型集中地（对齐 app_manager/models 范式，与 file-server 同构）。
//!
//! 规则：凡派生 `utoipa::ToSchema` 或 `utoipa::IntoParams` 的结构体/枚举
//! 一律定义在 `models/` 下，handlers 与 service 都从这里引用。类型名、
//! serde 属性、schema 名是 wire 契约，改动须同批核查 routes.rs 守卫测试。
//!
//! 分组：[`task`]（构建任务域共享：BuildTaskKind/Status/Snapshot）/
//! [`request`]（workspace/文件镜像/dev 生命周期域请求体 + Query + multipart
//! 占位）/ [`app_files`]（rcoder app-files 转发链内部契约请求）/
//! [`response`]（HttpResult.data 载荷）。跨 crate 共享类型
//! （KilledPid/BinaryFile/FileOp）从 `file_server::models` 引用（洋葱模型
//! 单向依赖，单一事实源在 file-server）。子模块经 glob 展平再导出，
//! 消费方一律 `crate::models::X`。

pub mod app_files;
pub mod request;
pub mod response;
pub mod task;

pub use app_files::*;
pub use request::*;
pub use response::*;
pub use task::*;

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use serde_json::Value;

    /// DTO 集中纪律守卫：`ToSchema` / `IntoParams` 派生只允许出现在 `models/`
    /// 目录（新增 DTO 散落回 handlers/service 会在此报红）。
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
        assert!(visited > 8, "sanity: 至少扫描 8 个源文件, 实际 {visited}");
    }

    /// 跨 crate 边界守卫：本 crate 不得引用 `file_server::handlers`——file-server
    /// 的 HTTP 边界层只对自己的路由开放；跨 crate 共享实现一律走
    /// `file_server::ops` / `service` / `models` / `error` / `extract`。
    /// 匹配串拼接构造避免守卫自身命中。
    #[test]
    fn never_import_file_server_handlers() {
        let marker = ["file_server::", "handlers"].concat();
        let src = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut offenders = Vec::new();
        visit(
            &src,
            &mut |file, content| {
                let relative = file.strip_prefix(&src).unwrap_or(file);
                for line in content.lines() {
                    let trimmed = line.trim_start();
                    if trimmed.starts_with("//") || trimmed.starts_with("//!") {
                        continue;
                    }
                    if line.contains(&marker) {
                        offenders.push(format!("{}: {}", relative.display(), line.trim()));
                    }
                }
            },
            &mut 0usize,
        );
        assert!(
            offenders.is_empty(),
            "跨 crate 禁止引用 {}（共享实现走 ops/service/models）：\n{}",
            marker,
            offenders.join("\n")
        );
    }

    /// wire 键风格守卫：本 crate 是 userApp 业务接口（未上线新契约），请求与
    /// 响应键一律 snake_case——`rename_all = "camelCase"` 与驼峰 JSON/multipart
    /// 字面量零容忍（camelCase 是 computer 域 TS 契约的形状，仅存在于
    /// file-server crate，不经本域）。匹配串拼接构造避免守卫自身命中。
    #[test]
    fn wire_keys_stay_snake_case() {
        /// camel 键清单：由本守卫锁定的历史违例词形（wire 键或 multipart 字段名）。
        const CAMEL_KEYS: &[(&str, &str)] = &[
            ("app", "Id"),
            ("user", "Id"),
            ("enable", "Git"),
            ("skill", "Urls"),
            ("agent", "Id"),
            ("file", "Path"),
            ("file", "Paths"),
            ("custom", "TargetDir"),
            ("file", "Size"),
            ("file", "Name"),
            ("exit", "Code"),
            ("total", "Lines"),
            ("start", "Index"),
            ("log", "FileName"),
            ("workspace", "Root"),
            ("updated", "Skills"),
            ("total", "Count"),
            ("success", "Count"),
            ("fail", "Count"),
            ("target", "Dir"),
            ("files", "Count"),
            ("file", "ProxyUrl"),
            ("max", "Visit"),
            ("timeout", "Ms"),
            ("tail", "Lines"),
            ("exclude", "Dirs"),
            ("rename", "From"),
        ];
        let src = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut offenders = Vec::new();
        visit(
            &src,
            &mut |file, content| {
                let relative = file.strip_prefix(&src).unwrap_or(file);
                for line in content.lines() {
                    let trimmed = line.trim_start();
                    if trimmed.starts_with("//") || trimmed.starts_with("//!") {
                        continue;
                    }
                    if line.contains("rename_all = \"camelCase\"") {
                        offenders.push(format!("{}: {}", relative.display(), line.trim()));
                    }
                    for (head, tail) in CAMEL_KEYS {
                        let key = ["\"", *head, *tail, "\""].concat();
                        if line.contains(&key) {
                            offenders.push(format!(
                                "{}: 驼峰键 {} （userApp 契约一律 snake）",
                                relative.display(),
                                line.trim()
                            ));
                        }
                    }
                }
            },
            &mut 0usize,
        );
        assert!(
            offenders.is_empty(),
            "userApp 域 wire 键一律 snake_case（camelCase 仅属 computer 域 TS 契约）：\n{}",
            offenders.join("\n")
        );
    }

    /// 共享实现消费边界守卫：对 `file_server::ops` 只允许消费 `*_core`（类型化
    /// 业务核心）与工具函数——返回 `Json<Value>` 的 `*_impl` 是 computer 域 TS
    /// 拼装，经本域即驼峰键泄漏。白名单：zip/download 两函数返回二进制流
    /// Response（无 wire 键，跨域中性）。
    #[test]
    fn ops_consumption_only_core_and_binary() {
        let src = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src");
        let binary_allow = ["zip_workspace", "download_all_files"];
        let mut offenders = Vec::new();
        visit(
            &src,
            &mut |file, content| {
                let relative = file.strip_prefix(&src).unwrap_or(file);
                for line in content.lines() {
                    let trimmed = line.trim_start();
                    if trimmed.starts_with("//") || trimmed.starts_with("//!") {
                        continue;
                    }
                    if !line.contains("file_server::ops") {
                        continue;
                    }
                    for token in line.split(|c: char| !c.is_ascii_alphanumeric() && c != '_') {
                        if token.ends_with("_impl")
                            && !binary_allow
                                .iter()
                                .any(|allow| token == [*allow, "_impl"].concat())
                        {
                            offenders.push(format!(
                                "{}: {}（消费 *_core；*_impl 是 computer 域 TS 拼装）",
                                relative.display(),
                                token
                            ));
                        }
                    }
                }
            },
            &mut 0usize,
        );
        assert!(
            offenders.is_empty(),
            "file_server::ops 消费边界：只许 *_core / 二进制白名单（zip/download）：\n{}",
            offenders.join("\n")
        );
    }

    /// wire 键风格代表用例：snake 接受 + camel 拒绝（对齐
    /// `create_workspace_body_is_snake_case` 先例；旧 camel wire 已废弃。
    /// camel 夹具经 concat 拼装避免守卫自命中）。
    #[test]
    fn request_bodies_are_snake_case() {
        use super::{UserappExecCommandBody, UserappGenerateFileBody};

        let exec = serde_json::json!({
            "app_id": "app-1", "user_id": "u1", "command": "true"
        });
        let body: UserappExecCommandBody = serde_json::from_value(exec).expect("snake accept");
        assert_eq!(body.app_id, "app-1");
        let legacy_text = [
            r#"{ "app"#,
            r#"Id": "app-1", "user"#,
            r#"Id": "u1", "command": "true" }"#,
        ]
        .concat();
        let legacy: Value = serde_json::from_str(&legacy_text).expect("legacy fixture parse");
        assert!(serde_json::from_value::<UserappExecCommandBody>(legacy).is_err());

        let payload = serde_json::json!({
            "app_id": "app-1", "user_id": "u1",
            "file_name": "a.txt", "content": "x"
        });
        let body: UserappGenerateFileBody = serde_json::from_value(payload).expect("snake accept");
        assert_eq!(body.file_name, "a.txt");
        let legacy_text = [
            r#"{ "app"#,
            r#"Id": "app-1", "user"#,
            r#"Id": "u1", "file"#,
            r#"Name": "a.txt", "content": "x" }"#,
        ]
        .concat();
        let legacy: Value = serde_json::from_str(&legacy_text).expect("legacy fixture parse");
        assert!(serde_json::from_value::<UserappGenerateFileBody>(legacy).is_err());
    }

    /// Query 参数 snake 代表：search-files 的复合词（max_visit/timeout_ms）
    /// 接受 snake、拒绝 camel。
    #[test]
    fn query_params_are_snake_case() {
        use super::UserappSearchFilesQuery;

        let q: UserappSearchFilesQuery = serde_urlencoded::from_str(
            "app_id=a&user_id=u&kw=x&limit=5&max_visit=9&timeout_ms=100",
        )
        .expect("snake query accept");
        assert_eq!(q.max_visit, "9");
        let legacy = [
            "app",
            "Id=a&user",
            "Id=u&kw=x&limit=5&max",
            "Visit=9&timeout",
            "Ms=100",
        ]
        .concat();
        assert!(
            serde_urlencoded::from_str::<UserappSearchFilesQuery>(&legacy).is_err(),
            "camel query 已废弃"
        );
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
