//! URI 编解码 (对齐 JS `decodeURIComponent` / `encodeURIComponent`)。
//!
//! decodeURIComponent: 非法 % 保留原串, 不抛错 (对齐 nuwax 路由层)。
//! encodeURIComponent: 保留 [A-Za-z0-9-_.!~*'()], 其余按 UTF-8 字节百分号编码
//! (多字节字符逐字节, 与 JS 一致)。

/// JS decodeURIComponent 等价。
pub fn decode_uri_component(s: &str) -> String {
    let b = s.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%'
            && i + 2 < b.len()
            && let (Some(h), Some(l)) = (hex_digit(b[i + 1]), hex_digit(b[i + 2]))
        {
            out.push(h * 16 + l);
            i += 3;
            continue;
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8(out).unwrap_or_else(|e| String::from_utf8_lossy(e.as_bytes()).into_owned())
}

fn hex_digit(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

/// JS encodeURIComponent 等价。
pub fn encode_uri_component(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for &b in s.as_bytes() {
        if b.is_ascii_alphanumeric()
            || matches!(
                b,
                b'-' | b'_' | b'.' | b'!' | b'~' | b'*' | b'\'' | b'(' | b')'
            )
        {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decode_uri_percent() {
        assert_eq!(decode_uri_component("%E4%B8%AD"), "中");
        assert_eq!(decode_uri_component("hello%20world"), "hello world");
        assert_eq!(decode_uri_component("plain"), "plain");
        assert_eq!(decode_uri_component("bad%ZZ"), "bad%ZZ"); // 非法保留
    }

    #[test]
    fn encode_uri_component_matches_js() {
        // 保留 unreserved + !~*'()
        assert_eq!(encode_uri_component("a-b.c_1!~*'()"), "a-b.c_1!~*'()");
        // 空格 → %20, 斜杠 → %2F
        assert_eq!(encode_uri_component("a b/c"), "a%20b%2Fc");
        // 中文逐字节 (中 = E4 B8 AD)
        assert_eq!(encode_uri_component("中"), "%E4%B8%AD");
        // 互逆
        assert_eq!(
            decode_uri_component(&encode_uri_component("a b/中文")),
            "a b/中文"
        );
    }
}
