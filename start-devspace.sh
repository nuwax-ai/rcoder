#!/bin/bash
set -e

export RUST_LOG=${RUST_LOG:-debug}
export RCODER_PORT=${RCODER_PORT:-8090}

echo "🚀 RCoder DevSpace 开发环境已就绪"
echo "🔧 RCoder Port: $RCODER_PORT"
echo "📊 Log Level: $RUST_LOG"
echo ""
echo "💡 开发提示："
echo "   - 使用 make devspace-run 启动服务（自动编译并运行）"
echo "   - 或进入容器手动执行: cargo run --bin rcoder --features ebpf-debug,pyroscope,otel,debug,kubernetes -- --port $RCODER_PORT"
echo ""
echo "⏳ 等待命令执行..."
echo ""

# 保持容器运行，等待命令
exec tail -f /dev/null
