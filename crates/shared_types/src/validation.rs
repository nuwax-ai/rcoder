//! Validation utilities for converting garde errors to AppError

use crate::AppError;
use garde::Report;

/// 将 Garde Report 转换为 AppError
///
/// # Example
/// ```ignore
/// request.validate().map_err(garde_err_to_app_error)?;
/// ```
pub fn garde_err_to_app_error(report: Report) -> AppError {
    let errors: Vec<String> = report
        .iter()
        .map(|(path, err)| format!("{}: {}", path, err.message()))
        .collect();
    let message = errors.join("; ");
    AppError::validation_error(&message)
}

/// Userapp `app_id` 长度上限（Fail Fast 校验用）。
///
/// 推导链（K8s 部署形态）：StatefulSet controller 自动给每个 pod 打
/// `apps.kubernetes.io/controller-revision-hash` label，其值 = ControllerRevision
/// 名 = `{sts 名}-{10 位 hash}`。K8s label 值上限 63 字节：
///
/// ```text
/// rcoder-app-builder-(19) + app_id + -hash(11) ≤ 63  →  app_id ≤ 33
/// ```
///
/// identifier 白名单（`IDENTIFIER_RE`）允许 64 字符，但超 33 的 app_id 在
/// K8s 下创建 builder STS 必然 `FailedCreate: invalid metadata.labels`，
/// 表象是含糊的 ensure 500/连接超时（真因只在 kubectl events）——入口
/// Fail Fast 把它变成明确的 400。Docker 模式理论上限更宽，统一 33 取最紧
/// 约束（与 user_id ≤23 的既有限制同类：K8s 资源名约束传导到业务标识）。
pub const USERAPP_APP_ID_MAX_LEN: usize = 33;

/// 校验路径标识符（project_id, agent_work_dir 等）
///
/// # 规则
/// - 仅允许 `[a-zA-Z0-9_-]`
/// - 长度 1-64 字符
/// - 不允许 `.`（防止路径穿越）
/// - 不允许 `/`（防止路径注入）
///
/// # 错误
/// 返回描述校验失败原因的字符串
pub fn validate_identifier(value: &str, field_name: &str) -> Result<(), String> {
    if value.is_empty() {
        return Err(format!("{} 不能为空", field_name));
    }
    if value.len() > 64 {
        return Err(format!("{} 长度超过 64 字符: {}", field_name, value.len()));
    }
    if !value
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    {
        return Err(format!(
            "{} 包含非法字符: '{}'，仅允许字母、数字、下划线和连字符",
            field_name, value
        ));
    }
    Ok(())
}

/// 标识符白名单正则（garde 内置 pattern 规则用；DTO 字段
/// `#[garde(pattern(shared_types::IDENTIFIER_RE))]` 声明式标记）。
///
/// 语义与 [`validate_identifier`] 一致：字母数字下划线连字符、1-64 字符、
/// 防路径穿越/注入。`\A...\z` 严格锚定（`^...$` 在 Rust regex 默认允许尾部
/// 换行，白名单外的 `\n` 会漏过）。
pub static IDENTIFIER_RE: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
    regex::Regex::new(r"\A[a-zA-Z0-9_-]{1,64}\z").expect("identifier whitelist regex")
});

// ── 多平台绝对路径规范化（自 file-server workspace.rs 提取，单一事实源）──────
//
// 首个消费方为 file-server 的项目绑定目录 workspaceDir（TS nuwax-file-server
// 同构契约）与 agent_work_dir 绝对路径形态（/computer/chat 常规项目场景）；
// app-cli / file-server-proxy 等多平台 crate 后续可复用。
//
// ⚠️ TS 待同步锚点：verbatim（\\?\）de-verbatim 处理是 Rust 侧先行修复——
// TS nuwax-file-server src/utils/computer/workspaceContext.js 的 canonicalizeDir
// （~:113）同款盲区（\\?\ 被折叠成 //?/ 产出 Windows 非法路径形态），需按
// strip_verbatim 同规则补齐（\\?\C:\x → C:\x、\\?\UNC\s\a → \\s\a、非法
// verbatim / \\.\ 设备路径显式拒绝），保持两上游灰度切流行为同构。

