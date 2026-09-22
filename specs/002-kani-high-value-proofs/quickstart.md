# Quickstart: 在 rcoder 跑 Kani 有界证明

## 0. 前置（机器级，非 Cargo 依赖）

```bash
cargo install --locked kani-verifier
cargo kani setup
kani --version          # 期望: Kani Rust Verifier 0.68.x + CBMC 6.x
```

**不需要**在业务 crate 的 `[dependencies]` / `[dev-dependencies]` 里加 `kani`。  
可选（仅 IDE）：

```toml
[target.'cfg(kani_ra)'.dependencies]
kani = { git = "https://github.com/model-checking/kani" }
```

## 1. Harness 门控约定

```rust
#[cfg(kani)]
mod kani_proofs {
    use super::*;

    #[kani::proof]
    #[kani::unwind(12)]
    fn path_ok_implies_within() {
        let base: [u8; 8] = kani::any();
        let rel: [u8; 12] = kani::any();
        // assume / assert — 调用真实 pub fn，禁止复制算法
    }
}
```

- `cargo build` / `cargo test` / 发布构建：`cfg(kani)` 为假 → 完全不编译 harness。  
- `cargo kani`：自动注入 `kani` crate 并打开 `cfg(kani)`。

## 2. 跑证明

```bash
# 单 harness
cargo kani -p file-server --harness path_ok_implies_within

# 整包
cargo kani -p shared_types

# 展开界限（UNWINDING 失败时先调这个，禁止关断言）
cargo kani -p file-server --default-unwind 16 --harness path_ok_implies_within
```

**结果判读**  
| 输出 | 含义 | 动作 |
|---|---|---|
| `VERIFICATION:- SUCCESSFUL` 且 failed=0 | 有界证明通过 | 记 EvidenceRecord=Proved |
| `Failed Checks: unwinding assertion` | 未展开完 | 增大 `--unwind` 或重写循环；**不是**通过 |
| `Status: FAILURE`（assertion） | 性质为假 | `--concrete-playback inplace` 生成反例单测 |
| 大量 `UNDETERMINED` | 模型含不支持/过深展开 | 收缩定长输入、去 `String`/`fmt` |

## 3. 反例回灌 nextest

```bash
cargo kani -p shared_types --harness <id> --concrete-playback inplace
cargo nextest run -p shared_types --no-fail-fast
```

## 4. 独立门禁（实现阶段提供）

```bash
make verify-kani          # 只跑证明，不进 make test
```

## 5. Pilot 效果（2026-09-22，模型演示）

本机 `/tmp/kani-pilot`（固定缓冲 `ensure_within` 模型）：

- `proof_dot_segment_is_noop` → **SUCCESSFUL**（~87s）
- 其余 harness → UNWINDING 失败（harness 内 `for` 迭代 + 循环上界）

结论：链路可用；实现阶段 harness **必须调用真实函数**并认真配 unwind。
