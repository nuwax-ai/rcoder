# ============================================================================
# Rust 测试：nextest 为默认入口；严格业务 E2E 使用下方独立启动器。
# TEST_FEATURES= 可验证默认 features；NEXTEST_ARGS 可传 -p / -E 等筛选。
# 不要并发执行共享 Cargo target 的多个 Make 测试目标。
# ============================================================================
TEST_FEATURES ?= --all-features
NEXTEST_ARGS ?=

.PHONY: test test-default test-unit test-integration test-blocking test-app-cli test-doc test-all

test:
	cargo nextest run --workspace --no-fail-fast $(TEST_FEATURES) $(NEXTEST_ARGS)

test-default:
	cargo nextest run --workspace --no-fail-fast $(NEXTEST_ARGS)

test-unit:
	cargo nextest run --workspace --lib --no-fail-fast $(TEST_FEATURES) $(NEXTEST_ARGS)

# Rust integration test targets；不是 Compose 核心业务 E2E。
test-integration:
	cargo nextest run --workspace --test '*' --no-fail-fast $(TEST_FEATURES) $(NEXTEST_ARGS)

test-blocking:
	cargo nextest run --workspace --features testing --test '*_blocking*' --no-fail-fast --test-threads=1 $(NEXTEST_ARGS)

# app-cli 被根 workspace 排除，必须单独运行。
test-app-cli:
	cargo nextest run --manifest-path crates/app-cli/Cargo.toml --no-fail-fast $(TEST_FEATURES) $(NEXTEST_ARGS)

# nextest 不覆盖 doctest；两部分串行执行，任一失败整体失败。
test-doc:
	@status=0; \
	cargo test --workspace --doc --no-fail-fast $(TEST_FEATURES) || status=1; \
	cargo test --manifest-path crates/app-cli/Cargo.toml --doc --no-fail-fast $(TEST_FEATURES) || status=1; \
	exit $$status

# Rust 完整检查：即便某部分失败仍收集其他部分结果；不隐式部署或运行 E2E。
# 显式 recipe 保证 make -j 时各子步骤仍串行。
test-all:
	@status=0; \
	$(MAKE) test || status=1; \
	$(MAKE) test-app-cli || status=1; \
	$(MAKE) test-doc || status=1; \
	exit $$status

# ============================================================================
# Rust e2e 黑盒集成测试（tests-e2e crate；JSONL 报告供 agent 追溯）
# ============================================================================

# 显式入口严格验收：缺少前置、skip、aborted、空筛选和报告遗漏均失败。
# E2E_SUITE=compose_userapp_dev E2E_FILTER=case_name 可聚焦；无环境 workspace 测试仍可 skip。
.PHONY: test-e2e test-e2e-compose test-e2e-compose-deploy test-e2e-k8s

test-e2e:
	python3 tests-e2e/tools/run.py --group userapp

test-e2e-compose:
	python3 tests-e2e/tools/run.py --group compose

test-e2e-compose-deploy:
	python3 tests-e2e/tools/run.py --group deploy

# 仅显式选择测试集群。
test-e2e-k8s:
	python3 tests-e2e/tools/run.py --group k8s $(if $(RUN_LB),--ignored,)

# Real K8s userApp acceptance; explicit NodePort URLs and SSH test target required.
.PHONY: test-e2e-k8s-userapp
test-e2e-k8s-userapp:
	python3 tests-e2e/tools/k8s_userapp.py --ssh "$(TEST_K8S_SSH)" --url "$(RCODER_URL)" --proxy-url "$(E2E_PINGORA_URL)"

# deploy-host 宿主机 Published 形态严格 E2E（前置：make dev-host-published 已运行）
.PHONY: test-e2e-host
test-e2e-host:
	python3 tests-e2e/tools/run.py --group host

.PHONY: test-e2e-host-userapp
test-e2e-host-userapp:
	E2E_FILTER=host_userapp_dev_compute_no_llm python3 tests-e2e/tools/run.py --group host

# deploy-host Direct 直拨形态严格 E2E（前置：make dev-host-direct 已运行——
# RCODER_DEPLOY_HOST_REACH=direct 的宿主机 rcoder；反向断言 fail-loud 模式门）
.PHONY: test-e2e-host-direct
test-e2e-host-direct:
	python3 tests-e2e/tools/run.py --group host_direct

# deploy-host 宿主机 K8s 形态严格 E2E（前置：CONTAINER_RUNTIME=kubernetes 的宿主机 rcoder）
.PHONY: test-e2e-host-k8s
test-e2e-host-k8s:
	python3 tests-e2e/tools/run.py --group host_k8s

.PHONY: test-e2e-host-k8s-userapp
test-e2e-host-k8s-userapp:
	E2E_FILTER=host_k8s_userapp_dev_compute_no_llm python3 tests-e2e/tools/run.py --group host_k8s

# e2e 可用性辅助：场景清单（含最近一次 verdict/耗时）/ 三份登记一致性秒级校验
.PHONY: test-e2e-list test-e2e-check
test-e2e-list:
	python3 tests-e2e/tools/run.py --list
test-e2e-check:
	python3 tests-e2e/tools/run.py --check-registry

