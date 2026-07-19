#!/bin/bash
# ============================================================================
# file-server API 批量测试脚本 (命令行, 不依赖 IDE)
# 用法: bash docker/tests/run-tests.sh [base_url]
# 默认: http://127.0.0.1:60001
# ============================================================================
set -euo pipefail

BASE_URL="${1:-http://127.0.0.1:60001}"
PROJECT="test-api-$(date +%s)"
PASS=0; FAIL=0

ok()   { echo "  ✅ $1"; PASS=$((PASS+1)); }
fail() { echo "  ❌ $1"; FAIL=$((FAIL+1)); }

# HTTP 请求辅助 (检查 success: true)
# 用法: check "名称" curl参数...
check() {
  local name="$1"; shift
  local resp; resp=$(curl -s --max-time 15 "$@" 2>/dev/null || echo '{}')
  if echo "$resp" | python3 -c "import sys,json;d=json.load(sys.stdin);exit(0 if d.get('success') else 1)" 2>/dev/null; then
    ok "$name"
  else
    fail "$name — $(echo "$resp" | python3 -c "import sys,json;d=json.load(sys.stdin);print(d.get('message',d.get('error',{}).get('message',''))[:60])" 2>/dev/null)"
  fi
}

echo "🔍 file-server API 测试 ($BASE_URL)"
echo "📋 测试项目: $PROJECT"
echo ""

echo "--- 健康检查 ---"
# file-server /health 返回 {"status":"ok"} (非 {success:true})
HEALTH_RESP=$(curl -s --max-time 5 "$BASE_URL/health" 2>/dev/null || echo '{}')
if echo "$HEALTH_RESP" | python3 -c "import sys,json;d=json.load(sys.stdin);exit(0 if d.get('status')=='ok' or d.get('success') else 1)" 2>/dev/null; then
  ok "health"
else
  fail "health"
fi

echo ""
echo "--- 项目 CRUD ---"
check "create-project (vue3)" -X POST "$BASE_URL/api/project/create-project" \
  -H 'Content-Type: application/json' \
  -d "{\"projectId\":\"$PROJECT\",\"templateType\":\"vue3\"}"

check "get-project-content" "$BASE_URL/api/project/get-project-content?projectId=$PROJECT"

echo ""
echo "--- Git 操作 ---"
check "git init" -X POST "$BASE_URL/api/git/init" \
  -H 'Content-Type: application/json' \
  -d "{\"workspaceType\":\"pageApp\",\"projectId\":\"$PROJECT\"}"

check "git add" -X POST "$BASE_URL/api/git/add" \
  -H 'Content-Type: application/json' \
  -d "{\"workspaceType\":\"pageApp\",\"projectId\":\"$PROJECT\"}"

check "git commit" -X POST "$BASE_URL/api/git/commit" \
  -H 'Content-Type: application/json' \
  -d "{\"workspaceType\":\"pageApp\",\"projectId\":\"$PROJECT\",\"message\":\"test\"}"

check "git status" "$BASE_URL/api/git/status?workspaceType=pageApp&projectId=$PROJECT"

check "git log" "$BASE_URL/api/git/log?workspaceType=pageApp&projectId=$PROJECT"

check "git branches" "$BASE_URL/api/git/branches?workspaceType=pageApp&projectId=$PROJECT"

echo ""
echo "--- Build ---"
check "port-pool-status" "$BASE_URL/api/build/port-pool-status"
check "list-dev" "$BASE_URL/api/build/list-dev"

echo ""
echo "--- 清理 ---"
check "delete-project" "$BASE_URL/api/project/delete-project?projectId=$PROJECT"

echo ""
echo "================================"
echo "✅ Passed: $PASS"
echo "❌ Failed: $FAIL"
echo "================================"
[ "$FAIL" -eq 0 ] && exit 0 || exit 1
