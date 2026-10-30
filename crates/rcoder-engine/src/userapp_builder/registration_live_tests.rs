//! F1 现有入口行为验证（任务书 specs/idle-recovery-followup-20261030 §4.1–4.3）。
//!
//! 真实共享 PG（projects 与 userapp 控制库同库 → `requires_durable_confirmation`
//! 为真）× 隔离个人 K8s 命名空间里的活体 builder。按“先验证，不预设新增接口”
//! 的顺序，先记录既有 Ensure / Adopt 在缺 canonical 登记时的真实行为：
//! 回执三场景（当前 Pod 回执 / 仅旧 Pod 回执 / 无回执）能否经原入口收敛、
//! 是否保持原 Pod/controller/PVC、以及 ensure 对已停计算的唤醒副作用。
//!
//! 本模块与 docker_manager `k8s_builder_control/live_tests.rs` 同范式：
//! `#[ignore]` + 显式环境变量，绝不进入默认门禁。所需环境：
//! - `RCODER_PG_TEST_DSN`：本轮一次性 PG（测试内再按用例建独立 database）。
//! - `RCODER_F1_LAB_NAMESPACE`：专用隔离命名空间，强制 `rcoder-f1-` 前缀。
//! - `KUBECONFIG`：指向该集群（或进程内 in-cluster）。
//! - 可选 `RCODER_F1_LAB_IMAGE`（默认 busybox:1.36，节点本地已存在）。
//! - 可选 `RCODER_F1_LAB_STORAGE_CLASS`（默认 ceph-rbd）。
use super::*;
use k8s_openapi::api::apps::v1::StatefulSet;
use k8s_openapi::api::core::v1::{Namespace, PersistentVolumeClaim, Pod};
use kube::api::{Api, DeleteParams, PostParams, Preconditions};
use shared_types::{
    AdoptBuilderRequest, BuilderControlResult, ServiceType, UserAppExecutionContext,
    UserAppResourceBinding,
};

const STS_PREFIX: &str = "rcoder-app-builder-";
const LAB_NS_PREFIX: &str = "rcoder-f1-";
const BUILDER_PORT: u16 = 60000;

struct Lab {
    namespace: String,
    image: String,
    storage_class: String,
    dsn: String,
}

fn lab_config() -> Lab {
    // 实验室诊断输出：RUST_LOG 控制级别（如 rcoder_engine=debug）。
    let _subscriber = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_test_writer()
        .try_init();
    let namespace = std::env::var("RCODER_F1_LAB_NAMESPACE")
        .expect("RCODER_F1_LAB_NAMESPACE (dedicated rcoder-f1-* namespace) is required");
    assert!(
        namespace.starts_with(LAB_NS_PREFIX),
        "use a dedicated {LAB_NS_PREFIX}* namespace"
    );
    let dsn = std::env::var("RCODER_PG_TEST_DSN")
        .expect("RCODER_PG_TEST_DSN is required for the shared-registry lab contract");
    assert!(!dsn.is_empty());
    Lab {
        namespace,
        // 节点本地已有（含 python3 与 busybox）：凭据读取在容器内 exec
        // `python3`，并要求 60000 端口监听进程的可执行名是 agent_runner。
        image: std::env::var("RCODER_F1_LAB_IMAGE").unwrap_or_else(|_| {
            "swr.cn-north-4.myhuaweicloud.com/ddn-k8s/docker.io/library/python:3-alpine".into()
        }),
        storage_class: std::env::var("RCODER_F1_LAB_STORAGE_CLASS")
            .unwrap_or_else(|_| "ceph-rbd".into()),
        dsn,
    }
}

/// DSN 的 server 段（去掉库名路径）。
fn server_base(dsn: &str) -> String {
    let trimmed = dsn.trim_end_matches('/');
    trimmed
        .rsplit_once('/')
        .expect("DSN with a database path")
        .0
        .to_string()
}

