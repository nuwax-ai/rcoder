# RCoder Makefile
# 包含子模块: docker, dev, k8s, test, pyroscope

# Phony targets
.PHONY: help \
	build install install-agent uninstall \
	dev-build dev-up dev-restart dev-down dev-logs \
	dev-build-k8s dev-up-k8s dev-restart-k8s dev-down-k8s dev-logs-k8s \
	docker-build docker-build-base docker-build-master docker-build-master-base \
	docker-build-agent-runner docker-build-agent-base docker-build-agent-production \
	docker-pre-download-libreoffice docker-clean-libreoffice-downloads \
	update-image-tag \
	test test-unit test-integration test-all test-blocking \
	test-ebpf-install test-ebpf-no-install test-ebpf-debug test-pyroscope-offcpu \
	pyroscope-up pyroscope-down pyroscope-logs \
	k8s-offline-bundle k8s-offline-import k8s-offline-images-list k8s-offline-clean

# 包含子 Makefile
include make/docker.mk
include make/libreoffice.mk
include make/dev.mk
include make/k8s.mk
include make/k8s-offline.mk
include make/test.mk
include make/pyroscope.mk

# 本地编译（仅编译，不构建镜像）
build:
	@echo "🔨 本地编译 rcoder..."
	@cargo build --release --bin rcoder
	@echo "✅ 编译完成！"
	@echo "可执行文件: ./target/release/rcoder"

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

# 默认目标：显示帮助信息
help:
	@echo "rcoder 开发模式 Makefile"
	@echo ""
	@echo "📦 编译和安装："
	@echo "  make build          - 本地编译 rcoder（仅编译）"
	@echo "  make install        - 安装所有二进制到 ~/.cargo/bin/"
	@echo "  make install-agent  - 仅安装 codex-acp-agent"
	@echo "  make uninstall      - 卸载所有二进制"
	@echo ""
	@echo "🐳 Docker 镜像构建："
	@echo "  make docker-build                 - 构建所有 Docker 镜像"
	@echo "  make docker-build-base           - 构建所有基础镜像（很少需要）"
	@echo "  make docker-build-master         - 仅构建 master-rcoder 镜像（需要基础镜像）"
	@echo "  make docker-build-master-base    - 仅构建 master-rcoder-base 基础镜像"
	@echo "  make docker-build-agent-runner  - 仅构建 rcoder-agent-runner 镜像（启用 eBPF 调试）"
	@echo "  make docker-build-agent-base     - 仅构建 rcoder-agent-base 基础镜像"
	@echo "  make docker-build-agent-production - 构建生产镜像（无 eBPF 工具，镜像更小）"
	@echo "  make docker-pre-download-libreoffice - 预下载 LibreOffice（避免每次构建重新下载）"
	@echo "  make docker-clean-libreoffice-downloads - 清理已下载的 LibreOffice 文件"
	@echo "  make dev-build                    - 本地编译 + 构建 Docker 镜像（一键完成）"
	@echo ""
	@echo "🔧 开发模式命令："
	@echo "  make dev-up         - 启动开发模式容器（使用镜像内编译的二进制）"
	@echo "  make dev-restart    - 重启开发模式容器（重新构建镜像并启动）"
	@echo "  make dev-down       - 停止开发模式容器"
	@echo "  make dev-logs       - 查看开发模式容器日志"
	@echo ""
	@echo "☸️  K8s 开发模式命令："
	@echo "  make dev-build-k8s  - 构建 K8s 镜像（启用 kubernetes feature）"
	@echo "  make dev-up-k8s    - 启动 K8s 开发模式（标准 kubectl apply）"
	@echo "  make dev-restart-k8s - 重启 K8s 开发模式（重新构建镜像+部署）"
	@echo "  make dev-down-k8s  - 停止 K8s 开发模式（存储层 + 应用层 + RBAC + Namespace）"
	@echo "  make dev-logs-k8s  - 查看 K8s 开发模式日志"
	@echo ""
	@echo "📦 离线部署 (政企内网用):"
	@echo "  make k8s-offline-bundle        - 打包完整离线包 (images + helm + manifests)"
	@echo "  make k8s-offline-import BUNDLE=<tgz> INSTALL_ARGS=\"--mode=direct --env=dev\""
	@echo "                                 - 解压并一键部署 (在离线机器上跑)"
	@echo "  make k8s-offline-images-list  - 打印所有离线依赖镜像清单"
	@echo "  make k8s-offline-clean        - 清理构建产物"
	@echo ""
	@echo "📊 Pyroscope 持续剖析："
	@echo "  make pyroscope-up   - 启动 Pyroscope Server"
	@echo "  make pyroscope-down - 停止 Pyroscope Server"
	@echo "  make pyroscope-logs - 查看 Pyroscope 日志"
	@echo ""
	@echo "🧪 测试命令："
	@echo "  make test           - 运行所有测试"
	@echo "  make test-unit      - 运行单元测试"
	@echo "  make test-integration - 运行集成测试"
	@echo "  make test-blocking  - 运行极端场景测试（包含阻塞）"
	@echo "  make test-all       - 运行完整测试套件（所有 features）"
	@echo ""
	@echo "开发模式工作流程："
	@echo "  1. make dev-build    # 首次：构建所有 Docker 镜像（容器内编译）"
	@echo "  2. make dev-up       # 启动容器"
	@echo "  3. 修改代码后: make dev-restart  # 重新构建镜像+重启容器"
	@echo ""
	@echo "💡 提示："
	@echo "  - dev-build: 在 Docker 容器内编译，确保 Linux 兼容性"
	@echo "  - dev-restart: 每次代码修改都需要重新构建镜像（容器内重新编译）"
