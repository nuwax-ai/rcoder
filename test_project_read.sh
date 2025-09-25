#!/bin/bash

echo "测试 /project/read 接口"

# 创建测试项目
mkdir -p ./project_workspace/test_project

# 创建一些测试文件
cat > ./project_workspace/test_project/test.rs << 'EOF'
fn main() {
    println!("Hello, world!");
}
EOF

cat > ./project_workspace/test_project/README.md << 'EOF'
# Test Project

This is a test project.
EOF

# 启动服务器（在后台运行）
cargo run --bin rcoder &
SERVER_PID=$!

# 等待服务器启动
sleep 3

# 测试接口
curl -X POST http://localhost:3000/project/read \
  -H "Content-Type: application/json" \
  -d '{"project_id": "test_project"}' | jq .

# 停止服务器
kill $SERVER_PID