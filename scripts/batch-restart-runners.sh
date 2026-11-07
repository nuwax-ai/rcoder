#!/usr/bin/env bash
# 批量重启 computer-agent-runner（经 rcoder /computer/pod/restart 接口，StatefulSet 级销毁重建）
#
# 用途: 镜像升级后把存量 runner 切到新版本（restart 会按当前 rcoder 配置重建 StatefulSet,
#       与 kubectl delete pod 的区别: 后者由 K8s controller 用旧 spec 秒级重建, 镜像不更新）。
#
# ⚠️ resource_limits 必须显式传(本脚本已内置 8Gi 规格): 直连 restart 不传会落 configmap
#    kubernetes_config.services[].resource_limits 默认值(生产曾因此缩水到 4Gi)。
#    Java 侧 ensurePod/restart 新版已显式传全字段; 本脚本与之同构(cpu=2/mem=8Gi/swap=16Gi/storage=10Gi)。
#
# 用法(在可访问目标集群 kubectl 的跳板机上):
#   bash scripts/batch-restart-runners.sh                    # DRY-RUN: 只打印执行计划
#   CONFIRM=1 bash scripts/batch-restart-runners.sh          # 全量执行
#   ONLY=1789876151 CONFIRM=1 bash scripts/batch-restart-runners.sh   # 金丝雀: 只处理名称含该串的
#
# 行为: 枚举 runner StatefulSet → pg(projects 表)解析 user_id/project_id 映射
#       → 逐个调 restart(带 resource_limits) → 等滚动完成 → 断言 image+memory 双达标才计成功。
# 认证: 读 configmap api_key_auth 段; enabled=false 时免认证, true 时从 rcoder 容器内提取 key(不落盘)。
set -uo pipefail

NS="nuwax-k8s-prod"
TARGET_IMAGE_TAG="0.1.414"
TARGET_IMAGE="nuwax-docker-images-registry.cn-hangzhou.cr.aliyuncs.com/${NS}/rcoder-computer-agent-runner:${TARGET_IMAGE_TAG}"
RCODER_POD="nuwax-k8s-prod-rcoder-0"   # 固定用副本0串行调用(admission 存储级,任一副本均可)
PG_POD="nuwax-k8s-prod-pg-1"
RCODER_PORT="${RCODER_PORT:-8086}"
# 8Gi 内存规格(与 Java ensurePod 同构: cpu=2, memory=8Gi, swap=16Gi, storage=10Gi)
# ⚠️ 必传: 直连 restart 不传 resource_limits 会落 configmap 默认(生产=4Gi, 缩水!)
RL_CPU="2"
RL_MEM="8589934592"
RL_SWAP="17179869184"
RL_STORAGE="10Gi"

say() { echo "[$(date +%H:%M:%S)] $*"; }

# ---- 1. api_key: 读 api_key_auth 段 (enabled:false 时无需认证) ----
AUTH_FLAG=$(kubectl get cm ${NS}-rcoder-config -n ${NS} -o jsonpath='{.data.config\.yml}' 2>/dev/null \
  | grep -A2 '^api_key_auth:' | grep 'enabled:' | head -1 | grep -oE 'true|false')
if [ "${AUTH_FLAG}" = "true" ]; then
  API_KEY=$(kubectl exec -n ${NS} ${RCODER_POD} -c rcoder -- sh -c \
    "grep -A2 '^api_key_auth:' /app/config/config.yml | grep 'api_key:' | head -1" 2>/dev/null \
    | sed 's/.*api_key:[[:space:]]*//; s/^"//; s/"$//' | tr -d '[:space:]')
  [ -z "${API_KEY}" ] && { say "❌ api_key_auth.enabled=true 但未提取到 api_key"; exit 1; }
  AUTH_HDR=(-H "x-api-key: ${API_KEY}")
  say "✓ api_key 认证开启, key 已提取"
else
  AUTH_HDR=()
  say "✓ api_key_auth.enabled=false (无需认证)"
fi

# ---- 2. 枚举需要切换的 runner sts ----
declare -a PENDING=()
while read -r sts image; do
  [ -z "$sts" ] && continue
  # ONLY=statefulset名 前缀匹配时仅处理该目标(金丝雀模式)
  if [ -n "${ONLY:-}" ] && [[ "$sts" != *"${ONLY}"* ]]; then continue; fi
  if [[ "$image" == *":${TARGET_IMAGE_TAG}" ]]; then
    say "✓ 已是 ${TARGET_IMAGE_TAG}, 跳过: $sts"
  else
    PENDING+=("$sts")
    say "• 待切换: $sts  (当前: ${image##*:})"
  fi
