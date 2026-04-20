# Tasks: K8s Runtime Support Fix

**Branch**: `dev-k8s` | **Date**: 2026-04-20
**Plan**: [plan.md](./plan.md) | **Spec**: [spec.md](./spec.md)

## Task List

### Phase 1: Critical Fixes (P0)

---

### Task 1: Fix `global::init_global_docker_manager_with_config()` K8s path
**File**: `crates/docker_manager/src/lib.rs`  
**Priority**: P0  
**Estimated Time**: 30 min  
**Status**: TODO

**Problem**: When `CONTAINER_RUNTIME=kubernetes`, the function calls `RuntimeManager::init()` but then still tries to set `GLOBAL_DOCKER_MANAGER` which fails silently or causes issues.

**Changes**:
```rust
#[cfg(feature = "kubernetes")]
pub async fn init_global_docker_manager_with_config(
    config: DockerManagerConfig,
) -> DockerResult<()> {
    let runtime_type = RuntimeType::from_env();
    crate::runtime::RuntimeManager::init(config.clone())
        .await
        .map_err(|e| DockerError::ConfigurationError(e.to_string()))?;
    info!("Runtime initialized with config");

    if runtime_type == RuntimeType::Docker {
        let manager = Arc::new(DockerManager::new(config).await?);
        GLOBAL_DOCKER_MANAGER.set(manager).map_err(|_| {
            DockerError::IoError(std::io::Error::new(
                std::io::ErrorKind::AlreadyExists,
                "global DockerManager already initialized",
            ))
        })?;
        info!("DockerManager initialized with config");
    }
    // K8s mode: RuntimeManager is initialized, GLOBAL_DOCKER_MANAGER stays empty (ok)

    Ok(())
}
```

**Verification**:
- [ ] `cargo check -p docker_manager --features kubernetes` passes
- [ ] Unit test `test_runtime_type_from_env_kubernetes` passes

---

### Task 2: Verify `main.rs` runtime initialization flow
**File**: `crates/rcoder/src/main.rs`  
**Priority**: P0  
**Estimated Time**: 30 min  
**Status**: TODO

**Problem**: Need to verify that after calling `init_global_docker_manager_with_config()`, `RuntimeManager::get()` works correctly for K8s mode.

**Current Code** (lines 181-186):
```rust
if let Err(e) =
    docker_manager::global::init_global_docker_manager_with_config(docker_manager_config).await
{
    error!("Docker Manager initializefailed: {}", e);
    return Err(anyhow::anyhow!("Docker Manager initialization failed: {}", e));
}
```

**Verification**:
- [ ] K8s mode: `RuntimeManager::get().await` returns `Arc<dyn ContainerRuntime>`
- [ ] Docker mode: `get_global_docker_manager().await` returns `Arc<DockerManager>`
- [ ] Both modes can call `cleanup_all()` without error

---

### Task 3: Fix `stop_container` in KubernetesRuntime to handle service_type
**File**: `crates/docker_manager/src/runtime/kubernetes_runtime.rs`  
**Priority**: P0  
**Estimated Time**: 45 min  
**Status**: TODO

**Problem**: `stop_container(&self, project_id: &str)` only takes project_id, but K8s creates different pod types (RCoder vs ComputerAgentRunner) with different prefixes.

**Current**:
```rust
async fn stop_container(&self, project_id: &str) -> ContainerRuntimeResult<()> {
    let pod_name = self.pod_name(project_id, &ServiceType::RCoder);  // Always RCoder!
    // ...
}
```

**Fix Options**:

Option A - Add service_type parameter to `stop_container`:
```rust
async fn stop_container(&self, project_id: &str, service_type: ServiceType) -> ContainerRuntimeResult<()> {
    let pod_name = self.pod_name(project_id, &service_type);
    // ...
}
```
⚠️ This breaks the trait signature.