/// 每个用例独占一个 database：本轮自己的 PG 实例内新建，互不复用。
async fn isolated_database(base_dsn: &str) -> String {
    let name = format!("f1t{}", &uuid::Uuid::new_v4().simple().to_string()[..20]);
    let mut admin = toasty::Db::builder().connect(base_dsn).await.unwrap();
    toasty::sql::statement(format!("CREATE DATABASE {name}"))
        .exec(&mut admin)
        .await
        .unwrap();
    format!("{}/{}", server_base(base_dsn), name)
}

fn builder_pod_running(pod: &Pod) -> bool {
    pod.status.as_ref().is_some_and(|status| {
        status.phase.as_deref() == Some("Running")
            && status.container_statuses.as_ref().is_some_and(|statuses| {
                statuses
                    .iter()
                    .any(|cs| cs.state.as_ref().is_some_and(|s| s.running.is_some()))
            })
    })
}

struct Fixture {
    state: Arc<AppState>,
    store: Arc<rcoder_storage::userapp_lifecycle::TursoUserAppStore>,
    projects: Arc<rcoder_storage::pg::PgStore>,
    client: kube::Client,
    namespace: String,
    app_id: String,
    lifecycle_id: String,
    sts_name: String,
    pod_name: String,
    pvc_name: String,
    _database: String,
    server_dsn: String,
}

struct PhysicalIdentity {
    sts_uid: String,
    pod_uid: String,
    pvc_uid: String,
}

impl Fixture {
    /// 组装共享形态 AppState：projects=PgStore、控制库=同库 PG、
    /// `userapp_storage.postgres=None`（`shares_connection_scope` 短路为真）。
    /// 不通过配置假 UserApp DSN 绕开共享分支。
    async fn new(lab: &Lab, tag: &str) -> Self {
        rustls::crypto::ring::default_provider()
            .install_default()
            .map_err(|_| anyhow!("test TLS bootstrap: crypto provider is already installed"))
            .unwrap();
        // 运行时构造读取的 RCODER_K8S_* / KUBECONFIG 由调用命令行注入；
        // 这里只断言指向本实验室，绝不修改进程环境。
        let runtime_namespace =
            std::env::var("RCODER_K8S_NAMESPACE").expect("RCODER_K8S_NAMESPACE");
        assert_eq!(
            runtime_namespace, lab.namespace,
            "RCODER_K8S_NAMESPACE must point at the isolated lab namespace"
        );
        let database = isolated_database(&lab.dsn).await;
        let config = rcoder_storage::config::PostgresConfig {
            url: Some(database.clone()),
            ..Default::default()
        };
        let store = Arc::new(
            rcoder_storage::userapp_lifecycle::TursoUserAppStore::connect(&config)
                .await
                .unwrap(),
        );
        let runtime = Arc::new(
            docker_manager::runtime::KubernetesRuntime::new(Default::default())
                .await
                .unwrap(),
        );
        let mut state = (*builder_runtime_test_state(runtime, store.clone(), None).await).clone();
        let projects = Arc::new(
            rcoder_storage::pg::PgStore::connect(
                &config,
                lab.namespace.clone(),
                "cluster.local".into(),
            )
            .await
            .unwrap(),
        );
        state.projects = Arc::new(rcoder_storage::ProjectStoreBackend::Postgres(
            projects.clone(),
        ));
        state.config.userapp_storage.postgres = None;
        state.config.userapp_storage.ensure_timeout_seconds = 180;
        let state = Arc::new(state);
        let client = kube::Client::try_default().await.unwrap();

        let app_id = format!(
            "f1{tag}{}",
            &uuid::Uuid::new_v4().simple().to_string()[..10]
        );
        let lifecycle = state.userapp_store.ensure_identity(&app_id).await.unwrap();
        let sts_name = format!("{STS_PREFIX}{app_id}");
        let pod_name = format!("{sts_name}-0");
        let pvc_name = format!("f1ws-{app_id}");
        let fixture = Self {
            state,
            store,
            projects,
            client,
            namespace: lab.namespace.clone(),
            app_id,
            lifecycle_id: lifecycle.lifecycle_id.clone(),
            sts_name,
            pod_name,
            pvc_name,
            _database: database,
            server_dsn: server_base(&lab.dsn),
        };
        fixture.assert_namespace_isolated(&lab.namespace).await;
        fixture
    }

