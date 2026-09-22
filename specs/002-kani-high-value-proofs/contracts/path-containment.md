# Contract: 路径包含性（PathContainment）

**Theme**: PathContainment  
**Subjects**:
- `file_server::path_safety::ensure_within(base, relative) -> AppResult<PathBuf>`
- `file_server::path_safety::ensure_within_path(base, target) -> AppResult<PathBuf>`
- `file_server::path_safety::safe_within_or_skip(base, relative) -> Option<PathBuf>`
- `file_server::path_safety::safe_zip_entry(extract_path, entry_name) -> AppResult<PathBuf>`

## Properties (for-all inputs in bounded domain)

### P1. Containment
```text
∀ base, relative:
  ensure_within(base, relative) = Ok(p)
  ⇒ p.starts_with(base.clean())
```

### P2. Escape rejection
```text
∀ base, relative:
  ¬(base.join(relative).clean().starts_with(base.clean()))
  ⇒ ensure_within(base, relative) = Err
```

### P3. Zip-slip rejection
```text
∀ extract, entry:
  ¬(extract.join(entry).clean() == extract.clean()
    ∨ extract.join(entry).clean().starts_with(extract.clean()))
  ⇒ safe_zip_entry(extract, entry) = Err
```

### P4. Skip-style equivalence
```text
∀ base, relative:
  safe_within_or_skip(base, relative).is_some()
  ⇔ ensure_within(base, relative).is_ok()
```

### P5. No panic
```text
∀ base, relative (incl. non-UTF-8 for *_path / any bytes for &str APIs):
  函数返回 Result/Option，不 panic、不 abort
```

## Bounded domain (harness)

- `base`: 定长字节/短 `&str`，`max_input` 例如 8–16 字节，可 `assume` 非空。  
- `relative` / `entry`: 定长 12–24 字节；允许 `..`、`.`、`/`、盘符样例字符。  
- `unwind`: 覆盖 `path-clean`/`join` 相关循环；UNWINDING 失败 = 未证明。

## Counterexample playback

失败时用 `cargo kani --concrete-playback inplace --harness <id>` 生成 nextest 反例；修复前必须能红。

## Non-goals

- symlink 真实解析（`ensure_resolved_within` 碰 FS）→ 后续 stubbing 批次。  
- 根目录白名单（模块注释声明信任调用方）。
