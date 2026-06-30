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