    async fn assert_namespace_isolated(&self, namespace: &str) {
        let api: Api<Namespace> = Api::all(self.client.clone());
        let ns = api.get(namespace).await.unwrap();
        // 只允许本实验室命名空间：任何不属于 rcoder-f1-* 的现网命名空间都不碰。
        assert!(ns.metadata.name.as_deref() == Some(namespace));
    }

    /// 用与生产一致的盖章（rcoder.io/application-id / lifecycle-id 等）创建
    /// PVC + StatefulSet（stub 容器在 60000 端口答复 /api/version 探针）。
    async fn create_live_builder(&self, lab: &Lab) -> PhysicalIdentity {
        let context = UserAppExecutionContext {
            app_id: self.app_id.clone(),
            lifecycle_id: self.lifecycle_id.clone(),
            operation_id: format!(
                "f1origin-{}",
                &uuid::Uuid::new_v4().simple().to_string()[..8]
            ),
            executor_id: "f1-origin-executor".into(),
            request_fingerprint: "f".repeat(64),
        };
        let pvcs: Api<PersistentVolumeClaim> =
            Api::namespaced(self.client.clone(), &self.namespace);
        let mut claim: PersistentVolumeClaim = serde_json::from_value(serde_json::json!({
            "apiVersion":"v1","kind":"PersistentVolumeClaim",
            "metadata":{"name":self.pvc_name,"annotations":context.resource_metadata()},
            "spec":{"accessModes":["ReadWriteOnce"],"storageClassName":lab.storage_class,
                "resources":{"requests":{"storage":"1Gi"}}}
        }))
        .unwrap();
        claim
            .metadata
            .annotations
            .as_mut()
            .unwrap()
            .insert("f1-lab-sentinel".into(), "owned-by-f1-lab".into());
        let claim = pvcs.create(&PostParams::default(), &claim).await.unwrap();
        let pvc_uid = claim.metadata.uid.clone().unwrap();
        // 生产标准标签全集（k8s_service::build_standard_labels 同源）：
        // expected_scope / 家族解析按这些值判定归属。
        let labels = serde_json::json!({
            "app.kubernetes.io/name": "user-app-builder",
            "app.kubernetes.io/instance": self.app_id,
            "app.kubernetes.io/version": "v1",
            "app.kubernetes.io/component": "agent",
            "app.kubernetes.io/managed-by": "rcoder-runtime",
            "app.kubernetes.io/part-of": "rcoder",
            "rcoder.io/service-type": "user-app-builder",
            "rcoder.io/identifier": self.app_id,
            "f1-lab-owner": self.app_id,
        });
        // 与生产 builder 完全一致的工作区视图（verify_existing_builder_views
        // 逐项核对该形态）：workspace 卷按 subPath 挂 4 处。stub 用 busybox
        // httpd 直接服务 data 卷——/api/version 供就绪探针，/sentinel 作数据
        // 连续性证据（首启带存在守卫写入，Pod 换代后仍可读到原值）。
        let data_dir = shared_types::paths::USERAPP_DEV_DATA;
        let home_mount = format!("{}/{}", shared_types::paths::USERAPP_DEV_HOME, self.app_id);
        let boot = format!(
            "mkdir -p {data_dir}/api {home_mount} {logs} {store}; \
             [ -e {data_dir}/sentinel ] || printf f1-preserved > {data_dir}/sentinel; \
             [ -e {data_dir}/api/version ] || printf ok > {data_dir}/api/version; \
             cp /usr/local/bin/python3 /tmp/agent_runner; \
             exec /tmp/agent_runner -m http.server {port} --directory {data_dir}",
            logs = shared_types::paths::USERAPP_DEV_LOGS,
            store = shared_types::paths::USERAPP_DEV_AGENT_STORE,
            port = BUILDER_PORT,
        );
        let mounts = serde_json::json!([
            {"name":"workspace","mountPath":home_mount,"subPath":self.app_id},
            {"name":"workspace","mountPath":data_dir,"subPath":"data"},
            {"name":"workspace","mountPath":shared_types::paths::USERAPP_DEV_LOGS,"subPath":"logs"},
            {"name":"workspace","mountPath":shared_types::paths::USERAPP_DEV_AGENT_STORE,"subPath":"agent-store"}
        ]);
        let sts: StatefulSet = serde_json::from_value(serde_json::json!({
            "apiVersion":"apps/v1","kind":"StatefulSet",
            "metadata":{"name":self.sts_name,"labels":labels,"annotations":context.resource_metadata()},
            "spec":{"replicas":1,"serviceName":self.sts_name,
                "selector":{"matchLabels":{"f1-lab-owner":self.app_id}},
                "template":{"metadata":{"labels":labels,"annotations":context.resource_metadata()},
                    "spec":{"terminationGracePeriodSeconds":3,"containers":[{
                        "name":"agent","image":lab.image,"imagePullPolicy":"IfNotPresent",
                        "command":["sh","-c",boot],
                        "ports":[{"containerPort":BUILDER_PORT}],
                        "volumeMounts":mounts}],
                    "volumes":[{"name":"workspace","persistentVolumeClaim":{"claimName":self.pvc_name}}]}}}
        }))
        .unwrap();
        let workloads: Api<StatefulSet> = Api::namespaced(self.client.clone(), &self.namespace);
        let created = workloads
            .create(&PostParams::default(), &sts)
            .await
            .unwrap();
        let sts_uid = created.metadata.uid.clone().unwrap();
        self.wait_pod_running().await;
        let pod_uid = self.current_pod().await.metadata.uid.clone().unwrap();
        PhysicalIdentity {
            sts_uid,
            pod_uid,
            pvc_uid,
        }
    }

