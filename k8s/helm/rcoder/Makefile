# ============================================================
# RCoder Helm Chart Makefile
# 在 helm/rcoder 目录下执行
# ============================================================

CHART_DIR := $(shell pwd)
NAMESPACE ?= nuwax-rcoder

.PHONY: install install-dev install-prod upgrade uninstall status template

# 默认安装 (使用 values.yaml)
install:
	@echo "部署 RCoder (默认配置)..."
	helm upgrade --install rcoder $(CHART_DIR) \
		--namespace $(NAMESPACE) \
		--create-namespace \
		--values $(CHART_DIR)/values.yaml \
		--wait --timeout 5m

# 开发环境安装
install-dev:
	@echo "部署 RCoder (开发环境)..."
	helm upgrade --install rcoder $(CHART_DIR) \
		--namespace $(NAMESPACE) \
		--create-namespace \
		--values $(CHART_DIR)/values-dev.yaml \
		--wait --timeout 5m

# 生产环境安装
install-prod:
	@echo "部署 RCoder (生产环境)..."
	helm upgrade --install rcoder $(CHART_DIR) \
		--namespace $(NAMESPACE) \
		--create-namespace \
		--values $(CHART_DIR)/values-production.yaml \
		--wait --timeout 5m

# 升级
upgrade:
	@echo "升级 RCoder..."
	helm upgrade rcoder $(CHART_DIR) \
		--namespace $(NAMESPACE) \
		--values $(CHART_DIR)/values-production.yaml \
		--wait --timeout 5m

# 卸载
uninstall:
	@echo "卸载 RCoder..."
	helm uninstall rcoder --namespace $(NAMESPACE)

# 查看状态
status:
	helm status rcoder --namespace $(NAMESPACE)

# 查看 release 列表
list:
	helm list --namespace $(NAMESPACE)

# 回滚
rollback:
	@read -p "输入要回滚的版本号: " VERSION; \
	helm rollback rcoder $$VERSION --namespace $(NAMESPACE)

# 渲染模板 (不部署)
template:
	helm template rcoder $(CHART_DIR) --values $(CHART_DIR)/values-production.yaml

# 打包 Chart
package:
	helm package $(CHART_DIR)

# 查看历史
history:
	helm history rcoder --namespace $(NAMESPACE)