/// 绑定目录/工作目录绝对路径长度上限（对齐 TS `normalizeWorkspaceDir` 的 512）。
pub const ABSOLUTE_DIR_MAX_LEN: usize = 512;

/// agent_work_dir 绝对路径段数上限（规范化后非空段计数；UNC 前导空段不计）。
///
/// 常规项目场景 `/home/user/{projectType}/{projectId}` = 4 段，留一倍余量。
pub const AGENT_WORK_DIR_MAX_SEGMENTS: usize = 8;

/// 判断字符串是否为绝对路径形态（跨平台字符串规则，非宿主语义）：
/// POSIX `/a/b`、Windows 盘符 `X:/a/b`（含反斜杠 `X:\a\b` 原始形态）、
/// UNC `//server/share`，以及 Windows verbatim 扩展长度路径
/// `\\?\C:\a\b` / `\\?\UNC\server\share`。
///
/// 判定前做与 [`normalize_absolute_dir`] 同款的 trim + verbatim 剥离 +
/// 分隔符归一，保证 `is_absolute_path_like(x)` 与 normalize 的绝对判定一致。
/// 非法 verbatim（`\\?\foo` 无盘符非 UNC）与设备命名空间（`\\.\`）返回
/// false——不是合法绝对路径形态。
pub fn is_absolute_path_like(value: &str) -> bool {
    let trimmed = value.trim();
    let effective = match strip_verbatim(trimmed) {
        Ok(Some(inner)) => inner,
        Ok(None) => std::borrow::Cow::Borrowed(trimmed),
        Err(_) => return false,
    };
    let collapsed = canonicalize_dir(&effective);
    collapsed.starts_with('/') || is_drive_form(&collapsed)
}

