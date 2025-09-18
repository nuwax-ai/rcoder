# nuwax_parser 库使用示例

这个示例展示如何在你的应用程序中使用 `nuwax_parser` 库来实现前后端项目文件同步。

## 主要功能

### 1. 读取项目并转换为 V0ParseResult

```rust
use nuwax_parser::project_to_v0_result;
use std::path::PathBuf;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // 项目路径
    let project_path = PathBuf::from("./my-react-project");

    // 读取项目并转换为 V0ParseResult
    let v0_result = project_to_v0_result(&project_path, true).await?;

    println!("读取了 {} 个文件", v0_result.files.len());
    println!("Block ID: {}", v0_result.block_id);

    // 现在可以将 v0_result 发送给前端
    Ok(())
}
```

### 2. 将 V0ParseResult 写回文件系统

```rust
use nuwax_parser::{project_to_v0_result, write_v0_result_to_project};
use std::path::PathBuf;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let project_path = PathBuf::from("./my-react-project");

    // 1. 先读取项目
    let mut v0_result = project_to_v0_result(&project_path, true).await?;

    // 2. 模拟前端修改文件
    for file in &mut v0_result.files {
        if file.file_type == "typescriptreact" {
            file.content = "// Modified by frontend\n".to_string() + &file.content;
            file.hash = nuwax_parser::calculate_hash(&file.content);
            file.is_edit = true;
        }
    }

    // 3. 写回文件系统
    let written_files = write_v0_result_to_project(&project_path, &v0_result).await?;
    println!("成功写入 {} 个文件", written_files.len());

    Ok(())
}
```

### 3. HTTP 服务器集成示例

```rust
use axum::{extract::Path, http::StatusCode, Json};
use nuwax_parser::{project_to_v0_result, write_v0_result_to_project};
use serde::Deserialize;

#[derive(Deserialize)]
struct ProjectPath {
    path: String,
}

// GET /api/project/{path} - 读取项目
async fn get_project(
    Path(ProjectPath { path }): Path<ProjectPath>,
) -> Result<Json<V0ParseResult>, StatusCode> {
    match project_to_v0_result(&path, true).await {
        Ok(v0_result) => Ok(Json(v0_result)),
        Err(_) => Err(StatusCode::INTERNAL_SERVER_ERROR),
    }
}

// POST /api/project/{path} - 保存项目
async fn save_project(
    Path(ProjectPath { path }): Path<ProjectPath>,
    Json(v0_result): Json<V0ParseResult>,
) -> Result<Json<Vec<String>>, StatusCode> {
    match write_v0_result_to_project(&path, &v0_result).await {
        Ok(files) => Ok(Json(files)),
        Err(_) => Err(StatusCode::INTERNAL_SERVER_ERROR),
    }
}
```

### 4. 完整的双向同步示例

```rust
use nuwax_parser::{project_to_v0_result, write_v0_result_to_project, calculate_hash};
use std::path::PathBuf;

struct ProjectSync {
    project_path: PathBuf,
}

impl ProjectSync {
    pub fn new<P: AsRef<Path>>(project_path: P) -> Self {
        Self {
            project_path: project_path.as_ref().to_path_buf(),
        }
    }

    /// 读取项目发送给前端
    pub async fn read_for_frontend(&self) -> anyhow::Result<V0ParseResult> {
        project_to_v0_result(&self.project_path, true).await
    }

    /// 从前端接收修改并保存
    pub async fn save_from_frontend(&self, v0_result: V0ParseResult) -> anyhow::Result<Vec<String>> {
        write_v0_result_to_project(&self.project_path, &v0_result).await
    }

    /// 智能同步：只更新有变化的文件
    pub async fn smart_sync(&self, v0_result: V0ParseResult) -> anyhow::Result<Vec<String>> {
        // 读取当前项目状态
        let current_result = self.read_for_frontend().await?;

        let mut updated_files = Vec::new();

        // 比较每个文件
        for new_file in v0_result.files {
            let should_update = if let Some(old_file) = current_result.files.iter()
                .find(|f| f.file_path == new_file.file_path) {
                // 只有当 hash 不同时才更新
                old_file.hash != new_file.hash
            } else {
                // 新文件
                true
            };

            if should_update {
                updated_files.push(new_file);
            }
        }

        // 只更新变化的文件
        let filtered_result = V0ParseResult {
            block_id: v0_result.block_id,
            files: updated_files,
        };

        self.save_from_frontend(filtered_result).await
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let sync = ProjectSync::new("./my-project");

    // 读取项目
    let v0_result = sync.read_for_frontend().await?;
    println!("读取了 {} 个文件", v0_result.files.len());

    // 保存项目
    let saved_files = sync.save_from_frontend(v0_result).await?;
    println!("保存了 {} 个文件", saved_files.len());

    Ok(())
}
```

