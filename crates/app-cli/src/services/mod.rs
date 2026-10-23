//! 服务承载域：supervisor 编排核心、supervisord 引擎适配（XML-RPC）、服务规格
//! 与执行入口、type=static 静态托管和工作区索引服务。
pub mod run_service;
pub mod static_hosting;
pub mod supervisor;
pub mod supervisord_host;
pub mod svc_spec;
pub mod workspace_index;
pub mod xmlrpc;

/// 请求路径百分号解码（`%20` 等），供工作区索引与静态托管共用。
///
/// 语义：非法 `%XX` 原样保留（后续文件读取 404/SPA fallback 兜底）；`+` 不作
/// 空格解码；解出非法 UTF-8 按 lossy 替换。按字节解析，原始多字节
/// request-target 不会 panic。
pub(crate) fn percent_decode_path(path: &str) -> String {
    percent_encoding::percent_decode_str(path)
        .decode_utf8_lossy()
        .into_owned()
}

#[cfg(test)]
mod tests {
    use super::percent_decode_path;

    #[test]
    fn percent_decode_path_matrix() {
        assert_eq!(percent_decode_path("/a%20b"), "/a b");
        assert_eq!(percent_decode_path("/%E4%B8%AD"), "/中");
        // 单次解码：%25 只解出字面 %
        assert_eq!(percent_decode_path("/%25%E4%B8%AD"), "/%中");
        // 非法序列与孤立 % 原样保留；+ 不是空格
        assert_eq!(percent_decode_path("/%G0%+x"), "/%G0%+x");
        // 非法 UTF-8 字节 lossy 替换
        assert_eq!(percent_decode_path("/%FF"), "/\u{FFFD}");
        // 原始多字节字符不做二次处理、不 panic
        assert_eq!(percent_decode_path("/%中"), "/%中");
    }
}
