//! 统一响应助手 (多数 success 响应是 `{success:true, ...payload}`)。

use serde::Serialize;
use serde_json::{Value, json};

use crate::extract::AppJson as Json;

/// 把任意可序列化 payload 包成 `{success:true, ...payload}` (字段平铺)。
pub fn success<T: Serialize>(payload: T) -> Json<Value> {
    let mut v = serde_json::to_value(payload).unwrap_or_else(|_| json!({}));
    if let Some(obj) = v.as_object_mut() {
        obj.insert("success".to_string(), json!(true));
    } else {
        v = json!({ "success": true, "data": v });
    }
    Json(v)
}

/// 仅带 message 的成功响应。
pub fn success_msg(message: &str) -> Json<Value> {
    Json(json!({ "success": true, "message": message }))
}

/// 自捕式失败响应 (HTTP 200 + `{success:false, message}`, 用于 nuwax 路由自捕分支)。
pub fn failure_msg(message: &str) -> Json<Value> {
    Json(json!({ "success": false, "message": message }))
}

/// deprecated 响应 (GIT_ENABLED 时, HTTP 200 + `{success:false, deprecated:true, message}`)。
pub fn deprecated(message: &str) -> Json<Value> {
    Json(json!({ "success": false, "deprecated": true, "message": message }))
}
