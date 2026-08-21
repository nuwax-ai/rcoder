//! PostgreSQL 标识符与字面量的校验 / 转义工具
//!
//! PG 标识符规则 (SQL 标准 + PG 扩展):
//! - 长度 1..=63 字节 (与 PostgreSQL `NAMEDATALEN - 1` 一致；超长时显式报错)
//! - 首字符: 字母 (a-zA-Z) 或下划线 `_`
//! - 后续字符: 字母 / 数字 / 下划线
//! - 大小写保留 (加双引号时不折叠为小写)
//!
//! 防御原则: 白名单校验为主 + SQL 转义 (replace) 为纵深防御。两者缺一会增加注入面。

use std::result::Result;

/// PG 标识符校验 — 白名单, 拒绝即报错
///
/// 规则: `[a-zA-Z_][a-zA-Z0-9_]*`, 长度 1..=63 字节。
/// `str::len()` 返回字节数；当前白名单只允许 ASCII，因此也等于字符数。
pub fn validate_pg_identifier(name: &str) -> Result<(), String> {
    if name.is_empty() || name.len() > 63 {
        return Err(format!(
            "PG identifier must be 1..=63 bytes, got {len}",
            len = name.len()
        ));
    }
    let mut chars = name.chars();
    let Some(first) = chars.next() else {
        return Err("PG identifier must not be empty".to_string());
    };
    if !(first.is_ascii_alphabetic() || first == '_') {
        return Err("PG identifier must start with letter or '_'".to_string());
    }
    if !chars.all(|c| c.is_ascii_alphanumeric() || c == '_') {
        return Err("PG identifier: only [a-zA-Z0-9_] allowed after the first char".to_string());
    }
    Ok(())
}

/// PG SQL 字符串字面量转义 — 标准 escape `'` → `''`
///
/// 注意: 仅转义单引号。完整防注入需配合 validate 白名单使用；
/// 不能防御 `\` 在 standard_conforming_strings=off 模式下的歧义, 但 PG 14+ 默认 on。
pub fn pg_escape_literal(value: &str) -> String {
    value.replace('\'', "''")
}

/// PG 标识符转义 — 标识符里 `"` → `""` (配合双引号引用)
pub fn pg_quote_ident(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

// ── 容器内 PG 凭据对齐命令构造（userApp dev/prod 双环境共用；经各自执行通道跑 sh -c） ──

/// sh 单引号安全包裹（`'` → `'\''`）——密码等自由文本进 shell 环境变量的标准转义。
pub fn pg_shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

/// 凭据验证命令（TCP scram 认证）：exit 0 = 传入密码与该账号当前密码一致。
///
/// 走 `-h 127.0.0.1` 强制 TCP（镜像 initdb `--auth-host=scram-sha-256`），
/// 不落 trust 通道；`username` 须先过 [`validate_pg_identifier`] 白名单。
pub fn pg_verify_credentials_cmd(username: &str, password: &str) -> String {
    format!(
        "PGPASSWORD={} psql -h 127.0.0.1 -U {} -d postgres -tAc 'SELECT 1'",
        pg_shell_quote(password),
        pg_quote_ident(username)
    )
}

/// 角色存在检查命令（本地 trust 免密，`$POSTGRES_USER` 为镜像 ENV）。
pub fn pg_role_exists_cmd(username: &str) -> String {
    format!(
        "psql -U \"$POSTGRES_USER\" -d postgres -tAc \"SELECT 1 FROM pg_roles WHERE rolname='{}'\"",
        pg_escape_literal(username)
    )
}

/// 密码重置命令（本地 trust 免密 ALTER USER；任意已存在账号）。
pub fn pg_alter_password_cmd(username: &str, password: &str) -> String {
    format!(
        "psql -U \"$POSTGRES_USER\" -d postgres -v ON_ERROR_STOP=1 -c \"ALTER USER {} WITH PASSWORD '{}'\"",
        pg_quote_ident(username),
        pg_escape_literal(password)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_ok() {
        assert!(validate_pg_identifier("my_db").is_ok());
        assert!(validate_pg_identifier("_hidden").is_ok());
        assert!(validate_pg_identifier("Db123").is_ok());
        assert!(validate_pg_identifier(&"a".repeat(63)).is_ok());
    }

    #[test]
    fn validate_rejects_empty() {
        assert!(validate_pg_identifier("").is_err());
    }

    #[test]
    fn validate_rejects_too_long() {
        assert!(validate_pg_identifier(&"a".repeat(64)).is_err());
    }

    #[test]
    fn validate_rejects_start_digit() {
        assert!(validate_pg_identifier("1db").is_err());
    }

    #[test]
    fn validate_rejects_dash() {
        assert!(validate_pg_identifier("my-db").is_err());
    }

    #[test]
    fn validate_rejects_space() {
        assert!(validate_pg_identifier("my db").is_err());
    }

    #[test]
    fn validate_rejects_injection() {
        assert!(validate_pg_identifier("foo'; DROP TABLE users;--").is_err());
        assert!(validate_pg_identifier("$(whoami)").is_err());
        assert!(validate_pg_identifier("`id`").is_err());
        assert!(validate_pg_identifier("\"").is_err());
    }

    #[test]
    fn escape_literal() {
        assert_eq!(pg_escape_literal("it's"), "it''s");
        assert_eq!(pg_escape_literal("no_quote"), "no_quote");
        assert_eq!(pg_escape_literal("' OR '1'='1"), "'' OR ''1''=''1");
    }

    #[test]
    fn quote_ident() {
        assert_eq!(pg_quote_ident("my db"), "\"my db\"");
        assert_eq!(pg_quote_ident("weird\"name"), "\"weird\"\"name\"");
    }

    #[test]
    fn shell_quote_escapes_single_quote() {
        assert_eq!(pg_shell_quote("plain"), "'plain'");
        assert_eq!(pg_shell_quote("it's"), r"'it'\''s'");
        assert_eq!(pg_shell_quote("a'; rm -rf /"), r"'a'\''; rm -rf /'");
    }

    #[test]
    fn verify_cmd_forces_tcp_scram() {
        let cmd = pg_verify_credentials_cmd("app", "s3cret");
        assert!(cmd.starts_with("PGPASSWORD='s3cret' psql -h 127.0.0.1"));
        assert!(cmd.contains(r#"-U "app""#));
    }

    #[test]
    fn alter_cmd_escapes_password_literal() {
        let cmd = pg_alter_password_cmd("app", "pa'ss");
        assert!(cmd.contains(r#"ALTER USER "app" WITH PASSWORD 'pa''ss'"#));
    }
}
