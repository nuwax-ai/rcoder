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

# deploy-host 宿主机形态严格 E2E（前置：make dev-host 已运行）
.PHONY: test-e2e-host
test-e2e-host:
	python3 tests-e2e/tools/run.py --group host

# deploy-host Direct 直拨形态严格 E2E（前置：make dev-host-direct 已运行——
# RCODER_DEPLOY_HOST_REACH=direct 的宿主机 rcoder；反向断言 fail-loud 模式门）
.PHONY: test-e2e-host-direct
test-e2e-host-direct:
	python3 tests-e2e/tools/run.py --group host_direct

# deploy-host 宿主机 K8s 形态严格 E2E（前置：CONTAINER_RUNTIME=kubernetes 的宿主机 rcoder）
.PHONY: test-e2e-host-k8s
test-e2e-host-k8s:
	python3 tests-e2e/tools/run.py --group host_k8s

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
