# HttpProjectManager 项目加载功能使用示例

## 功能概述

`HttpProjectManager` 现在支持在初始化时自动加载 `working_dir` 中现有的项目信息。这包括：

1. **自动项目发现**：扫描工作目录，查找以 UUID 命名的项目目录
2. **元数据持久化**：每个项目目录包含 `project_metadata.json` 文件存储项目信息
3. **创建时间恢复**：从元数据文件或目录修改时间获取项目创建时间
4. **向后兼容**：支持没有元数据文件的老项目

## 使用方法

### 1. 创建带有项目加载的管理器

```rust
use std::path::PathBuf;
use http_server::http_interface::HttpProjectManager;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // 指定工作目录
    let working_dir = PathBuf::from("./projects");
    
    // 创建管理器并加载现有项目
    let project_manager = HttpProjectManager::new_with_loading(working_dir).await?;
    
    // 现在可以列出所有加载的项目
    let projects = project_manager.list_projects().await;
    println!("加载了 {} 个项目", projects.len());
    
    for project in projects {
        println!("项目 ID: {}, 路径: {}, 创建时间: {}", 
                 project.id, 
                 project.path.display(), 
                 project.created_at);
    }
    
    Ok(())
}
```

### 2. 创建新项目

```rust
use http_server::http_interface::{HttpProjectManager, CreateProjectRequest};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let working_dir = PathBuf::from("./projects");
    let project_manager = HttpProjectManager::new_with_loading(working_dir).await?;
    
    // 创建新项目
    let request = CreateProjectRequest {
        user_id: "user123".to_string(),
    };
    
    let new_project = project_manager.create_project(request).await?;
    println!("创建了新项目: {} 在 {}", new_project.id, new_project.path.display());
    
    Ok(())
}
```

## 项目目录结构

每个项目目录的结构如下：

```
projects/
├── 01234567-89ab-cdef-0123-456789abcdef/     # 项目 UUID 目录
│   ├── project_metadata.json                 # 项目元数据文件
│   └── ...                                   # 项目文件
├── 12345678-9abc-def0-1234-56789abcdef0/
│   ├── project_metadata.json
│   └── ...
└── ...
```

## 元数据文件格式

`project_metadata.json` 文件包含以下信息：

```json
{
  "id": "01234567-89ab-cdef-0123-456789abcdef",
  "path": "/path/to/projects/01234567-89ab-cdef-0123-456789abcdef",
  "created_at": "2024-01-01T00:00:00Z"
}
```

## 向后兼容性

- 如果项目目录存在但没有 `project_metadata.json` 文件，系统会使用目录的修改时间作为创建时间
- 如果元数据文件损坏，系统会回退到使用目录修改时间
- 所有现有的项目目录都会被自动识别和加载

## 错误处理

系统会优雅地处理各种错误情况：

- 无法解析的目录名会被忽略
- 损坏的元数据文件不会阻止其他项目的加载
- 权限问题会被记录但不会导致整个加载过程失败

## 日志输出

系统会输出详细的日志信息：

```
INFO Loading existing projects from: ./projects
DEBUG Loaded project: 01234567-89ab-cdef-0123-456789abcdef from ./projects/01234567-89ab-cdef-0123-456789abcdef
INFO Loaded 5 existing projects
INFO Created new project: 12345678-9abc-def0-1234-56789abcdef0 at ./projects/12345678-9abc-def0-1234-56789abcdef0
```
