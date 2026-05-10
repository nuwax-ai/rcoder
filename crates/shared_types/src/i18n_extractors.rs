//! Locale-aware request extractors.
//!
//! These wrappers map Axum extractor rejections into unified `AppError` so
//! HTTP error responses are consistently localized through `AppError::into_response`.

use std::collections::HashMap;

use axum::{
    extract::{
        FromRequest, Json, Request,
        rejection::JsonRejection,
    },
};
use axum::http::Uri;
use serde::de::DeserializeOwned;

use crate::AppError;

/// JSON 或 Query 参数提取器
///
/// 支持两种输入方式：
/// 1. JSON body: `{"project_id": "xxx"}`
/// 2. Query params: `?project_id=xxx`
///
/// 合并策略：Query String 作为基础值，JSON Body 覆盖（当 JSON 值非 null 时）
/// - 如果 query string 和 JSON body 都有同一个字段，JSON body 的值优先
/// - 如果 JSON body 中某个字段为 null 或不存在，使用 query string 的值
///
/// 适用于需要同时兼容 GET（query）和 POST（body）两种调用方式的接口。
pub struct I18nJsonOrQuery<T>(pub T);

impl<S, T> FromRequest<S> for I18nJsonOrQuery<T>
where
    S: Send + Sync,
    T: DeserializeOwned + serde::Serialize + Send,
    Json<T>: FromRequest<S, Rejection = JsonRejection>,
{
    type Rejection = AppError;

    async fn from_request(req: Request, state: &S) -> Result<Self, Self::Rejection> {
        // 1. 使用 http::Uri 提取 query string
        let uri: Uri = req.uri().clone();
        let query_string = uri.query().unwrap_or("");

        // 2. 解析 query string 为 serde_json::Value
        let query_value: serde_json::Value = if query_string.is_empty() {
            serde_json::Value::Object(serde_json::Map::new())
        } else {
            // 使用 serde_urlencoded 解析为 HashMap，然后转为 Value
            match serde_urlencoded::from_str::<HashMap<String, serde_json::Value>>(query_string) {
                Ok(map) => serde_json::Value::Object(map.into_iter().collect()),
                Err(_) => {
                    return Err(AppError::with_i18n_key(
                        crate::error_codes::ERR_INVALID_PARAMS,
                        "error.invalid_params",
                    ));
                }
            }
        };

        // 3. 尝试解析 JSON body
        match Json::<T>::from_request(req, state).await {
            Ok(Json(json_value)) => {
                // 4. JSON 存在，合并两者（JSON 优先）
                let json_value_as_value = serde_json::to_value(&json_value)
                    .unwrap_or(serde_json::Value::Null);
                let merged = deep_merge(query_value, json_value_as_value);
                // 反序列化为 T
                let result = serde_json::from_value(merged)
                    .map_err(|_| AppError::with_i18n_key(
                        crate::error_codes::ERR_INVALID_PARAMS,
                        "error.invalid_params",
                    ))?;
                Ok(Self(result))
            }
            Err(_) => {
                // 5. JSON 解析失败，检查 query string 是否为空
                if query_string.is_empty() {
                    return Err(AppError::with_i18n_key(
                        crate::error_codes::ERR_INVALID_PARAMS,
                        "error.invalid_params",
                    ));
                }
                // 使用 query string 反序列化
                let result = serde_json::from_value(query_value)
                    .map_err(|_| AppError::with_i18n_key(
                        crate::error_codes::ERR_INVALID_PARAMS,
                        "error.invalid_params",
                    ))?;
                Ok(Self(result))
            }
        }
    }
}

/// 深度合并两个 JSON 对象
///
/// base: 基础值（query string）
/// override: 覆盖值（JSON body）
///
/// 规则：override 中的非 null 值会覆盖 base 中的对应值
fn deep_merge(base: serde_json::Value, override_: serde_json::Value) -> serde_json::Value {
    match (base, override_) {
        (serde_json::Value::Object(mut base_map), serde_json::Value::Object(override_map)) => {
            for (key, override_value) in override_map {
                let base_value = base_map.remove(&key).unwrap_or(serde_json::Value::Null);
                let merged_value = if override_value.is_null() {
                    // override 值为 null，使用 base 的值
                    base_value
                } else if override_value.is_object() && base_value.is_object() {
                    // 两者都是对象，递归合并
                    deep_merge(base_value, override_value)
                } else {
                    // override 有值（非 null 且不是对象），直接使用 override
                    override_value
                };
                base_map.insert(key, merged_value);
            }
            serde_json::Value::Object(base_map)
        }
        // 如果 override 不是对象，直接使用 override
        (_, override_) => override_,
    }
}

impl<T> I18nJsonOrQuery<T>
where
    T: garde::Validate,
    T::Context: Default,
{
    /// 校验并转换为 AppError
    ///
    /// 使用方法：
    /// ```ignore
    /// let I18nJsonOrQuery(request) = I18nJsonOrQuery(request).validate_into_app_error()?;
    /// ```
    pub fn validate_into_app_error(self) -> Result<Self, AppError> {
        self.0.validate().map_err(crate::validation::garde_err_to_app_error)?;
        Ok(self)
    }
}
