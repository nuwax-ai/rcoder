# Contract: 身份 Fail-closed（IdentityFailClosed）

**Theme**: IdentityFailClosed  
**Subjects**:
- `UserAppExecutionContext::validate_identity(&self, app_id: &str) -> Result<(), String>`
- `UserAppDeletionCheckpoint::validate(&self) -> Result<(), String>`
- `UserAppDeletionCheckpoint::validate_operation(&self, operation: &UserAppOperationRecord) -> Result<(), String>`
- `HotDeploymentFailureEvidence::validate / validate_success`

## Properties

### P1. Identity binding
```text
∀ ctx, app_id:
  ctx.validate_identity(app_id) = Ok
  ⇒ ctx.app_id == app_id
```

### P2. Field whitelist / fingerprint
```text
∀ ctx:
  validate_identity 成功
  ⇒ app_id/lifecycle_id/operation_id/executor_id ∈ IDENTIFIER 文法
  ∧ request_fingerprint 为 64 位 ASCII hex
```

### P3. Deletion operation membership (Q01 谓词层)
```text
∀ checkpoint, operation:
  checkpoint.validate_operation(operation) = Ok
  ⇒ checkpoint.context.{app_id, lifecycle_id, operation_id, executor_id, request_fingerprint}
     == operation.{app_id, lifecycle_id, operation_id, executor_id, request_fingerprint}
  ∧ checkpoint.kind == operation.kind
```

### P4. Hot evidence exclusivity (D03/D04 谓词层)
```text
∀ evidence, record:
  ¬( evidence.validate(record).is_ok() ∧ evidence.validate_success(record).is_ok() )
```
（failed 收口与 success 收口不得同时成立）

### P5. Incomplete resources rejected
```text
∀ resources, operation_id:
  ∃ r ∈ resources: r.name ∨ r.uid 为空
  ∨ (r.kind ≠ Container ∧ r.resource_version 为空)
  ⇒ validate_deletion_resources 失败
```

## Bounded domain

- 字符串字段：短定长 ASCII（identifier 文法字符集）+ 允许少量非法字符以覆盖拒绝路径。  
- `request_fingerprint`: `[u8; 8]` 缩小模型或 `assume` 长度/hex 谓词分层证明（64 hex 全空间过大时拆两条：长度、字符集）。  
- `UserAppDeletionCheckpoint`：字段级 `kani::any()`，对 enum 用有限变体；**避免**展开 `serde_json::Value`（`validate_outcome` 内 JSON 解析可 stub 或后置批次）。

## Counterexample playback

失败反例必须锁「身份错配被拒 / 两收口互斥」，禁止弱化断言。

## Non-goals

- 并发下旧删除 vs 新身份（Q01 时序）→ E2E。  
- JSON checkpoint 全量 serde  round-trip → 单测/模糊测试批次。
