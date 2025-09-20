#!/bin/bash
# Plan架构重构验证脚本

echo "🔍 验证Plan架构重构..."

cd /Volumes/soddygo/git_work/rcoder

# 1. 编译检查
echo "1. 编译检查..."
cargo check --package rcoder > /dev/null 2>&1
if [ $? -eq 0 ]; then
    echo "   ✅ 编译成功"
else
    echo "   ❌ 编译失败"
    exit 1
fi

# 2. 检查PlanManager是否已移除
echo "2. 检查PlanManager移除..."
if grep -q "plan_manager:" crates/rcoder/src/main.rs; then
    echo "   ❌ PlanManager字段仍然存在"
    exit 1
else
    echo "   ✅ PlanManager字段已移除"
fi

# 3. 检查plan_update_to_progress_event函数是否已移除
if grep -q "plan_update_to_progress_event" crates/rcoder/src/main.rs; then
    echo "   ❌ plan_update_to_progress_event函数仍然存在"
    exit 1
else
    echo "   ✅ plan_update_to_progress_event函数已移除"
fi

# 4. 检查plan_api模块是否已移除
if grep -q "use plan_api::" crates/rcoder/src/main.rs; then
    echo "   ❌ plan_api模块仍被引用"
    exit 1
else
    echo "   ✅ plan_api模块引用已移除"
fi

# 5. 检查Plan路由是否已移除
if grep -q "plan_routes()" crates/rcoder/src/main.rs; then
    echo "   ❌ Plan路由仍然存在"
    exit 1
else
    echo "   ✅ Plan路由已移除"
fi

# 6. 检查StreamUpdate::Plan是否在stream_update_to_progress_event中处理
if grep -A 10 "StreamUpdate::Plan" crates/rcoder/src/main.rs | grep -q "ProgressEventType::PlanUpdate"; then
    echo "   ✅ StreamUpdate::Plan正确处理"
else
    echo "   ❌ StreamUpdate::Plan处理逻辑缺失"
    exit 1
fi

# 7. 检查只有一个数据源（SessionManager）
if grep -q "session_manager.get_session" crates/rcoder/src/main.rs; then
    echo "   ✅ 使用SessionManager作为统一数据源"
else
    echo "   ❌ SessionManager未被使用"
    exit 1
fi

echo ""
echo "🎉 Plan架构重构验证通过！"
echo ""
echo "📊 重构总结："
echo "   ✅ 移除了冗余的PlanManager"
echo "   ✅ 移除了独立的Plan API端点"
echo "   ✅ 简化了progress_stream逻辑"
echo "   ✅ Plan数据统一通过ACP StreamUpdate处理"
echo ""
echo "🚀 新架构特点："
echo "   - 单一数据源：所有数据来自ACP SessionManager"
echo "   - 统一处理：stream_update_to_progress_event处理所有事件"
echo "   - 简化API：只保留/progress/{session_id}端点"
echo "   - 符合规范：Plan由agent生成，SSE推送"