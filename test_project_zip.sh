#!/bin/bash

echo "测试项目压缩和下载功能"

# 创建测试项目
mkdir -p ./project_workspace/test_zip_project
cd ./project_workspace/test_zip_project

# 创建一些测试文件
echo "# Test Project" > README.md
echo "fn main() { println!(\"Hello, world!\"); }" > main.rs

# 创建子目录和文件
mkdir -p src
echo "pub mod utils;" > src/lib.rs
echo "pub fn hello() -> String { \"Hello\".to_string() }" > src/utils.rs

# 创建 node_modules 目录（应该被排除）
mkdir -p node_modules
echo '{"name": "test", "version": "1.0.0"}' > node_modules/package.json
echo "console.log('test');" > node_modules/test.js

# 创建其他文件
mkdir -p config
echo "debug = true" > config/app.toml

cd ../..

echo "创建的测试项目结构："
find ./project_workspace/test_zip_project -type f | head -10

# 启动服务器（在后台运行）
cargo run --bin rcoder &
SERVER_PID=$!

# 等待服务器启动
sleep 3

echo ""
echo "=== 测试项目压缩接口 ==="
# 测试压缩接口
curl -X POST http://localhost:3000/project/zip \
  -H "Content-Type: application/json" \
  -d '{"project_id": "test_zip_project"}' | jq .

echo ""
echo "=== 测试项目下载接口 ==="
# 测试下载接口
curl -I http://localhost:3000/project/download/test_zip_project

# 停止服务器
kill $SERVER_PID

# 清理
echo ""
echo "清理测试文件..."
rm -rf ./project_workspace/test_zip_project
echo "测试完成！"