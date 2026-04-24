# ============================================================================
# K8s 离线部署 bundle
#
# make k8s-offline-bundle          构建离线交付包 (rcoder-offline-*.tar.gz)
# make k8s-offline-import BUNDLE=  解压并执行 install.sh (客户机器用)
# make k8s-offline-images-list     仅打印所有离线依赖镜像清单
# make k8s-offline-clean           清理构建产物
# ============================================================================

OFFLINE_VERSION           ?= $(shell git describe --tags --always 2>/dev/null || echo dev)
OFFLINE_ARCH              ?= $(shell uname -m)
OFFLINE_WORKDIR           ?= dist/offline-build
OFFLINE_OUTPUT            ?= dist/rcoder-offline-$(OFFLINE_VERSION)-$(OFFLINE_ARCH).tar.gz
OFFLINE_IMAGES_FILE       := k8s/offline/images.txt

LONGHORN_VERSION          ?= v1.7.2
JUICEFS_CSI_VERSION       ?= v0.31.3

# 跳过拉取/重新构建镜像 (如已经拉好, 节省时间)
OFFLINE_SKIP_PULL         ?= 0

.PHONY: k8s-offline-bundle k8s-offline-import k8s-offline-images-list k8s-offline-clean

# ----------------------------------------------------------------------------
# k8s-offline-images-list: 打印清单 (可 pipe 给其他工具)
# ----------------------------------------------------------------------------
k8s-offline-images-list:
	@grep -vE '^\s*#|^\s*$$' $(OFFLINE_IMAGES_FILE)

