//! Per-application operation lease on the native `coordination.k8s.io` Lease
//! object. The holder renews `renewTime` on a cadence; a lease whose renewal
//! stopped for `LEASE_TTL_SECONDS` is expired and any replica may take it over
//! with a resourceVersion-conditioned patch (exactly one taker wins the CAS).
//! Crash residue is therefore bounded by the TTL instead of requiring operator
//! recovery. The explicit completion path still deletes the lease under
//! uid + live resourceVersion preconditions; a dropped incomplete holder only
//! stops renewing — the object is left for expiry so a successor can never
//! race a late API-server write from the dead holder. Mutation-layer fencing
//! (uid/resourceVersion preconditions plus durable admission) remains the
//! final guard regardless of lease state.
//!
//! Migration: acquisition only ever creates Lease objects. Legacy ConfigMap
//! leases with the same name are released through the ConfigMap fallback in
//! the captured validate/release paths (bound-terminal ones via the recovery
//! scanner, unbound ones via the orphan sweep), after which the fallback can
//! be retired. During the rolling upgrade window an old replica may still
//! hold a ConfigMap while a new replica acquires the Lease for the same app —
//! concurrent operations remain impossible because durable PG admission is
//! the first serialization layer for every mutating path.
use super::kubernetes_runtime::KubernetesRuntime;
use container_runtime_api::{ContainerRuntimeError, ContainerRuntimeResult};
use k8s_openapi::api::coordination::v1::{Lease, LeaseSpec};
use k8s_openapi::api::core::v1::ConfigMap;
use kube::{
    Api,
    api::{DeleteParams, ListParams, Patch, PatchParams, PostParams, Preconditions},
};
use shared_types::{AppOperationLease, ServiceType};

// 拆分（file-server 大文件范式）：`lease` Lease 域原语（TTL/续约/接管/释放，
// 含 OperationLease 与 AppOperationLease/Drop impl）；`ops` KubernetesRuntime
// 的操作租约 impl 块（acquire/validate/release/sweep）；`helpers` ConfigMap
// 身份校验与命名自由函数。lease/helpers 经 pub(super) + 私有 glob 供子模块
// （含测试）互见；ops 方法 pub(crate) 供 runtime 兄弟模块经类型调用。

mod helpers;
mod lease;
mod ops;
#[cfg(test)]
mod tests;
#[cfg(all(test, feature = "kubernetes"))]
mod wire_tests;

use helpers::*;
use lease::*;
