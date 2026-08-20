//! rcoder-e2e：黑盒 e2e 集成测试 crate。
//!
//! 公共层（common/）放 lib 目标：cargo 的每个 tests/*.rs 是独立编译的
//! 测试二进制，共享代码放 tests/ 子目录会对未见使用的目标产生 dead_code
//! 误报；lib 目标只编译一次，测试经 `use rcoder_e2e as common` 引用。
//!
//! 运行: make test-e2e-compose / make test-e2e-k8s（见 make/test.mk）。

pub mod common;
