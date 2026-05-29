//! gRPC 请求 locale 解析

const ACCEPT_LANGUAGE_METADATA_KEY: &str = "accept-language";

pub fn locale_from_grpc_request<T>(request: &tonic::Request<T>) -> &'static str {
    shared_types::parse_accept_language(
        request
            .metadata()
            .get(ACCEPT_LANGUAGE_METADATA_KEY)
            .and_then(|v| v.to_str().ok()),
    )
}

pub fn localized(locale: &'static str, zh_cn: &str, zh_tw: &str, en_us: &str) -> String {
    match locale {
        "zh-CN" => zh_cn.to_string(),
        "zh-TW" => zh_tw.to_string(),
        _ => en_us.to_string(),
    }
}