Option B - Cache service_type in pod_cache:
```rust
// Store (identifier, service_type) in cache when creating container
self.pod_cache.write().await.insert(identifier.to_string(), (pod_info, service_type.clone()));

// In stop_container, lookup cached service_type or default to RCoder
let service_type = self.get_cached_service_type(identifier).await.unwrap_or(ServiceType::RCoder);
```

Option C - Use `stop_container_by_identifier` which already has service_type:
```rust
async fn stop_container(&self, project_id: &str) -> ContainerRuntimeResult<()> {
    // Try RCoder first, then ComputerAgentRunner
    match self.stop_container_by_identifier(project_id, &ServiceType::RCoder).await {
        Ok(()) => Ok(()),
        Err(ContainerRuntimeError::ContainerStopError(_)) => {
            // Try ComputerAgentRunner if RCoder not found
            self.stop_container_by_identifier(project_id, &ServiceType::ComputerAgentRunner).await
        }
        Err(e) => Err(e),
    }
}
```
✅ **Recommended** - Uses existing trait method, handles both types

**Implementation**: Use Option C

**Verification**:
- [ ] `stop_container("user123")` correctly deletes ComputerAgentRunner pod when it exists
- [ ] `stop_container("project123")` correctly deletes RCoder pod when it exists

---

### Task 4: Fix Makefile k8s commands
**File**: `Makefile`  
**Priority**: P1  
**Estimated Time**: 20 min  
**Status**: TODO

**Problem**: 
1. `dev-up-k8s` and `dev-restart-k8s` do `kubectl apply` followed by `kubectl set image`, but the image in deployment.yaml is hardcoded to `rcoder:test`
2. `dev-restart-k8s` uses `rollout restart` which doesn't pick up the new image from `set image`

**Current (broken)**:
```makefile
dev-up-k8s:
    kubectl apply -f k8s/manifests/rcoder-deployment.yaml  # Uses image: rcoder:test
    kubectl set image deployment/rcoder rcoder=$(IMAGE)    # But this updates it
    ...

dev-restart-k8s: dev-build-k8s
    kubectl apply -f k8s/manifests/rcoder-deployment.yaml
    kubectl set image deployment/rcoder rcoder=$(IMAGE)    # Sets new image
    kubectl rollout restart deploy/rcoder                 # But restart uses deployment.yaml, not set image!
```

**Fix**:
```makefile
# Use sed to replace image in deployment.yaml before applying
define apply_with_image
    kubectl apply -f k8s/manifests/namespace.yaml
    kubectl apply -f k8s/manifests/serviceaccount.yaml
    sed "s|image: rcoder:test|image: $(IMAGE)|" k8s/manifests/rcoder-deployment.yaml | kubectl apply -f -
    kubectl apply -f k8s/manifests/rcoder-service.yaml
endef

dev-up-k8s:
    $(call apply_with_image)
    kubectl rollout status deploy/rcoder -n $(K8S_NAMESPACE) --timeout=$(ROLLOUT_TIMEOUT)

dev-restart-k8s: dev-build-k8s
    kubectl delete pods -n $(K8S_NAMESPACE) -l app=rcoder --ignore-not-found
    kubectl rollout status deploy/rcoder -n $(K8S_NAMESPACE) --timeout=$(ROLLOUT_TIMEOUT)
```

**Key changes**:
1. Use `sed` to replace image tag at apply time
2. `dev-restart-k8s` uses `kubectl delete pods` instead of `rollout restart` (simpler, faster)
3. `dev-down-k8s` stays the same (already correct)

**Verification**:
- [ ] `make dev-up-k8s IMAGE=rcoder:test-k8s` deploys with correct image
- [ ] `make dev-restart-k8s IMAGE=rcoder:test-k8s` rebuilds and redeploys
- [ ] `make dev-down-k8s` cleans up properly

---

### Task 5: Add K8s mode to Cargo features in rcoder
**File**: `crates/rcoder/Cargo.toml`  
**Priority**: P1  
**Estimated Time**: 10 min  
**Status**: TODO

**Problem**: The `kubernetes` feature in rcoder passes to docker_manager, but need to verify it's properly configured.