    async fn wait_pod_running(&self) {
        let pods: Api<Pod> = Api::namespaced(self.client.clone(), &self.namespace);
        tokio::time::timeout(std::time::Duration::from_secs(120), async {
            loop {
                if let Some(pod) = pods.get_opt(&self.pod_name).await.unwrap() {
                    if builder_pod_running(&pod) {
                        return;
                    }
                    if pod.status.as_ref().and_then(|s| s.phase.as_deref()) == Some("Failed")
                        || pod
                            .status
                            .as_ref()
                            .and_then(|s| s.reason.clone())
                            .is_some_and(|r| r.contains("Evicted"))
                    {
                        panic!("builder stub Pod failed: {:?}", pod.status);
                    }
                }
                tokio::time::sleep(std::time::Duration::from_millis(400)).await;
            }
        })
        .await
        .expect("builder stub Pod to become Running");
    }

    async fn current_pod(&self) -> Pod {
        let pods: Api<Pod> = Api::namespaced(self.client.clone(), &self.namespace);
        pods.get(&self.pod_name).await.unwrap()
    }

    async fn sts_uid(&self) -> String {
        let workloads: Api<StatefulSet> = Api::namespaced(self.client.clone(), &self.namespace);
        workloads
            .get(&self.sts_name)
            .await
            .unwrap()
            .metadata
            .uid
            .unwrap()
    }

    async fn pvc_uid(&self) -> String {
        let pvcs: Api<PersistentVolumeClaim> =
            Api::namespaced(self.client.clone(), &self.namespace);
        pvcs.get(&self.pvc_name)
            .await
            .unwrap()
            .metadata
            .uid
            .unwrap()
    }

