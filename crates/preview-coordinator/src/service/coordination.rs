use super::*;

#[async_trait::async_trait]
impl PreviewCoordination for PreviewCoordinator {
    async fn start_dev(
        &self,
        req: PreviewStartRequest,
    ) -> Result<PreviewStartEnvelope, PreviewCoordinationError> {
        self.admit_and_start(&req.identity, req.base_path).await
    }

    async fn stop_dev(
        &self,
        req: &PreviewStopRequest,
    ) -> Result<PreviewStopEnvelope, PreviewCoordinationError> {
        let active = self.store.list_active().await.map_err(store_error)?;
        let targets: Vec<PreviewInstanceRecord> = active
            .into_iter()
            .filter(|r| r.project_id == req.project_id)
            .collect();
        if targets.is_empty() {
            return Ok(PreviewStopEnvelope {
                success: true,
                message: "No running process found".to_string(),
                project_id: req.project_id.clone(),
                reason: None,
            });
        }
        let mut all_ok = true;
        for row in targets {
            if let Err(error) = self.coordinated_stop(&row).await {
                tracing::warn!(
                    preview_key = %row.preview_key,
                    "coordinated stop failed: {error}"
                );
                all_ok = false;
            }
        }
        Ok(PreviewStopEnvelope {
            success: true,
            message: if all_ok {
                "Stopped".to_string()
            } else {
                "Partially stopped but continue execution".to_string()
            },
            project_id: req.project_id.clone(),
            reason: None,
        })
    }

    async fn restart_dev(
        &self,
        req: PreviewRestartRequest,
    ) -> Result<PreviewRestartEnvelope, PreviewCoordinationError> {
        let active = self.store.list_active().await.map_err(store_error)?;
        let targets: Vec<PreviewInstanceRecord> = active
            .into_iter()
            .filter(|r| r.project_id == req.identity.project_id)
            .collect();
        let mut stop_all_ok = true;
        for row in targets {
            if let Err(error) = self.coordinated_stop(&row).await {
                tracing::warn!(preview_key = %row.preview_key, "restart stop phase: {error}");
                stop_all_ok = false;
            }
        }
        if !stop_all_ok {
            return Err(unavailable(
                "restart stop phase failed on one or more instances",
            ));
        }
        let env = self.admit_and_start(&req.identity, req.base_path).await?;
        Ok(PreviewRestartEnvelope {
            success: true,
            message: "Development server restart successfully".to_string(),
            project_id: env.project_id,
            pid: env.pid,
            port: env.port,
        })
    }

