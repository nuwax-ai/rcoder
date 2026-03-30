use tonic::metadata::MetadataValue;

const ACCEPT_LANGUAGE_METADATA_KEY: &str = "accept-language";

/// Get locale from current HTTP request context (task-local).
pub fn current_grpc_locale() -> &'static str {
    shared_types::current_request_locale()
}

/// Inject `accept-language` metadata into a gRPC request.
pub fn inject_accept_language_metadata<T>(request: &mut tonic::Request<T>, locale: &'static str) {
    match MetadataValue::try_from(locale) {
        Ok(value) => {
            request
                .metadata_mut()
                .insert(ACCEPT_LANGUAGE_METADATA_KEY, value);
        }
        Err(err) => {
            tracing::warn!(
                "[GRPC_LOCALE] Invalid accept-language metadata value: locale={}, error={}",
                locale,
                err
            );
        }
    }
}

/// Build a gRPC request with `accept-language` metadata.
pub fn new_request_with_locale<T>(message: T, locale: &'static str) -> tonic::Request<T> {
    let mut request = tonic::Request::new(message);
    inject_accept_language_metadata(&mut request, locale);
    request
}

#[cfg(test)]
mod tests {
    use super::{current_grpc_locale, inject_accept_language_metadata};

    #[test]
    fn test_inject_accept_language_metadata() {
        let mut request = tonic::Request::new(());
        inject_accept_language_metadata(&mut request, "zh-CN");
        let value = request
            .metadata()
            .get("accept-language")
            .and_then(|v| v.to_str().ok());
        assert_eq!(value, Some("zh-CN"));
    }

    #[tokio::test]
    async fn test_current_grpc_locale_fallback_default() {
        assert_eq!(current_grpc_locale(), "en-US");
    }
}