    /// 另一独立连接读登记（跨副本可见性，非仅内存）。
    async fn peer_registration(&self) -> Option<ContainerBasicInfo> {
        let peer = rcoder_storage::pg::PgStore::connect(
            &rcoder_storage::config::PostgresConfig {
                url: Some(format!("{}/{}", self.server_dsn, self.database_name())),
                ..Default::default()
            },
            self.namespace.clone(),
            "cluster.local".into(),
        )
        .await
        .unwrap();
        peer.get(&self.app_id).and_then(|p| p.container_info())
    }

    fn database_name(&self) -> &str {
        self._database
            .rsplit_once('/')
            .map(|(_, name)| name)
            .unwrap_or("postgres")
    }

    /// 直接在用例独立 database 上数 ensure_builder 操作（受控 SQL，仅测试）。
    async fn ensure_builder_op_count(&self) -> usize {
        let mut admin = toasty::Db::builder()
            .connect(&format!("{}/{}", self.server_dsn, self.database_name()))
            .await
            .unwrap();
        let rows = toasty::sql::query(
            "SELECT operation_id FROM userapp_operations WHERE app_id=$1 AND kind='ensure_builder'",
        )
        .bind(&self.app_id)
        .exec(&mut admin)
        .await
        .unwrap();
        rows.len()
    }

    /// 经登记存储 API 删除 canonical 登记（模拟“回执在、登记行缺失”的现场形态）。
    async fn delete_registration_row(&self) {
        let current = self
            .projects
            .get(&self.app_id)
            .expect("registration row exists before deletion");
        let identity = current.persistence_identity();
        let removed = self
            .projects
            .remove_durable_if_container_identity(
                &self.app_id,
                &identity.generation,
                &identity
                    .container
                    .as_ref()
                    .expect("container identity")
                    .physical_uid
                    .clone()
                    .unwrap_or_default(),
            )
            .await
            .unwrap();
        assert!(removed, "registration row must be removed for the scenario");
        // 存储镜像已按 API 一致移除；其持久化队列可能残留孤立的 containers
        // 行（比现场更苛刻的形态会让 rebind 落入“引用缺失”分支）。用例库
        // 内用受控 SQL 把两行删净，对齐现场“回执在、登记行缺失”的真实形态。
        let mut admin = toasty::Db::builder()
            .connect(&format!("{}/{}", self.server_dsn, self.database_name()))
            .await
            .unwrap();
        toasty::sql::statement("DELETE FROM containers WHERE container_name=$1")
            .bind(format!("{STS_PREFIX}{}", self.app_id))
            .exec(&mut admin)
            .await
            .unwrap();
        toasty::sql::statement("DELETE FROM projects WHERE project_id=$1")
            .bind(&self.app_id)
            .exec(&mut admin)
            .await
            .unwrap();
    }

    async fn binding(&self, workload_uid: &str) -> Option<UserAppResourceBinding> {
        self.state
            .userapp_store
            .get_resource_binding(&ServiceType::UserappBuilder, workload_uid)
            .await
            .unwrap()
    }

    async fn adopt(&self, pod_uid: &str) -> Result<BuilderControlResult> {
        adoption::execute(
            &self.state,
            &self.app_id,
            AdoptBuilderRequest {
                lifecycle_id: self.lifecycle_id.clone(),
                request_id: format!(
                    "f1-adopt-{}",
                    &uuid::Uuid::new_v4().simple().to_string()[..8]
                ),
                expected_container_id: pod_uid.to_string(),
            },
        )
        .await
    }

    /// UID 围栏删除本用例的 STS；PVC 保留（数据卷不动，命名空间本就隔离）。
    async fn cleanup(&self) {
        let _shutdown = self.store.shutdown().await;
        let workloads: Api<StatefulSet> = Api::namespaced(self.client.clone(), &self.namespace);
        if let Ok(Some(current)) = workloads.get_opt(&self.sts_name).await {
            let uid = current.metadata.uid.clone().unwrap_or_default();
            let resource_version = current.metadata.resource_version.clone();
            let _cleanup = workloads
                .delete(
                    &self.sts_name,
                    &DeleteParams {
                        preconditions: Some(Preconditions {
                            uid: Some(uid),
                            resource_version,
                        }),
                        propagation_policy: Some(kube::api::PropagationPolicy::Foreground),
                        ..Default::default()
                    },
                )
                .await;
        }
    }

