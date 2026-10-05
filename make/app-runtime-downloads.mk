# ============================================================================
# app-runtime-base 预下载链路（对齐生产仓 build-agent-docker 的
# download-ttyd / download-node / download-deno / download-go-cache）
#
# 服务 docker-build-app-runtime-base：docker/app-runtime-base/Dockerfile 需要
#   downloads/ttyd-${TARGETARCH}          —— 本文件 download-ttyd
#   cache/pingap-v*-linux-gnu-*-full.tar.gz —— make/docker.mk download-pingap-cache
#   cache/node-v*-linux-<arch>.tar.gz     —— 本文件 download-node
#   cache/deno-<amd64|arm64>              —— 本文件 download-deno
#   cache/go<ver>.linux-<arch>.tar.gz     —— 本文件 download-go-cache
#
# 版本与生产仓、Dockerfile 内钉死值对齐；修改版本时同步
# docker/app-runtime-base/Dockerfile 的 ENV（GO_VERSION 精确匹配文件名）。
# 缓存统一放仓内 .cache/（已 gitignore，可弃）；分发产物在构建上下文内。
# ============================================================================

# ttyd Web 终端（prebuilt 单二进制, GitHub release; 容器内 curl release 不稳,
# 改宿主预下载 COPY 进镜像）。与 Dockerfile ENV TTYD_VERSION 对齐。
TTYD_VERSION ?= 1.7.7
TTYD_CACHE_DIR := .cache/ttyd
TTYD_URL_BASE := https://github.com/tsl0922/ttyd/releases/download/$(TTYD_VERSION)

# Node prebuilt（npmmirror; Dockerfile 通配 node-v*-linux-<arch>.tar.gz 解压）
# ⚠️ 分发前必须清同架构旧版: 消费方 Dockerfile 为通配 COPY + 通配解压,
# 新旧并存时 tar 把第二个文件当压缩包成员名 → "Not found in archive" 构建失败。
NODE_RUNTIME_VERSION ?= 22.23.2
NODE_CACHE_DIR := .cache/node

# Deno prebuilt（deno.land/install.sh 国内不稳, 改预下载 npmmirror; zip 解压出单二进制 deno-<arch>）
DENO_VERSION ?= 2.9.7
DENO_CACHE_DIR := .cache/deno

# Go tarball（golang.google.cn 主, aliyun 备; Dockerfile 按精确文件名 go${GO_VERSION} 解压）
# 必须与 docker/app-runtime-base/Dockerfile ENV GO_VERSION 一致。
GO_VERSION ?= 1.26.4
GO_CACHE_DIR := .cache/go

APP_RUNTIME_DOWNLOADS := docker/app-runtime-base/downloads
APP_RUNTIME_CACHE := docker/app-runtime-base/cache

.PHONY: download-ttyd download-ttyd-amd64 download-ttyd-arm64 download-node download-deno download-go-cache

download-ttyd: download-ttyd-amd64 download-ttyd-arm64
	@echo "✅ ttyd $(TTYD_VERSION) 就绪: $(APP_RUNTIME_DOWNLOADS)"

download-ttyd-amd64:
	@mkdir -p $(TTYD_CACHE_DIR) $(APP_RUNTIME_DOWNLOADS)
	@BIN="$(TTYD_CACHE_DIR)/ttyd-amd64"; \
	if [ ! -x "$$BIN" ]; then \
		echo "↓ 下载 ttyd $(TTYD_VERSION) amd64 (gh-proxy 主源, GitHub 备源)..."; \
		curl -fsSL --retry 3 --retry-delay 3 -o "$$BIN" "https://gh-proxy.org/$(TTYD_URL_BASE)/ttyd.x86_64" \
			|| curl -fsSL --retry 3 --retry-delay 3 -o "$$BIN" "$(TTYD_URL_BASE)/ttyd.x86_64" \
			|| { rm -f "$$BIN"; echo "❌ ttyd amd64 下载失败"; exit 1; }; \
		chmod +x "$$BIN"; \
	fi; \
	cp "$$BIN" "$(APP_RUNTIME_DOWNLOADS)/ttyd-amd64"; \
	echo "✅ ttyd amd64 分发到 $(APP_RUNTIME_DOWNLOADS)"

download-ttyd-arm64:
	@mkdir -p $(TTYD_CACHE_DIR) $(APP_RUNTIME_DOWNLOADS)
	@BIN="$(TTYD_CACHE_DIR)/ttyd-arm64"; \
	if [ ! -x "$$BIN" ]; then \
		echo "↓ 下载 ttyd $(TTYD_VERSION) arm64 (gh-proxy 主源, GitHub 备源)..."; \
		curl -fsSL --retry 3 --retry-delay 3 -o "$$BIN" "https://gh-proxy.org/$(TTYD_URL_BASE)/ttyd.aarch64" \
			|| curl -fsSL --retry 3 --retry-delay 3 -o "$$BIN" "$(TTYD_URL_BASE)/ttyd.aarch64" \
			|| { rm -f "$$BIN"; echo "❌ ttyd arm64 下载失败"; exit 1; }; \
		chmod +x "$$BIN"; \
	fi; \
	cp "$$BIN" "$(APP_RUNTIME_DOWNLOADS)/ttyd-arm64"; \
	echo "✅ ttyd arm64 分发到 $(APP_RUNTIME_DOWNLOADS)"