## API 参考手册

### 主要函数

#### `project_to_v0_result(project_path, ignore_hidden) -> Result<V0ParseResult>`
- **参数**:
  - `project_path`: 项目根目录路径
  - `ignore_hidden`: 是否忽略隐藏目录（如 .git, .claude）
- **返回**: `V0ParseResult` 结构体
- **用途**: 从现有项目创建 V0ParseResult

#### `write_v0_result_to_project(project_path, v0_result) -> Result<Vec<String>>`
- **参数**:
  - `project_path`: 项目根目录路径
  - `v0_result`: 要写入的 V0ParseResult
- **返回**: 成功写入的文件路径列表
- **用途**: 将 V0ParseResult 写入文件系统

#### `calculate_hash(content) -> String`
- **参数**: `content`: 文件内容字符串
- **返回**: SHA256 哈希值的十六进制字符串
- **用途**: 计算文件内容的哈希值

### 核心结构体

#### `V0ParseResult`
```rust
pub struct V0ParseResult {
    pub block_id: String,           // 唯一标识符
    pub files: Vec<V0FileEntry>,    // 文件列表
}
```

#### `V0FileEntry`
```rust
pub struct V0FileEntry {
    pub file_type: String,          // 文件类型 (typescriptreact, css, etc.)
    pub file_path: PathBuf,         // 相对文件路径
    pub is_merged: bool,            // 是否已合并
    pub is_edit: bool,              // 是否正在编辑
    pub is_quick_edit: bool,        // 是否快速编辑
    pub url: Option<String>,        // 远程文件 URL (图片等)
    pub content: String,            // 文件内容
    pub hash: String,               // SHA256 哈希值
}
```

## 使用场景

### 1. 前后端分离的编辑器
- 前端请求项目文件进行编辑
- 前端修改后发送回后端保存
- 支持哈希验证，避免不必要的文件写入

### 2. 项目模板系统
- 读取模板项目作为基础
- 根据用户需求修改文件内容
- 生成新的项目实例

### 3. 代码生成器
- 读取现有项目结构
- 根据配置生成或修改代码文件
- 保持项目的完整性

### 4. 自动化构建工具
- 监控项目文件变化
- 自动同步到远程存储
- 支持增量更新

## 注意事项

1. **文件路径**: 所有路径都是相对于项目根目录的
2. **隐藏目录**: 默认会忽略以 `.` 开头的目录和文件
3. **哈希验证**: 写入时会检查文件是否已存在且内容相同
4. **URL 文件**: 支持从 URL 下载文件（如图片）
5. **错误处理**: 所有函数都返回 `anyhow::Result`，便于错误处理
6. **异步操作**: 所有文件操作都是异步的，使用 `tokio` 运行时

## 运行示例

```bash
# 运行内置示例
cargo run --bin project_to_v0 /path/to/project /path/to/output

# 运行测试
cargo test -p nuwax_parser

# 运行完整演示
cargo run -p nuwax_parser --bin test_nuwax_parser
```