    /// 经 builder 的 dev file-server 端口读数据哨兵（HTTP 直读 data 卷，
    /// 与就绪探针同一证据面，无需 exec 权限）。
    async fn exec_sentinel_preserved(&self) -> bool {
        let pod = self.current_pod().await;
        let ip = pod
            .status
            .and_then(|status| status.pod_ip)
            .unwrap_or_default();
        let Ok(response) = crate::http_client::shared_client()
            .get(format!("http://{ip}:{BUILDER_PORT}/sentinel"))
            .timeout(std::time::Duration::from_secs(5))
            .send()
            .await
        else {
            return false;
        };
        response.status().is_success()
            && response
                .text()
                .await
                .map(|body| body.trim() == "f1-preserved")
                .unwrap_or(false)
    }
}

/// 场景 S3 + S4：无回执、缺 canonical 登记。
/// 1) 既有 Adopt 在“登记缺失”时只写物理绑定（当前边界）；
/// 2) 既有 Ensure 经“缺确认 → creation::ensure 新受理”收敛登记，
///    保持原 Pod/controller/PVC 身份与数据。
#[tokio::test]
#[ignore = "requires personal lab: RCODER_PG_TEST_DSN + RCODER_F1_LAB_NAMESPACE (rcoder-f1-*) + KUBECONFIG"]
async fn missing_registration_without_receipt_converges_via_existing_ensure() {
    let lab = lab_config();
    let fixture = Fixture::new(&lab, "s3").await;
    let outcome = async {
        let physical = fixture.create_live_builder(&lab).await;
        // 无 canonical 登记：另一连接同样读不到。
        assert!(fixture.peer_registration().await.is_none());

        // S4：显式 Adopt——缺登记分支现在经“仅核验并发布登记”受控完成步骤
        //（F1 改造）：绑定 + 登记一体落库，Pod/controller 身份保持。
        fixture.adopt(&physical.pod_uid).await.unwrap();
        let binding = fixture
            .binding(&physical.sts_uid)
            .await
            .expect("adopt writes the physical binding");
        assert_eq!(binding.lifecycle_id, fixture.lifecycle_id);
        let adopted_registration = fixture
            .peer_registration()
            .await
            .expect("explicit adoption completes the missing registration");
        assert_eq!(adopted_registration.container_id, physical.pod_uid);
        assert_eq!(
            adopted_registration.workload_uid.as_deref(),
            Some(physical.sts_uid.as_str())
        );
        assert_eq!(fixture.sts_uid().await, physical.sts_uid);
        assert_eq!(fixture.pvc_uid().await, physical.pvc_uid);
        assert!(fixture.exec_sentinel_preserved().await);
        // 回到“登记行缺失”形态再验 Ensure 的无回执收敛。
        fixture.delete_registration_row().await;

        // S3：既有 Ensure——新受理 → 复用既有 STS/PVC → 自有新回执 → 原子发布。
        let ensured = ensure_userapp_builder(&fixture.state, &fixture.app_id)
            .await
            .expect("existing ensure converges the missing registration");
        assert_eq!(ensured.container_id, physical.pod_uid, "Pod UID preserved");
        assert_eq!(
            ensured.workload_uid.as_deref(),
            Some(physical.sts_uid.as_str()),
            "controller UID preserved"
        );
        let registered = fixture
            .peer_registration()
            .await
            .expect("registration visible to an independent connection");
        assert_eq!(registered.container_id, physical.pod_uid);
        assert_eq!(
            registered.workload_uid.as_deref(),
            Some(physical.sts_uid.as_str())
        );
        // 物理身份未动：Pod/controller/PVC UID 与数据哨兵保持。
        assert_eq!(fixture.sts_uid().await, physical.sts_uid);
        assert_eq!(fixture.pvc_uid().await, physical.pvc_uid);
        let pods: Api<Pod> = Api::namespaced(fixture.client.clone(), &fixture.namespace);
        let pod = pods.get(&fixture.pod_name).await.unwrap();
        assert_eq!(pod.metadata.uid.unwrap(), physical.pod_uid);
        assert!(fixture.exec_sentinel_preserved().await);
        let ops = fixture.ensure_builder_op_count().await;
        assert_eq!(ops, 1, "exactly one ensure_builder admission converged it");
        anyhow::Ok(())
    }
    .await;
    fixture.cleanup().await;
    outcome.expect("S3/S4 scenario must hold");
}

