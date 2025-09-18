# V0FileData 同步使用示例

这个示例展示如何处理前端传来的 V0FileData 数据，解析并覆盖后端文件系统。

## 主要同步方式

### 方式1: 直接使用 V0FileData (推荐)

```rust
use nuwax_parser::{V0FileData, V0FileSync};
use std::path::PathBuf;
use anyhow::Result;

#[tokio::main]
async fn main() -> Result<()> {
    // 1. 前端传来 V0FileData JSON 字符串
    let v0_json = r#"
    {
        "blockId": "frontend-generated-id",
        "source": "[V0_FILE]typescriptreact:file=\"src/App.tsx\" isMerged=\"true\"\nimport React from 'react';\n\nexport default function App() {\n  return <div>Hello World</div>;\n}\n[V0_FILE]css:file=\"src/App.css\" isMerged=\"true\"\n.App { text-align: center; }"
    }
    "#;

    // 2. 解析 V0FileData
    let v0_data = V0FileData::from_json(v0_json)?;

    // 3. 创建同步器，指定后端项目路径
    let project_path = PathBuf::from("./backend-project");
    let sync = V0FileSync::new(&project_path);

    // 4. 同步文件到后端 (会自动处理哈希验证、URL下载等)
    let synced_files = sync.sync_files(&v0_data).await?;

    println!("✅ 成功同步 {} 个文件到 {:?}", synced_files.len(), project_path);
    for file in synced_files {
        println!("  - {}", file);
    }

    Ok(())
}
```

### 方式2: 先解析为 V0ParseResult 再同步

```rust
use nuwax_parser::{V0FileData, write_v0_result_to_project};
use std::path::PathBuf;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // 1. 前端传来的 V0FileData
    let v0_json = /* 前端传来的 JSON */;
    let v0_data = V0FileData::from_json(v0_json)?;

    // 2. 解析为 V0ParseResult (这样可以查看和修改文件信息)
    let v0_result = v0_data.parse_source()?;

    println!("📊 解析到 {} 个文件:", v0_result.files.len());
    for (i, file) in v0_result.files.iter().enumerate() {
        println!("  {}. {} ({}) - {} bytes",
            i + 1,
            file.file_path.display(),
            file.file_type,
            file.content.len()
        );
    }

    // 3. 直接写入后端文件系统
    let project_path = PathBuf::from("./backend-project");
    let written_files = write_v0_result_to_project(&project_path, &v0_result).await?;

    println!("✅ 成功写入 {} 个文件", written_files.len());

    Ok(())
}
```

### 方式3: 智能同步 (增量更新)

```rust
use nuwax_parser::{V0FileData, V0FileSync, project_to_v0_result};
use std::path::PathBuf;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let project_path = PathBuf::from("./backend-project");

    // 1. 读取当前项目状态 (用于比较)
    let current_result = project_to_v0_result(&project_path, true).await?;

    // 2. 解析前端传来的新数据
    let frontend_json = /* 前端传来的 JSON */;
    let frontend_data = V0FileData::from_json(frontend_json)?;
    let frontend_result = frontend_data.parse_source()?;

    // 3. 比较差异，只更新有变化的文件
    let mut updated_count = 0;
    let mut new_files = Vec::new();

    for new_file in frontend_result.files {
        let should_update = if let Some(old_file) = current_result.files.iter()
            .find(|f| f.file_path == new_file.file_path) {
            // 只有当内容不同时才更新
            old_file.hash != new_file.hash
        } else {
            // 新文件
            true
        };

        if should_update {
            new_files.push(new_file);
            updated_count += 1;
        }
    }

    println!("🔄 检测到 {} 个文件需要更新", updated_count);

    // 4. 创建只包含更新文件的 V0ParseResult
    let filtered_result = nuwax_parser::V0ParseResult {
        block_id: frontend_result.block_id,
        files: new_files,
    };

    // 5. 写入文件系统
    let written_files = write_v0_result_to_project(&project_path, &filtered_result).await?;
    println!("✅ 成功更新 {} 个文件", written_files.len());

    Ok(())
}
```

## HTTP 服务器集成示例

### Axum 路由处理

