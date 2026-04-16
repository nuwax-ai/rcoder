use std::future::Future;

use crate::i18n::DEFAULT_LOCALE;

tokio::task_local! {
    static REQUEST_LOCALE: &'static str;
}

/// 在当前异步任务作用域内设置请求语言
pub async fn scope_request_locale<F>(locale: &'static str, fut: F) -> F::Output
where
    F: Future,
{
    REQUEST_LOCALE.scope(locale, fut).await
}

/// 获取当前请求语言（若未设置则回退默认语言）
pub fn current_request_locale() -> &'static str {
    REQUEST_LOCALE
        .try_with(|locale| *locale)
        .unwrap_or(DEFAULT_LOCALE)
}
