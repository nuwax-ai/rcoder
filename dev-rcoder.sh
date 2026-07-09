#!/usr/bin/env bash
# =============================================================================
# dev-rcoder.sh —— devspace pod 内 rcoder 服务生命周期管理
# =============================================================================
# 用途:本地改码后,devspace sync 自动推到 /app,再用本脚本「增量编译 + 后台重启」,
#       无需重建镜像即可在 K8s(orbstack rcoder-dev)里快速验证 rcoder 逻辑。
#
# 调用方式(二选一):
#   1) 宿主机:devspace run svc-restart   (见 devspace.yaml commands,自动 devspace enter)
#   2) 直接:  kubectl exec -n rcoder-dev <pod> -- bash /app/dev-rcoder.sh restart
#
# 动作:
#   build    增量编译 rcoder(debug,k8s features),前台等完成
#   start    后台启动已编译的二进制(nohup,脱离 exec 会话,PID 写 /tmp/rcoder.pid)
#   stop     按 PID 停止
#   restart  stop → 增量编译 → start(改码后用这个)
#   status   进程在否 + /health 探活
#   logs [N] tail rcoder 日志(优先 /app/logs/rcoder.YYYY-MM-DD,回退 /tmp/rcoder.out)
# =============================================================================
set -euo pipefail

# Dockerfile 把 cargo 装在 /usr/local/cargo/bin,kubectl exec 的 login shell 会重置 PATH,补上
export PATH="/usr/local/cargo/bin:${PATH}"
cd /app

PORT="${RCODER_PORT:-8290}"
FEATURES="${RCODER_FEATURES:-ebpf-debug,pyroscope,otel,debug,kubernetes}"
BIN="/app/target/debug/rcoder"
PID_FILE="/tmp/rcoder.pid"
OUT_FILE="/tmp/rcoder.out"
TODAY_LOG="/app/logs/rcoder.$(date +%Y-%m-%d)"

running() {
    [ -f "$PID_FILE" ] && kill -0 "$(cat "$PID_FILE" 2>/dev/null)" 2>/dev/null
}

do_build() {
    echo "[build] cargo build --features $FEATURES (增量,首次较慢)"
    CONTAINER_RUNTIME=kubernetes cargo build --bin rcoder --features "$FEATURES"
}

do_start() {
    if running; then
        echo "[start] 已在运行 pid=$(cat "$PID_FILE"),跳过"
        return 0
    fi
    if [ ! -x "$BIN" ]; then
        echo "[start] 二进制不存在 $BIN,请先 build" >&2
        exit 1
    fi
    CONTAINER_RUNTIME=kubernetes RUST_LOG="${RUST_LOG:-debug}" \
        nohup "$BIN" --port "$PORT" >"$OUT_FILE" 2>&1 &
    echo $! >"$PID_FILE"
    sleep 1
    if running; then
        echo "[start] 已启动 pid=$(cat "$PID_FILE") port=$PORT (日志:logs)"
    else
        echo "[start] 启动失败,看 $OUT_FILE:" >&2
        tail -n 30 "$OUT_FILE" >&2 || true
        rm -f "$PID_FILE"
        exit 1
    fi
}

do_stop() {
    if running; then
        local pid; pid="$(cat "$PID_FILE")"
        kill "$pid" 2>/dev/null || true
        # 优雅等 2s,仍存活则 SIGKILL
        for _ in 1 2; do sleep 1; kill -0 "$pid" 2>/dev/null || break; done
        kill -9 "$pid" 2>/dev/null || true
        echo "[stop] 已停止 pid=$pid"
    else
        echo "[stop] 未在运行"
    fi
    rm -f "$PID_FILE"
}

do_status() {
    if running; then
        echo "[status] running pid=$(cat "$PID_FILE")"
    else
        echo "[status] stopped"
    fi
    if command -v curl >/dev/null 2>&1; then
        local code; code=$(curl -s -o /dev/null -w '%{http_code}' -m 2 "localhost:$PORT/health" 2>/dev/null || echo "000")
        echo "[status] /health -> HTTP $code"
    fi
}

do_logs() {
    local n="${1:-80}"
    if [ -f "$TODAY_LOG" ]; then
        echo "[logs] $TODAY_LOG (tail $n):"
        tail -n "$n" "$TODAY_LOG" 2>/dev/null || true
    else
        echo "[logs] 无 $TODAY_LOG,回退 $OUT_FILE (tail $n):"
        tail -n "$n" "$OUT_FILE" 2>/dev/null || true
    fi
}

case "${1:-status}" in
    build)   do_build ;;
    start)   do_start ;;
    stop)    do_stop ;;
    restart) do_stop; do_build; do_start ;;
    status)  do_status ;;
    logs)    shift; do_logs "$@" ;;
    *) echo "usage: dev-rcoder.sh {build|start|stop|restart|status|logs [N]}" >&2; exit 1 ;;
esac
