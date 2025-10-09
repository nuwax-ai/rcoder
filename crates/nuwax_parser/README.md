# Nuwax Parser

Nuwax Parser 是专为 Nuwax 平台设计的高性能 Rust 文件解析和同步工具包，用于在前端和后端系统之间实现无缝的文件同步。内置支持哈希验证、URL文件下载和隐藏目录过滤功能。

## 功能特性

- **多格式支持**: 支持 CSS、TypeScript React、JavaScript、JSON、图片（JPG/PNG）和纯文本文件
- **哈希验证**: 基于 SHA256 的文件完整性检查
- **URL文件下载**: 自动下载远程文件（图片、资源文件）
- **隐藏目录过滤**: 自动排除以 "." 开头的目录（如 .claude）
- **WASM兼容**: 可编译为 WebAssembly 供前端使用
- **文件同步**: 智能双向文件同步，支持变更检测
- **便捷API**: 简单易用的函数式接口

## 快速开始

### 读取项目并转换为 V0ParseResult

```rust
use nuwax_parser::project_to_v0_result;
use std::path::PathBuf;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let project_path = PathBuf::from("./my-react-project");
    let v0_result = project_to_v0_result(&project_path, true).await?;

    println!("读取了 {} 个文件", v0_result.files.len());
    println!("Block ID: {}", v0_result.block_id);

    Ok(())
}
```

### 同步 V0ParseResult 到文件系统

```rust
use nuwax_parser::{project_to_v0_result, sync_v0_result_to_project};
use std::path::PathBuf;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let project_path = PathBuf::from("./my-project");

    // 1. 读取项目
    let v0_result = project_to_v0_result(&project_path, true).await?;

    // 2. 修改文件内容（可选）
    // v0_result.files[0].content = "新的内容".to_string();

    // 3. 同步到文件系统（包括删除多余的文件）
    let sync_result = sync_v0_result_to_project(&project_path, &v0_result).await?;
    println!("同步完成:");
    println!("  - 写入 {} 个文件", sync_result.written_files.len());
    println!("  - 删除 {} 个文件", sync_result.deleted_files.len());

    Ok(())
}
```

### 处理前端传来的 V0FileData

```rust
use nuwax_parser::{V0FileData, sync_v0_result_to_project};
use std::path::PathBuf;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // 前端传来的 V0FileData JSON
    let frontend_json = r#"
    {
        "blockId": "frontend-generated-id",
        "source": "[V0_FILE]typescriptreact:file=\"src/App.tsx\" isMerged=\"true\"\nimport React from 'react';\n\nexport default function App() {\n  return <div>Hello World</div>;\n}\n[V0_FILE]css:file=\"src/App.css\" isMerged=\"true\"\n.App { text-align: center; }"
    }
    "#;

    // 1. 解析 V0FileData
    let v0_data = V0FileData::from_json(frontend_json)?;

    // 2. 解析为 V0ParseResult
    let v0_result = v0_data.parse_source()?;

    // 3. 完全同步到文件系统
    let project_path = PathBuf::from("./backend-project");
    let sync_result = sync_v0_result_to_project(&project_path, &v0_result).await?;

    println!("✅ 成功同步到后端:");
    println!("  - 写入 {} 个文件", sync_result.written_files.len());
    println!("  - 删除 {} 个文件", sync_result.deleted_files.len());

    Ok(())
}
```

**注意**: `sync_v0_result_to_project` 会确保后端文件系统与前端 V0ParseResult 完全一致：

1. **更新或写入文件**: 前端存在的文件会在后端创建或更新
2. **删除多余文件**: 后端存在但前端不存在的文件会被删除
3. **清理空目录**: 自动清理删除文件后留下的空目录
4. **安全保护**: 不会删除重要文件和目录，包括：
   - 重要配置文件：package.json、Cargo.toml、README.md、LICENSE、.gitignore
   - 所有以 "." 开头的隐藏文件和目录（如 .claude、.git、.vscode、.idea 等）
   - 隐藏目录中的所有文件都会被保护，不会被删除

## 文件格式规范

### V0 文件结构

解析器处理包含文件元数据和内容的 JSON 格式：

```json
{
  "blockId": "unique-identifier-string",
  "source": "[V0_FILE]filetype:file=\"path/to/file\" isMerged=\"true\" url=\"https://...\"\n[文件内容]"
}
```

### 文件段落格式

源文件中的每个文件都用 `[V0_FILE]` 标记标记，遵循以下格式：