/// 校验并归一化绝对路径目录（对齐 TS `normalizeWorkspaceDir` + `canonicalizeDir`，
/// nuwax-file-server f979df7；规则自 file-server workspace.rs 原样提取）：
///
/// - trim 后长度 ≤ 512；无控制字符（`\x00`-`\x1f`、`\x7f`）
/// - Windows verbatim 扩展长度路径 de-verbatim：`\\?\C:\x` → `C:/x`、
///   `\\?\UNC\s\a` → `//s/a`（产出可用路径——verbatim 形态只认反斜杠且按
///   字面处理，归一化输出若保留 `//?/` 前缀在 Windows 上是非法路径）；
///   非法 verbatim（内层无盘符非 UNC）与设备命名空间 `\\.\` 显式拒绝
/// - 分隔符归一：`\`→`/`、连续 `/` 折叠、UNC 前导 `//` 保留、盘符首字母大写
/// - 必须绝对路径（`/` 开头或 `X:/` 盘符形态）
/// - 不得含 `.` / `..` 段（fail-fast 拒绝，不静默归一）
///
/// 绝对路径判断是**跨平台字符串规则**（任意宿主平台上 POSIX 与 Windows 两种
/// 形态都接受，对齐 TS 平台无关校验）。
///
/// # 为什么不用 `std::path::Component`
///
/// `components()` 是宿主平台语义：Linux/mac 上 `C:/x` 被解析为两个普通段
/// （非绝对路径，会误拒）、`//server/share` 的 UNC 前导被折叠丢失；且它把
/// `.` 段**静默规范化掉**（std 文档明示 CurDir 被 normalize away），与"含点段
/// 须拒绝"的 fail-fast 语义冲突。TS/Rust 两侧行为须同构（TS 校验刻意平台
/// 无关），故此处为纯字符串实现。
///
/// 已知非目标：正斜杠 `//?/C:/x` 不识别为 verbatim（非合法 Win32 verbatim
/// 形态、现实调用方不产生），与非 verbatim 输入同路径处理。
///
/// 返回规范化后的路径（调用方以返回值为准，勿继续用原始输入）。
pub fn normalize_absolute_dir(raw: &str, field_name: &str) -> Result<String, String> {
    let dir = raw.trim();
    // JS `.length` 计 UTF-16 单元，此处取 Unicode 标量数——补充字符边缘有差异
    // （emoji JS 计 2、此处计 1），不影响 ASCII 路径。长度按剥 verbatim 前缀前
    // 的原文计（与 TS 入口先查长度一致）。
    if dir.chars().count() > ABSOLUTE_DIR_MAX_LEN {
        return Err(format!("{field_name} length exceeds 512"));
    }
    if dir.chars().any(|c| {
        let code = c as u32;
        code <= 0x1f || code == 0x7f
    }) {
        return Err(format!("{field_name} contains illegal characters"));
    }
    // verbatim 剥离在 canonicalize 之前——先剥 `\\?\` 再归一分隔符，
    // 否则 `\\?\` 被折叠成 `//?/` 产出 Windows 非法形态
    let effective = match strip_verbatim(dir) {
        Ok(Some(inner)) => inner,
        Ok(None) => std::borrow::Cow::Borrowed(dir),
        Err(e) => return Err(format!("{field_name} {e}")),
    };
    let dir = canonicalize_dir(&effective);
    if !(dir.starts_with('/') || is_drive_form(&dir)) {
        return Err(format!(
            "{field_name} must be an absolute path (POSIX /a/b, Windows C:/a/b or UNC //server/share/a/b)"
        ));
    }
    if dir.split('/').any(|seg| seg == "." || seg == "..") {
        return Err(format!("{field_name} must not contain dot segments"));
    }
    Ok(dir)
}

/// 盘符形态：`X:/`（首字符 ASCII 字母 + `:` + `/`），对齐 TS 正则 `^[A-Za-z]:\/.*`。
fn is_drive_form(dir: &str) -> bool {
    let bytes = dir.as_bytes();
    bytes.len() >= 3 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':' && bytes[2] == b'/'
}

/// 分隔符统一为 `/` 并折叠连续分隔符（保留 UNC 的前导 `//`）；Windows 盘符统一
/// 大写（大小写不敏感），不做宿主机语义解析——镜像 TS `canonicalizeDir`。
fn canonicalize_dir(dir: &str) -> String {
    let unc = dir.starts_with("//") || dir.starts_with("\\\\");
    let mut collapsed = String::with_capacity(dir.len());
    let mut in_sep = false;
    for ch in dir.chars() {
        let normalized = if ch == '\\' { '/' } else { ch };
        if normalized == '/' {
            if !in_sep {
                collapsed.push('/');
            }
            in_sep = true;
        } else {
            collapsed.push(normalized);
            in_sep = false;
        }
    }
    if unc {
        collapsed.insert(0, '/');
    }
    // 盘符首字母大写（^[a-z]:/ → ^[A-Z]:/；首字节已验证 ASCII，切片边界安全）
    let bytes = collapsed.as_bytes();
    if bytes.len() >= 3 && bytes[0].is_ascii_lowercase() && bytes[1] == b':' && bytes[2] == b'/' {
        collapsed[0..1].make_ascii_uppercase();
    }
    collapsed
}

