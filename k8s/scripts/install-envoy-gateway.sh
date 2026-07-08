#!/usr/bin/env bash
# ============================================================================
# 本地 devspace 一次性安装 Envoy Gateway
# ----------------------------------------------------------------------------
# 用途：本地 OrbStack/devspace 测试 rcoder UserApp 的 HTTPRoute（外部 HTTP 访问
#       /apps/{app_id}）。装好后 EG 持久存在（envoy-gateway-system），devspace 重启
#       无需重装；rcoder 创建的 HTTPRoute 会被 EG 自动路由到 app Service。
#
# 用法：
#   bash k8s/scripts/install-envoy-gateway.sh           # 安装
#   bash k8s/scripts/install-envoy-gateway.sh uninstall # 卸载
#
# 与生产（build-agent-docker/k8s/envoy-gateway/install.sh）对齐：同样的 EG 版本、
# GatewayClass(envoy-gateway)、Gateway(nuwax-gateway/default)。rcoder env 已设
# RCODER_K8S_GATEWAY_NAME=nuwax-gateway / NAMESPACE=default（见 k8s/config/deployment.yaml）。
# ============================================================================
set -e

EG_VERSION="${EG_VERSION:-v1.8.1}"
EG_NS="envoy-gateway-system"
GATEWAY_NS="${GATEWAY_NS:-default}"
GATEWAY_NAME="${GATEWAY_NAME:-nuwax-gateway}"

uninstall() {
  echo "==== 卸载 Envoy Gateway ===="
  kubectl delete gateway "${GATEWAY_NAME}" -n "${GATEWAY_NS}" --ignore-not-found=true 2>/dev/null || true
  kubectl delete gatewayclass envoy-gateway --ignore-not-found=true 2>/dev/null || true
  helm uninstall eg -n "${EG_NS}" 2>/dev/null || true
  kubectl delete namespace "${EG_NS}" --ignore-not-found=true 2>/dev/null || true
  echo "✓ 已卸载（Gateway API CRD 保留，无害）"
  exit 0
}

[ "${1:-}" = "uninstall" ] && uninstall

echo "==== 安装 Envoy Gateway ${EG_VERSION}（本地 devspace 测试用）===="

# 1. Helm 安装 EG controller（自带 Gateway API CRDs + 默认 GatewayClass envoy-gateway）
echo "📦 Step 1: Helm 安装 EG controller..."
helm repo add egoci oci://docker.io/envoyproxy/gateway-helm 2>/dev/null || true
helm upgrade --install eg oci://docker.io/envoyproxy/gateway-helm \
  --version "${EG_VERSION}" \
  -n "${EG_NS}" \
  --create-namespace \
  --wait \
  --timeout 5m
echo "  ✓ EG controller 已装（${EG_NS}）"

# 2. GatewayClass（EG Helm 默认创建 envoy-gateway；幂等确保存在）
echo "📦 Step 2: 确保 GatewayClass envoy-gateway..."
cat <<'EOF' | kubectl apply -f -
apiVersion: gateway.networking.k8s.io/v1
kind: GatewayClass
metadata:
  name: envoy-gateway
spec:
  controllerName: gateway.envoyproxy.io/gatewayclass-controller
EOF
echo "  ✓ GatewayClass envoy-gateway"

# 3. Gateway nuwax-gateway（default ns，HTTP:80，允许所有 ns 的 HTTPRoute 接入）
#    rcoder 的 UserApp HTTPRoute 在 rcoder-dev ns，靠 allowedRoutes.from=All 接入。
echo "📦 Step 3: 创建 Gateway ${GATEWAY_NS}/${GATEWAY_NAME}..."
cat <<EOF | kubectl apply -f -
apiVersion: gateway.networking.k8s.io/v1
kind: Gateway
metadata:
  name: ${GATEWAY_NAME}
  namespace: ${GATEWAY_NS}
spec:
  gatewayClassName: envoy-gateway
  listeners:
  - name: http
    port: 80
    protocol: HTTP
    allowedRoutes:
      namespaces:
        from: All
EOF
echo "  ✓ Gateway ${GATEWAY_NS}/${GATEWAY_NAME}"

# 4. 等 Envoy proxy 起来（LoadBalancer 分配地址）
echo "📦 Step 4: 等待 Envoy proxy 就绪（LoadBalancer 地址）..."
kubectl wait gateway -n "${GATEWAY_NS}" "${GATEWAY_NAME}" --for=condition=Programmed --timeout=120s 2>/dev/null \
  || echo "  ⚠️ Gateway Programmed 等待超时（继续；EG 可能还在拉镜像）"

echo ""
echo "==== ✅ Envoy Gateway 就绪 ===="
echo ""
echo "外部访问入口（OrbStack LoadBalancer 分配的地址，端口 80）："
ENVOY_IP=$(kubectl get svc -n "${EG_NS}" -l "gateway.envoyproxy.io/owning-gateway-name=${GATEWAY_NAME}" \
  -o jsonpath='{.items[0].status.loadBalancer.ingress[0].ip}' 2>/dev/null || true)
if [ -n "${ENVOY_IP}" ]; then
  echo "  http://${ENVOY_IP}"
else
  echo "  （LoadBalancer 地址尚未分配，稍后查：）"
  echo "  kubectl get svc -n ${EG_NS} -l gateway.envoyproxy.io/owning-gateway-name=${GATEWAY_NAME}"
fi
echo ""
echo "rcoder UserApp HTTPRoute 测试：创建 app 后访问"
echo "  http://${ENVOY_IP:-<envoy-ip>}/apps/<app_id>"
echo ""
echo "⚠️ 前提：rcoder env 已配 RCODER_K8S_GATEWAY_NAME=${GATEWAY_NAME} /"
echo "   RCODER_K8S_GATEWAY_NAMESPACE=${GATEWAY_NS}（k8s/config/deployment.yaml 已设）。"
echo "   EG 一次性安装，devspace 重启无需重装。"
