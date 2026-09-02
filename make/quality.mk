# ============================================================================
# 质量检查命令（cargo-audit / cargo-deny / cargo-geiger / cargo-llvm-cov / cargo-fuzz）
# 前置: cargo install cargo-audit cargo-deny cargo-geiger cargo-llvm-cov cargo-fuzz
# ============================================================================

# 依赖安全公告扫描（RustSec 数据库；首跑自动拉取，需联网）
audit:
	@echo "🔍 RustSec 安全公告扫描..."
	@cargo audit

# 依赖许可证 / 重复版本 / 来源管控（配置: 根目录 deny.toml）
deny:
	@echo "🔍 依赖许可证与来源检查..."
	@cargo deny check

# 依赖 unsafe 用量审计（自身代码已 deny unsafe，本命令看第三方依赖面）
# ⚠️ 已知限制（cargo-geiger 0.13）:
#   1. 不支持 virtual manifest——必须用 --manifest-path 指向具体成员（-p 也不支持）
#   2. 大依赖树不可行——rcoder 全树（892 包）实测 78 分钟无产出须终止；
#      按成员小范围跑（默认 shared_types），GEIGER_MANIFEST 可换其他成员
# 例: make geiger / make geiger GEIGER_MANIFEST=crates/agent_runner/Cargo.toml
GEIGER_MANIFEST ?= crates/shared_types/Cargo.toml

geiger:
	@echo "🔍 依赖 unsafe 审计（$(GEIGER_MANIFEST)；大依赖树耗时不可行，见注释）..."
	@cargo geiger --manifest-path $(CURDIR)/$(GEIGER_MANIFEST)

# 一键质量三连: audit + deny + geiger（任一失败不中断，末尾汇总退出）
quality:
	@echo "🔍 质量三连检查（audit + deny + geiger）..."
	@status=0; \
	echo "" && echo "────────── [1/3] cargo audit ──────────" && (cargo audit || status=$$?); \
	echo "" && echo "────────── [2/3] cargo deny check ──────────" && (cargo deny check || status=$$?); \
	echo "" && echo "────────── [3/3] cargo geiger（$(GEIGER_MANIFEST)）──────────" && (cargo geiger --manifest-path $(CURDIR)/$(GEIGER_MANIFEST) || status=$$?); \
	echo ""; \
	if [ $$status -eq 0 ]; then \
		echo "✅ 质量检查全部通过"; \
	else \
		echo "❌ 质量检查存在失败项（见上方分段输出）"; \
	fi; \
	exit $$status

# 测试覆盖率 HTML 报告（llvm-cov；排除 rcoder-e2e——环境门控型测试不可达即 skip）
# ⚠️ 首次需插桩编译全 workspace，较慢
# 例: make coverage / make coverage COV_PKGS="-p shared_types -p app_manager"
coverage:
	@echo "🔍 测试覆盖率（llvm-cov；排除 rcoder-e2e）..."
	@cargo llvm-cov --workspace --exclude rcoder-e2e --html --output-dir target/llvm-cov/html $(COV_PKGS)
	@echo "✅ 覆盖率报告: target/llvm-cov/html/html/index.html"

# ============================================================================
# 🔬 cargo-fuzz 模糊测试（fuzz/ 独立 crate；自动切换 nightly 工具链）
# 目标集中在 shared_types 纯函数解析面（K8s Quantity / semver / 聊天配置）
# ============================================================================

FUZZ_SECONDS ?= 60

# 模糊测试冒烟（自动枚举 fuzz/fuzz_targets/ 下全部目标）
# ⚠️ RUSTUP_TOOLCHAIN=nightly 显式指定——cargo-fuzz 内部裸调 cargo 不会自动切 nightly
# 例: make fuzz                          # 全目标各 60 秒
#      make fuzz FUZZ_SECONDS=300        # 全目标各 300 秒
#      make fuzz FUZZ_TARGET=fuzz_quantity  # 单目标
fuzz:
	@if [ ! -d fuzz/fuzz_targets ]; then \
		echo "❌ fuzz/ 不存在（先在仓库根执行: cargo fuzz init）"; \
		exit 1; \
	fi; \
	if [ -n "$(FUZZ_TARGET)" ]; then \
		targets="$(FUZZ_TARGET)"; \
	else \
		targets=$$(ls fuzz/fuzz_targets/*.rs 2>/dev/null | xargs -n1 basename | sed 's/\.rs$$//'); \
	fi; \
	echo "🔬 fuzz 目标: $$targets（每目标 $(FUZZ_SECONDS) 秒）"; \
	status=0; \
	for t in $$targets; do \
		echo "" && echo "── $$t ──"; \
		RUSTUP_TOOLCHAIN=nightly cargo fuzz run $$t -- -max_total_time=$(FUZZ_SECONDS) || status=$$?; \
	done; \
	if [ $$status -eq 0 ]; then echo "✅ fuzz 冒烟无 crash"; else echo "❌ fuzz 发现问题（见上方输出；崩溃样本在 fuzz/artifacts/）"; fi; \
	exit $$status
