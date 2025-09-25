#!/bin/bash

echo "测试 ProjectZipper 功能"

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

cd ../..

# 运行测试
echo "运行 ZIP 压缩测试..."
cargo test --package nuwax_parser project_zip::tests -- --nocapture

# 清理
rm -rf ./project_workspace/test_zip_project