/// 场景 S1：当前 Pod 回执仍在（Service 确认 + Succeeded 检查点），
/// 仅 canonical 登记行缺失 → 修复路径不新增受理即恢复登记。
#[tokio::test]
#[ignore = "requires personal lab: RCODER_PG_TEST_DSN + RCODER_F1_LAB_NAMESPACE (rcoder-f1-*) + KUBECONFIG"]
async fn current_pod_receipt_repairs_registration_without_new_admission() {
    let lab = lab_config();
    let fixture = Fixture::new(&lab, "s1").await;
    let physical = fixture.create_live_builder(&lab).await;
    fixture.adopt(&physical.pod_uid).await.unwrap();
    ensure_userapp_builder(&fixture.state, &fixture.app_id)
        .await
        .expect("initial convergence produces the current-Pod receipt");
    let before = fixture.ensure_builder_op_count().await;
    assert_eq!(before, 1);
    let converged_pod = fixture.current_pod().await.metadata.uid.clone().unwrap();

    // 现场形态：登记行丢失，回执仍在。
    fixture.delete_registration_row().await;
    assert!(fixture.peer_registration().await.is_none());

    let repaired = ensure_userapp_builder(&fixture.state, &fixture.app_id)
        .await
        .expect("receipt-backed repair restores the registration");
    assert_eq!(repaired.container_id, converged_pod);
    let after = fixture.ensure_builder_op_count().await;
    assert_eq!(
        after, before,
        "repair must not admit a new ensure_builder operation"
    );
    let registered = fixture
        .peer_registration()
        .await
        .expect("registration restored for another connection");
    assert_eq!(registered.container_id, converged_pod);
    assert_eq!(fixture.sts_uid().await, physical.sts_uid);
    fixture.cleanup().await;
}