    async fn keep_alive_dev(
        &self,
        req: &PreviewKeepAliveRequest,
    ) -> Result<PreviewKeepAliveEnvelope, PreviewCoordinationError> {
        let row = self.locate_by_port(req.port).await?;
        let Some(row) = row else {
            // 无活跃实例（旧语义=探活失败重建）→ 统一受理重建
            let env = self
                .admit_and_start(&req.identity, req.base_path.clone())
                .await?;
            return Ok(Self::rebuilt_envelope(env));
        };
        if row.project_id != req.identity.project_id {
            // 端口命中他人实例（复用/身份不符）：不动作、不重建
            return Ok(Self::degraded(
                &req.identity.project_id,
                degraded_reason::PORT_MISMATCH,
                "preview port belongs to another project",
            ));
        }
        match row.state {
            PreviewInstanceState::Ready if self.heartbeat_fresh(&row) => {
                self.activity
                    .record(&row.preview_key, &row.instance_id, chrono::Utc::now());
                Ok(PreviewKeepAliveEnvelope {
                    success: true,
                    message: "Development server is alive".to_string(),
                    project_id: row.project_id.clone(),
                    pid: row.pid,
                    port: row.port,
                    action: None,
                    reason: None,
                })
            }
            PreviewInstanceState::Ready => {
                // 心跳陈旧：回环 verify 宿主
                match self.verify_via_host(&row).await {
                    Ok(report) if report.identity_match && report.alive => {
                        note_store_result(
                            self.store
                                .refresh_heartbeat(&row.preview_key, &row.instance_id)
                                .await,
                        );
                        self.activity.record(
                            &row.preview_key,
                            &row.instance_id,
                            chrono::Utc::now(),
                        );
                        Ok(PreviewKeepAliveEnvelope {
                            success: true,
                            message: "Development server is alive".to_string(),
                            project_id: row.project_id.clone(),
                            pid: row.pid,
                            port: row.port,
                            action: None,
                            reason: None,
                        })
                    }
                    Ok(report) if report.identity_match => {
                        // 宿主确认死：收敛死实例后统一受理重建
                        note_store_result(
                            self.store
                                .mark_failed(
                                    &row.preview_key,
                                    &row.instance_id,
                                    row.revision,
                                    "host verified process dead",
                                )
                                .await,
                        );
                        let env = self
                            .admit_and_start(&req.identity, req.base_path.clone())
                            .await?;
                        Ok(Self::rebuilt_envelope(env))
                    }
                    Ok(_) | Err(_) if self.settle_if_host_absent(&row).await? => {
                        let env = self
                            .admit_and_start(&req.identity, req.base_path.clone())
                            .await?;
                        Ok(Self::rebuilt_envelope(env))
                    }
                    Ok(_) => Ok(Self::degraded(
                        &req.identity.project_id,
                        degraded_reason::INSTANCE_UNKNOWN,
                        "preview host registration mismatch",
                    )),
                    Err(_) => Ok(Self::degraded(
                        &req.identity.project_id,
                        degraded_reason::HOST_UNAVAILABLE,
                        "preview host unreachable",
                    )),
                }
            }
            PreviewInstanceState::Starting => Err(conflict(
                "project dev server is already starting, please wait",
            )),
            PreviewInstanceState::Stopping => {
                if self.settle_if_host_absent(&row).await? {
                    let env = self
                        .admit_and_start(&req.identity, req.base_path.clone())
                        .await?;
                    Ok(Self::rebuilt_envelope(env))
                } else {
                    Ok(Self::degraded(
                        &req.identity.project_id,
                        degraded_reason::INSTANCE_UNKNOWN,
                        "preview instance is stopping; retry on next keep-alive",
                    ))
                }
            }
            PreviewInstanceState::Unknown => match self.recovery_evidence(&row).await {
                Some(evidence) => {
                    note_store_result(
                        self.store
                            .resolve_unknown_stopped(&row.preview_key, &row.instance_id, &evidence)
                            .await,
                    );
                    let env = self
                        .admit_and_start(&req.identity, req.base_path.clone())
                        .await?;
                    Ok(Self::rebuilt_envelope(env))
                }
                None => Ok(Self::degraded(
                    &req.identity.project_id,
                    degraded_reason::INSTANCE_UNKNOWN,
                    "preview instance state unknown and host pod still exists",
                )),
            },
            PreviewInstanceState::Stopped | PreviewInstanceState::Failed => {
                // 终态（旧语义=探活失败重建）→ 统一受理重建
                let env = self
                    .admit_and_start(&req.identity, req.base_path.clone())
                    .await?;
                Ok(Self::rebuilt_envelope(env))
            }
        }
    }

    async fn list_dev(&self) -> Result<Vec<PreviewListEntry>, PreviewCoordinationError> {
        let rows = self.store.list_active().await.map_err(store_error)?;
        Ok(rows
            .into_iter()
            .map(|row| PreviewListEntry {
                project_id: row.project_id.clone(),
                pid: row.pid,
                port: row.port,
                state: format!("{:?}", row.state).to_lowercase(),
                host: row.pod_name.clone().or_else(|| Some(row.host_id.clone())),
                started_at: None,
            })
            .collect())
    }

    async fn read_dev_log(
        &self,
        project_id: &str,
        log_type: &str,
        start_index: usize,
    ) -> Result<ExecutorLogChunk, PreviewCoordinationError> {
        let rows = self.store.list_active().await.map_err(store_error)?;
        let Some(row) = rows.into_iter().find(|r| r.project_id == project_id) else {
            return Ok(ExecutorLogChunk {
                logs: Vec::new(),
                total_lines: 0,
                log_file_name: String::new(),
            });
        };
        if row.host_id == self.host.host_id {
            return self
                .executor
                .read_log_local(project_id, log_type, start_index)
                .await
                .map_err(|e| unavailable(e.to_string()));
        }
        let Some(pod_ip) = row.pod_ip.clone() else {
            return Err(unavailable(format!(
                "log target host {} lacks pod ip",
                row.host_id
            )));
        };
        self.dispatch
            .remote_log(&pod_ip, project_id, log_type, start_index)
            .await
            .map_err(unavailable)
    }

