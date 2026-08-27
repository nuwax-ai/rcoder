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
/// `username` 须先过 [`validate_pg_identifier`] 白名单（调用方保证）；
/// SQL 参数整体单引号包裹作纵深防御。
pub fn pg_role_exists_cmd(username: &str) -> String {
    let sql = format!(
        "SELECT 1 FROM pg_roles WHERE rolname='{}'",
        pg_escape_literal(username)
    );
    format!(
        "psql -U \"$POSTGRES_USER\" -d postgres -tAc {}",
        pg_shell_quote(&sql)
    )
}

/// 密码重置命令（本地 trust 免密 ALTER USER；任意已存在账号）。
///
/// `-c` 的 SQL 参数整体经 [`pg_shell_quote`] 单引号包裹——密码是自由文本，
/// 不能落 shell 双引号（`$`/反引号/`"` 在双引号内保持活性：注入 + 含特殊字符
/// 的密码先被 shell 改写、复验必失败的密码损坏双重问题）。SQL 串内的 `'` 已由
/// [`pg_escape_literal`] 转为 `''`，在 shell 单引号内安全。
pub fn pg_alter_password_cmd(username: &str, password: &str) -> String {
    let sql = format!(
        "ALTER USER {} WITH PASSWORD '{}'",
        pg_quote_ident(username),
        pg_escape_literal(password)
    );
    format!(
        "psql -U \"$POSTGRES_USER\" -d postgres -v ON_ERROR_STOP=1 -c {}",
        pg_shell_quote(&sql)
    )
}

/// 超户自身密码重置命令（本地 trust；重置目标 = 当前连接用户 = `$POSTGRES_USER`）。
///
/// 用 SQL 的 `CURRENT_USER` 取代把 `"$POSTGRES_USER"` 内嵌进命令行——后者依赖
/// shell 双引号开合的巧合展开（POSTGRES_USER 含空格还会分词），前者由 psql 会话
/// 身份直接解析，无变量展开依赖。
pub fn pg_alter_current_user_password_cmd(password: &str) -> String {
    let sql = format!(
        "ALTER USER CURRENT_USER WITH PASSWORD '{}'",
        pg_escape_literal(password)
    );
    format!(
        "psql -U \"$POSTGRES_USER\" -d postgres -v ON_ERROR_STOP=1 -c {}",
        pg_shell_quote(&sql)
    )
}

/// 建号命令（本地 trust `CREATE ROLE ... LOGIN`；userApp 账号 upsert 的"不存在"分支）。
///
/// `username` 须先过 [`validate_pg_identifier`] 白名单（调用方保证）；
/// 标识符经 [`pg_quote_ident`]、密码字面量经 [`pg_escape_literal`]、SQL 整体经
/// [`pg_shell_quote`] 单引号包裹——三层防线与 [`pg_alter_password_cmd`] 同款。
pub fn pg_create_role_cmd(username: &str, password: &str) -> String {
    let sql = format!(
        "CREATE ROLE {} LOGIN PASSWORD '{}'",
        pg_quote_ident(username),
        pg_escape_literal(password)
    );
    format!(
        "psql -U \"$POSTGRES_USER\" -d postgres -v ON_ERROR_STOP=1 -c {}",
        pg_shell_quote(&sql)
    )
}

/// 库存在性检查命令（本地 trust `-tAc`：命中输出 `1`、未命中输出空——
/// 比 CREATE 失败后解析 stderr 稳定，PG 不支持 CREATE DATABASE IF NOT EXISTS）。
/// `db` 须先过 [`validate_pg_identifier`] 白名单；SQL 参数整体单引号包裹作纵深防御。
pub fn pg_database_exists_cmd(db: &str) -> String {
    let sql = format!(
        "SELECT 1 FROM pg_database WHERE datname='{}'",
        pg_escape_literal(db)
    );
    format!(
        "psql -U \"$POSTGRES_USER\" -d postgres -tAc {}",
        pg_shell_quote(&sql)
    )
}

/// 建库命令（本地 trust `CREATE DATABASE`，可选 OWNER）。
/// `db`/`owner` 须先过 [`validate_pg_identifier`] 白名单（调用方保证）。
pub fn pg_create_database_cmd(db: &str, owner: Option<&str>) -> String {
    let owner_clause = owner
        .map(|o| format!(" OWNER {}", pg_quote_ident(o)))
        .unwrap_or_default();
    let sql = format!("CREATE DATABASE {}{owner_clause}", pg_quote_ident(db));
    format!(
        "psql -U \"$POSTGRES_USER\" -d postgres -v ON_ERROR_STOP=1 -c {}",
        pg_shell_quote(&sql)
    )
}