```rust
use axum::{extract::Path, http::StatusCode, Json};
use nuwax_parser::{V0FileData, V0FileSync, write_v0_result_to_project};
use serde::Deserialize;

#[derive(Deserialize)]
struct ProjectPath {
    path: String,
}

// 接收 V0FileData JSON 并同步到文件系统
async fn sync_v0_data(
    Path(ProjectPath { path }): Path<ProjectPath>,
    Json(v0_data): Json<V0FileData>,
) -> Result<Json<SyncResponse>, StatusCode> {
    match write_v0_result_to_project(&path, &v0_data.parse_source().map_err(|_| StatusCode::BAD_REQUEST)?).await {
        Ok(files) => Ok(Json(SyncResponse {
            synced_files: files,
            count: files.len(),
            success: true,
        })),
        Err(_) => Err(StatusCode::INTERNAL_SERVER_ERROR),
    }
}

#[derive(serde::Serialize)]
struct SyncResponse {
    synced_files: Vec<String>,
    count: usize,
    success: bool,
}

// 或者直接处理 V0FileData 的 sync_files 方法
async fn sync_directly(
    Path(ProjectPath { path }): Path<ProjectPath>,
    Json(v0_data): Json<V0FileData>,
) -> Result<Json<SyncResponse>, StatusCode> {
    let sync = V0FileSync::new(&path);
    match sync.sync_files(&v0_data).await {
        Ok(files) => Ok(Json(SyncResponse {
            synced_files: files,
            count: files.len(),
            success: true,
        })),
        Err(_) => Err(StatusCode::INTERNAL_SERVER_ERROR),
    }
}
```

### 完整的服务器示例

```rust
use axum::{routing::{post, get}, Router, Json};
use nuwax_parser::{V0FileData, project_to_v0_result, write_v0_result_to_project};
use std::net::SocketAddr;

#[tokio::main]
async fn main() {
    let app = Router::new()
        .route("/api/project/:path/sync", post(sync_project))
        .route("/api/project/:path", get(get_project));

    let addr = SocketAddr::from(([127, 0, 0, 1], 3000));
    println!("Server running on {}", addr);

    axum::Server::bind(&addr)
        .serve(app.into_make_service())
        .await
        .unwrap();
}

// GET 读取项目供前端编辑
async fn get_project(Path(path): Path<String>) -> Result<Json<V0ParseResult>, StatusCode> {
    match project_to_v0_result(&path, true).await {
        Ok(result) => Ok(Json(result)),
        Err(_) => Err(StatusCode::INTERNAL_SERVER_ERROR),
    }
}

// POST 接收前端修改并同步
async fn sync_project(Path(path): Path<String>, Json(v0_data): Json<V0FileData>) -> Result<Json<SyncResponse>, StatusCode> {
    match v0_data.parse_source() {
        Ok(v0_result) => {
            match write_v0_result_to_project(&path, &v0_result).await {
                Ok(files) => Ok(Json(SyncResponse {
                    synced_files: files,
                    count: files.len(),
                    success: true,
                })),
                Err(_) => Err(StatusCode::INTERNAL_SERVER_ERROR),
            }
        }
        Err(_) => Err(StatusCode::BAD_REQUEST),
    }
}

#[derive(serde::Serialize)]
struct SyncResponse {
    synced_files: Vec<String>,
    count: usize,
    success: bool,
}
```

## 前端调用示例

### JavaScript/Fetch

```javascript
// 前端发送 V0FileData 到后端
async function syncProject(v0Data, projectPath) {
    const response = await fetch(`/api/project/${encodeURIComponent(projectPath)}/sync`, {
        method: 'POST',
        headers: {
            'Content-Type': 'application/json',
        },
        body: JSON.stringify(v0Data),
    });

    const result = await response.json();
    console.log('Sync result:', result);
    return result;
}

// 使用示例
const v0Data = {
    blockId: "unique-id-123",
    source: "[V0_FILE]typescriptreact:file=\"src/App.tsx\" isMerged=\"true\"\n// React component code here..."
};

syncProject(v0Data, './my-project')
    .then(result => console.log('Synced', result.count, 'files'))
    .catch(error => console.error('Sync failed:', error));
```

## 关键特性

### 1. 自动处理
- **哈希验证**: 自动检测文件是否需要更新
- **目录创建**: 自动创建不存在的目录结构
- **URL下载**: 自动处理图片等远程资源文件
- **权限处理**: 自动处理文件写入权限

### 2. 安全性
- **路径清理**: 防止路径遍历攻击
- **内容验证**: 验证文件完整性
- **错误处理**: 完整的错误处理机制

### 3. 性能优化
- **增量更新**: 只更新有变化的文件
- **异步操作**: 高性能的异步文件操作
- **内存管理**: 合理的内存使用

### 4. 灵活性
- **多种API**: 提供 V0FileData 和 V0ParseResult 两种接口
- **自定义过滤**: 支持隐藏目录过滤
- **扩展性**: 易于扩展和定制

选择最适合你使用场景的方式即可！