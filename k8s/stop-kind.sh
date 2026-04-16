#!/bin/bash
set -e

echo "=== Stopping Kind cluster for RCoder ==="

# 删除集群
if kind get clusters 2>/dev/null | grep -q "rcoder-dev"; then
    echo "Deleting Kind cluster 'rcoder-dev'..."
    kind delete cluster --name rcoder-dev
    echo "Cluster deleted"
else
    echo "Kind cluster 'rcoder-dev' does not exist"
fi

echo "=== Done ==="