**Current**:
```toml
[features]
# Kubernetes 支持：启用 Kubernetes 运行时模式
# 启用后可通过 CONTAINER_RUNTIME=kubernetes 环境变量切换到 K8s 模式
kubernetes = ["docker_manager/kubernetes"]
```

**Verification**:
- [ ] `cargo build --features kubernetes` works
- [ ] `cargo build --features kubernetes --package rcoder` produces binary with K8s support

---

### Task 6: Test K8s mode end-to-end
**Priority**: P0  
**Estimated Time**: 60 min  
**Status**: TODO

**Prerequisites**: Kind cluster or real K8s cluster available

**Test Steps**:
```bash
# 1. Build K8s image
make dev-build-k8s IMAGE=rcoder:test-k8s

# 2. Deploy to K8s
make dev-up-k8s IMAGE=rcoder:test-k8s

# 3. Check rcoder pod is running
kubectl get pods -n rcoder
kubectl logs -n rcoder -l app=rcoder

# 4. Test /health endpoint
NODE_PORT=$(kubectl get svc rcoder -n rcoder -o jsonpath='{.spec.ports[0].nodePort}')
curl http://localhost:$NODE_PORT/health

# 5. Test /chat endpoint (creates RCoder agent pod)
curl -X POST http://localhost:$NODE_PORT/chat \
  -H "Content-Type: application/json" \
  -d '{"prompt": "hello"}'

# 6. Check agent pod was created
kubectl get pods -n rcoder

# 7. Test /computer/chat endpoint (creates ComputerAgentRunner pod)
curl -X POST http://localhost:$NODE_PORT/computer/chat \
  -H "Content-Type: application/json" \
  -d '{"user_id": "test-user", "prompt": "hello"}'

# 8. Check computer agent pod was created
kubectl get pods -n rcoder -l user_id=test-user

# 9. Test cleanup (delete chat session)
# ... verify pods are deleted

# 10. Cleanup
make dev-down-k8s
```

**Expected Results**:
- [ ] `/health` returns healthy
- [ ] `/chat` creates `rcoder-agent-{project_id}` pod
- [ ] `/computer/chat` creates `computer-agent-runner-{user_id}` pod
- [ ] Both pods are reachable via gRPC from rcoder main pod
- [ ] Cleanup properly deletes pods

---

### Task 7: Verify Docker mode still works (regression test)
**Priority**: P1  
**Estimated Time**: 30 min  
**Status**: TODO

**Test Steps**:
```bash
# 1. Build Docker image
make dev-build

# 2. Start Docker Compose
make dev-up

# 3. Test /health
curl http://localhost:8087/health

# 4. Test /chat
curl -X POST http://localhost:8087/chat \
  -H "Content-Type: application/json" \
  -d '{"prompt": "hello"}'

# 5. Verify container created
docker ps | grep rcoder-agent

# 6. Cleanup
make dev-down
```

**Expected Results**:
- [ ] All existing functionality works as before
- [ ] No regression in Docker mode

---

## Task Summary

| # | Task | Priority | Status | Dependencies |
|---|------|----------|--------|--------------|
| 1 | Fix `init_global_docker_manager_with_config()` | P0 | TODO | - |
| 2 | Verify `main.rs` runtime init | P0 | TODO | Task 1 |
| 3 | Fix `stop_container` service_type | P0 | TODO | - |
| 4 | Fix Makefile k8s commands | P1 | TODO | - |
| 5 | Verify Cargo features | P1 | TODO | - |
| 6 | Test K8s mode end-to-end | P0 | TODO | Tasks 1-5 |
| 7 | Regression test Docker mode | P1 | TODO | - |

---

## Completion Criteria

- [ ] All P0 tasks complete
- [ ] K8s mode can create/manage RCoder agent pods
- [ ] K8s mode can create/manage ComputerAgentRunner pods
- [ ] Docker mode regression tests pass
- [ ] Makefile k8s commands work correctly
- [ ] Code compiles with both `kubernetes` feature enabled and disabled
