# ============================================================================
# dbx-web 本地构建（nuwax-ai/dbx fork，test 分支）
# 对齐生产仓 build-agent-docker makefiles/16-app-runtime.mk 的 build-dbx-fork：
# 本仓直接拉 fork 源码构建 dbx-web 双架构，不再从生产仓 build_config 复制。
# fork 改动: connections.json 导入改 upsert 语义（按 id 覆盖吸收 + 播种
# connection_secrets）——rcoder 改密链（align-credentials/reset-password）重写
# connections.json + 重启 dbx 即完成 local-pg 预置连接同步。dev 与生产必须同源，
# 故 fork repo/branch 与生产仓保持一致。
#
# 持久化根（DBX_PERSIST_ROOT）与生产仓同一处：fork/分支相同 → 产物等价，跳过
# 守卫互通——任一仓构建过且 commit 未变时，另一仓秒级跳过交叉构建仅重新分发。
# 源码缓存（可弃的纯镜像）留在仓库内 .cache/（已 gitignore）；编译产物(stage)
# 与跳过戳记在持久根，git clean/checkout 不炸缓存（与生产仓同策略）。
# ============================================================================

# gh 镜像包装（与生产仓 01-init.mk wrap-github-url 同实现；非 github.com 地址
# 原样使用，不会被拼出 gh-proxy/https://github.com/<镜像URL> 的怪地址）
USE_GITHUB_MIRROR ?= false
GITHUB_MIRROR_URL ?= https://gh-proxy.org/https://github.com/
wrap-github-url = $(if $(findstring github.com,$(1)),$(if $(findstring true,$(USE_GITHUB_MIRROR)),$(GITHUB_MIRROR_URL)$(subst https://github.com/,,$(1)),$(1)),$(1))

DBX_FORK_REPO ?= https://github.com/nuwax-ai/dbx.git
DBX_FORK_BRANCH ?= test
DBX_FORK_CACHE := .cache/dbx-fork-src
DBX_PERSIST_ROOT ?= $(HOME)/.cache/nuwax-build/dbx
DBX_STAGE ?= $(DBX_PERSIST_ROOT)/stage
# 跳过守卫戳记 "<branch> <commit>": 全量构建+分发成功后写入; commit 未变且持久
# stage 仍是 fork 构建(含 ensure-local-pg)时 build-dbx-fork 直接跳过 20-40min/架构
# 的交叉构建(仅重新分发 stage→downloads, 秒级)。强制重建: FORCE_DBX_FORK=1
DBX_FORK_STAMP ?= $(DBX_PERSIST_ROOT)/fork.stamp
DBX_FORK_IMAGE := dbx-fork-local
# deploy/Dockerfile build 阶段内 pip（ziglang）默认 pypi.org——国内网络不可达，注入镜像源
DBX_FORK_PIP_INDEX ?= https://mirrors.aliyun.com/pypi/simple
DBX_FORK_REPO_WRAPPED := $(call wrap-github-url,$(DBX_FORK_REPO))

# 分发目标：本仓两个镜像构建上下文的 downloads/（Dockerfile COPY 的输入）
DBX_CONTEXTS := docker/rcoder-agent-runner/downloads docker/app-runtime-base/downloads

# buildx builder（fork 构建不依赖本地基础镜像，docker/driver 均可）：
#   macOS + OrbStack: orbstack builder（OrbStack 要求 builder 与 context 一致）
#   Linux: default（host daemon 的 docker driver builder）
ifeq ($(shell uname -s),Darwin)
  DBX_BUILDER ?= orbstack
else
  DBX_BUILDER ?= default
endif

.PHONY: update-dbx-fork build-dbx-fork build-dbx-fork-real distribute-dbx-stage clean-dbx-fork-cache

# 拉取/更新 fork 源码缓存（幂等; 分支不匹配删缓存重 clone——shallow clone 无
# 其他分支 ref）。fetch 失败即中止: 自动缓存语义=跟远端最新, 静默用旧缓存构建
# 比失败更危险。reset 后 clean -fdx 清 untracked/ignored 残留——工作树恒 ≡
# 全新 clone。缓存是纯镜像(depth-1 无 merge base), clean 无误伤; .git 级损坏
# （fetch/reset 报错）才需要手动 make clean-dbx-fork-cache。
# 尾部注入 .dockerignore（上游仓没有）: 排除 .git/docs 等未被 COPY 的死重。
update-dbx-fork:
	@if [ -d "$(DBX_FORK_CACHE)/.git" ] \
		&& [ "$$(git -C $(DBX_FORK_CACHE) rev-parse --abbrev-ref HEAD 2>/dev/null)" != "$(DBX_FORK_BRANCH)" ]; then \
		echo "→ dbx fork 缓存分支不匹配, 重建为 $(DBX_FORK_BRANCH)..."; \
		rm -rf $(DBX_FORK_CACHE); \
	fi
	@if [ ! -d "$(DBX_FORK_CACHE)/.git" ]; then \
		echo "→ clone dbx fork $(DBX_FORK_REPO_WRAPPED) ($(DBX_FORK_BRANCH), 主源; 失败回退直连)..."; \
		git clone --depth 1 -b $(DBX_FORK_BRANCH) $(DBX_FORK_REPO_WRAPPED) $(DBX_FORK_CACHE) \
			|| git clone --depth 1 -b $(DBX_FORK_BRANCH) $(DBX_FORK_REPO) $(DBX_FORK_CACHE) \
			|| { echo "❌ dbx fork clone 失败; 可手动: git clone -b $(DBX_FORK_BRANCH) $(DBX_FORK_REPO) $(DBX_FORK_CACHE)"; exit 1; }; \
	else \
		echo "→ 更新 dbx fork 缓存 ($(DBX_FORK_BRANCH))..."; \
		CUR_URL=$$(git -C $(DBX_FORK_CACHE) config --get remote.origin.url 2>/dev/null || true); \
		if [ "$$CUR_URL" != "$(DBX_FORK_REPO_WRAPPED)" ]; then \
			echo "→ 缓存 origin 是旧源 ($$CUR_URL), 改指 $(DBX_FORK_REPO_WRAPPED)"; \
			git -C $(DBX_FORK_CACHE) remote set-url origin $(DBX_FORK_REPO_WRAPPED) || exit 1; \
		fi; \
		git -C $(DBX_FORK_CACHE) fetch --depth 1 origin $(DBX_FORK_BRANCH) \
			&& git -C $(DBX_FORK_CACHE) reset --hard origin/$(DBX_FORK_BRANCH) \
			&& git -C $(DBX_FORK_CACHE) clean -fdx; \
	fi
	@printf '%s\n' '.git' '**/target' '**/node_modules' \
		'docs' 'examples' '.github' 'agents' > $(DBX_FORK_CACHE)/.dockerignore

# 只清源码缓存（.git 损坏等）; 不清分发产物与跳过守卫戳记——产物仍对应当前
# commit 时 build-dbx-fork 会继续跳过（语义正确: 源码缓存 ≠ 产物失效）。
clean-dbx-fork-cache:
	@rm -rf $(DBX_FORK_CACHE)
	@echo "✅ dbx fork 源码缓存已清理: $(DBX_FORK_CACHE)"

# 跳过守卫: commit 未变 + 持久 stage 仍是 fork 构建(ensure-local-pg 特征串) →
# 跳过交叉构建, 仅重新分发; 否则转内部全量目标。
build-dbx-fork: update-dbx-fork
	@COMMIT=$$(git -C $(DBX_FORK_CACHE) rev-parse HEAD); \
	SHORT=$$(git -C $(DBX_FORK_CACHE) rev-parse --short HEAD); \
	GUARD_OK=1; \
	if [ "$(FORCE_DBX_FORK)" = "1" ]; then \
		echo "→ FORCE_DBX_FORK=1, 强制重建 dbx fork"; \
		GUARD_OK=0; \
	elif [ ! -f "$(DBX_FORK_STAMP)" ] \
		|| [ "$$(cat $(DBX_FORK_STAMP) 2>/dev/null)" != "$(DBX_FORK_BRANCH) $$COMMIT" ]; then \
		GUARD_OK=0; \
	else \
		[ -d "$(DBX_STAGE)/dbx-static" ] \
			&& grep -aq ensure-local-pg "$(DBX_STAGE)/dbx-web-amd64" \
			&& grep -aq ensure-local-pg "$(DBX_STAGE)/dbx-web-arm64" \
			|| { GUARD_OK=0; echo "→ $(DBX_STAGE) 产物缺失/非 fork 构建, 需重建"; }; \
	fi; \
	if [ "$$GUARD_OK" = "1" ]; then \
		echo "✓ dbx fork 无更新（$(DBX_FORK_BRANCH) @ $$SHORT）, 跳过构建（仅重新分发; 强制重建: make build-dbx-fork FORCE_DBX_FORK=1）"; \
		$(MAKE) distribute-dbx-stage; \
	else \
		$(MAKE) build-dbx-fork-real; \
	fi

# 从持久 stage 分发到本仓各构建上下文 downloads/（Dockerfile COPY downloads/dbx-web-*
# 的输入）。守卫"跳过"时也必须跑本目标, 否则 Dockerfile 的 COPY 直接失败。
distribute-dbx-stage:
	@for ctx in $(DBX_CONTEXTS); do \
		mkdir -p "$$ctx"; \
		cp $(DBX_STAGE)/dbx-web-amd64 $(DBX_STAGE)/dbx-web-arm64 "$$ctx"/; \
		rm -rf "$$ctx"/dbx-static; \
		cp -R $(DBX_STAGE)/dbx-static "$$ctx"/dbx-static; \
	done
	@echo "✅ dbx-web 已从 $(DBX_STAGE) 分发到构建上下文 downloads/"

# 内部目标: 全量构建+分发, 勿直接调（走 build-dbx-fork 守卫）。deploy/Dockerfile
# build 阶段全 --platform=$BUILDPLATFORM（rust 原生交叉，无 QEMU 仿真）。
# OrbStack docker driver 的 BuildKit 偶发 frontend 容器启动即崩（速败, 层缓存
# 完好）: <60s 自动重试至多 3 次; 慢失败(如编译错)不重试。
build-dbx-fork-real: update-dbx-fork
	@echo "🔨 构建 dbx-web（fork $(DBX_FORK_BRANCH) @ $$(git -C $(DBX_FORK_CACHE) rev-parse --short HEAD), rust 交叉全量约 20-40min/架构）"
	@cd $(DBX_FORK_CACHE) && for arch in arm64 amd64; do \
		ok=0; \
		for attempt in 1 2 3; do \
			t0=$$(date +%s); \
			docker buildx build --builder $(DBX_BUILDER) --platform linux/$$arch \
				-f deploy/Dockerfile -t $(DBX_FORK_IMAGE):$$arch \
				--build-arg PIP_INDEX_URL=$(DBX_FORK_PIP_INDEX) --load . \
				&& { ok=1; break; }; \
			elapsed=$$(( $$(date +%s) - t0 )); \
			if [ $$elapsed -gt 60 ]; then break; fi; \
			echo "⚠️ dbx fork $$arch 第 $$attempt 次构建失败（$${elapsed}s 速败, 疑 OrbStack BuildKit 偶发 frontend grpc 崩溃）, 重试..."; \
			sleep 3; \
		done; \
		[ "$$ok" = "1" ] || { echo "❌ dbx fork $$arch 构建失败"; exit 1; }; \
	done
	@mkdir -p $(DBX_STAGE)
	@for arch in arm64 amd64; do \
		CID=$$(docker create --platform linux/$$arch $(DBX_FORK_IMAGE):$$arch); \
		docker cp "$$CID":/usr/local/bin/dbx-web $(DBX_STAGE)/dbx-web-$$arch \
			|| { docker rm -f "$$CID" >/dev/null 2>&1; echo "❌ 抽取 fork dbx-web 二进制失败"; exit 1; }; \
		if [ "$$arch" = "arm64" ]; then \
			rm -rf $(DBX_STAGE)/dbx-static.tmp $(DBX_STAGE)/dbx-static; \
			docker cp "$$CID":/app/static $(DBX_STAGE)/dbx-static.tmp \
				&& mv $(DBX_STAGE)/dbx-static.tmp $(DBX_STAGE)/dbx-static; \
		fi; \
		docker rm "$$CID" >/dev/null; \
		chmod +x $(DBX_STAGE)/dbx-web-$$arch; \
	done
	@grep -aq "ensure-local-pg" $(DBX_STAGE)/dbx-web-arm64 \
		&& grep -aq "ensure-local-pg" $(DBX_STAGE)/dbx-web-amd64 \
		|| { echo "❌ fork 特征串缺失（local_pg.rs 未编入——构建了上游代码？）"; exit 1; }
	$(MAKE) distribute-dbx-stage
	@printf '%s %s\n' '$(DBX_FORK_BRANCH)' "$$(git -C $(DBX_FORK_CACHE) rev-parse HEAD)" > $(DBX_FORK_STAMP)
	@echo "✅ fork dbx-web 双架构 + 静态前端 已分发（ensure-local-pg 特征串已验证, 跳过守卫戳记已更新: $(DBX_FORK_STAMP)）"
