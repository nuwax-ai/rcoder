#!/usr/bin/env bash
# ============================================================================
# 本地 devspace 一次性确保 Cilium Gateway (替代旧 Envoy Gateway)
# ----------------------------------------------------------------------------
# 用途：本地 OrbStack/devspace 测试 rcoder UserApp 的 HTTPRoute（外部 HTTP 访问
#       /apps/{app_id}）。确保 Cilium Gateway 就绪后，devspace 重启无需重跑；
#       rcoder 创建的 HTTPRoute 会被 Cilium Gateway 自动路由到 app Service。
#
# 用法：
#   bash k8s/scripts/install-cilium-gateway.sh           # 确保 Gateway 资源
#   bash k8s/scripts/install-cilium-gateway.sh uninstall # 删 Gateway
#
# 与生产（build-agent-docker/k8s/deploy.sh ensure_cilium_gateway）对齐：
# 同样的 GatewayClass(cilium)、Gateway(nuwax-gateway/default)。
# rcoder env 已设 RCODER_K8S_GATEWAY_NAME=nuwax-gateway / NAMESPACE=default
# （见 k8s/config/deployment.yaml）。
#
# ⚠️ 前提：本地集群已装 Cilium (helm install cilium + gatewayAPI.enabled=true
#           + kubeProxyReplacement=true)。否则 Gateway controller 不启动。
# ============================================================================
set -e

GATEWAY_NS="${GATEWAY_NS:-default}"
GATEWAY_NAME="${GATEWAY_NAME:-nuwax-gateway}"

uninstall() {
  echo "==== 删除 Cilium Gateway ===="
  kubectl delete gateway "${GATEWAY_NAME}" -n "${GATEWAY_NS}" --ignore-not-found=true 2>/dev/null || true
  echo "✓ 已删除（Cilium GatewayClass 由 helm 管理，不删）"
  exit 0
}

[ "${1:-}" = "uninstall" ] && uninstall

echo "==== 确保 Cilium Gateway ${GATEWAY_NS}/${GATEWAY_NAME} (class=cilium) ===="

# 1. 确保 GatewayClass cilium (Cilium helm 自动创建，幂等确保)
echo "📦 Step 1: 确保 GatewayClass cilium..."
cat <<'EOF' | kubectl apply -f - 2>/dev/null || true
apiVersion: gateway.networking.k8s.io/v1
kind: GatewayClass
metadata:
  name: cilium
spec:
  controllerName: io.cilium/gateway-controller
EOF
echo "  ✓ GatewayClass cilium"

# 2. Gateway nuwax-gateway (default ns, HTTP:80, 允许所有 ns 的 HTTPRoute 接入)
#    rcoder 的 UserApp HTTPRoute 在 rcoder-dev ns，靠 allowedRoutes.from=All 接入。
echo "📦 Step 2: 创建 Gateway ${GATEWAY_NS}/${GATEWAY_NAME} (class=cilium)..."
cat <<EOF | kubectl apply -f -
apiVersion: gateway.networking.k8s.io/v1
kind: Gateway
metadata:
  name: ${GATEWAY_NAME}
  namespace: ${GATEWAY_NS}
spec:
  gatewayClassName: cilium
  listeners:
  - name: http
    port: 80
    protocol: HTTP
    allowedRoutes:
      namespaces:
        from: All
EOF
echo "  ✓ Gateway ${GATEWAY_NS}/${GATEWAY_NAME}"

# 3. 等 Gateway Programmed (Cilium 分配 LoadBalancer 地址)
echo "📦 Step 3: 等待 Gateway Programmed (LoadBalancer 地址)..."
kubectl wait gateway -n "${GATEWAY_NS}" "${GATEWAY_NAME}" --for=condition=Programmed --timeout=120s 2>/dev/null \
  || echo "  ⚠️ Gateway Programmed 等待超时（Cilium gatewayAPI 没开？检查 kubeProxyReplacement=true）"

echo ""
echo "==== ✅ Cilium Gateway 就绪 ===="
echo ""
echo "外部访问入口（OrbStack LoadBalancer 分配的地址，端口 80）："
GATEWAY_IP=$(kubectl get gateway -n "${GATEWAY_NS}" "${GATEWAY_NAME}" -o jsonpath='{.status.addresses[0].value}' 2>/dev/null || true)
if [ -n "${GATEWAY_IP}" ]; then
  echo "  http://${GATEWAY_IP}"
else
  echo "  （地址未分配，稍后查：kubectl get gateway -n ${GATEWAY_NS} ${GATEWAY_NAME}）"
fi
echo ""
echo "rcoder UserApp HTTPRoute 测试：创建 app 后访问"
echo "  http://${GATEWAY_IP:-<gateway-ip>}/apps/<app_id>"
echo ""
echo "⚠️ 前提：rcoder env 已配 RCODER_K8S_GATEWAY_NAME=${GATEWAY_NAME} /"
echo "   RCODER_K8S_GATEWAY_NAMESPACE=${GATEWAY_NS}（k8s/config/deployment.yaml 已设）。"
echo "   本地集群需先装 Cilium (gatewayAPI + KPR)。"
