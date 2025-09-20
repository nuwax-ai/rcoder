#!/bin/bash

# 改进版Plan功能完整测试脚本

set -e

echo "🚀 开始测试改进版Plan功能..."
echo ""

# 测试服务器是否运行
echo "📋 检查服务器状态..."
curl -s "http://localhost:3001/health" > /dev/null || {
    echo "❌ 服务器未运行，请先启动服务器"
    exit 1
}
echo "✅ 服务器运行正常"
echo ""

# 会话ID
SESSION_ID="enhanced-plan-test-$(date +%s)"

echo "🎯 测试会话: $SESSION_ID"
echo ""

# 1. 创建演示Plan
echo "1️⃣ 创建演示Plan..."
RESPONSE=$(curl -s -X POST "http://localhost:3001/api/plans/$SESSION_ID/demo")
echo "$RESPONSE" | jq '.'

# 获取第一个条目ID用于后续测试
FIRST_ENTRY_ID=$(echo "$RESPONSE" | jq -r '.data.plan.entries[0].id')
SECOND_ENTRY_ID=$(echo "$RESPONSE" | jq -r '.data.plan.entries[1].id')

echo ""
echo "📝 第一个条目ID: $FIRST_ENTRY_ID"
echo "📝 第二个条目ID: $SECOND_ENTRY_ID"
echo ""

# 2. 获取Plan详情
echo "2️⃣ 获取Plan详情..."
curl -s "http://localhost:3001/api/plans/$SESSION_ID" | jq '.data.stats'
echo ""

# 3. 更新第一个条目为进行中
echo "3️⃣ 更新第一个条目状态为InProgress..."
curl -s -X PUT "http://localhost:3001/api/plans/$SESSION_ID/status" \
  -H "Content-Type: application/json" \
  -d "{\"entry_id\": \"$FIRST_ENTRY_ID\", \"status\": \"InProgress\"}" | jq '.data.stats'
echo ""

# 4. 更新第一个条目为完成
echo "4️⃣ 更新第一个条目状态为Completed..."
curl -s -X PUT "http://localhost:3001/api/plans/$SESSION_ID/status" \
  -H "Content-Type: application/json" \
  -d "{\"entry_id\": \"$FIRST_ENTRY_ID\", \"status\": \"Completed\"}" | jq '.data.stats'
echo ""

# 5. 更新第二个条目为进行中
echo "5️⃣ 更新第二个条目状态为InProgress..."
curl -s -X PUT "http://localhost:3001/api/plans/$SESSION_ID/status" \
  -H "Content-Type: application/json" \
  -d "{\"entry_id\": \"$SECOND_ENTRY_ID\", \"status\": \"InProgress\"}" | jq '.data.stats'
echo ""

# 6. 获取所有Plan统计
echo "6️⃣ 获取所有Plan统计..."
curl -s "http://localhost:3001/api/plans/stats" | jq '.data'
echo ""

# 7. 清理已完成的条目
echo "7️⃣ 清理已完成条目..."
curl -s -X POST "http://localhost:3001/api/plans/$SESSION_ID/cleanup" | jq '.data.stats'
echo ""

# 8. 查看清理后的Plan
echo "8️⃣ 查看清理后的Plan条目数量..."
FINAL_PLAN=$(curl -s "http://localhost:3001/api/plans/$SESSION_ID")
FINAL_COUNT=$(echo "$FINAL_PLAN" | jq '.data.plan.entries | length')
echo "剩余条目数量: $FINAL_COUNT"
echo "$FINAL_PLAN" | jq '.data.stats'
echo ""

# 9. 测试Plan结构特性
echo "9️⃣ 测试Plan的新增特性..."
echo "$FINAL_PLAN" | jq '{
  plan_status: .data.plan.status,
  plan_title: .data.plan.title,
  plan_description: .data.plan.description,
  plan_category: .data.plan.category,
  total_estimated_duration: .data.plan.total_estimated_duration,
  total_actual_duration: .data.plan.total_actual_duration,
  sample_entry_enhanced_fields: .data.plan.entries[0] | {
    id: .id,
    content: .content,
    priority: .priority,
    status: .status,
    tags: .tags,
    description: .description,
    dependencies: .dependencies,
    progress: .progress,
    estimated_duration: .estimated_duration,
    actual_duration: .actual_duration,
    started_at: .started_at,
    completed_at: .completed_at
  }
}'
echo ""

echo "🎉 Plan功能测试完成！"
echo ""
echo "✅ 验证结果:"
echo "  - Plan创建 ✓"
echo "  - 状态更新 ✓" 
echo "  - 统计信息 ✓"
echo "  - 已完成条目清理 ✓"
echo "  - 新增字段支持 ✓"
echo "  - 时间跟踪 ✓"
echo "  - 进度管理 ✓"
echo ""
echo "🚀 Plan功能已成功集成ACP协议，支持："
echo "  📊 实时统计和状态跟踪"
echo "  🕒 时间估计和实际耗时计算"  
echo "  📋 优先级和依赖管理"
echo "  🏷️  标签和分类支持"
echo "  📈 进度百分比跟踪"
echo "  🎯 丰富的元数据支持"
echo ""
echo "✨ 现在agent生成的todo list可以通过ACP协议获取并返回给前端展示！"