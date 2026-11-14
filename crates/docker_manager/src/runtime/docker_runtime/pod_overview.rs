//! `/computer/pod/overview` 的 docker compose 实现（方案：docs/pod-overview.md P1）。
//!
//! 一次请求 = 1 次权威 list（bollard `all=true`，**不是** RCoder 内存登记表——
//! 未登记的 mysql/redis 等容器也必须出现）+ 身份关联（登记表按容器名对齐，
//! 查不到身份就留空，**不从名字猜造**）+ N 次 inspect（并发 8）+ events。
//!
//! 事件链的三个硬事实（独立复审 R2 证实）：
//! - **`since`/`until` 必须是 Unix 秒**：CLI 的 "1h" 写法由 CLI 换算，直接
//!   传 Engine API 返回 400（invalid timestamp）；
//! - **容器事件判别字段是顶层 `Type=container`**，不是 Attributes.container
//!   （后者只在 network connect 等场景出现，正常容器事件没有它）；
//! - **流错误必须传播**：吞成 break 会把 HTTP 500 伪装成"无事件"。
//!   timebox 到点或截断要标注 partial，不能当完整成功。
//!   另：官方只回放最近 256 条——"最近 1h"是覆盖上界，不是完整历史。

use std::collections::HashMap;

use bollard::models::{ContainerInspectResponse, ContainerState, EventMessageTypeEnum};
use bollard::query_parameters::{
    EventsOptionsBuilder, InspectContainerOptionsBuilder, ListContainersOptionsBuilder,
};
use container_runtime_api::AppEventInfo;
use container_runtime_api::pod_overview::{
    PodHealth, PodIdentity, PodOverviewDegraded, PodOverviewItem, PodOverviewSnapshot,
    PodResourcesSpec, PodVolumeSpec,
};
use futures_util::StreamExt;
use tracing::warn;

/// 事件采集窗口（秒）。docker 只回放最近 256 条，这是上界不是保证。
const EVENT_WINDOW_SECS: i64 = 3600;
/// timebox：events 是无限流，回放完会继续推实时事件。
const EVENT_TIMEBOX_MS: u64 = 300;
/// 防御性上限：异常 busy 的 daemon 不至于把内存吃爆。
const EVENT_HARD_CAP: usize = 2000;