    async fn port_pool_status(&self) -> Result<PreviewPortPoolStatus, PreviewCoordinationError> {
        let rows = self.store.list_active().await.map_err(store_error)?;
        let allocations: Vec<PreviewPortAllocation> = rows
            .iter()
            .filter_map(|r| {
                r.port.map(|port| PreviewPortAllocation {
                    project_id: r.project_id.clone(),
                    port,
                })
            })
            .collect();
        Ok(PreviewPortPoolStatus {
            port_range: format!("{}-{}", PREVIEW_PORT_MIN, PREVIEW_PORT_MAX),
            total_allocated: allocations.len(),
            allocations,
        })
    }

    async fn resolve_route(&self, port: u16) -> PreviewRouteResolution {
        if !is_preview_port(port) {
            return PreviewRouteResolution::NotPreview;
        }
        if let Some(cached) = self.route_cache.get(port) {
            return cached;
        }
        match self.store.find_active_by_port(port).await {
            Ok(Some(row)) => {
                let resolution = if row.host_id == self.host.host_id {
                    PreviewRouteResolution::Local {
                        instance_id: row.instance_id.clone(),
                        port,
                    }
                } else if let Some(host_ip) = row.pod_ip.clone() {
                    PreviewRouteResolution::Forward {
                        instance_id: row.instance_id.clone(),
                        port,
                        host_ip,
                    }
                } else {
                    // 远端宿主行缺 pod_ip（异常/单机形态）：降级 legacy，不缓存
                    return PreviewRouteResolution::Unavailable;
                };
                self.route_cache.put_positive(port, resolution.clone());
                resolution
            }
            Ok(None) => {
                self.route_cache.put_negative(port);
                PreviewRouteResolution::NotPreview
            }
            Err(error) => {
                // 权威库不可用：不缓存，降级 legacy（不劣于现状）
                tracing::warn!(port, "preview route resolve degraded: {error}");
                PreviewRouteResolution::Unavailable
            }
        }
    }

    async fn invalidate_route(&self, port: u16) {
        self.route_cache.invalidate(port);
    }

    async fn check_forward(&self, instance_id: &str, port: u16) -> PreviewForwardCheck {
        let Ok(Some(row)) = self.store.find_active_by_port(port).await else {
            // 权威库错误或实例不存在：410（调用方重解析后负缓存/NotFound 收敛）
            return PreviewForwardCheck::IdentityMismatch;
        };
        if row.instance_id != instance_id
            || row.port != Some(port)
            || row.host_id != self.host.host_id
        {
            return PreviewForwardCheck::IdentityMismatch;
        }
        // 登记匹配即放行（纯内存查找，无探活——存活是心跳轮询的职责；
        // 转发到已死 vite 得到连接拒绝，语义与现状一致）。
        match self
            .executor
            .registration_matches(&row.preview_key, &row.instance_id)
            .await
        {
            Ok(true) => PreviewForwardCheck::Allowed,
            _ => PreviewForwardCheck::NotReady,
        }
    }

    async fn internal_stop(
        &self,
        preview_key: &str,
        instance_id: &str,
        operation_id: &str,
        revision: i64,
    ) -> Result<ExecutorStopOutcome, PreviewCoordinationError> {
        let row = self.store.get(preview_key).await.map_err(store_error)?;
        let authorized = row.is_some_and(|row| {
            row.instance_id == instance_id
                && row.operation_id == operation_id
                && row.revision == revision
                && row.state == PreviewInstanceState::Stopping
                && row.host_id == self.host.host_id
        });
        if !authorized {
            return Ok(ExecutorStopOutcome::IdentityMismatch);
        }
        self.executor
            .stop_local(preview_key, instance_id)
            .await
            .map_err(|e| unavailable(e.to_string()))
    }

    async fn internal_verify(
        &self,
        preview_key: &str,
        instance_id: &str,
    ) -> Result<ExecutorVerifyReport, PreviewCoordinationError> {
        self.executor
            .verify_local(preview_key, instance_id)
            .await
            .map_err(|e| unavailable(e.to_string()))
    }
}
