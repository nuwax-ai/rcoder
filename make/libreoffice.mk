# ============================================================================
# LibreOffice 预下载配置（避免每次构建都重新下载）
# ============================================================================
# LibreOffice 版本（必须与 Dockerfile.base 保持一致）
LIBREOFFICE_VERSION := 25.8.5

# 下载目录（相对于项目根目录）
LIBREOFFICE_DOWNLOAD_DIR := docker/rcoder-agent-runner/downloads

# x86_64 下载信息
LIBREOFFICE_X86_FILE := LibreOffice_$(LIBREOFFICE_VERSION)_Linux_x86-64_deb.tar.gz
LIBREOFFICE_X86_URL := https://mirror.csclub.uwaterloo.ca/tdf/libreoffice/stable/$(LIBREOFFICE_VERSION)/deb/x86_64/LibreOffice_$(LIBREOFFICE_VERSION)_Linux_x86-64_deb.tar.gz
LIBREOFFICE_X86_FALLBACK := https://download.documentfoundation.org/libreoffice/stable/$(LIBREOFFICE_VERSION)/deb/x86_64/LibreOffice_$(LIBREOFFICE_VERSION)_Linux_x86-64_deb.tar.gz
LIBREOFFICE_X86_PATH := $(LIBREOFFICE_DOWNLOAD_DIR)/$(LIBREOFFICE_X86_FILE)

# aarch64 下载信息
LIBREOFFICE_ARM_FILE := LibreOffice_$(LIBREOFFICE_VERSION)_Linux_aarch64_deb.tar.gz
LIBREOFFICE_ARM_URL := https://mirror.csclub.uwaterloo.ca/tdf/libreoffice/stable/$(LIBREOFFICE_VERSION)/deb/aarch64/LibreOffice_$(LIBREOFFICE_VERSION)_Linux_aarch64_deb.tar.gz
LIBREOFFICE_ARM_FALLBACK := https://download.documentfoundation.org/libreoffice/stable/$(LIBREOFFICE_VERSION)/deb/aarch64/LibreOffice_$(LIBREOFFICE_VERSION)_Linux_aarch64_deb.tar.gz
LIBREOFFICE_ARM_PATH := $(LIBREOFFICE_DOWNLOAD_DIR)/$(LIBREOFFICE_ARM_FILE)

# docker-build-agent-base 构建时使用的架构（检测宿主 Docker 架构）
# OrbStack on Apple Silicon = linux/arm64, 普通 Linux = linux/amd64
# 注意：docker version --format '{{.Server.Arch}}' 在 OrbStack 上返回 "arm64"，不是 "aarch64"
DOCKER_HOST_ARCH := $(shell docker version --format '{{.Server.Arch}}' 2>/dev/null || echo "amd64")
ifeq ($(DOCKER_HOST_ARCH),arm64)
	LIBREOFFICE_ARCH := arm64
	LIBREOFFICE_FILE := $(LIBREOFFICE_ARM_FILE)
else ifeq ($(DOCKER_HOST_ARCH),aarch64)
	LIBREOFFICE_ARCH := arm64
	LIBREOFFICE_FILE := $(LIBREOFFICE_ARM_FILE)
else
	LIBREOFFICE_ARCH := amd64
	LIBREOFFICE_FILE := $(LIBREOFFICE_X86_FILE)
endif

# ============================================================================
# LibreOffice 预下载目标
# ============================================================================

