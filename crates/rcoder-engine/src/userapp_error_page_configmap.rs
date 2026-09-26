//! 错误页 ConfigMap 后端（`kubernetes` feature 专属；不引入该依赖时
//! `kubernetes-configmap` 后端配置显式报错，不静默降级）。
//!
//! 约束（proxy-error-page.md §8.2）：
//! - 只操作**固定** namespace/name/key——请求方不能指定任意对象或路径；
//! - 创建缺失对象；存在时带 resourceVersion 条件更新，并发冲突 → 409 语义；
//! - DELETE 只移除页面 key，保留同一对象其他 data/binaryData/metadata；
//! - 对象总数据 ≤1 MiB，超限明确失败，不删其他 key 腾空间；
//! - immutable 对象报配置冲突，不删对象重建。

use k8s_openapi::api::core::v1::ConfigMap;
use kube::api::{Api, ObjectMeta, Patch, PatchParams, PostParams};
use serde_json::json;

use super::userapp_error_page::ResolvedConfig;

/// ConfigMap 操作失败（code 与服务层错误码对齐）。
pub struct ConfigmapOpError {
    pub code: &'static str,
    pub message: String,
}

fn configmap_error(message: String) -> ConfigmapOpError {
    ConfigmapOpError {
        code: "ERROR_PAGE_STORE_IO",
        message,
    }
}

fn conflict_error(message: String) -> ConfigmapOpError {
    ConfigmapOpError {
        code: "ERROR_PAGE_STORE_CONFLICT",
        message,
    }
}

/// 读取当前对象并返回页面 key 的内容摘要（对象缺失/key 缺失 → None）。
pub(crate) async fn read_key_sha256(
    config: &ResolvedConfig,
) -> Result<Option<String>, ConfigmapOpError> {
    let key = page_key(config);
    let api = configmap_api(config).await?;
    let name = current_name(config);
    match api.get(&name).await {
        Ok(object) => Ok(object
            .data
            .as_ref()
            .and_then(|data| data.get(&key))
            .map(|value| crate::userapp_error_page::sha256_hex(value.as_bytes()))),
        Err(kube::Error::Api(error)) if error.code == 404 => Ok(None),
        Err(error) => Err(configmap_error(format!(
            "read configmap {}: {error}",
            current_name(config)
        ))),
    }
}

/// upsert 固定 key（保留其他 key；resourceVersion 条件更新；冲突报 409 语义）。
pub(crate) async fn upsert_key(
    config: &ResolvedConfig,
    content: &[u8],
    max_total_bytes: usize,
) -> Result<(), ConfigmapOpError> {
    let name = current_name(config);
    let key = page_key(config);
    let Ok(text) = std::str::from_utf8(content) else {
        return Err(configmap_error("page content is not valid UTF-8".into()));
    };
    let api = configmap_api(config).await?;
    let existing = match api.get(&name).await {
        Ok(object) => Some(object),
        Err(kube::Error::Api(error)) if error.code == 404 => None,
        Err(error) => return Err(configmap_error(format!("read configmap {name}: {error}"))),
    };
    match existing {
        None => {
            let object = ConfigMap {
                metadata: ObjectMeta {
                    name: Some(name.clone()),
                    namespace: config.configmap_namespace.clone(),
                    ..Default::default()
                },
                data: Some(std::collections::BTreeMap::from([(
                    key.clone(),
                    text.to_string(),
                )])),
                ..Default::default()
            };
            if let Err(error) = api.create(&PostParams::default(), &object).await {
                // 并发创建：按冲突上报，不默默重试覆盖另一位操作者
                if matches!(&error, kube::Error::Api(api_error) if api_error.code == 409) {
                    return Err(conflict_error(format!(
                        "configmap {name} created concurrently; retry"
                    )));
                }
                return Err(configmap_error(format!("create configmap {name}: {error}")));
            }
            Ok(())
        }
        Some(mut object) => {
            if object.immutable == Some(true) {
                return Err(conflict_error(format!(
                    "configmap {name} is immutable; recreate it without the page key or drop the override"
                )));
            }
            let mut data = object.data.clone().unwrap_or_default();
            // 总量校验：保留其他 key 后不得超过上限（不能删其他 key 腾空间）
            let other_bytes: usize = data
                .iter()
                .filter(|(other, _)| other.as_str() != key)
                .map(|(_, value)| value.len())
                .sum();
            if other_bytes + content.len() > max_total_bytes {
                return Err(configmap_error(format!(
                    "configmap {name} total data would exceed {} bytes (other keys keep {} bytes)",
                    max_total_bytes, other_bytes
                )));
            }
            data.insert(key.clone(), text.to_string());
            object.data = Some(data);
            // 条件更新：resourceVersion 变化即 409
            let version = object.metadata.resource_version.clone().unwrap_or_default();
            let params = PatchParams {
                field_manager: Some("rcoder-error-page".into()),
                force: false,
                dry_run: false,
                ..Default::default()
            };
            let patch = json!({
                "metadata": {"resourceVersion": version},
                "data": object.data,
            });
            if let Err(error) = api.patch(&name, &params, &Patch::Merge(&patch)).await {
                if matches!(&error, kube::Error::Api(api_error) if api_error.code == 409) {
                    return Err(conflict_error(format!(
                        "configmap {name} changed concurrently; retry with fresh content"
                    )));
                }
                return Err(configmap_error(format!("update configmap {name}: {error}")));
            }
            Ok(())
        }
    }
}

