#!/bin/bash
set -e

echo "=== Testing RCoder in K8s ==="

# 获取 NodePort
NODE_PORT=$(kubectl get svc rcoder -n nuwax-rcoder -o jsonpath='{.spec.ports[0].nodePort}')
echo "NodePort: $NODE_PORT"

# 获取节点 IP（优先使用第一个 IPv4，避免 dual-stack 返回多个地址导致 URL 拼接失败）
NODE_IP=$(kubectl get nodes -o jsonpath='{.items[0].status.addresses[?(@.type=="InternalIP")].address}' | awk '{print $1}')
echo "Node IP: $NODE_IP"

BASE_URL="http://${NODE_IP}:${NODE_PORT}"

echo ""
echo "Testing /health endpoint..."
curl -s "${BASE_URL}/health" | jq .

echo ""
echo "Testing /chat endpoint..."
CHAT_RESP=$(curl -s -X POST "${BASE_URL}/chat" \
  -H "Content-Type: application/json" \
  -d '{"prompt": "hello"}')
echo "${CHAT_RESP}" | jq .
PROJECT_ID=$(echo "${CHAT_RESP}" | jq -r '.data.project_id // empty')

echo ""
echo "Testing /computer/chat endpoint..."
USER_ID="k8s-test-user"
COMPUTER_RESP=$(curl -s -X POST "${BASE_URL}/computer/chat" \
  -H "Content-Type: application/json" \
  -d "{\"user_id\":\"${USER_ID}\",\"prompt\":\"hello from k8s\"}")
echo "${COMPUTER_RESP}" | jq .
COMPUTER_PROJECT_ID=$(echo "${COMPUTER_RESP}" | jq -r '.data.project_id // empty')

echo ""
echo "Verifying created pods by labels..."
if [ -n "${PROJECT_ID}" ]; then
  kubectl get pods -n nuwax-rcoder -l "project_id=${PROJECT_ID}" -o wide
fi
kubectl get pods -n nuwax-rcoder -l "user_id=${USER_ID}" -o wide

echo ""
echo "Verifying pod status API..."
curl -s "${BASE_URL}/computer/pod/status?user_id=${USER_ID}" | jq .
if [ -n "${PROJECT_ID}" ]; then
  curl -s "${BASE_URL}/computer/pod/status?project_id=${PROJECT_ID}" | jq .
fi
if [ -n "${COMPUTER_PROJECT_ID}" ]; then
  curl -s "${BASE_URL}/computer/pod/status?project_id=${COMPUTER_PROJECT_ID}" | jq .
fi

echo ""
echo "=== Test complete ==="