/// 识别并剥离 Windows verbatim 扩展长度路径前缀（`\\?\`），作用于 **trim 后、
/// canonicalize 前**的字符串——顺序不可换：先归一分隔符会把 `\\?\` 折叠成
/// `//?/`，产出 Windows 上非法的路径形态（verbatim 只认反斜杠且按字面处理）。
///
/// 返回：
/// - `Ok(None)`：非 verbatim 输入，调用方走原路径
/// - `Ok(Some(inner))`：合法 verbatim，inner 为剥前缀后的内层（UNC 形态已补回
///   双反斜杠）——盘符分支借用输入零拷贝，UNC 分支重建字符串
/// - `Err(msg)`：以 `\\?\` 开头但内层无盘符非 UNC（含裸前缀、双重前缀），或
///   设备命名空间 `\\.\`（现状靠点段检查碰巧拒绝，此处显式化）
///
/// 前缀只认反斜杠形态；正斜杠 `//?/` 非合法 Win32 verbatim（现实调用方不
/// 产生），不识别。`\\?\UNC` 匹配含尾分隔符的 9 字符且 `UNC` 大小写不敏感
/// （防 `\\?\UNCserver\share` 误判为 UNC）。
fn strip_verbatim(trimmed: &str) -> Result<Option<std::borrow::Cow<'_, str>>, String> {
    if let Some(rest) = trimmed.strip_prefix(r"\\?\") {
        // \\?\UNC\server\share → \\server\share：
        // strip_prefix 已剥 \\?\（4 字符），此处再剥 UNC\（4 字符）、前补 \\，
        // 等价于剥 9 字符 \\?\UNC\——无三反斜杠中间态，后续 canonicalize_dir
        // 对 \\ 前缀走 UNC 保留分支。
        // UNC 匹配用**字节切片**比较：str 切片 rest[..3] 在多字节 UTF-8 序列上
        // 会 panic（char boundary violation，如 \\?\é中），畸形输入不得炸校验层
        let rest_bytes = rest.as_bytes();
        if rest_bytes.len() >= 4
            && rest_bytes[..3].eq_ignore_ascii_case(b"UNC")
            && rest_bytes[3] == b'\\'
        {
            // 前三字节已证为 ASCII（UNC），&rest[4..] 字符边界安全
            let after_unc = &rest[4..];
            return Ok(Some(std::borrow::Cow::Owned(format!(r"\\{after_unc}"))));
        }
        // 盘符内层：X:\... 或 x:/...（字母 + :，大小写不限；有无根由后续
        // 绝对形态判定兜底——如 \\?\C: 剥后 C: 非 X:/ 形态被拒）
        if rest_bytes.len() >= 2 && rest_bytes[0].is_ascii_alphabetic() && rest_bytes[1] == b':' {
            return Ok(Some(std::borrow::Cow::Borrowed(rest)));
        }
        return Err(
            "verbatim prefix \\\\?\\ must be followed by a drive letter or UNC".to_string(),
        );
    }
    if trimmed.starts_with(r"\\.\") {
        return Err("device namespace paths (\\\\.\\) are not supported".to_string());
    }
    Ok(None)
}

/// 校验 `agent_work_dir`（/computer/chat 等对话接口的工作目录入参），两种形态：
///
/// - **单段目录名**（非绝对形态）：走现行 [`validate_identifier`] 语义——
///   替代 project_id 参与工作目录路径拼接的最后一段（历史行为不变）
/// - **绝对路径**：走 [`normalize_absolute_dir`] 多平台校验（POSIX/盘符/UNC、
///   拒 `.`/`..` 段、拒控制字符、限长），并限段数 1..=[`AGENT_WORK_DIR_MAX_SEGMENTS`]
///   （裸根 `/`、`//` 段数为 0，拒绝）。常规项目场景 Java 传子容器内绝对路径
///   `/home/user/{projectType}/{projectId}`
///
/// 绝对形态不逐段限制 identifier 字符集——Windows 盘符 `:`、含空格/中文的
/// 目录段是合法路径成分；防穿越由拒点段 + 分隔符归一覆盖。
pub fn validate_agent_work_dir(value: &str) -> Result<(), String> {
    if !is_absolute_path_like(value) {
        return validate_identifier(value, "agent_work_dir");
    }
    let normalized = normalize_absolute_dir(value, "agent_work_dir")?;
    let segments = normalized.split('/').filter(|seg| !seg.is_empty()).count();
    if segments == 0 || segments > AGENT_WORK_DIR_MAX_SEGMENTS {
        return Err(format!(
            "agent_work_dir 路径段数须在 1-{AGENT_WORK_DIR_MAX_SEGMENTS} 之间: {value}"
        ));
    }
    Ok(())
}