download-node:
	@mkdir -p $(NODE_CACHE_DIR) $(APP_RUNTIME_CACHE)
	@for arch in x64 arm64; do \
		FILE="$(NODE_CACHE_DIR)/node-v$(NODE_RUNTIME_VERSION)-linux-$$arch.tar.gz"; \
		if [ ! -f "$$FILE" ]; then \
			echo "↓ 下载 Node $(NODE_RUNTIME_VERSION) $$arch (npmmirror)..."; \
			curl -fsSL --retry 3 --retry-delay 5 -o "$$FILE" \
				"https://registry.npmmirror.com/-/binary/node/v$(NODE_RUNTIME_VERSION)/node-v$(NODE_RUNTIME_VERSION)-linux-$$arch.tar.gz" \
				|| { echo "❌ 下载 Node $$arch 失败"; rm -f "$$FILE"; exit 1; }; \
		else echo "✓ Node $(NODE_RUNTIME_VERSION) $$arch 已缓存"; fi; \
		rm -f $(APP_RUNTIME_CACHE)/node-v*-linux-$$arch.tar.gz; \
		cp "$$FILE" "$(APP_RUNTIME_CACHE)/"; \
	done
	@find $(NODE_CACHE_DIR) -name 'node-v*.tar.gz' ! -name 'node-v$(NODE_RUNTIME_VERSION)-*' -delete
	@echo "✅ Node $(NODE_RUNTIME_VERSION) 分发到 $(APP_RUNTIME_CACHE)（旧版已清）"

download-deno:
	@mkdir -p $(DENO_CACHE_DIR) $(APP_RUNTIME_CACHE)
	@for pair in "x86_64 amd64" "aarch64 arm64"; do \
		set -- $$pair; SRC=$$1; DST=$$2; \
		ZIP="$(DENO_CACHE_DIR)/deno-v$(DENO_VERSION)-linux-$$SRC.zip"; \
		BIN="$(DENO_CACHE_DIR)/deno-$$DST"; \
		if [ ! -f "$$BIN" ]; then \
			echo "↓ 下载 Deno $(DENO_VERSION) $$DST (npmmirror)..."; \
			curl -fsSL --retry 3 --retry-delay 5 -o "$$ZIP" \
				"https://registry.npmmirror.com/-/binary/deno/v$(DENO_VERSION)/deno-$$SRC-unknown-linux-gnu.zip" \
				|| { echo "❌ 下载 Deno $$DST 失败"; rm -f "$$ZIP"; exit 1; }; \
			unzip -o "$$ZIP" -d "$(DENO_CACHE_DIR)" >/dev/null; \
			install -m 0755 "$(DENO_CACHE_DIR)/deno" "$$BIN"; \
			rm -f "$(DENO_CACHE_DIR)/deno" "$$ZIP"; \
		else echo "✓ Deno $(DENO_VERSION) $$DST 已缓存"; fi; \
		cp "$$BIN" "$(APP_RUNTIME_CACHE)/deno-$$DST"; \
	done
	@echo "✅ Deno $(DENO_VERSION) 分发到 $(APP_RUNTIME_CACHE)"

download-go-cache:
	@mkdir -p $(GO_CACHE_DIR) $(APP_RUNTIME_CACHE)
	@for arch in amd64 arm64; do \
		FILE="$(GO_CACHE_DIR)/go$(GO_VERSION).linux-$$arch.tar.gz"; \
		if [ ! -f "$$FILE" ]; then \
			echo "↓ 下载 Go $(GO_VERSION) $$arch (golang.google.cn 主, aliyun 备)..."; \
			curl -fsSL --retry 5 --retry-delay 5 -o "$$FILE" "https://golang.google.cn/dl/go$(GO_VERSION).linux-$$arch.tar.gz" \
				|| curl -fsSL --retry 3 --retry-delay 5 -o "$$FILE" "https://mirrors.aliyun.com/golang/go$(GO_VERSION).linux-$$arch.tar.gz" \
				|| { echo "❌ 下载 Go $$arch 失败"; rm -f "$$FILE"; exit 1; }; \
		else echo "✓ Go $(GO_VERSION) $$arch 已缓存"; fi; \
		rm -f $(APP_RUNTIME_CACHE)/go*.linux-$$arch.tar.gz; \
		cp "$$FILE" "$(APP_RUNTIME_CACHE)/"; \
	done
	@echo "✅ Go $(GO_VERSION) 分发到 $(APP_RUNTIME_CACHE)（旧版已清）"
