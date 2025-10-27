.PHONY: help build dev install install-agent uninstall

# 默认目标：显示帮助信息
help:
	@echo "rcoder 项目 Makefile"
	@echo ""
	@echo "可用命令："
	@echo "  make build          - 安装 codex-acp-agent + 编译项目（release）"
	@echo "  make dev            - 安装 codex-acp-agent + 编译项目（debug）"
	@echo "  make install        - 安装所有二进制到 ~/.cargo/bin/"
	@echo "  make install-agent  - 仅安装 codex-acp-agent"
	@echo "  make uninstall      - 卸载所有二进制"
	@echo ""

# 完整构建（release）
build: install-agent
	@echo "🔨 编译 rcoder（release 模式）..."
	@cargo build --release --bin rcoder
	@echo ""
	@echo "✅ 构建完成！"
	@echo "运行: ./target/release/rcoder"

# 开发构建（debug）
dev: install-agent
	@echo "🔨 编译 rcoder（debug 模式）..."
	@cargo build --bin rcoder
	@echo ""
	@echo "✅ 开发构建完成！"
	@echo "运行: cargo run --bin rcoder"

# 安装 codex-acp-agent
install-agent:
	@echo "📦 安装 codex-acp-agent..."
	@cargo install --path crates/codex-acp-agent --bin codex-acp-agent --force
	@echo "✅ codex-acp-agent 已安装到: ~/.cargo/bin/codex-acp-agent"

# 安装所有二进制到系统
install: build
	@echo "📥 安装 rcoder 到系统..."
	@cargo install --path crates/rcoder --bin rcoder --force
	@echo ""
	@echo "✅ 已安装："
	@echo "  - codex-acp-agent -> ~/.cargo/bin/codex-acp-agent"
	@echo "  - rcoder -> ~/.cargo/bin/rcoder"

# 卸载所有二进制
uninstall:
	@echo "🗑️  卸载二进制..."
	@cargo uninstall rcoder 2>/dev/null || echo "rcoder 未安装"
	@cargo uninstall codex-acp-agent 2>/dev/null || echo "codex-acp-agent 未安装"
	@echo "✅ 卸载完成"