# ----------------------------------------------------------------------------
# k8s-offline-bundle: 打包离线交付物
# ----------------------------------------------------------------------------
k8s-offline-bundle:
	@command -v docker >/dev/null || { echo "❌ 需要 docker"; exit 1; }
	@command -v helm   >/dev/null || { echo "❌ 需要 helm";   exit 1; }
	@command -v curl   >/dev/null || { echo "❌ 需要 curl";   exit 1; }

	@echo "📦 构建离线 bundle..."
	@echo "   版本: $(OFFLINE_VERSION) ($(OFFLINE_ARCH))"
	@echo "   输出: $(OFFLINE_OUTPUT)"
	@echo ""

	@rm -rf $(OFFLINE_WORKDIR)
	@mkdir -p $(OFFLINE_WORKDIR)/images $(OFFLINE_WORKDIR)/charts \
	          $(OFFLINE_WORKDIR)/longhorn $(OFFLINE_WORKDIR)/juicefs-csi

	@echo "[1/5] 拉取镜像 (最多重试 3 次)..."
	@if [ "$(OFFLINE_SKIP_PULL)" = "1" ]; then \
		echo "  ⏭  跳过 (OFFLINE_SKIP_PULL=1)"; \
	else \
		grep -vE '^\s*#|^\s*$$' $(OFFLINE_IMAGES_FILE) | while read img; do \
			echo "  🡣 $$img"; \
			for attempt in 1 2 3; do \
				if docker pull $$img; then \
					break; \
				fi; \
				if [ $$attempt -eq 3 ]; then \
					echo "❌ 拉取失败 (3 次重试): $$img"; exit 1; \
				fi; \
				echo "  ⚠️  第 $$attempt 次失败, 等 $$((attempt*5))s 后重试..."; \
				sleep $$((attempt*5)); \
			done; \
		done; \
	fi

	@echo ""
	@echo "[2/5] 导出 images.tar (包含所有镜像)..."
	@IMGS=$$(grep -vE '^\s*#|^\s*$$' $(OFFLINE_IMAGES_FILE) | tr '\n' ' '); \
		docker save $$IMGS -o $(OFFLINE_WORKDIR)/images/all-images.tar
	@cp $(OFFLINE_IMAGES_FILE) $(OFFLINE_WORKDIR)/images/images.txt
	@echo "  ✅ images.tar 大小: $$(du -sh $(OFFLINE_WORKDIR)/images/all-images.tar | awk '{print $$1}')"

	@echo ""
	@echo "[3/5] 打包 RCoder Helm chart..."
	@helm package k8s/helm/rcoder -d $(OFFLINE_WORKDIR)/charts
	@ls -1 $(OFFLINE_WORKDIR)/charts | sed 's/^/  ✓ /'

	@echo ""
	@echo "[4/5] 下载 Longhorn + JuiceFS CSI manifests..."
	@curl -fsSL https://raw.githubusercontent.com/longhorn/longhorn/$(LONGHORN_VERSION)/deploy/longhorn.yaml \
		-o $(OFFLINE_WORKDIR)/longhorn/longhorn-$(LONGHORN_VERSION).yaml
	@echo "  ✅ longhorn-$(LONGHORN_VERSION).yaml"
	@curl -fsSL https://raw.githubusercontent.com/juicedata/juicefs-csi-driver/$(JUICEFS_CSI_VERSION)/deploy/k8s.yaml \
		-o $(OFFLINE_WORKDIR)/juicefs-csi/juicefs-csi-$(JUICEFS_CSI_VERSION).yaml
	@echo "  ✅ juicefs-csi-$(JUICEFS_CSI_VERSION).yaml"

	@echo ""
	@echo "[5/5] 拷贝脚本和 values..."
	@cp -r k8s/manifests $(OFFLINE_WORKDIR)/
	@cp k8s/offline/install.sh         $(OFFLINE_WORKDIR)/
	@cp k8s/offline/rewrite-registry.sh $(OFFLINE_WORKDIR)/
	@cp k8s/offline/README.md          $(OFFLINE_WORKDIR)/
	@cp k8s/helm/rcoder/values-dev.yaml     $(OFFLINE_WORKDIR)/
	@cp k8s/helm/rcoder/values-prod.yaml    $(OFFLINE_WORKDIR)/
	@cp k8s/helm/rcoder/values-offline.yaml $(OFFLINE_WORKDIR)/
	@chmod +x $(OFFLINE_WORKDIR)/*.sh

	@mkdir -p dist
	@echo ""
	@echo "📦 压缩 bundle..."
	@tar czf $(OFFLINE_OUTPUT) -C $(OFFLINE_WORKDIR) .

	@echo ""
	@echo "✅ 离线 bundle 已生成"
	@echo "   文件: $(OFFLINE_OUTPUT)"
	@echo "   大小: $$(du -sh $(OFFLINE_OUTPUT) | awk '{print $$1}')"
	@echo ""
	@echo "客户使用:"
	@echo "  tar xzf $$(basename $(OFFLINE_OUTPUT)) && bash install.sh --mode=direct --env=dev"

# ----------------------------------------------------------------------------
# k8s-offline-import: 解压 bundle 并一键部署 (在离线机器上)
# ----------------------------------------------------------------------------
k8s-offline-import:
	@if [ -z "$(BUNDLE)" ]; then \
		echo "❌ 需要指定 BUNDLE=path/to/rcoder-offline-*.tar.gz"; exit 1; \
	fi
	@if [ ! -f "$(BUNDLE)" ]; then \
		echo "❌ 未找到 $(BUNDLE)"; exit 1; \
	fi
	@echo "📥 解压 $(BUNDLE) ..."
	@mkdir -p dist/offline-import
	@tar xzf $(BUNDLE) -C dist/offline-import
	@echo "🚀 执行 install.sh ..."
	@cd dist/offline-import && bash install.sh $(INSTALL_ARGS)

# ----------------------------------------------------------------------------
# 清理
# ----------------------------------------------------------------------------
k8s-offline-clean:
	@rm -rf $(OFFLINE_WORKDIR) dist/offline-import
	@echo "✅ 清理完成 (保留 dist/rcoder-offline-*.tar.gz)"