```
[V0_FILE]{文件类型}:file="{文件路径}" {属性}
{文件内容}
```

#### 支持的文件类型

| 扩展名 | 文件类型 | 描述 |
|--------|----------|------|
| `.tsx` | `typescriptreact` | TypeScript React 组件 |
| `.ts` | `typescript` | TypeScript 文件 |
| `.jsx` | `javascriptreact` | JavaScript React 组件 |
| `.js` | `javascript` | JavaScript 文件 |
| `.css` | `css` | CSS 样式表 |
| `.json` | `json` | JSON 配置文件 |
| `.jpg/.jpeg` | `jpg` | JPEG 图片 |
| `.png` | `png` | PNG 图片 |
| `.gif` | `gif` | GIF 图片 |
| `.md` | `markdown` | Markdown 文件 |
| `.txt` | `plaintext` | 纯文本文件 |
| `.rs` | `rust` | Rust 源代码文件 |
| `.toml` | `toml` | TOML 配置文件 |

#### 文件属性

| 属性 | 类型 | 必需 | 描述 |
|------|------|------|------|
| `file` | 字符串 | ✅ | 项目内的相对文件路径 |
| `isMerged` | 布尔值 | ❌ | 文件是否处于合并状态（默认：false） |
| `isEdit` | 布尔值 | ❌ | 文件是否处于编辑状态（默认：false） |
| `isQuickEdit` | 布尔值 | ❌ | 文件是否处于快速编辑模式（默认：false） |
| `url` | 字符串 | ❌ | 文件下载的远程 URL（用于图片/资源文件） |

### 内容处理规则

#### 1. 文本文件（CSS、TS、JS、JSON 等）
- 内容以纯文本形式存储在 source 字段中
- 行结束符按原样保留
- 完全支持 Unicode 字符
- 哈希值从原始内容字节计算

#### 2. 二进制文件（通过 URL 的图片）
- source 字段中的内容为空（0 字节）
- 同步期间从指定的 URL 下载文件
- 哈希值从下载的内容计算
- URL 必须可通过 HTTP/HTTPS 访问

#### 3. 哈希计算
- 使用 SHA256 算法
- 在原始文件内容字节上计算
- 存储为 64 字符的十六进制字符串
- 用于变更检测和完整性验证

### 隐藏目录过滤

解析器自动排除：
- 以 `.` 开头的文件和目录（如 `.claude`、`.git`）
- Claude Code 记忆文件：`CLAUDE.md`
- 常见的 IDE/配置目录
- 系统文件和元数据

**过滤路径示例：**
- `.claude/`
- `.git/`
- `.vscode/`
- `.DS_Store`
- `CLAUDE.md`
- `node_modules/.cache/`

**注意：** `CLAUDE.md` 是 Claude Code 工具的记忆文件，不会被传给前端，但会在后端受到保护不被删除。

## API 参考

### 便捷函数

#### `project_to_v0_result(project_path, ignore_hidden)` -> `Result<V0ParseResult>`
从项目路径直接创建 V0ParseResult

```rust
let v0_result = project_to_v0_result("./my-project", true).await?;
```

#### `sync_v0_result_to_project(project_path, v0_result)` -> `Result<SyncResult>`
完全同步 V0ParseResult 到文件系统（包括删除多余文件）

```rust
let sync_result = sync_v0_result_to_project("./my-project", &v0_result).await?;
println!("写入 {} 个文件", sync_result.written_files.len());
println!("删除 {} 个文件", sync_result.deleted_files.len());
```

#### `project_to_v0_json(project_path, ignore_hidden)` -> `Result<String>`
生成 V0 格式的 JSON 字符串

```rust
let v0_json = project_to_v0_json("./my-project", true).await?;
```

#### `calculate_hash(content)` -> `String`
计算文件内容的 SHA256 哈希值

```rust
let hash = calculate_hash("文件内容");
```

### 核心结构体

#### `V0ParseResult`
```rust
pub struct V0ParseResult {
    pub files: Vec<V0FileEntry>,  // 文件列表
    pub block_id: String,         // 唯一标识符
}
```

#### `V0FileEntry`
```rust
pub struct V0FileEntry {
    pub file_type: String,          // 文件类型
    pub file_path: PathBuf,         // 相对文件路径
    pub is_merged: bool,            // 是否已合并
    pub is_edit: bool,              // 是否正在编辑
    pub is_quick_edit: bool,        // 是否快速编辑
    pub url: Option<String>,        // 远程文件 URL
    pub content: String,            // 文件内容
    pub hash: String,               // SHA256 哈希值
}
```

