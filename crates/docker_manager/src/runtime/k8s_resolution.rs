//! 契约四：分层解析器（注册表零包袱方案 §2 契约四）。
//!
//! 公共层只做**候选发现 + 身份验证**：label 候选（类型分流 selector 单一
//! 事实源）→ workload 身份核验（ownerReference 派生 workload 名/UID）→
//! 冲突检测（多 workload 候选不自动择一）。**Ready 策略由调用方应用**：
//! 诊断（必须能返回 Pending/CrashLoopBackOff/未 Ready）、业务就绪、停止与
//! 缺席确认各自判定。
//!
//! 发现与缺席确认分离：selector 空集 ≠ 缺席。缺席必须有证据——owner 对象
//! 404，或调用方按**已捕获的物理 Pod 名+UID** 复核（`confirm_builder_
//! compute_absent` 的既有序列）；本层在候选为空时顺带按 STS 规范名 GET
//! 兜底（"标签缺失/漂移不能把存在的 Pod 藏起来"），仍空才查 owner 对象
//! 归类 `Absent` / `WorkloadWithoutPod`。
use k8s_openapi::api::core::v1::Pod;
use kube::api::ListParams;
use shared_types::ServiceType;

use super::KubernetesRuntime;
use super::k8s_pod::K8sPodOps;

/// 分层解析结果。`Unknown` 永不授权清理/接管/退休（处置表末行语义）。
#[derive(Debug)]
pub(crate) enum PodResolution {
    /// 找到唯一 workload 候选（附 pod 实体：诊断类调用方需要容器状态细节；
    /// Ready 判定由调用方按 `info.status` 应用策略）。
    Present {
        info: Box<container_runtime_api::RuntimeContainerInfo>,
        pod: Box<Pod>,
    },
    /// 候选与 owner 对象均确认不存在（404 证据）。
    Absent { evidence: String },
    /// 无候选 Pod，但 owner workload 对象仍在——重建中/标签漂移，不判停、
    /// 不判缺席（处置表"Pod 暂缺、workload 期望运行"行）。
    WorkloadWithoutPod { workload_name: String },
    /// 多候选身份核验失败（不同 workload_uid 并存等）——不自动择一。
    Conflict { reason: String },
    /// API 错误/观察不完整——结果未知。
    Unknown { reason: String },
}