# e2e 报告目录修剪：默认 dry-run 只列清单；APPLY=1 执行删除。
# DAYS/KEEP 可覆盖（默认 14 天 / 至少保留 30 个最新 run）；DEDUPE=1 额外把
# 各 run 的 bin/ 副本收敛为 _bin/ 内容寻址硬链接；_bin 按剩余 run 引用 GC。
.PHONY: test-e2e-prune
test-e2e-prune:
	python3 tests-e2e/tools/prune_reports.py --days "$${DAYS:-14}" --keep "$${KEEP:-30}" $(if $(APPLY),--apply) $(if $(DEDUPE),--dedupe)

# ============================================================================
# app-cli 运行恢复专项（recovery v2 plan §11：本地 app-runtime 故障矩阵）
# ============================================================================
# 一次构建当前 Linux 二进制，真实容器内注入 A/B/C/G/J/I/D/E 场景：
# 并发调用、owner 强杀/TERM、pkill 全部同名进程、挂死边界（SIGSTOP）、
# app-11 升级 fixture、同容器重启与同卷重建。不含 LLM/RCoder 控制面。
# 不清理工作卷（保留检查）；仅移除自建容器。ARCH 缺省取本机。
.PHONY: test-e2e-app-cli-recovery test-e2e-app-cli-recovery-build

APP_CLI_RECOVERY_ARCH ?= $(shell uname -m | sed 's/arm64/aarch64/;s/x86_64/x86_64/')
APP_CLI_RECOVERY_TARGET ?= $(APP_CLI_RECOVERY_ARCH)-unknown-linux-gnu.2.17
APP_CLI_RECOVERY_REPORT ?= tests-e2e/reports/app-cli-recovery-$(shell date +%Y%m%d-%H%M%S).json

# Linux 二进制（zigbuild，glibc 2.17 兼容 agent-runner 镜像）。
test-e2e-app-cli-recovery-build:
	@set -eu; \
	echo "🔨 构建 Linux app-cli / file-server-proxy ($(APP_CLI_RECOVERY_TARGET))"; \
	mkdir -p tests-e2e/reports/_bin; \
	cargo zigbuild --release --manifest-path crates/app-cli/Cargo.toml \
	  --target $(APP_CLI_RECOVERY_TARGET) --bin app-cli; \
	cargo zigbuild --release -p file-server-proxy \
	  --target $(APP_CLI_RECOVERY_TARGET); \
	cp crates/app-cli/target/$(APP_CLI_RECOVERY_ARCH)-unknown-linux-gnu/release/app-cli tests-e2e/reports/_bin/app-cli-linux; \
	cp target/$(APP_CLI_RECOVERY_ARCH)-unknown-linux-gnu/release/file-server-proxy tests-e2e/reports/_bin/file-server-proxy-linux

test-e2e-app-cli-recovery: test-e2e-app-cli-recovery-build
	python3 tests-e2e/tools/app_cli_recovery.py \
	  --app-cli tests-e2e/reports/_bin/app-cli-linux \
	  --file-server-proxy tests-e2e/reports/_bin/file-server-proxy-linux \
	  --report $(APP_CLI_RECOVERY_REPORT)
	@echo "📋 报告: $(APP_CLI_RECOVERY_REPORT)"

# ============================================================================
# app-cli K8s 实机恢复实验（recovery v2 plan §11.2/§11.3：RBD 锁链 + 同 Pod 重启）
# ============================================================================
# 真实集群（默认 k3s-131 个人测试集群）：ceph-rbd RWO 卷上两个同节点 Pod 经
# app-cli 真实 owner 获取链争锁/SIGKILL 释放/角色互换/跨节点卸载挂载交接；
# builder 形态 Downward API 绑定 Pod 的同容器原地重启（Pod UID 不变 +
# containerID/restartCount/pid1 纪元变化 + PVC 保留）。
# 凭据不入库：节点 ssh 映射经环境变量注入。
#   export K8S_LOCK_NODES='soddy=192.168.32.131:soddy:<pw> swufe-x10dai=192.168.32.226:swufe:<pw>'
#   export K8S_LOCK_CONTEXT=k3s-131
.PHONY: test-e2e-app-cli-k8s-lock
test-e2e-app-cli-k8s-lock: test-e2e-app-cli-recovery-build
	@set -eu; \
	test -n "$$K8S_LOCK_NODES" || { echo "K8S_LOCK_NODES 未设置（节点 ssh 映射，见上方注释）"; exit 2; }; \
	nodes=""; for m in $$K8S_LOCK_NODES; do nodes="$$nodes --node-ssh $$m"; done; \
	ARCH=$$(uname -m | sed 's/arm64/aarch64/'); \
	if [ "$$ARCH" = "aarch64" ]; then \
	  docker build --platform linux/amd64 -t rcoder/app-cli-recovery-test:local tests-e2e/reports/_k8s-img >/dev/null 2>&1 || \
	  { mkdir -p tests-e2e/reports/_k8s-img; cp crates/app-cli/target/x86_64-unknown-linux-gnu/release/app-cli tests-e2e/reports/_k8s-img/ 2>/dev/null || true; }; \
	fi; \
	echo "⚠️  镜像需为 linux/amd64 并导入集群节点（k3s ctr images import）"; \
	python3 tests-e2e/tools/app_cli_k8s_lock.py \
	  --context $${K8S_LOCK_CONTEXT:-k3s-131} \
	  --image $${K8S_LOCK_IMAGE:-registry.local/app-cli-recovery-test:local} \
	  $$nodes \
	  --report tests-e2e/reports/app-cli-k8s-lock-$$(date +%Y%m%d-%H%M%S).json