/// 移除固定 key（幂等；保留对象与其他内容；对象缺失 → Ok(false)）。
pub(crate) async fn delete_key(config: &ResolvedConfig) -> Result<bool, ConfigmapOpError> {
    let name = current_name(config);
    let key = page_key(config);
    let api = configmap_api(config).await?;
    let mut object = match api.get(&name).await {
        Ok(object) => object,
        Err(kube::Error::Api(error)) if error.code == 404 => return Ok(false),
        Err(error) => return Err(configmap_error(format!("read configmap {name}: {error}"))),
    };
    let Some(mut data) = object.data.clone() else {
        return Ok(false);
    };
    if data.remove(&key).is_none() {
        return Ok(false);
    }
    if object.immutable == Some(true) {
        return Err(conflict_error(format!(
            "configmap {name} is immutable; cannot remove the page key"
        )));
    }
    object.data = Some(data);
    let version = object.metadata.resource_version.clone().unwrap_or_default();
    let params = PatchParams {
        field_manager: Some("rcoder-error-page".into()),
        force: false,
        dry_run: false,
        ..Default::default()
    };
    let patch = json!({
        "metadata": {"resourceVersion": version},
        "data": object.data,
    });
    if let Err(error) = api.patch(&name, &params, &Patch::Merge(&patch)).await {
        if matches!(&error, kube::Error::Api(api_error) if api_error.code == 409) {
            return Err(conflict_error(format!(
                "configmap {name} changed concurrently; retry"
            )));
        }
        return Err(configmap_error(format!("update configmap {name}: {error}")));
    }
    Ok(true)
}

fn current_name(config: &ResolvedConfig) -> String {
    config.configmap_name.clone().unwrap_or_default()
}

fn page_key(config: &ResolvedConfig) -> String {
    config
        .configmap_key
        .clone()
        .unwrap_or_else(|| "userapp-error.html".to_string())
}

async fn configmap_api(config: &ResolvedConfig) -> Result<Api<ConfigMap>, ConfigmapOpError> {
    let namespace = config
        .configmap_namespace
        .as_deref()
        .ok_or_else(|| configmap_error("configmap_namespace is required".into()))?;
    let client = kube::Client::try_default()
        .await
        .map_err(|error| configmap_error(format!("kubernetes client unavailable: {error}")))?;
    Ok(Api::namespaced(client, namespace))
}
