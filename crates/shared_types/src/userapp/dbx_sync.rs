//! dbx 预置连接（local-pg）同步命令构造——改密链（align/reset-password）共用。
//!
//! 机制：重写容器内 `$DBX_DATA_DIR/connections.json`（local-pg 新凭据）并重启
//! dbx program——fork dbx（nuwax-fork-dbx `connection_seed.rs`）启动导入为按 id
//! upsert，吸收新密码进 `connection_secrets`（运行期唯一有效来源）。
//! database 字段经 `"$POSTGRES_DB"` 展开（容器内受控 env，镜像/平台注入）。
//! 密码经 json 层转义（[`pg_json_escape`]）+ shell 层单引号包裹
//! （[`crate::pg_utils::pg_shell_quote`]）双保险，且绝不落日志与错误信息。

use crate::pg_utils::pg_shell_quote;

/// JSON 字符串值转义（最小集：`\\` `"` 及控制字符）——密码/标识符进
/// connections.json 的 json 层正确性保障（shell 层安全由 [`pg_shell_quote`] 承担）。
pub fn pg_json_escape(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for c in value.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out
}

/// local-pg 预置连接的 json 头段（数组开括号 `[` 到 `database` 值开引号为止）——
/// `username`/`password` 已转义嵌入，database 值留给 `"$POSTGRES_DB"` 展开。
fn dbx_local_pg_head(username_json: &str, password_json: &str) -> String {
    format!(
        r#"[{{"id":"local-pg","name":"Local PostgreSQL","db_type":"postgres","host":"127.0.0.1","port":5432,"username":"{username_json}","password":"{password_json}","database":""#
    )
}

const DBX_JSON_TAIL: &str = "\"}]";
const DBX_SUPERVISORCTL_RESTART: &str =
    "supervisorctl -c /etc/supervisor/supervisord.conf restart dbx";

/// dbx 预置连接同步命令（指定账号版，**条件内建**）：
/// 仅当指定账号 == 容器内 `$POSTGRES_USER`（local-pg 预置连接在用的账号）时
/// 重写 + 重启；重置业务账号不动 local-pg。条件不满足 exit 0（无事可做）。
///
/// `username` 须先过 [`validate_pg_identifier`] 白名单（调用方保证）。
/// 密码经 [`pg_json_escape`]（json 层）+ [`pg_shell_quote`]（shell 层）双保险。
pub fn dbx_sync_cmd_for_user(username: &str, password: &str) -> String {
    let head = dbx_local_pg_head(&pg_json_escape(username), &pg_json_escape(password));
    format!(
        "if [ {} = \"$POSTGRES_USER\" ]; then printf '%s%s%s' {} \"$POSTGRES_DB\" '{}' > \"$DBX_DATA_DIR/connections.json\" && {DBX_SUPERVISORCTL_RESTART}; fi",
        pg_shell_quote(username),
        pg_shell_quote(&head),
        DBX_JSON_TAIL,
    )
}

/// dbx 预置连接同步命令（superuser 版，无条件）：重置目标就是 `$POSTGRES_USER`
/// （CURRENT_USER 语义），恒同步；username 经 `"$POSTGRES_USER"` 展开。
pub fn dbx_sync_cmd_superuser(password: &str) -> String {
    let head = dbx_superuser_head();
    let mid = dbx_superuser_mid(password);
    format!(
        "printf '%s%s%s%s%s' {} \"$POSTGRES_USER\" {} \"$POSTGRES_DB\" '{}' > \"$DBX_DATA_DIR/connections.json\" && {DBX_SUPERVISORCTL_RESTART}",
        pg_shell_quote(head),
        pg_shell_quote(&mid),
        DBX_JSON_TAIL,
    )
}

/// superuser 版 json 头段（到 `username` 值开引号为止，username 留给
/// `"$POSTGRES_USER"` 展开）。
fn dbx_superuser_head() -> &'static str {
    r#"[{"id":"local-pg","name":"Local PostgreSQL","db_type":"postgres","host":"127.0.0.1","port":5432,"username":""#
}