impl KubernetesRuntime {
    /// 按 identifier + service_type 解析当前 Pod 归属。见模块文档。
    pub(crate) async fn resolve_pod(
        &self,
        identifier: &str,
        service_type: &ServiceType,
    ) -> PodResolution {
        // 1) label 候选发现（类型分流 selector；错误即 Unknown——不吞）。
        let mut candidates: Vec<Pod> = Vec::new();
        for selector in pod_label_selectors(identifier, service_type) {
            let pods = match self
                .pods()
                .list(&ListParams::default().labels(&selector))
                .await
            {
                Ok(pods) => pods.items,
                Err(error) => {
                    return PodResolution::Unknown {
                        reason: format!("list pods with selector {selector}: {error}"),
                    };
                }
            };
            if !pods.is_empty() {
                candidates = pods;
                break;
            }
        }

        // 2) 候选身份核验：同族 selector 下不同 workload_uid 并存 = 冲突
        //（STS 单副本正常只有 0/1 个；Deployment 族 workload_uid 恒 None
        // 不参与该判据，其换代由部署捕获侧模板令牌核验）。
        if !candidates.is_empty() {
            let mut distinct_workloads: Vec<String> = Vec::new();
            for pod in &candidates {
                if let Some(uid) = Self::workload_uid_from_pod_owner(&pod.metadata)
                    && !distinct_workloads.iter().any(|seen| seen == &uid)
                {
                    distinct_workloads.push(uid);
                }
            }
            if distinct_workloads.len() > 1 {
                return PodResolution::Conflict {
                    reason: format!(
                        "multiple workloads match {identifier} ({service_type}): {}",
                        distinct_workloads.join(", ")
                    ),
                };
            }
        }
        if let Some(best) = pick_candidate(candidates) {
            let info = Self::runtime_info_from_pod(&best);
            return PodResolution::Present {
                info: Box::new(info),
                pod: Box::new(best),
            };
        }

        // 3) 无 label 候选：按 STS 规范名兜底 GET（标签漂移保护；Userapp
        //    Deployment 的 pod 名含 hash，派生名无意义，跳过）。
        if *service_type != ServiceType::Userapp {
            let pod_name = match self.agent_pod_name(identifier, service_type) {
                Ok(name) => name,
                Err(error) => {
                    return PodResolution::Unknown {
                        reason: format!("derive canonical pod name: {error}"),
                    };
                }
            };
            match self.pods().get(&pod_name).await {
                Ok(pod) => {
                    let info = Self::runtime_info_from_pod(&pod);
                    return PodResolution::Present {
                        info: Box::new(info),
                        pod: Box::new(pod),
                    };
                }
                Err(kube::Error::Api(error)) if error.code == 404 => {}
                Err(error) => {
                    return PodResolution::Unknown {
                        reason: format!("get pod {pod_name}: {error}"),
                    };
                }
            }
        }

        // 4) 缺席证据：owner workload 对象 404 = Absent；对象仍在 =
        //    WorkloadWithoutPod（重建中，不判停）。
        match *service_type {
            ServiceType::Userapp => {
                let name = self.app_deployment_name(identifier);
                match self.deployments_api().get(&name).await {
                    Ok(_) => PodResolution::WorkloadWithoutPod {
                        workload_name: name,
                    },
                    Err(kube::Error::Api(error)) if error.code == 404 => PodResolution::Absent {
                        evidence: format!("deployment {name} not found"),
                    },
                    Err(error) => PodResolution::Unknown {
                        reason: format!("get deployment {name}: {error}"),
                    },
                }
            }
            _ => {
                let name = match self.pod_name(identifier, service_type) {
                    Ok(name) => name,
                    Err(error) => {
                        return PodResolution::Unknown {
                            reason: format!("derive workload name: {error}"),
                        };
                    }
                };
                match self.statefulsets().get(&name).await {
                    Ok(_) => PodResolution::WorkloadWithoutPod {
                        workload_name: name,
                    },
                    Err(kube::Error::Api(error)) if error.code == 404 => PodResolution::Absent {
                        evidence: format!("statefulset {name} not found"),
                    },
                    Err(error) => PodResolution::Unknown {
                        reason: format!("get statefulset {name}: {error}"),
                    },
                }
            }
        }
    }
}

/// 候选择优：Running 优先，其次最新创建（确定性：时间戳并列时 max_by 保序
/// 取较先入列者——候选来自同一 selector list，顺序由 API server 决定）。
fn pick_candidate(candidates: Vec<Pod>) -> Option<Pod> {
    candidates
        .into_iter()
        .max_by(|a, b| candidate_rank(b).cmp(&candidate_rank(a)))
}

/// Ready 策略在此层只做择优排序用途：Running > Pending > 其余；真正的
/// 业务 Ready 判定由调用方应用（契约四分层）。
fn candidate_rank(pod: &Pod) -> (u8, i64) {
    use container_runtime_api::ContainerRuntimeStatus;
    let status_rank = match KubernetesRuntime::extract_pod_status(pod) {
        ContainerRuntimeStatus::Running => 2,
        ContainerRuntimeStatus::Pending => 1,
        _ => 0,
    };
    let created = pod
        .metadata
        .creation_timestamp
        .as_ref()
        .map(|ts| ts.0.as_second())
        .unwrap_or(0);
    (status_rank, created)
}