done < <(kubectl get statefulsets -n ${NS} -l rcoder.io/service-type=computer-agent-runner \
  -o jsonpath='{range .items[*]}{.metadata.name}{"\t"}{.spec.template.spec.containers[0].image}{"\n"}{end}' 2>/dev/null)

[ ${#PENDING[@]} -eq 0 ] && { say "没有需要切换的 runner"; exit 0; }
say "共 ${#PENDING[@]} 个待切换"

# ---- 3. pg 查每个 sts 的 user_id/project_id 映射 ----
declare -A MAP_U MAP_P
for sts in "${PENDING[@]}"; do
  row=$(kubectl exec -n ${NS} ${PG_POD} -c postgres -- psql -U postgres -d rcoder -t -A -F'|' -c \
    "SELECT user_id, project_id FROM projects WHERE container_name='${sts}' LIMIT 1;" 2>/dev/null)
  u="${row%%|*}"; p="${row##*|}"
  if [ -z "$u" ] || [ -z "$p" ] || [ "$u" == "$row" ]; then
    say "⚠️ ${sts}: pg 无映射(user_id/project_id), 手动处理; row='${row}'"
    continue
  fi
  MAP_U[$sts]="$u"; MAP_P[$sts]="$p"
done

# ---- 4. 打印执行计划 ----
echo ""; echo "========== 执行计划 (目标镜像 ${TARGET_IMAGE_TAG}) =========="
for sts in "${PENDING[@]}"; do
  [ -z "${MAP_U[$sts]:-}" ] && continue
  echo "  ${sts}  →  user_id=${MAP_U[$sts]}  project_id=${MAP_P[$sts]}"
done
echo "============================================================="; echo ""

if [ "${CONFIRM:-0}" != "1" ]; then
  say "DRY-RUN 完成。确认无误后执行: CONFIRM=1 bash $0"
  exit 0
fi

# ---- 5. 逐个 restart + 等滚动 + 验证 ----
OK=0; FAIL=0; FAILED_LIST=()
for sts in "${PENDING[@]}"; do
  u="${MAP_U[$sts]:-}"; p="${MAP_P[$sts]:-}"
  [ -z "$u" ] && { FAIL=$((FAIL+1)); FAILED_LIST+=("$sts(no-map)"); continue; }

  say "→ restart ${sts} (user=${u} project=${p})"
  resp=$(kubectl exec -n ${NS} ${RCODER_POD} -c rcoder -- curl -s -m 120 -X POST \
    "http://127.0.0.1:${RCODER_PORT}/computer/pod/restart" \
    -H "Content-Type: application/json" ${AUTH_HDR:+${AUTH_HDR[*]}} \
    -d "{\"user_id\":\"${u}\",\"project_id\":\"${p}\",\"service_type\":\"computer-agent-runner\",\"resource_limits\":{\"cpu\":${RL_CPU},\"memory\":${RL_MEM},\"swap\":${RL_SWAP},\"storage_size\":\"${RL_STORAGE}\"}}" 2>/dev/null)

  if ! echo "$resp" | grep -q '"success":true'; then
    say "  ❌ restart 失败: $(echo "$resp" | head -c 200)"
    FAIL=$((FAIL+1)); FAILED_LIST+=("$sts"); continue
  fi
  say "  ✓ restart 接口成功, 等待 StatefulSet 滚动..."
  if kubectl rollout status statefulset/"${sts}" -n ${NS} --timeout=300s >/dev/null 2>&1; then
    newimg=$(kubectl get statefulset "${sts}" -n ${NS} -o jsonpath='{.spec.template.spec.containers[0].image}')
    mem=$(kubectl get statefulset "${sts}" -n ${NS} -o jsonpath='{.spec.template.spec.containers[0].resources.limits.memory}')
    if [[ "$newimg" == *":${TARGET_IMAGE_TAG}" && ( "$mem" == "8Gi" || "$mem" == "8589934592" ) ]]; then
      say "  ✅ ${sts} 已切换 ${TARGET_IMAGE_TAG} / 内存 ${mem}"
      OK=$((OK+1))
    else
      say "  ⚠️ ${sts} 滚动完成但镜像异常: ${newimg}"
      FAIL=$((FAIL+1)); FAILED_LIST+=("$sts(img?)")
    fi
  else
    say "  ❌ ${sts} 滚动超时, 需人工检查"
    FAIL=$((FAIL+1)); FAILED_LIST+=("$sts(rollout)")
  fi
  sleep 5   # 间隔, 降低瞬时压力
done

echo ""; say "========== 汇总: 成功 ${OK} / 失败 ${FAIL} =========="
[ ${#FAILED_LIST[@]} -gt 0 ] && printf '  失败: %s\n' "${FAILED_LIST[@]}"
exit $([ $FAIL -eq 0 ] && echo 0 || echo 1)