impl super::DockerRuntime {
    /// 采集本 daemon 全部容器的概览（P1：spec + health + events，无 usage）。
    pub(crate) async fn pod_overview_inner(
        &self,
        events_limit: usize,
    ) -> container_runtime_api::ContainerRuntimeResult<PodOverviewSnapshot> {
        let mut degraded = Vec::new();
        let client = self.inner.get_docker_client();

        // 1) 权威列表（all=true，含停止的容器；失败即整体失败——没有列表
        //    这个查询没有意义）
        let list_options = ListContainersOptionsBuilder::new().all(true).build();
        let summary_list = client
            .list_containers(Some(list_options))
            .await
            .map_err(|e| {
                container_runtime_api::ContainerRuntimeError::DockerError(format!(
                    "list containers for overview: {e}"
                ))
            })?;

        // 2) 身份关联：登记表（内存缓存）按容器名对齐；查询失败按"无身份"
        //    处理（受管容器的身份缺失不影响未登记容器出现）。
        //    **直接用结构化槽位**，不再从 project_id 重新拆（复审 R3：
        //    重新拆会把 builder 的 app_id 拆成空串、丢 user_id）。
        let registered: HashMap<String, container_runtime_api::RuntimeContainerInfo> =
            match self.fetch_containers().await {
                Ok(list) => list
                    .into_iter()
                    .map(|info| (info.container_name.clone(), info))
                    .collect(),
                Err(e) => {
                    warn!("[POD_OVERVIEW] managed registry degraded: {e}");
                    degraded.push(PodOverviewDegraded {
                        section: "identity".to_string(),
                        reason: "managed_registry_failed".to_string(),
                        detail: Some(e.to_string()),
                    });
                    HashMap::new()
                }
            };

        // 3) 事件（全量收集，健康在截断前计算；失败降级但**如实标注**）
        let (full_events, events_partial) = match collect_recent_events(self).await {
            Ok((events, partial)) => (events, partial),
            Err(e) => {
                warn!("[POD_OVERVIEW] docker events degraded: {e}");
                degraded.push(PodOverviewDegraded {
                    section: "events".to_string(),
                    reason: "docker_events_failed".to_string(),
                    detail: Some(e),
                });
                (Vec::new(), false)
            }
        };
        if events_partial {
            degraded.push(PodOverviewDegraded {
                section: "events".to_string(),
                reason: "docker_events_partial".to_string(),
                detail: Some(
                    "timebox 或条数上限截断；且 docker 只回放最近 256 条，覆盖可能不足 1h"
                        .to_string(),
                ),
            });
        }

        // 4) 逐容器 inspect（并发 8；**失败要区分**：404 是列表与检查之间的
        //    真实删除竞态（静默跳过），其他错误是采集缺口——必须进 degraded，
        //    否则"规格/状态不可读"会伪装成默认值（F5））
        let names: Vec<String> = summary_list
            .iter()
            .filter_map(|c| c.names.as_ref().and_then(|n| n.first().cloned()))
            .map(|n| n.trim_start_matches('/').to_string())
            .collect();
        // 期望的物理身份集合（来自权威 list）：inspect 返回不同 CID =
        // 同名容器已被删除重建，规格/状态属于另一代，不能与本次 list 的
        // 身份/事件拼接（第二轮复审 #2）
        // 每个名字期望的 CID（来自权威 list 的**同一行**）：inspect 返回
        // 的 ID 必须与该行的 CID 一致——按"整表集合"判断会放行同名接替者
        // （B 接替了 A 的名字后 inspect 返回 B，B 也在集合中，结果把 A 行
        // 的身份/事件与 B 的规格/状态拼接）。空 ID 按**未确认**处理。
        let expected_cids: HashMap<String, String> = summary_list
            .iter()
            .filter_map(|c| {
                let name = c
                    .names
                    .as_ref()
                    .and_then(|n| n.first())
                    .map(|n| n.trim_start_matches('/').to_string())?;
                let cid = c.id.clone()?;
                Some((name, cid))
            })
            .collect();
        let (inspections, inspect_failures) = inspect_all(self, &names, &expected_cids).await;
        let real_failures: Vec<&String> = inspect_failures
            .iter()
            .filter(|(_, err)| !is_not_found_error(err))
            .map(|(name, _)| name)
            .collect();
        if !real_failures.is_empty() {
            degraded.push(PodOverviewDegraded {
                section: "inspect".to_string(),
                reason: "inspect_failed".to_string(),
                detail: Some(format!(
                    "{} 个容器 inspect 失败（规格/状态按未知处理）: {}",
                    real_failures.len(),
                    real_failures
                        .iter()
                        .take(5)
                        .map(|n| n.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                )),
            });
        }

        // 5) 组装（事件与健康都按 **CID** 关联——名字复用不能证明同一实例，F4）
        let mut items = Vec::with_capacity(summary_list.len());
        for container in &summary_list {
            let Some(raw_name) = container.names.as_ref().and_then(|n| n.first().cloned()) else {
                continue;
            };
            let name = raw_name.trim_start_matches('/').to_string();
            let cid = container.id.clone().unwrap_or_default();
            let inspect = inspections.get(&name);
            // 身份按 CID 核验：登记表里同名但不同 CID 的旧记录不能沿用（F4）
            let identity = registered
                .get(&name)
                .filter(|info| info.container_id == cid)
                .and_then(|info| {
                    info.service_type.map(|service_type| PodIdentity {
                        service_type: Some(service_type),
                        user_id: info.user_id.clone(),
                        project_id: info.project_id.clone(),
                        app_id: info.app_id.clone(),
                        pod_id: info.pod_id.clone(),
                    })
                });
            let state = inspect.and_then(|i| i.state.as_ref());
            let phase = state
                .and_then(|s| s.status.as_ref())
                .map(|st| st.to_string())
                .unwrap_or_else(|| "unknown".to_string());
            let running = state.and_then(|s| s.running).unwrap_or(false);
            // 本 CID 的全量事件（倒序）；健康用全量、展示按 limit 截断（R6）
            let own_events: Vec<&RawEvent> = full_events.iter().filter(|e| e.cid == cid).collect();
            let display_events: Vec<AppEventInfo> = own_events
                .iter()
                .take(events_limit)
                .map(|e| raw_to_event(e))
                .collect();

            items.push(PodOverviewItem {
                name: name.clone(),
                workload: None, // docker：容器名即 workload 身份
                identity,
                phase,
                ready: running,
                node: None,
                ip: inspect
                    .and_then(|i| {
                        i.network_settings.as_ref().and_then(|n| {
                            n.networks
                                .as_ref()
                                .and_then(|nets| nets.values().next())
                                .and_then(|ep| ep.ip_address.clone())
                        })
                    })
                    .filter(|ip| !ip.is_empty()),
                created_at: inspect.and_then(|i| {
                    i.created.as_ref().and_then(|c| {
                        chrono::DateTime::parse_from_rfc3339(&c.to_string())
                            .ok()
                            .map(|dt| dt.with_timezone(&chrono::Utc))
                    })
                }),
                spec: docker_spec(inspect),
                health: docker_health(state, &own_events),
                events: display_events,
            });
        }

        Ok(PodOverviewSnapshot {
            pods: items,
            degraded,
            source: "docker".to_string(),
            namespace: None,
        })
    }
}

/// 并发 inspect（上限 8）。返回 (成功表, 失败清单)——失败**上抛**给调用方
/// 区分 404（真实删除竞态）与其他错误（采集缺口，F5）。
async fn inspect_all(
    runtime: &super::DockerRuntime,
    names: &[String],
    expected_cids: &HashMap<String, String>,
) -> (
    HashMap<String, ContainerInspectResponse>,
    Vec<(String, bollard::errors::Error)>,
) {
    use std::sync::Arc;
    let client = Arc::new(runtime.inner.get_docker_client().clone());
    let mut out = HashMap::new();
    let mut failures = Vec::new();
    const CONCURRENCY: usize = 8;
    let mut stream = futures_util::stream::iter(names.to_vec())
        .map(|name| {
            let client = client.clone();
            async move {
                let options = InspectContainerOptionsBuilder::new().build();
                let result = client.inspect_container(&name, Some(options)).await;
                (name, result)
            }
        })
        .buffer_unordered(CONCURRENCY);
    while let Some((name, result)) = stream.next().await {
        match result {
            Ok(inspect) => {
                // 核验返回的物理身份（**逐行**）：返回 ID 必须等于**该行**的
                // 期望 CID；空 ID 无法确认归属，同样按未确认跳过——未确认
                // 的规格/状态不能填进这一行。
                let returned_id = inspect.id.clone().unwrap_or_default();
                let confirmed = expected_cids
                    .get(&name)
                    .is_some_and(|expected| !returned_id.is_empty() && returned_id == *expected);
                if !confirmed {
                    warn!(
                        "[POD_OVERVIEW] inspect {name} returned unconfirmed identity (id={returned_id:.12}), skipping to avoid mixing generations"
                    );
                    continue;
                }
                out.insert(name, inspect);
            }
            Err(e) => {
                warn!("[POD_OVERVIEW] inspect {name} failed: {e}");
                failures.push((name, e));
            }
        }
    }
    (out, failures)
}

/// 404 = 容器在 list 与 inspect 之间被删除（真实竞态，非采集故障）。
///
/// **必须按 Bollard 错误的真实状态码判断**：按文本匹配会把"HTTP 500 的
/// 正文里含 not found"或"403 正文里含 404"误当删除竞态，漏掉降级标注。
fn is_not_found_error(err: &bollard::errors::Error) -> bool {
    matches!(
        err,
        bollard::errors::Error::DockerResponseServerError {
            status_code: 404,
            ..
        }
    )
}

/// RawEvent → 对外事件形态（显示用容器名；CID 留在内部关联）。
fn raw_to_event(e: &RawEvent) -> AppEventInfo {
    let ts = chrono::DateTime::from_timestamp(
        e.time_nano.div_euclid(1_000_000_000),
        e.time_nano.rem_euclid(1_000_000_000) as u32,
    )
    .map(|dt| dt.to_rfc3339())
    .unwrap_or_default();
    AppEventInfo {
        event_type: classify_event(&e.action),
        reason: e.action.clone(),
        message: format!("{} {}", e.action, e.container_name),
        timestamp: ts,
        object: e.container_name.clone(),
        count: 1,
    }
}

/// 事件收集的中间形态（保留纳秒序，排序后再转 AppEventInfo）。
struct RawEvent {
    time_nano: i64,
    action: String,
    container_name: String,
    /// Actor.ID（容器物理身份）。**名字复用不能证明同一实例**：同名新 CID
    /// 的首次 start 不是旧 CID die 的重启（F4）。对外显示用名字，内部
    /// 关联必须按 CID。
    cid: String,
    /// die 事件的 exitCode 属性（docker 把退出码放在 Actor 属性里）。
    exit_code: Option<i64>,
}

/// 收集最近 1h 的 docker **容器**事件。
///
/// 返回 `(按时间倒序的事件, 是否截断)`；请求/解码错误传播为 Err
/// （R2-3：吞错会把失败伪装成"无事件"）。
async fn collect_recent_events(
    runtime: &super::DockerRuntime,
) -> Result<(Vec<RawEvent>, bool), String> {
    let client = runtime.inner.get_docker_client();
    let now = chrono::Utc::now().timestamp();
    let options = EventsOptionsBuilder::new()
        .since(&(now - EVENT_WINDOW_SECS).to_string()) // Unix 秒（R2-1："1h" 是 CLI 写法，API 会 400）
        .until(&now.to_string())
        .build();
    let mut stream = client.events(Some(options));

    let mut raw: Vec<RawEvent> = Vec::new();
    let mut stream_failed: Option<String> = None;
    let mut truncated = false;
    let deadline = tokio::time::Duration::from_millis(EVENT_TIMEBOX_MS);
    let collected = tokio::time::timeout(deadline, async {
        while let Some(item) = stream.next().await {
            match item {
                // 流错误传播：HTTP 500/解码失败不能伪装成"无事件"（R2-3）
                Err(e) => {
                    stream_failed = Some(e.to_string());
                    break;
                }
                Ok(message) => {
                    // 容器事件判别 = 顶层 Type==container（R2-2）
                    if message.typ.as_ref() != Some(&EventMessageTypeEnum::CONTAINER) {
                        continue;
                    }
                    let (Some(action), Some(actor)) =
                        (message.action.as_ref(), message.actor.as_ref())
                    else {
                        continue;
                    };
                    let Some(container_name) = actor
                        .attributes
                        .as_ref()
                        .and_then(|attrs| attrs.get("name"))
                    else {
                        continue;
                    };
                    raw.push(RawEvent {
                        time_nano: message.time_nano.unwrap_or_default(),
                        action: action.clone(),
                        container_name: container_name.clone(),
                        cid: actor.id.clone().unwrap_or_default(),
                        exit_code: actor
                            .attributes
                            .as_ref()
                            .and_then(|attrs| attrs.get("exitCode"))
                            .and_then(|code| code.parse::<i64>().ok()),
                    });
                    if raw.len() >= EVENT_HARD_CAP {
                        truncated = true;
                        break;
                    }
                }
            }
        }
    })
    .await;
    if collected.is_err() {
        truncated = true; // timebox 到点：用已收到的部分
    }
    if let Some(error) = stream_failed {
        return Err(error);
    }

    // 纳秒序倒排（R9：同秒事件不被字符串序抹掉）。返回 RawEvent，
    // 由组装侧按 CID 过滤后转对外形态（display 用容器名）。
    raw.sort_by_key(|e| std::cmp::Reverse(e.time_nano));
    Ok((raw, truncated))
}

/// 事件动作 → Warning/Normal（与 k8s 语义对齐：die/oom/kill/restart 是异常信号）。
fn classify_event(action: &str) -> String {
    match action {
        "die" | "oom" | "kill" | "restart" | "rename" => "Warning",
        // 实际动作形态是 "health_status: unhealthy" / "health_status: healthy"：
        // 只有 unhealthy 是异常信号（F7——裸 "health_status" 匹配不到真实动作）
        _ if action.starts_with("health_status") && action.contains("unhealthy") => "Warning",
        _ => "Normal",
    }
    .to_string()
}

/// docker 规格：HostConfig + Mounts。**0 = 不限制 → None**。
fn docker_spec(inspect: Option<&ContainerInspectResponse>) -> PodResourcesSpec {
    let Some(inspect) = inspect else {
        return PodResourcesSpec::default();
    };
    let host_config = inspect.host_config.as_ref();
    let cpu_limit = host_config.and_then(|h| {
        if h.nano_cpus.unwrap_or(0) > 0 {
            Some(h.nano_cpus.unwrap_or_default() as f64 / 1e9)
        } else {
            let quota = h.cpu_quota.unwrap_or(0);
            let period = h.cpu_period.unwrap_or(0);
            if quota > 0 && period > 0 {
                Some(quota as f64 / period as f64)
            } else {
                None
            }
        }
    });
    // Memory == 0 = 不限制（不是 0MB）——这里最容易踩的语义坑
    let mem_limit = host_config
        .and_then(|h| h.memory.filter(|m| *m > 0))
        .map(|m| m / 1_048_576);
    let volumes = inspect
        .mounts
        .as_ref()
        .map(|mounts| {
            mounts
                .iter()
                .map(|m| PodVolumeSpec {
                    volume_type: "docker_mount".to_string(),
                    name: m
                        .destination
                        .as_deref()
                        .unwrap_or("")
                        .trim_start_matches('/')
                        .replace('/', "_"),
                    mount_path: m.destination.clone(),
                    capacity_mb: None, // docker 无配额概念
                    host_path: m.source.clone(),
                })
                .collect()
        })
        .unwrap_or_default();
    PodResourcesSpec {
        cpu_request_cores: None, // docker 没有 request 概念
        cpu_limit_cores: cpu_limit,
        mem_request_mb: None,
        mem_limit_mb: mem_limit,
        ephemeral_storage_limit_mb: None, // docker：writable-layer 大小可取（size=true），默认关
        volumes,
        containers: Vec::new(), // docker 单容器
    }
}

/// docker 健康。`events` 是该容器**同一 CID** 的未截断全量事件（倒序）。
///
/// 语义（R6/R7/F3 修正）：
/// - **重启数** = die→start "恢复对"数（按 CID：旧 CID 的 die 与新 CID 的
///   首次 start 不是一次重启，F4）；
/// - **最近一次终止**按**退出周期**归因：docker 的常见 OOM 序列是
///   `oom → die(137) → start`——只看"最后一条终止事件是不是 oom"会漏报
///   （最后一条是 die）。正确做法：找到最近的 die/kill，再往更旧方向扫到
///   本周期的 start 边界为止，窗口内出现 oom 即该次退出是 OOM；
/// - **退出码**优先取 die 事件的 exitCode 属性（当前运行态的 State.ExitCode
///   是 0，会覆盖上周期的 137，F3）；单独 137 仍不证明 OOM。
fn docker_health(state: Option<&ContainerState>, events: &[&RawEvent]) -> PodHealth {
    let restart_count = count_recovery_cycles(events);
    let (oom_in_cycle, die_exit_code) = last_exit_cycle(events);
    let oom_killed_last = state.and_then(|s| s.oom_killed).unwrap_or(false) || oom_in_cycle;
    // 退出码：die 事件属性 > 当前 State（运行中为 0/None）
    let exit_code = die_exit_code
        .or_else(|| state.and_then(|s| s.exit_code))
        .map(|c| c as i32);
    let last_terminate_reason = if oom_killed_last {
        Some("OOMKilled".to_string())
    } else {
        exit_code
            .filter(|code| *code != 0)
            .map(|code| format!("Error({code})"))
    };
    PodHealth {
        restart_count,
        restart_source: "docker_events".to_string(),
        last_terminate_reason,
        last_exit_code: exit_code,
        oom_killed_last,
        waiting_reason: None, // docker 无 CrashLoopBackOff 概念
        detail: state
            .and_then(|s| s.error.clone())
            .filter(|e| !e.is_empty()),
    }
}

/// 最近一次退出周期：(是否 OOM, die 的退出码)。
///
/// 扫描倒序列表：第一条 die/kill 是最近一次终止；从它往更旧方向直到
/// `start`（周期边界）为止的窗口内出现 `oom` → 该次退出为 OOM。
fn last_exit_cycle(events: &[&RawEvent]) -> (bool, Option<i64>) {
    // 锚点**只认 die**（实际退出）：kill 信号不必然导致退出——OOM 恢复后
    // 收到不致命的 SIGHUP 也产生 kill 事件，把它当退出会误清最近 OOM。
    let Some(term_idx) = events.iter().position(|e| e.action == "die") else {
        return (false, None);
    };
    let exit_code = events[term_idx].exit_code;
    let oom_in_cycle = events[term_idx..]
        .iter()
        // 到 start 边界为止（不含）——start 属于上一周期
        .take_while(|e| e.action != "start")
        .any(|e| e.action == "oom");
    (oom_in_cycle, exit_code)
}

/// 统计 die→start 恢复对（events 倒序输入，从旧往新扫）。
fn count_recovery_cycles(events: &[&RawEvent]) -> u32 {
    let mut count = 0u32;
    let mut pending_die_cid: Option<&str> = None;
    for event in events.iter().rev() {
        // 升序扫描：die 记下 **CID**，随后同 CID 的 start 记一次恢复；
        // 孤立 die（容器停着没起来）不计；**跨 CID 的 start 不是旧实例的
        // 重启**（名字复用创建的新容器，F4）
        match event.action.as_str() {
            // 只有 die（实际退出）开启恢复对；oom/kill 是周期内上下文，
            // 不单独构成"等待 start"
            "die" => pending_die_cid = Some(event.cid.as_str()),
            "oom" | "kill" => {}
            "start" => {
                if let Some(die_cid) = pending_die_cid
                    && die_cid == event.cid
                {
                    count += 1;
                }
                pending_die_cid = None;
            }
            _ => {}
        }
    }
    count
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zero_memory_means_unlimited_not_zero_mb() {
        let with = |memory: Option<i64>| ContainerInspectResponse {
            host_config: Some(bollard::models::HostConfig {
                memory,
                ..Default::default()
            }),
            ..Default::default()
        };
        let spec = docker_spec(Some(&with(Some(0))));
        assert_eq!(spec.mem_limit_mb, None, "0 = 不限制，不是 0MB");
        let spec = docker_spec(Some(&with(Some(6 * 1_048_576 * 1024))));
        assert_eq!(spec.mem_limit_mb, Some(6144));
    }

    #[test]
    fn nano_cpus_converts_to_cores() {
        let response = ContainerInspectResponse {
            host_config: Some(bollard::models::HostConfig {
                nano_cpus: Some(1_500_000_000), // 1.5 核
                ..Default::default()
            }),
            ..Default::default()
        };
        let spec = docker_spec(Some(&response));
        assert_eq!(spec.cpu_limit_cores, Some(1.5));
    }
    /// 测试辅助：RawEvent（倒序输入按 time_nano 降序手工排列）。
    fn raw(action: &str, nano: i64, cid: &str, exit: Option<i64>) -> RawEvent {
        RawEvent {
            time_nano: nano,
            action: action.to_string(),
            container_name: "c".to_string(),
            cid: cid.to_string(),
            exit_code: exit,
        }
    }

    /// F3：docker 常见 OOM 序列 oom → die(137) → start（当前运行态
    /// OOMKilled 已清）必须报最近一次 OOM，且退出码取 die 的 137。
    #[test]
    fn oom_die_start_cycle_reports_last_oom() {
        // 倒序：start(3) die(2,137) oom(1)
        let events = [
            raw("start", 3, "cid1", None),
            raw("die", 2, "cid1", Some(137)),
            raw("oom", 1, "cid1", None),
        ];
        let state = ContainerState {
            oom_killed: Some(false), // 已恢复运行，标记已清
            exit_code: Some(0),      // 当前运行态
            ..Default::default()
        };
        let health = docker_health(Some(&state), &events.iter().collect::<Vec<_>>());
        assert!(
            health.oom_killed_last,
            "oom→die(137)→start 的最近周期是 OOM"
        );
        assert_eq!(
            health.last_exit_code,
            Some(137),
            "退出码取 die 的 137，不被当前运行态 0 覆盖"
        );
        assert_eq!(health.last_terminate_reason.as_deref(), Some("OOMKilled"));
        assert_eq!(health.restart_count, 1);
    }

    /// F4：旧 CID 的 die + 新 CID 的首次 start 不是一次重启。
    #[test]
    fn same_name_new_cid_start_is_not_restart() {
        let events = [
            raw("start", 3, "cid-new", None),
            raw("die", 2, "cid-old", Some(0)),
        ];
        let health = docker_health(None, &events.iter().collect::<Vec<_>>());
        assert_eq!(health.restart_count, 0, "跨 CID 不构成恢复对");
    }

    /// 第三轮 #1：kill 不构成实际退出——OOM 恢复后一个不致命的 kill
    /// （如 SIGHUP）不得清掉最近 OOM 归因。倒序：kill(3) start(2) die(1,137) oom(0)。
    #[test]
    fn non_fatal_kill_does_not_reset_oom_attribution() {
        let events = [
            raw("kill", 4, "c1", None),
            raw("start", 3, "c1", None),
            raw("die", 2, "c1", Some(137)),
            raw("oom", 1, "c1", None),
        ];
        let state = ContainerState {
            oom_killed: Some(false),
            exit_code: Some(0),
            ..Default::default()
        };
        let health = docker_health(Some(&state), &events.iter().collect::<Vec<_>>());
        assert!(
            health.oom_killed_last,
            "die 仍是最近的实际退出，其周期内 oom 有效"
        );
        assert_eq!(health.last_exit_code, Some(137));
    }

    /// 第三轮 #3：状态码判断（真实 Bollard 错误类型，非文本匹配）。
    #[test]
    fn not_found_detection_uses_status_code_only() {
        let not_found = bollard::errors::Error::DockerResponseServerError {
            status_code: 404,
            message: "No such container".to_string(),
        };
        assert!(is_not_found_error(&not_found));
        // 500 的正文含 "not found" —— 不是删除竞态
        let server_500 = bollard::errors::Error::DockerResponseServerError {
            status_code: 500,
            message: "backend said not found".to_string(),
        };
        assert!(
            !is_not_found_error(&server_500),
            "文本含 not found 但状态码 500"
        );
        // 403 正文含 "404" —— 不是删除竞态
        let forbidden = bollard::errors::Error::DockerResponseServerError {
            status_code: 403,
            message: "caller 404-ish token invalid".to_string(),
        };
        assert!(!is_not_found_error(&forbidden), "文本含 404 但状态码 403");
    }

    /// F7：health_status: unhealthy 归 Warning，healthy/starting 归 Normal。
    #[test]
    fn health_status_unhealthy_is_warning() {
        assert_eq!(classify_event("health_status: unhealthy"), "Warning");
        assert_eq!(classify_event("health_status: healthy"), "Normal");
        assert_eq!(classify_event("health_status: starting"), "Normal");
    }

    /// R6：一次策略重启（die,restart,start）只计 1，不是 2；
    /// 纯 stop（die 后无 start）不计。
    #[test]
    fn restart_counts_recovery_cycles_not_events() {
        // 倒序输入（新→旧）：start,restart,die = 一次策略重启
        let policy_restart = [
            raw("start", 3, "c1", None),
            raw("restart", 2, "c1", None),
            raw("die", 1, "c1", None),
        ];
        assert_eq!(
            count_recovery_cycles(&policy_restart.iter().collect::<Vec<_>>()),
            1,
            "die→(restart)→start 是一次重启"
        );
        let manual = [raw("start", 2, "c1", None), raw("die", 1, "c1", None)];
        assert_eq!(count_recovery_cycles(&manual.iter().collect::<Vec<_>>()), 1);
        let stopped = [raw("die", 1, "c1", None)];
        assert_eq!(
            count_recovery_cycles(&stopped.iter().collect::<Vec<_>>()),
            0,
            "正常停止不是重启"
        );
        let twice = [
            raw("start", 5, "c1", None),
            raw("die", 4, "c1", None),
            raw("start", 3, "c1", None),
            raw("die", 2, "c1", None),
        ];
        assert_eq!(count_recovery_cycles(&twice.iter().collect::<Vec<_>>()), 2);
    }
    /// R7 双极性：老 OOM 周期后又普通退出，OOM 不得粘住。
    /// 倒序：start(4) die(3,普通) start(2) oom(1) die(0)——最近周期无 oom。
    #[test]
    fn historical_oom_does_not_become_last_oom() {
        let events = [
            raw("start", 4, "c1", None),
            raw("die", 3, "c1", Some(1)),
            raw("start", 2, "c1", None),
            raw("oom", 1, "c1", None),
            raw("die", 0, "c1", Some(137)),
        ];
        let state = ContainerState {
            oom_killed: Some(false),
            exit_code: Some(0),
            ..Default::default()
        };
        let health = docker_health(Some(&state), &events.iter().collect::<Vec<_>>());
        assert!(
            !health.oom_killed_last,
            "最近周期（die=1 普通）无 oom → 不粘旧 OOM"
        );
        assert_eq!(health.last_terminate_reason.as_deref(), Some("Error(1)"));
    }

    /// R7：exit 137 单独出现不推断 OOM（SIGKILL 不只来自 OOM）。
    #[test]
    fn exit_137_alone_is_not_oom() {
        let events = [raw("die", 1, "c1", Some(137))];
        let state = ContainerState {
            oom_killed: Some(false),
            ..Default::default()
        };
        let health = docker_health(Some(&state), &events.iter().collect::<Vec<_>>());
        assert!(!health.oom_killed_last, "137 ≠ OOM");
        assert_eq!(health.last_terminate_reason.as_deref(), Some("Error(137)"));
    }

    #[test]
    fn classify_event_marks_die_oom_as_warning() {
        assert_eq!(classify_event("die"), "Warning");
        assert_eq!(classify_event("oom"), "Warning");
        assert_eq!(classify_event("start"), "Normal");
        assert_eq!(classify_event("pull"), "Normal");
    }
}