/// 场景 S2：只有旧 Pod 回执（Pod 原生换代、Service 确认指向旧 Pod UID），
/// 且登记行缺失 → 既有 Ensure 经新受理产生自有新回执并收敛到新 Pod。
#[tokio::test]
#[ignore = "requires personal lab: RCODER_PG_TEST_DSN + RCODER_F1_LAB_NAMESPACE (rcoder-f1-*) + KUBECONFIG"]
async fn replaced_pod_with_old_receipt_converges_via_fresh_receipt() {
    let lab = lab_config();
    let fixture = Fixture::new(&lab, "s2").await;
    let physical = fixture.create_live_builder(&lab).await;
    fixture.adopt(&physical.pod_uid).await.unwrap();
    ensure_userapp_builder(&fixture.state, &fixture.app_id)
        .await
        .expect("initial convergence");
    fixture.delete_registration_row().await;

    // 原生 Pod 换代（删除 Pod → STS 重建新 UID）。
    let pods: Api<Pod> = Api::namespaced(fixture.client.clone(), &fixture.namespace);
    pods.delete(&fixture.pod_name, &DeleteParams::default())
        .await
        .unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(120), async {
        loop {
            if let Some(pod) = pods.get_opt(&fixture.pod_name).await.unwrap()
                && builder_pod_running(&pod)
                && pod.metadata.uid.as_deref() != Some(physical.pod_uid.as_str())
            {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(400)).await;
        }
    })
    .await
    .expect("replacement Pod to become Running");
    let new_pod_uid = fixture.current_pod().await.metadata.uid.clone().unwrap();

    let ensured = ensure_userapp_builder(&fixture.state, &fixture.app_id)
        .await
        .expect("existing ensure converges through a fresh receipt");
    assert_eq!(ensured.container_id, new_pod_uid);
    assert_eq!(
        ensured.workload_uid.as_deref(),
        Some(physical.sts_uid.as_str()),
        "same controller across native Pod replacement"
    );
    let registered = fixture
        .peer_registration()
        .await
        .expect("registration converged for another connection");
    assert_eq!(registered.container_id, new_pod_uid);
    assert_eq!(fixture.sts_uid().await, physical.sts_uid);
    assert_eq!(fixture.pvc_uid().await, physical.pvc_uid);
    assert!(fixture.exec_sentinel_preserved().await);
    let ops = fixture.ensure_builder_op_count().await;
    assert_eq!(
        ops, 2,
        "old receipt is not reused; a fresh admission closed it"
    );
    fixture.cleanup().await;
}

/// 边界证据（§4.3/§4.7）：ensure 会唤醒被外部缩容到 0 的计算——
/// 因此后台对账/只读入口不能调用 ensure 来补登记。
#[tokio::test]
#[ignore = "requires personal lab: RCODER_PG_TEST_DSN + RCODER_F1_LAB_NAMESPACE (rcoder-f1-*) + KUBECONFIG"]
async fn externally_stopped_builder_is_woke_by_existing_ensure() {
    let lab = lab_config();
    let fixture = Fixture::new(&lab, "se").await;
    let physical = fixture.create_live_builder(&lab).await;
    fixture.adopt(&physical.pod_uid).await.unwrap();
    ensure_userapp_builder(&fixture.state, &fixture.app_id)
        .await
        .expect("initial convergence");

    // 外部缩容（kubectl scale 语义）：Pod 消失、STS 保留。
    let workloads: Api<StatefulSet> = Api::namespaced(fixture.client.clone(), &fixture.namespace);
    let patch = serde_json::json!({"spec":{"replicas":0}});
    workloads
        .patch(
            &fixture.sts_name,
            &kube::api::PatchParams::default(),
            &kube::api::Patch::Merge(&patch),
        )
        .await
        .unwrap();
    let pods: Api<Pod> = Api::namespaced(fixture.client.clone(), &fixture.namespace);
    tokio::time::timeout(std::time::Duration::from_secs(60), async {
        loop {
            if pods.get_opt(&fixture.pod_name).await.unwrap().is_none() {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(400)).await;
        }
    })
    .await
    .expect("stopped builder Pod to disappear");

    // 预期（当前行为）：ensure 不是只读——它对外部停机计算受理物理启动。
    // durable compute 控制记录即为副作用证据（stub 缺执行域注入时启动校验
    // 拒绝，恰恰说明真实启动动作已被尝试；真实镜像则会完成扩容）。
    // 任务书 §4.3/§4.7 的结论因此成立：后台对账/只读入口不得调用 ensure
    // 来补登记；已停计算只修预期停态（F2），不得为了登记去启动。
    let woke = ensure_userapp_builder(&fixture.state, &fixture.app_id).await;
    let status = fixture
        .state
        .userapp_store
        .read_compute_status(
            &fixture.app_id,
            &fixture.lifecycle_id,
            shared_types::UserAppOperationScope::Dev,
        )
        .await
        .unwrap();
    assert!(
        status.operation.is_some(),
        "ensure must have admitted a physical compute start against stopped compute (ensure result: {:?})",
        woke.as_ref().err()
    );
    fixture.cleanup().await;
}