#### `V0FileData`
```rust
pub struct V0FileData {
    pub block_id: String,  // 唯一标识符
    pub source: String,   // V0 格式的源字符串
}
```

#### `SyncResult`
```rust
pub struct SyncResult {
    pub written_files: Vec<String>,  // 写入的文件列表
    pub deleted_files: Vec<String>,  // 删除的文件列表
    pub success: bool,               // 同步是否成功
}
```

### 低级 API

#### `V0FileSync`
```rust
pub struct V0FileSync {
    // 使用基础路径创建新实例
    pub fn new<P: AsRef<Path>>(base_path: P) -> Self

    // 从 V0 数据同步文件
    pub async fn sync_files(&self, v0_data: &V0FileData) -> Result<Vec<String>>

    // 读取项目文件并过滤
    pub async fn read_project_files(&self, ignore_hidden: bool) -> Result<Vec<ProjectFile>>

    // 写入 V0ParseResult 到文件系统
    pub async fn write_v0_result(&self, v0_result: &V0ParseResult) -> Result<Vec<String>>
}
```

### 错误处理

该工具包使用 `anyhow::Result` 进行错误处理，包含自定义错误类型：

- `V0ParseError::InvalidFormat` - 文件格式错误
- `V0ParseError::MissingAttribute` - 缺少必需属性
- `V0ParseError::IoError` - 文件系统操作失败
- `V0ParseError::NetworkError` - URL 下载失败
- `V0ParseError::HashMismatch` - 文件完整性检查失败

## HTTP 服务器集成

### Axum 集成示例

```rust
use axum::{extract::Path, http::StatusCode, Json};
use nuwax_parser::{V0FileData, project_to_v0_result, write_v0_result_to_project};

// 读取项目发送给前端
async fn get_project(
    Path(project_path): Path<String>,
) -> Result<Json<V0ParseResult>, StatusCode> {
    match project_to_v0_result(&project_path, true).await {
        Ok(v0_result) => Ok(Json(v0_result)),
        Err(_) => Err(StatusCode::INTERNAL_SERVER_ERROR),
    }
}

// 接收前端修改并完全同步
async fn sync_project(
    Path(project_path): Path<String>,
    Json(v0_data): Json<V0FileData>,
) -> Result<Json<SyncResponse>, StatusCode> {
    match v0_data.parse_source() {
        Ok(v0_result) => {
            match sync_v0_result_to_project(&project_path, &v0_result).await {
                Ok(sync_result) => Ok(Json(SyncResponse {
                    success: sync_result.success,
                    written_files: sync_result.written_files,
                    deleted_files: sync_result.deleted_files,
                    total_written: sync_result.written_files.len(),
                    total_deleted: sync_result.deleted_files.len(),
                })),
                Err(_) => Err(StatusCode::INTERNAL_SERVER_ERROR),
            }
        }
        Err(_) => Err(StatusCode::BAD_REQUEST),
    }
}

#[derive(serde::Serialize)]
struct SyncResponse {
    success: bool,
    written_files: Vec<String>,
    deleted_files: Vec<String>,
    total_written: usize,
    total_deleted: usize,
}
```

### 智能同步示例

```rust
use nuwax_parser::{V0FileData, project_to_v0_result, sync_v0_result_to_project};

async fn smart_sync(
    project_path: String,
    v0_data: V0FileData,
) -> anyhow::Result<SyncResult> {
    // 1. 读取当前项目状态
    let current_result = project_to_v0_result(&project_path, true).await?;

    // 2. 解析前端传来的新数据
    let frontend_result = v0_data.parse_source()?;

    // 3. 过滤出有变化的文件
    let updated_files: Vec<V0FileEntry> = frontend_result.files
        .into_iter()
        .filter(|new_file| {
            !current_result.files.iter().any(|old_file| {
                old_file.file_path == new_file.file_path && old_file.hash == new_file.hash
            })
        })
        .collect();

    // 4. 创建只包含更新文件的 V0ParseResult
    let filtered_result = nuwax_parser::V0ParseResult {
        block_id: frontend_result.block_id,
        files: updated_files,
    };

    // 5. 智能同步到文件系统（只同步有变化的文件）
    sync_v0_result_to_project(&project_path, &filtered_result).await
}
```

## WASM 前端使用

