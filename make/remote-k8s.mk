# Independent opt-in workflow; dotenv is parsed by Python, never sourced by Make.
REMOTE_K8S_ACTIONS := doctor sync-start sync-status sync-stop build deploy test verify logs down
.PHONY: $(addprefix remote-k8s-,$(REMOTE_K8S_ACTIONS))
$(addprefix remote-k8s-,$(REMOTE_K8S_ACTIONS)):
	python3 tools/remote_k8s/main.py $(patsubst remote-k8s-%,%,$@) --suite "$(or $(SUITE),smoke)"