# 预下载 LibreOffice（下载两个架构的文件，确保 Docker 多平台构建可用）
docker-pre-download-libreoffice:
	@echo "📦 检查并下载 LibreOffice $(LIBREOFFICE_VERSION)..."
	@mkdir -p $(LIBREOFFICE_DOWNLOAD_DIR)
	@# ========== 下载 x86_64 ==========
	@echo ""; echo "=== 检查 x86_64 版本 ==="; \
	if [ -f "$(LIBREOFFICE_X86_PATH)" ] && [ -f "$(LIBREOFFICE_X86_PATH).sha256" ]; then \
		SAVED_HASH=$$(cat "$(LIBREOFFICE_X86_PATH).sha256"); \
		CALC_HASH=$$(sha256sum "$(LIBREOFFICE_X86_PATH)" 2>/dev/null | cut -d' ' -f1); \
		if [ "$$SAVED_HASH" = "$$CALC_HASH" ]; then \
			echo "✅ x86_64 已存在且有效，跳过下载"; \
		else \
			echo "❌ x86_64 Hash 不匹配，重新下载..."; \
			rm -f "$(LIBREOFFICE_X86_PATH)" "$(LIBREOFFICE_X86_PATH).sha256"; \
			echo "📥 下载 x86_64 版本..."; \
			curl -fSL -o "$(LIBREOFFICE_X86_PATH)" "$(LIBREOFFICE_X86_URL)" 2>/dev/null || \
			curl -fSL -L -o "$(LIBREOFFICE_X86_PATH)" "$(LIBREOFFICE_X86_FALLBACK)" 2>/dev/null || { \
				rm -f "$(LIBREOFFICE_X86_PATH)"; echo "❌ x86_64 下载失败"; exit 1; }; \
			CALC_HASH=$$(sha256sum "$(LIBREOFFICE_X86_PATH)" 2>/dev/null | cut -d' ' -f1); \
			echo "$$CALC_HASH" > "$(LIBREOFFICE_X86_PATH).sha256"; \
			echo "✅ x86_64 下载完成: $$CALC_HASH"; \
		fi; \
	else \
		echo "📥 下载 x86_64 版本..."; \
		curl -fSL -o "$(LIBREOFFICE_X86_PATH)" "$(LIBREOFFICE_X86_URL)" 2>/dev/null || \
		curl -fSL -L -o "$(LIBREOFFICE_X86_PATH)" "$(LIBREOFFICE_X86_FALLBACK)" 2>/dev/null || { \
			rm -f "$(LIBREOFFICE_X86_PATH)"; echo "❌ x86_64 下载失败"; exit 1; }; \
		CALC_HASH=$$(sha256sum "$(LIBREOFFICE_X86_PATH)" 2>/dev/null | cut -d' ' -f1); \
		echo "$$CALC_HASH" > "$(LIBREOFFICE_X86_PATH).sha256"; \
		echo "✅ x86_64 下载完成: $$CALC_HASH"; \
	fi
	@# ========== 下载 aarch64 ==========
	@echo ""; echo "=== 检查 aarch64 版本 ==="; \
	if [ -f "$(LIBREOFFICE_ARM_PATH)" ] && [ -f "$(LIBREOFFICE_ARM_PATH).sha256" ]; then \
		SAVED_HASH=$$(cat "$(LIBREOFFICE_ARM_PATH).sha256"); \
		CALC_HASH=$$(sha256sum "$(LIBREOFFICE_ARM_PATH)" 2>/dev/null | cut -d' ' -f1); \
		if [ "$$SAVED_HASH" = "$$CALC_HASH" ]; then \
			echo "✅ aarch64 已存在且有效，跳过下载"; \
		else \
			echo "❌ aarch64 Hash 不匹配，重新下载..."; \
			rm -f "$(LIBREOFFICE_ARM_PATH)" "$(LIBREOFFICE_ARM_PATH).sha256"; \
			echo "📥 下载 aarch64 版本..."; \
			curl -fSL -o "$(LIBREOFFICE_ARM_PATH)" "$(LIBREOFFICE_ARM_URL)" 2>/dev/null || \
			curl -fSL -L -o "$(LIBREOFFICE_ARM_PATH)" "$(LIBREOFFICE_ARM_FALLBACK)" 2>/dev/null || { \
				rm -f "$(LIBREOFFICE_ARM_PATH)"; echo "❌ aarch64 下载失败"; exit 1; }; \
			CALC_HASH=$$(sha256sum "$(LIBREOFFICE_ARM_PATH)" 2>/dev/null | cut -d' ' -f1); \
			echo "$$CALC_HASH" > "$(LIBREOFFICE_ARM_PATH).sha256"; \
			echo "✅ aarch64 下载完成: $$CALC_HASH"; \
		fi; \
	else \
		echo "📥 下载 aarch64 版本..."; \
		curl -fSL -o "$(LIBREOFFICE_ARM_PATH)" "$(LIBREOFFICE_ARM_URL)" 2>/dev/null || \
		curl -fSL -L -o "$(LIBREOFFICE_ARM_PATH)" "$(LIBREOFFICE_ARM_FALLBACK)" 2>/dev/null || { \
			rm -f "$(LIBREOFFICE_ARM_PATH)"; echo "❌ aarch64 下载失败"; exit 1; }; \
		CALC_HASH=$$(sha256sum "$(LIBREOFFICE_ARM_PATH)" 2>/dev/null | cut -d' ' -f1); \
		echo "$$CALC_HASH" > "$(LIBREOFFICE_ARM_PATH).sha256"; \
		echo "✅ aarch64 下载完成: $$CALC_HASH"; \
	fi
	@echo ""; echo "✅ LibreOffice $(LIBREOFFICE_VERSION) 下载检查完成"

# 清理 LibreOffice 下载文件
docker-clean-libreoffice-downloads:
	@echo "🗑️  清理 LibreOffice 下载文件..."
	@rm -rf $(LIBREOFFICE_DOWNLOAD_DIR)
	@echo "✅ LibreOffice 下载文件已清理"

# 构建 agent-base 基础镜像（包含所有系统依赖，很少需要重新构建）
docker-build-agent-base: docker-pre-download-libreoffice
	@echo "🐳 构建 rcoder-agent-base 基础镜像..."
	@echo "📍 镜像名称: rcoder-agent-base:latest"
	@echo "📦 LibreOffice: $(LIBREOFFICE_VERSION) ($(LIBREOFFICE_ARCH))"
	@echo "⏳ 这可能需要较长时间（包含所有系统依赖安装）..."
	@# CACHEBUST_NOVNC: 传入时间戳强制每次重新克隆 noVNC
	@# 使用 --platform 确保 TARGETARCH 正确传递给 Dockerfile.base
	@cd docker/rcoder-agent-runner && \
		docker buildx build --platform linux/$(LIBREOFFICE_ARCH) --load \
		--build-arg LIBREOFFICE_FILE=$(LIBREOFFICE_FILE) \
		--build-arg CACHEBUST_NOVNC=$$(date +%s) \
		-f Dockerfile.base -t rcoder-agent-base:latest .
	@echo "✅ rcoder-agent-base 基础镜像构建完成！"
	@echo "📤 推送基础镜像到阿里云仓库..."
	@docker tag rcoder-agent-base:latest nuwax-docker-images-registry.cn-hangzhou.cr.aliyuncs.com/dev/rcoder-agent-base:latest
	@skopeo copy docker-daemon:nuwax-docker-images-registry.cn-hangzhou.cr.aliyuncs.com/dev/rcoder-agent-base:latest docker://nuwax-docker-images-registry.cn-hangzhou.cr.aliyuncs.com/dev/rcoder-agent-base:latest
	@echo "✅ 基础镜像已推送: nuwax-docker-images-registry.cn-hangzhou.cr.aliyuncs.com/dev/rcoder-agent-base:latest"
	@echo "💡 提示: 平时开发只需运行 make dev-restart，无需重新构建基础镜像"