```rust
#[wasm_bindgen]
pub fn parse_project_to_v0(json_data: &str) -> Result<String, String> {
    let v0_data = V0FileData::from_json(json_data)
        .map_err(|e| e.to_string())?;

    let result = v0_data.parse_source()
        .map_err(|e| e.to_string())?;

    serde_json::to_string(&result).map_err(|e| e.to_string())
}

#[wasm_bindgen]
pub fn generate_v0_from_files(files_js: &JsValue) -> Result<String, String> {
    let files: Vec<ProjectFile> = serde_wasm_bindgen::from_value(files_js)
        .map_err(|e| e.to_string())?;

    generate_v0_format(&files).map_err(|e| e.to_string())
}
```

## 性能考虑

- **内存使用**: 文件加载到内存中进行处理
- **哈希计算**: SHA256 是 CPU 密集型操作，但提供强大的完整性保证
- **网络请求**: URL 下载是异步的，并遵循 HTTP 超时
- **目录遍历**: 使用 `walkdir` 进行高效的文件系统遍历
- **WASM 目标**: 编译为 WASM 时，考虑大文件的内存限制
- **增量更新**: 自动检测文件变化，只更新有变化的文件

## 安全说明

- URL 下载遵循 HTTP 重定向，但验证 SSL 证书
- 文件路径经过清理，防止目录遍历攻击
- 不执行来自文件内容的任意代码
- 哈希验证防止同步文件被篡改
- 隐藏目录过滤保护敏感文件

## 测试

运行测试套件：

```bash
cargo test -p nuwax_parser
```

使用示例数据运行完整测试：

```bash
cargo run -p nuwax_parser --bin test_nuwax_parser
```

## 项目文件读取器 (ProjectReader)

新增的通用项目文件读取器，可将任何项目目录转换为 `ProjectSourceCode` 结构，兼容 lovable-sourcecode.json 格式。

### 基本使用

```rust
use nuwax_parser::project_op::{ProjectReader, ProjectReadConfig};

// 使用默认配置
let reader = ProjectReader::new();
let project = reader.read_project("/path/to/project")?;

// 使用自定义配置
let config = ProjectReadConfig::new()
    .max_file_size(512 * 1024) // 512KB
    .include_hidden_files(true)
    .add_exclude_extension("tmp");

let reader = ProjectReader::with_config(config);
let project = reader.read_project("/path/to/project")?;
```

### 配置选项

```rust
let config = ProjectReadConfig::new()
    .add_exclude_file("secret.txt")          // 排除特定文件
    .add_exclude_dir("temp")                  // 排除特定目录
    .add_exclude_extension("log")             // 排除特定扩展名
    .max_file_size(1024 * 1024)              // 最大文件大小 1MB
    .include_hidden_dirs(false)               // 不包含隐藏目录
    .include_hidden_files(false);             // 不包含隐藏文件
```

### 默认排除规则

默认情况下会排除：
- **文件**: `CLAUDE.md`, `node_modules`, `.git`, `target`, `dist`, `build`
- **目录**: `.git`, `node_modules`, `target`, `dist`, `build`
- **扩展名**: `.lock`, `.log`
- **隐藏文件**: 所有以 `.` 开头的文件和目录
- **大文件**: 超过 1MB 的文件（内容不会被读取）

### 数据结构

```rust
let project = ProjectSourceCode::new()
    .with_files(vec![
        FileInfo::new("src/main.rs")
            .with_contents("fn main() { println!(\"Hello\"); }")
            .binary(false)
            .size_exceeded(false),
    ]);
```

### 示例程序

```bash
cargo run --example read_project -- /path/to/project
```

## 模块结构

```
src/
├── lib.rs                     # 主入口，包含文档和重新导出
├── model/                    # 数据结构定义
│   ├── source_code.rs        # ProjectSourceCode 和 FileInfo
│   └── mod.rs               # 模块导出
├── project_op/               # 项目操作模块
│   ├── project_read.rs       # 项目文件读取器
│   └── mod.rs               # 模块导出
├── types.rs                  # V0 格式相关结构体
├── parsing.rs                # V0 格式解析逻辑
├── sync.rs                   # 文件同步相关功能
├── utils.rs                  # 工具函数和便捷函数
└── bin/
    └── test_nuwax_parser.rs  # 测试工具
```

## 许可证

此工具包是 Nuwax 项目的一部分，采用 MIT OR Apache 2.0 许可证。

## 贡献

添加新文件类型支持时：
1. 更新 `utils.rs` 中的 `determine_file_type` 函数
2. 添加相应的测试
3. 更新此文档
4. 考虑 WASM 兼容性影响