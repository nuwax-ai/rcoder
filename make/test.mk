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

# 聚焦真实链：旧 code 管理目录、停服日志、同容器强杀恢复；不跑完整 E2E。
USERAPP_ROOT_LOGS_REPORT ?= tests-e2e/reports/userapp-root-logs-$(shell date +%Y%m%d-%H%M%S).json
.PHONY: test-e2e-userapp-root-logs
USERAPP_ROOT_LOGS_BUILD_SOURCE ?= tests-e2e/reports/userapp-root-logs-source.json
test-e2e-userapp-root-logs:
	python3 tests-e2e/tools/userapp_root_logs.py --write-build-source "$(USERAPP_ROOT_LOGS_BUILD_SOURCE)"
	$(MAKE) test-e2e-app-cli-recovery-build
	python3 tests-e2e/tools/userapp_root_logs.py \
	  --app-cli tests-e2e/reports/_bin/app-cli-linux \
	  --file-server-proxy tests-e2e/reports/_bin/file-server-proxy-linux \
	  --build-source "$(USERAPP_ROOT_LOGS_BUILD_SOURCE)" --report "$(USERAPP_ROOT_LOGS_REPORT)"

test-e2e-app-cli-recovery: test-e2e-app-cli-recovery-build
	python3 tests-e2e/tools/app_cli_recovery.py \
	  --app-cli tests-e2e/reports/_bin/app-cli-linux \
	  --file-server-proxy tests-e2e/reports/_bin/file-server-proxy-linux \
	  --report $(APP_CLI_RECOVERY_REPORT)
	@echo "📋 报告: $(APP_CLI_RECOVERY_REPORT)"

# ============================================================================
# app-cli K8s 实机恢复实验（recovery v2 plan §11.2/§11.3：RBD 锁链 + 同 Pod 重启）
# ============================================================================
# 显式个人集群实验：冻结的单平台镜像与构建回执先准备，不在测试时猜测旧缓存。
# K8S_LOCK_NODE_A/B 必须两个不同 Ready 节点。CephFS-only 不要求节点 SSH。
# all 另含 RBD + 同 Pod 重启，需要 K8S_LOCK_NODE_SSH（首节点免密 SSH 映射）。
.PHONY: test-e2e-app-cli-k8s-lock test-e2e-app-cli-cephfs-lock

test-e2e-app-cli-k8s-lock test-e2e-app-cli-cephfs-lock:
	@set -eu; \
	test -n "$$K8S_LOCK_CONTEXT" || { echo "K8S_LOCK_CONTEXT 未设置"; exit 2; }; \
	test -n "$$K8S_LOCK_IMAGE" || { echo "K8S_LOCK_IMAGE 必须是单平台 @sha256 镜像"; exit 2; }; \
	test -f "$${K8S_LOCK_BUILD_RECEIPT:-}" || { echo "K8S_LOCK_BUILD_RECEIPT 缺失"; exit 2; }; \
	test -n "$$K8S_LOCK_NODE_A" && test -n "$$K8S_LOCK_NODE_B" || { echo "须显式指定两个节点"; exit 2; }; \
	scenario=all; \
	if [ "$@" = "test-e2e-app-cli-cephfs-lock" ]; then scenario=cephfs-lock; fi; \
	mkdir -p tests-e2e/reports; \
	if [ "$$scenario" = "all" ]; then \
	  test -n "$$K8S_LOCK_NODE_SSH" || { echo "all 需要首节点免密 K8S_LOCK_NODE_SSH"; exit 2; }; \
	  set -- --node-ssh "$$K8S_LOCK_NODE_SSH"; \
	else set --; fi; \
	python3 tests-e2e/tools/app_cli_k8s_lock.py \
	  --context "$$K8S_LOCK_CONTEXT" --scenario "$$scenario" \
	  --image "$$K8S_LOCK_IMAGE" --build-receipt "$$K8S_LOCK_BUILD_RECEIPT" \
	  --source-dir "$${E2E_SOURCE_ROOT:-.}" \
	  --node "$$K8S_LOCK_NODE_A" --node "$$K8S_LOCK_NODE_B" \
	  --storage-class "$${K8S_LOCK_RBD_CLASS:-ceph-rbd}" \
	  --cephfs-class "$${K8S_LOCK_CEPHFS_CLASS:-cephfs}" \
	  "$$@" --report "tests-e2e/reports/app-cli-$$scenario-$$(date +%Y%m%d-%H%M%S).json"
