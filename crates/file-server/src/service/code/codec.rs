//! URI 编解码 (对齐 JS `decodeURIComponent` / `encodeURIComponent`)。
//!
//! decodeURIComponent: 非法 % 保留原串, 不抛错 (对齐 nuwax 路由层)。
//! encodeURIComponent: 保留 [A-Za-z0-9-_.!~*'()], 其余按 UTF-8 字节百分号编码
//! (多字节字符逐字节, 与 JS 一致)。

use percent_encoding::{AsciiSet, NON_ALPHANUMERIC, percent_decode_str, utf8_percent_encode};

/// `encodeURIComponent` 不编码的非字母数字 ASCII 字符。
const URI_COMPONENT_ENCODE_SET: &AsciiSet = &NON_ALPHANUMERIC
    .remove(b'-')
    .remove(b'_')
    .remove(b'.')
    .remove(b'!')
    .remove(b'~')
    .remove(b'*')
    .remove(b'\'')
    .remove(b'(')
    .remove(b')');

/// 解码 URI component；非法输入按 nuwax 兼容策略保留或 lossy 替换，而不是抛异常。
pub fn decode_uri_component(s: &str) -> String {
    percent_decode_str(s).decode_utf8_lossy().into_owned()
}

/// JS encodeURIComponent 等价。
pub fn encode_uri_component(s: &str) -> String {
    utf8_percent_encode(s, URI_COMPONENT_ENCODE_SET).to_string()
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
        assert_eq!(decode_uri_component("bad%FF"), "bad�"); // 非法 UTF-8 lossy
        assert_eq!(decode_uri_component("a+b"), "a+b"); // URI component 不把 + 当空格
    }

    #[test]
    fn encode_uri_component_matches_js() {
        // 保留 unreserved + !~*'()
        assert_eq!(encode_uri_component("a-b.c_1!~*'()"), "a-b.c_1!~*'()");
        // 空格 → %20, 斜杠 → %2F
        assert_eq!(encode_uri_component("a b/c"), "a%20b%2Fc");
        // 中文逐字节 (中 = E4 B8 AD)
        assert_eq!(encode_uri_component("中"), "%E4%B8%AD");
        assert_eq!(
            encode_uri_component(";/?:@&=+$,#%"),
            "%3B%2F%3F%3A%40%26%3D%2B%24%2C%23%25"
        );
        // 互逆
        assert_eq!(
            decode_uri_component(&encode_uri_component("a b/中文")),
            "a b/中文"
        );
    }
}
