#!/bin/bash
set -e

echo "=== Undeploying RCoder from K8s cluster ==="

# 删除 Deployment 和 Service
kubectl delete -f manifests/rcoder-deployment.yaml --ignore-not-found
kubectl delete -f manifests/rcoder-service.yaml --ignore-not-found

# 删除 RBAC 配置
kubectl delete -f manifests/serviceaccount.yaml --ignore-not-found

# 删除 namespace
kubectl delete -f manifests/namespace.yaml --ignore-not-found

echo "=== RCoder undeployed ==="