/// superuser 版 json 中段（**闭 username 引号起**，password 转义嵌入，到
/// `database` 值开引号为止，database 留给 `"$POSTGRES_DB"` 展开）。
fn dbx_superuser_mid(password: &str) -> String {
    format!(
        r#"","password":"{}","database":""#,
        pg_json_escape(password)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_escape_minimal_set() {
        assert_eq!(pg_json_escape("plain"), "plain");
        assert_eq!(pg_json_escape(r#"a"b\c"#), r#"a\"b\\c"#);
        assert_eq!(pg_json_escape("l\nr\tc"), "l\\nr\\tc");
        assert_eq!(pg_json_escape("\u{1}"), "\\u0001");
        // 单引号在 json 内合法字符，不转义（shell 层由 pg_shell_quote 处理）
        assert_eq!(pg_json_escape("it's"), "it's");
    }
    #[test]
    fn dbx_sync_cmd_for_user_is_conditional_on_postgres_user() {
        let cmd = dbx_sync_cmd_for_user("app", "s3cret");
        assert!(
            cmd.starts_with("if [ 'app' = \"$POSTGRES_USER\" ]; then"),
            "指定账号须条件同步: {cmd}"
        );
        assert!(
            cmd.contains("> \"$DBX_DATA_DIR/connections.json\""),
            "写到 dbx 数据目录: {cmd}"
        );
        assert!(
            cmd.contains("supervisorctl -c /etc/supervisor/supervisord.conf restart dbx"),
            "重启 dbx program: {cmd}"
        );
        assert!(cmd.trim_end().ends_with("fi"));
        // 密码在 json 头段内且整体经 shell 单引号包裹（元字符失活）
        let head = dbx_local_pg_head("app", "s3cret");
        assert!(cmd.contains(&pg_shell_quote(&head)), "got: {cmd}");
        assert_dbx_json_parses(&head);
        // database 经 $POSTGRES_DB 展开（printf 多段拼接）
        assert!(cmd.contains("\"$POSTGRES_DB\""));
    }
    #[test]
    fn dbx_sync_cmd_for_user_escapes_password() {
        // 密码含 json 元字符 + shell 元字符：json 层转义 + shell 层单引号包裹
        let password: String = ['a', '$', '`', 'b', '"', 'c', '\\', 'd']
            .into_iter()
            .collect();
        let cmd = dbx_sync_cmd_for_user("app", &password);
        let head = dbx_local_pg_head("app", &pg_json_escape(&password));
        assert!(cmd.contains(&pg_shell_quote(&head)), "got: {cmd}");
        assert_dbx_json_parses(&head);
    }
    #[test]
    fn dbx_sync_cmd_superuser_expands_env() {
        let cmd = dbx_sync_cmd_superuser("pw1");
        assert!(cmd.starts_with("printf '%s%s%s%s%s'"), "got: {cmd}");
        assert!(cmd.contains("\"$POSTGRES_USER\""));
        assert!(cmd.contains("\"$POSTGRES_DB\""));
        // mid 段从生产构造函数取（防测试/生产字面量脱节——历史 bug：mid 丢
        // username 闭引号，测试手写字面量恰好"正确"没锁住）
        let mid = dbx_superuser_mid("pw1");
        assert!(cmd.contains(&pg_shell_quote(&mid)));
        // head+U+mid+D+tail 重组完整 json 并解析（生产各段同源拼装）
        let full = format!("{}U{}D{}", dbx_superuser_head(), mid, DBX_JSON_TAIL);
        let v: serde_json::Value = serde_json::from_str(&full)
            .unwrap_or_else(|e| panic!("superuser 拼装 json 不可解析: {e}\n{full}"));
        assert_eq!(v[0]["username"], "U");
        assert_eq!(v[0]["password"], "pw1");
        assert_eq!(v[0]["database"], "D");
        // superuser 版无条件（重置目标就是 $POSTGRES_USER）
        assert!(!cmd.contains("if ["));
    }
    /// 重组 printf 段为完整 json 并验证可解析（database 展开位以 D 占位）——
    /// 历史 bug：DBX_JSON_TAIL 丢 database 闭引号，dbx 侧解析失败空导入。
    fn assert_dbx_json_parses(head: &str) {
        let reassembled = format!(r#"{head}D{}"#, "\"}]");
        let v: serde_json::Value = serde_json::from_str(&reassembled)
            .unwrap_or_else(|e| panic!("拼装 json 不可解析: {e}\n{reassembled}"));
        assert_eq!(v[0]["id"], "local-pg");
        assert_eq!(v[0]["database"], "D");
    }
}