// ── dbx 预置连接（local-pg）同步命令（改密链 align/reset-password 共用） ──
//
// 机制:重写容器内 `$DBX_DATA_DIR/connections.json`（local-pg 新凭据）并重启
// dbx program——fork dbx（connection_seed.rs）启动导入为按 id upsert，吸收新
// 密码进 `connection_secrets`（运行期唯一有效来源）。database 字段经
// `"$POSTGRES_DB"` 展开（容器内受控 env，镜像/平台注入）。

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
    format!(r#"","password":"{}","database":""#, pg_json_escape(password))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 重组 printf 段为完整 json 并验证可解析（database 展开位以 D 占位）——
    /// 历史 bug：DBX_JSON_TAIL 丢 database 闭引号，dbx 侧解析失败空导入。
    fn assert_dbx_json_parses(head: &str) {
        let reassembled = format!(r#"{head}D{}"#, "\"}]");
        let v: serde_json::Value = serde_json::from_str(&reassembled)
            .unwrap_or_else(|e| panic!("拼装 json 不可解析: {e}\n{reassembled}"));
        assert_eq!(v[0]["id"], "local-pg");
        assert_eq!(v[0]["database"], "D");
    }

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
        // 双层转义正确性：PG 层 ' → ''（SQL 串内），再经 shell 层整体单引号包裹
        // （' → '\''）——期望串用同一构造器合成，避免手算两层叠加
        let sql = format!(
            "ALTER USER {} WITH PASSWORD '{}'",
            pg_quote_ident("app"),
            pg_escape_literal("pa'ss")
        );
        assert!(cmd.contains(&pg_shell_quote(&sql)), "got: {cmd}");
    }

    #[test]
    fn alter_cmd_shell_quotes_sql_argument() {
        // shell 注入面：密码含 $/`/" 时不得在命令行保持活性——SQL 参数须整体单引号包裹。
        // 断言用运行时构造的串，避开 raw 字符串与嵌套引号的定界歧义。
        let password: String = ['a', '$', '`', 'b', '"', 'c'].into_iter().collect();
        let cmd = pg_alter_password_cmd("app", &password);
        let expected_prefix = format!("-c 'ALTER USER {} WITH PASSWORD '", pg_quote_ident("app"));
        assert!(
            cmd.contains(&expected_prefix),
            "SQL 参数未单引号包裹: {cmd}"
        );
        let double_quoted_sql = "-c \"ALTER".to_string();
        assert!(
            !cmd.contains(&double_quoted_sql),
            "不应再有双引号包 SQL: {cmd}"
        );
    }

    #[test]
    fn alter_current_user_cmd_has_no_variable_expansion_dependency() {
        let cmd = pg_alter_current_user_password_cmd("pw");
        assert!(cmd.contains("ALTER USER CURRENT_USER"));
        assert!(cmd.contains(r#"-c 'ALTER"#));
    }

    #[test]
    fn create_role_cmd_shape_and_escaping() {
        let cmd = pg_create_role_cmd("biz_user", "pa'ss");
        // SQL 层：标识符双引号 + 密码 ' → ''；shell 层整体单引号包裹
        let sql = format!(
            "CREATE ROLE {} LOGIN PASSWORD '{}'",
            pg_quote_ident("biz_user"),
            pg_escape_literal("pa'ss")
        );
        assert_eq!(
            cmd,
            format!(
                "psql -U \"$POSTGRES_USER\" -d postgres -v ON_ERROR_STOP=1 -c {}",
                pg_shell_quote(&sql)
            )
        );
        // 注入面：密码里的 shell 元字符被整体关在单引号内
        assert!(cmd.contains(&pg_shell_quote(&sql)));
    }

    #[test]
    fn database_exists_cmd_uses_plain_output() {
        let cmd = pg_database_exists_cmd("mydb");
        // -tAc 纯输出（命中 1/未命中空），不靠 stderr 文本判定；
        // SQL 经 shell 层转义（' → '\''），期望串用同款构造器合成避免手算叠加
        let sql = format!(
            "SELECT 1 FROM pg_database WHERE datname='{}'",
            pg_escape_literal("mydb")
        );
        assert!(cmd.contains(" -tAc "));
        assert!(cmd.contains(&pg_shell_quote(&sql)));
    }

    #[test]
    fn create_database_cmd_owner_clause() {
        let bare = pg_create_database_cmd("mydb", None);
        assert!(bare.contains(r#"CREATE DATABASE "mydb""#));
        assert!(!bare.contains("OWNER"));
        let owned = pg_create_database_cmd("mydb", Some("biz_user"));
        assert!(owned.contains(r#"CREATE DATABASE "mydb" OWNER "biz_user""#));
    }

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
}