/// pod 定位 label selector 候选（单一事实源：发现层与既有 label 查询共用）。
///
/// 生产 UserApp Deployment 与 agent/builder STS 族的标签体系不同——两族共享
/// `app.kubernetes.io/instance={id}` 键（builder 与生产同 app_id 时同值），仅凭
/// 它无法分流；单键 + limit(1) 时生产 pod（字典序排前）会被稳定捞走，以 inspect
/// 真实值污染 builder 注册表（app 23 事故）。故按 service_type 拼双键：
/// - Userapp（生产 Deployment，标签由 app_manager `build_app_labels` 写入）：
///   instance + managed-by=rcoder-app-manager
/// - 其余（STS 族，标签由 `build_standard_labels` 写入，恒带
///   rcoder.io/service-type）：instance + rcoder.io/service-type
///
/// 第二候选取各族的 rcoder.io 专属键（identifier vs app-id），同样带类型维度。
pub(crate) fn pod_label_selectors(identifier: &str, service_type: &ServiceType) -> Vec<String> {
    // 穷尽列举（09-19 教训：_ 通配臂关闭编译器穷尽检查，新增变体静默漏接）；
    // STS 族四变体同臂（家族归一在臂内执行），Userapp 专属标签集单臂
    match service_type {
        ServiceType::Userapp => vec![
            format!(
                "app.kubernetes.io/instance={identifier},app.kubernetes.io/managed-by={}",
                super::k8s_deployment::APP_MANAGED_BY
            ),
            format!(
                "rcoder.io/app-id={identifier},app.kubernetes.io/managed-by={}",
                super::k8s_deployment::APP_MANAGED_BY
            ),
        ],
        ServiceType::WebAgentRunner
        | ServiceType::ComputerAgentRunner
        | ServiceType::ComputerNormalProject
        | ServiceType::UserappBuilder => {
            // 家族归一：label 由创建侧写家族值（常规项目与 Computer 同容器同 label），
            // selector 必须用家族值才能命中既有 STS/Pod，否则会误判不存在而重建
            let family = service_type.container_family_key();
            vec![
                format!("app.kubernetes.io/instance={identifier},rcoder.io/service-type={family}"),
                format!("rcoder.io/identifier={identifier},rcoder.io/service-type={family}"),
            ]
        }
    }
}

#[cfg(test)]
mod label_selector_tests {
    use super::pod_label_selectors;
    use shared_types::ServiceType;

    /// Userapp 走 managed-by 维度（生产 Deployment 无 rcoder.io/service-type 标签），
    /// 且两个候选都必须含 managed-by=rcoder-app-manager——这是把生产 pod 从
    /// builder 查询里分流出去的决定性维度。
    #[test]
    fn userapp_selectors_use_app_manager_dimension() {
        let selectors = pod_label_selectors("23", &ServiceType::Userapp);
        assert_eq!(selectors.len(), 2);
        assert!(
            selectors
                .iter()
                .all(|s| !s.contains("rcoder.io/service-type"))
        );
        assert!(
            selectors
                .iter()
                .all(|s| s.contains("managed-by=rcoder-app-manager"))
        );
        assert!(selectors[0].contains("app.kubernetes.io/instance=23"));
        assert!(selectors[1].contains("rcoder.io/app-id=23"));
    }

    /// STS 族（含 UserappBuilder）带 rcoder.io/service-type 维度——builder 与生产
    /// Deployment 同 app_id 共享 instance 键，service-type 是唯一分键。
    #[test]
    fn sts_family_selectors_carry_service_type_dimension() {
        for st in [
            ServiceType::UserappBuilder,
            ServiceType::WebAgentRunner,
            ServiceType::ComputerAgentRunner,
        ] {
            let selectors = pod_label_selectors("42", &st);
            assert_eq!(selectors.len(), 2, "{st}");
            assert!(
                selectors
                    .iter()
                    .all(|s| s.contains(&format!("rcoder.io/service-type={st}"))),
                "{st}"
            );
            assert!(selectors[0].contains("app.kubernetes.io/instance=42"));
            assert!(selectors[1].contains("rcoder.io/identifier=42"));
        }
    }

    /// 常规项目（ComputerNormalProject）查询的 selector 恒用族代表词——
    /// label 写入侧（build_standard_labels）就是族值，本义查询必须归一命中，
    /// 否则 NormalProject 会话会误判容器不存在而重建。
    #[test]
    fn normal_project_selectors_use_family_key() {
        for st in [
            ServiceType::ComputerNormalProject,
            ServiceType::ComputerAgentRunner,
        ] {
            let selectors = pod_label_selectors("42", &st);
            assert!(
                selectors
                    .iter()
                    .all(|s| s.contains("rcoder.io/service-type=computer-agent-runner")),
                "{st} selector 必须用族代表词"
            );
        }
    }
}
