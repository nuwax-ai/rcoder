# Independent opt-in workflow; dotenv is parsed by Python, never sourced by Make.
# 参数在 make 解析期导出（export），recipe 不做任何 shell 插值——SUITE/CASE/
# RUN 含引号、空格或 $() 也只作为环境字面值到达 Python；值合法性（已知套件/
# 已注册场景/32-hex RUN）由 Python 侧 fail-fast 校验。
REMOTE_K8S_SUITE ?= $(or $(SUITE),smoke)
REMOTE_K8S_CASE ?= $(CASE)
REMOTE_K8S_RUN ?= $(RUN)
export REMOTE_K8S_SUITE REMOTE_K8S_CASE REMOTE_K8S_RUN
REMOTE_K8S_ACTIONS := doctor sync-start sync-status sync-stop build deploy test verify logs down status check retest-failed
.PHONY: $(addprefix remote-k8s-,$(REMOTE_K8S_ACTIONS))
$(addprefix remote-k8s-,$(REMOTE_K8S_ACTIONS)):
	python3 tools/remote_k8s/main.py $(patsubst remote-k8s-%,%,$@)