/// 按 service_type 校验 `agent_work_dir`：绝对路径形态仅 ComputerAgentRunner
/// 支持（Web 链路 work_dir 会流入容器挂载配置、UserappBuilder 定位键恒为
/// app_id，均不可混入绝对路径——fail-fast 显式拒绝而非静默忽略）。
pub fn validate_agent_work_dir_for_service(
    service_type: &crate::ServiceType,
    value: &str,
) -> Result<(), String> {
    match service_type {
        crate::ServiceType::ComputerAgentRunner => validate_agent_work_dir(value),
        _ => {
            if is_absolute_path_like(value) {
                Err("absolute agent_work_dir is only supported for ComputerAgentRunner".to_string())
            } else {
                validate_identifier(value, "agent_work_dir")
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identifier_regex_matches_validate_identifier_semantics() {
        // 同语义：合法集
        for ok in ["user123", "my-project_01", "A-B_C", "a", "12345"] {
            assert!(IDENTIFIER_RE.is_match(ok), "{ok}");
            assert!(validate_identifier(ok, "f").is_ok());
        }
        // 拒绝集：穿越/注入/超长/尾部换行（$ 锚定漏洞回归锚）
        let too_long = "x".repeat(65);
        for bad in ["../etc", "foo/bar", "", "a b", "abc\n", too_long.as_str()] {
            assert!(!IDENTIFIER_RE.is_match(bad), "{bad:?}");
        }
    }

    #[test]
    fn test_validate_identifier_accepts_valid() {
        assert!(validate_identifier("user123", "user_id").is_ok());
        assert!(validate_identifier("my-project_01", "project_id").is_ok());
        assert!(validate_identifier("a", "id").is_ok());
        assert!(validate_identifier("A-B_C", "id").is_ok());
        assert!(validate_identifier("12345", "id").is_ok());
    }

    #[test]
    fn test_validate_identifier_rejects_traversal() {
        assert!(validate_identifier("../etc", "user_id").is_err());
        assert!(validate_identifier("..\\etc", "user_id").is_err());
        assert!(validate_identifier("foo/bar", "user_id").is_err());
        assert!(validate_identifier("..", "user_id").is_err());
        assert!(validate_identifier(".", "user_id").is_err());
    }

    #[test]
    fn test_validate_identifier_rejects_special_chars() {
        assert!(validate_identifier("user id", "user_id").is_err());
        assert!(validate_identifier("user;rm", "user_id").is_err());
        assert!(validate_identifier("user$id", "user_id").is_err());
        assert!(validate_identifier("user@id", "user_id").is_err());
    }

    #[test]
    fn test_validate_identifier_rejects_empty() {
        assert!(validate_identifier("", "user_id").is_err());
    }

    #[test]
    fn test_validate_identifier_rejects_too_long() {
        let long_id = "a".repeat(65);
        assert!(validate_identifier(&long_id, "user_id").is_err());
        let ok_id = "a".repeat(64);
        assert!(validate_identifier(&ok_id, "user_id").is_ok());
    }

    // ── normalize_absolute_dir / is_absolute_path_like（与 file-server 既有
    // 测试同款矩阵，验证搬家零行为变化）──────────────────────────────────

    #[test]
    fn normalize_absolute_dir_accepts_posix_absolute() {
        assert_eq!(normalize_absolute_dir("/a/b", "f").expect("posix"), "/a/b");
        assert_eq!(
            normalize_absolute_dir("  /a/b  ", "f").expect("trim"),
            "/a/b"
        );
        assert_eq!(
            normalize_absolute_dir("//a//b", "f").expect("unc lead"),
            "//a/b"
        );
        assert_eq!(normalize_absolute_dir("/", "f").expect("bare root"), "/");
        assert_eq!(normalize_absolute_dir("//", "f").expect("bare unc"), "//");
    }

    #[test]
    fn normalize_absolute_dir_normalizes_windows_forms() {
        assert_eq!(
            normalize_absolute_dir("c:/x", "f").expect("drive upper"),
            "C:/x"
        );
        assert_eq!(
            normalize_absolute_dir("C:\\a\\b", "f").expect("backslash"),
            "C:/a/b"
        );
        assert_eq!(
            normalize_absolute_dir("\\\\srv\\share\\a", "f").expect("unc backslash"),
            "//srv/share/a"
        );
        assert_eq!(
            normalize_absolute_dir("//srv/share/a", "f").expect("unc slash"),
            "//srv/share/a"
        );
        assert_eq!(
            normalize_absolute_dir("C:/", "f").expect("minimal drive"),
            "C:/"
        );
    }

    #[test]
    fn normalize_absolute_dir_rejects_invalid() {
        // 相对路径 / 相对盘符 / 点段 / 控制字符 / 超长
        assert!(normalize_absolute_dir("a/b", "f").is_err());
        assert!(normalize_absolute_dir("./a", "f").is_err());
        assert!(normalize_absolute_dir("C:", "f").is_err());
        assert!(normalize_absolute_dir("/a/../b", "f").is_err());
        assert!(normalize_absolute_dir("/a/./b", "f").is_err());
        assert!(normalize_absolute_dir("/a\x01b", "f").is_err());
        let long = format!("/{}", "a".repeat(ABSOLUTE_DIR_MAX_LEN));
        assert!(normalize_absolute_dir(&long, "f").is_err());
    }

    #[test]
    fn is_absolute_path_like_matches_normalize_semantics() {
        for ok in [
            "/home/user/web/p1",
            "/a",
            "/",
            "C:/Users/dev",
            "c:/x",
            "C:\\a\\b",
            "\\\\srv\\share\\a",
            "//srv/share/a",
            "  /a/b  ",
        ] {
            assert!(is_absolute_path_like(ok), "{ok:?}");
        }
        for bad in ["a/b", "abc", "web/p1", "C:", "a\\b", "", "  "] {
            assert!(!is_absolute_path_like(bad), "{bad:?}");
        }
    }

    // ── Windows verbatim（\\?\）de-verbatim ──────────────────────────────

    #[test]
    fn normalize_absolute_dir_de_verbatims_drive_form() {
        assert_eq!(
            normalize_absolute_dir(r"\\?\C:\Users\dev\proj", "f").expect("verbatim drive"),
            "C:/Users/dev/proj"
        );
        assert_eq!(
            normalize_absolute_dir(r"\\?\c:/x", "f").expect("lowercase drive"),
            "C:/x"
        );
        // 盘根 / 尾随分隔（与非 verbatim 输入同款行为）
        assert_eq!(
            normalize_absolute_dir(r"\\?\C:\", "f").expect("drive root"),
            "C:/"
        );
        assert_eq!(
            normalize_absolute_dir(r"\\?\C:\x\", "f").expect("trailing sep"),
            "C:/x/"
        );
    }

    #[test]
    fn normalize_absolute_dir_de_verbatims_unc_form() {
        assert_eq!(
            normalize_absolute_dir(r"\\?\UNC\server\share\p", "f").expect("verbatim unc"),
            "//server/share/p"
        );
        // 混合分隔 + 小写 unc 关键字（大小写不敏感）
        assert_eq!(
            normalize_absolute_dir(r"\\?\unc\server/share\p", "f").expect("mixed sep unc"),
            "//server/share/p"
        );
        // 缺 share 不收紧（与非 verbatim 输入 //server 的既有放行一致，TS 同款）
        assert_eq!(
            normalize_absolute_dir(r"\\?\UNC\server", "f").expect("unc no share"),
            "//server"
        );
    }

    #[test]
    fn normalize_absolute_dir_rejects_invalid_verbatim() {
        // 内层无盘符非 UNC / 双重前缀 / 裸前缀 / 设备命名空间 / 点段
        assert!(normalize_absolute_dir(r"\\?\foo\bar", "f").is_err());
        assert!(normalize_absolute_dir(r"\\?\\?\C:\x", "f").is_err());
        assert!(normalize_absolute_dir(r"\\?\", "f").is_err());
        assert!(normalize_absolute_dir(r"\\?\UNCserver\share", "f").is_err()); // UNC 后无分隔符
        assert!(normalize_absolute_dir(r"\\.\PhysicalDrive0", "f").is_err());
        assert!(normalize_absolute_dir(r"\\?\C:\a\..\b", "f").is_err());
        // \\?\C:（无根）剥后非 X:/ 形态，由绝对判定拒绝
        assert!(normalize_absolute_dir(r"\\?\C:", "f").is_err());
    }

    /// panic 回归锚：`\\?\` 后跟多字节 UTF-8 序列时，UNC 匹配的字节切片比较
    /// 不得触发 char boundary panic（str 切片 rest[..3] 在 é中/🦀 等序列上会炸）
    #[test]
    fn verbatim_with_multibyte_inner_does_not_panic() {
        // é(2字节)+中(3字节)：字节 3 边界落在「中」的中间
        assert!(normalize_absolute_dir(r"\\?\é中", "f").is_err());
        assert!(!is_absolute_path_like(r"\\?\é中"));
        assert!(validate_agent_work_dir(r"\\?\é中").is_err());
        // 🦀(4字节) 开头：字节 3 边界落在 emoji 中间；UNC 假阳性形态同测
        assert!(normalize_absolute_dir(r"\\?\🦀UNC\x", "f").is_err());
        assert!(normalize_absolute_dir(r"\\?\🦀", "f").is_err());
        // 中文目录段（非 ASCII 但合法路径成分）不 panic——首字节非盘符即 Err
        assert!(normalize_absolute_dir(r"\\?\中文\目录", "f").is_err());
    }

    #[test]
    fn normalize_absolute_dir_verbatim_unc_empty_tail() {
        // \\?\UNC\（空剩余）→ Ok("//")：与非 verbatim 输入 // 的既有放行一致
        assert_eq!(
            normalize_absolute_dir(r"\\?\UNC\", "f").expect("verbatim unc empty"),
            "//"
        );
    }

    #[test]
    fn is_absolute_path_like_handles_verbatim() {
        // 合法 verbatim（盘符/UNC）→ true
        assert!(is_absolute_path_like(r"\\?\C:\Users\dev\proj"));
        assert!(is_absolute_path_like(r"\\?\UNC\server\share\p"));
        // 非法 verbatim / 设备命名空间 → false（不是合法绝对路径形态）
        assert!(!is_absolute_path_like(r"\\?\foo\bar"));
        assert!(!is_absolute_path_like(r"\\.\PhysicalDrive0"));
        // 非 verbatim 的 UNC 不受影响
        assert!(is_absolute_path_like(r"\\srv\share\a"));
    }

    #[test]
    fn validate_agent_work_dir_verbatim_segments_count_inner() {
        // 段数按剥前缀后的内层计（? 幽灵段消失）：C:,a..g = 8 段 Ok
        let ok = r"\\?\C:\a\b\c\d\e\f\g";
        assert!(validate_agent_work_dir(ok).is_ok(), "{ok}");
        // 9 段超上限
        let too_deep = r"\\?\C:\a\b\c\d\e\f\g\h";
        assert!(validate_agent_work_dir(too_deep).is_err());
        // 合法 verbatim 整体过校验（调用方以 raw 值透传，Windows 下作 cwd）
        assert!(validate_agent_work_dir(r"\\?\C:\Users\dev\proj").is_ok());
        assert!(validate_agent_work_dir(r"\\?\UNC\server\share\p").is_ok());
        // 非法 verbatim 拒绝
        assert!(validate_agent_work_dir(r"\\?\foo\bar").is_err());
    }

    // ── validate_agent_work_dir：两形态分派 ──────────────────────────────

    #[test]
    fn validate_agent_work_dir_accepts_single_segment() {
        // 单段语义与 validate_identifier 一致（历史行为）
        assert!(validate_agent_work_dir("custom_workspace_123").is_ok());
        assert!(validate_agent_work_dir("p1").is_ok());
        assert!(validate_agent_work_dir("").is_err());
        assert!(validate_agent_work_dir("../etc").is_err());
        assert!(validate_agent_work_dir("a b").is_err());
    }

    #[test]
    fn validate_agent_work_dir_accepts_absolute_forms() {
        // 常规项目主场景：子容器内绝对路径
        assert!(validate_agent_work_dir("/home/user/web/proj_1").is_ok());
        assert!(validate_agent_work_dir("/home/user").is_ok());
        // 不限制前缀（容器即边界）；Windows / UNC 形态
        assert!(validate_agent_work_dir("/srv/data/p1").is_ok());
        assert!(validate_agent_work_dir("C:/Users/dev/proj").is_ok());
        assert!(validate_agent_work_dir("//srv/share/p1").is_ok());
        // 尾随空白容忍（trim）；中文/空格段合法（不逐段 identifier）
        assert!(validate_agent_work_dir("  /home/user/web/p1  ").is_ok());
        assert!(validate_agent_work_dir("/home/user/我的项目").is_ok());
    }

    #[test]
    fn validate_agent_work_dir_rejects_absolute_edge_cases() {
        // 裸根（段数 0）/ 穿越段 / 相对多段 / 控制字符 / 超段数
        assert!(validate_agent_work_dir("/").is_err());
        assert!(validate_agent_work_dir("//").is_err());
        assert!(validate_agent_work_dir("/a/../b").is_err());
        assert!(validate_agent_work_dir("/a/./b").is_err());
        assert!(validate_agent_work_dir("home/user/p1").is_err()); // 相对多段 → 单段分支拒 '/'
        let deep = format!(
            "/{}",
            (0..=AGENT_WORK_DIR_MAX_SEGMENTS)
                .map(|i| format!("s{i}"))
                .collect::<Vec<_>>()
                .join("/")
        );
        assert!(validate_agent_work_dir(&deep).is_err()); // 9 段超上限
    }

    #[test]
    fn validate_agent_work_dir_for_service_dispatches_by_service_type() {
        use crate::ServiceType;
        // Computer：两形态均放行
        assert!(
            validate_agent_work_dir_for_service(
                &ServiceType::ComputerAgentRunner,
                "/home/user/web/p1"
            )
            .is_ok()
        );
        assert!(
            validate_agent_work_dir_for_service(&ServiceType::ComputerAgentRunner, "custom_dir")
                .is_ok()
        );
        // Web/Userapp/UserappBuilder：单段现行语义，绝对路径显式拒绝
        for st in [
            ServiceType::WebAgentRunner,
            ServiceType::Userapp,
            ServiceType::UserappBuilder,
        ] {
            assert!(
                validate_agent_work_dir_for_service(&st, "custom_dir").is_ok(),
                "{st:?}"
            );
            let err = validate_agent_work_dir_for_service(&st, "/home/user/web/p1")
                .expect_err("absolute must be rejected");
            assert!(
                err.contains("only supported for ComputerAgentRunner"),
                "{st:?}: {err}"
            );
        }
    }
}
