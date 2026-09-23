# ============================================================================
# Kani 有界模型检查（独立门禁，不进 make test）。
# 前置: cargo install --locked kani-verifier && cargo kani setup
# 依赖策略: 业务 Cargo.toml 不引入普通 kani 依赖；harness 一律 #[cfg(kani)]。
# 用法:
#   make verify-kani
#   make verify-kani KANI_PKGS="-p shared_types"
#   make verify-kani KANI_ARGS="--harness path_ok_implies_within"
# ============================================================================
KANI_PKGS ?= -p file-server -p shared_types
KANI_ARGS ?=

.PHONY: verify-kani

verify-kani:
	@echo "🔬 Kani 有界证明（$(KANI_PKGS)）..."
	@command -v cargo-kani >/dev/null 2>&1 || command -v kani >/dev/null 2>&1 || { \
		echo "❌ 未安装 kani-verifier: cargo install --locked kani-verifier && cargo kani setup"; \
		exit 1; \
	}
	@status=0; \
	for pkg in file-server shared_types; do \
		case " $(KANI_PKGS) " in \
			*" -p $$pkg "*) \
				echo "" && echo "────────── cargo kani -p $$pkg ──────────"; \
				cargo kani -p $$pkg $(KANI_ARGS) || status=$$?; \
				;; \
		esac; \
	done; \
	echo ""; \
	if [ $$status -eq 0 ]; then \
		echo "✅ Kani 验证完成（见各 harness SUMMARY；UNWINDING 不得计为通过）"; \
	else \
		echo "❌ Kani 验证存在失败/unwinding（禁止 --no-unwinding-assertions 掩盖）"; \
	fi; \
	exit $$status
