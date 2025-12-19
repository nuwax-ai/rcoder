# DuckDB 内存数据库替代 DashMap 设计规范

## 1. 背景与目标

### 1.1 背景

#### 1.1.3 系统约束

**重要约束**: 数据规模较小，无需考虑大数据量优化策略

- **数据量评估**: DuckDB 数据库中的数据量不会很大
- **统计查询特点**: 统计类查询（如容器数量统计）需要统计所有数据，不受时间范围限制
- **设计影响**: 可以简化索引策略，优先考虑查询的简洁性和可维护性

当前 `crates/rcoder` 模块使用 `DashMap` 来管理运行时状态数据，但存在两个不同的业务场景：

#### 1.1.1 RCoder 场景 (当前主要使用)
- **容器标识**: `project_id` 对应一个容器
- **容器命名**: `rcoder-agent-{project_id}`
- **工作目录**: `/app/project_workspace/{project_id}`
- **Agent实例**: 每个容器只运行一个 Agent 实例
- **映射关系**: `project_id -> ProjectAndContainerInfo`

### 1.1.2 重要约束
- **数据规模**: 数据量较小，无需考虑大数据量优化
- **统计查询**: 统计类查询（如容器数量统计）需要统计所有数据，不受时间范围限制
- **查询模式**: 主要为精确查询和全表统计，无需复杂的范围查询优化
- **事务要求**: 大部分操作不需要强事务保证，只有 `agent_status` 状态变更需要原子性
- **结构体分类**: 公共结构体放在 shared_types 模块，专用结构体放在 duckdb_manager 模块
- **内存模式**: DuckDB 使用内存模式，每次容器重启都是全新状态

#### 1.1.2 ComputerAgentRunner 场景 (新功能)
- **容器标识**: `user_id` 对应一个容器
- **容器命名**: `computer-agent-runner-{user_id}`
- **工作目录**: `/home/user` (通过挂载配置映射)
- **Agent实例**: 一个容器内可以运行多个 `project_id` 的 Agent 实例
- **映射关系**: `user_id -> ProjectAndContainerInfo`

### 1.1.3 模块架构约束
- **专用模块**: 创建 `crates/duckdb_manager` 模块专门管理数据库操作
- **接口封装**: 以 lib 库形式提供统一的数据访问接口
- **业务隔离**: `crates/rcoder` 通过接口使用，避免直接数据库操作
- **职责分离**: 数据库操作与业务逻辑分离，其他模块不直接操作数据库

#### 1.1.3 现有 DashMap 结构

```rust
pub struct AppState {
    /// 活跃的项目和容器映射
    /// - RCoder: project_id -> ProjectAndContainerInfo
    /// - ComputerAgentRunner: user_id -> ProjectAndContainerInfo
    pub project_and_agent_map: DashMap<String, Arc<ProjectAndContainerInfo>>,

    /// 会话映射, session_id -> ProjectAndContainerInfo
    pub sessions: DashMap<String, Arc<ProjectAndContainerInfo>>,

    /// 会话到容器ID的映射, session_id -> container_id
    pub session_to_container_id: DashMap<String, String>,
}
```

### 1.2 业务目标

使用 DuckDB 内存模式替代当前的 `DashMap`，实现：

1. **统一数据模型**: 通过关系型数据库设计，提供更清晰的数据结构和关系
2. **SQL 查询能力**: 支持复杂查询，如按时间范围筛选闲置容器、按服务类型统计等
3. **事务支持**: 保证多表操作的原子性
4. **内存模式**: 数据随容器重启重置，无需持久化
5. **高性能**: DuckDB 的列式存储和向量化执行引擎提供高效查询
6. **双模式支持**: 同时支持 RCoder 和 ComputerAgentRunner 两种业务场景

### 1.3 DuckDB 简介

DuckDB 是一个嵌入式分析型数据库，具有以下特点：

- **嵌入式**: 无需外部服务器，直接嵌入应用程序
- **内存模式**: 支持纯内存数据库，重启后数据清零
- **高性能**: 列式存储 + 向量化执行
- **Rust 支持**: 官方提供 `duckdb` crate

---

## 2. 现状分析

### 2.1 现有数据结构

#### AppState 中的 DashMap 字段

```rust
// crates/rcoder/src/router.rs
pub struct AppState {
    pub config: AppConfig,
    
    /// 活跃的会话映射, session_id -> ProjectAndContainerInfo
    pub sessions: DashMap<String, Arc<ProjectAndContainerInfo>>,
    
    /// 活跃的项目和容器映射, project_id -> ProjectAndContainerInfo
    pub project_and_agent_map: DashMap<String, Arc<ProjectAndContainerInfo>>,
    
    /// 会话到容器ID的映射, session_id -> container_id
    pub session_to_container_id: DashMap<String, String>,
    
    // ... 其他字段
}
```

#### ProjectAndContainerInfo 结构

```rust
// crates/shared_types/src/model/agent_project_runner_model.rs

pub struct ProjectCoreState {
    pub project_id: String,
    pub user_id: Option<String>,
    pub session_id: Option<String>,
    pub last_activity: DateTime<Utc>,
    pub created_at: DateTime<Utc>,
}

pub struct ProjectExtendedState {
    pub model_provider: Option<ModelProviderConfig>,
    pub container: Option<ContainerBasicInfo>,
    pub request_id: Option<String>,
    pub status: Option<AgentStatus>,
    pub service_type: Option<ServiceType>,
}

pub struct ContainerBasicInfo {
    pub container_id: String,
    pub container_name: String,
    pub container_ip: String,
    pub internal_port: u16,
    pub external_port: u16,
    pub project_id: String,
    pub status: String,
    pub created_at: DateTime<Utc>,
    pub service_url: String,
}
```

### 2.2 现有操作模式

| 操作类型 | 使用场景 | 频率 |
|---------|---------|------|
| 插入/更新 | 新会话创建、容器启动 | 中 |
| 查询(按Key) | 获取容器信息、SSE 连接建立 | 高 |
| 查询(遍历) | 按 session_id 查找项目、闲置检测 | 中 |
| 删除 | 容器清理、会话过期 | 低 |
| 条件查询 | 闲置超时检测 (`last_activity` 筛选) | 低(定时任务) |

### 2.3 现有代码依赖分析

使用 DashMap 的文件：

| 文件路径 | 使用方式 |
|---------|---------|
| `crates/rcoder/src/router.rs` | 定义 AppState |
| `crates/rcoder/src/handler/chat_handler.rs` | 项目信息的获取/创建/更新 |
| `crates/rcoder/src/handler/computer_chat_handler.rs` | ComputerAgent 映射管理 |
| `crates/rcoder/src/handler/agent_session_notification.rs` | 会话查找 |
| `crates/rcoder/src/handler/pod_handler.rs` | Pod 容器管理 |
| `crates/rcoder/src/proxy_agent/cleanup_task.rs` | 闲置容器清理 |
| `crates/rcoder/src/service/container_status_checker.rs` | 容器状态更新 |

---

## 3. 数据库设计

### 3.1 表结构设计

#### 3.1.1 容器信息表 (containers)

存储所有容器的基本信息，每个容器对应一个服务实例。

| 列名 | 类型 | 约束 | 说明 |
|-----|------|------|------|
| container_id | VARCHAR | PRIMARY KEY | 容器唯一标识 |
| container_name | VARCHAR | NOT NULL | 容器名称 |
| container_ip | VARCHAR | NOT NULL | 容器 IP 地址 |
| internal_port | INTEGER | NOT NULL | 内部端口 |
| external_port | INTEGER | NOT NULL | 外部端口 |
| service_type | VARCHAR | NOT NULL | 服务类型 (RCoder/ComputerAgentRunner) |
| status | VARCHAR | NOT NULL | 容器状态 |
| service_url | VARCHAR | NOT NULL | 服务 URL |
| created_at | TIMESTAMP | NOT NULL | 创建时间 |
| last_activity | TIMESTAMP | NOT NULL | 最后活动时间 |

```sql
CREATE TABLE containers (
    container_id VARCHAR PRIMARY KEY,
    container_name VARCHAR NOT NULL,
    container_ip VARCHAR NOT NULL,
    internal_port INTEGER NOT NULL,
    external_port INTEGER NOT NULL,
    service_type VARCHAR NOT NULL,  -- 'RCoder' 或 'ComputerAgentRunner'
    status VARCHAR NOT NULL,
    service_url VARCHAR NOT NULL,
    created_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
    last_activity TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP
);

-- 索引 (无外键约束)
CREATE INDEX idx_containers_service_type ON containers(service_type);
CREATE INDEX idx_containers_last_activity ON containers(last_activity);
```

#### 3.1.2 统一项目表 (projects) - 已合并session信息

存储所有类型的项目信息和关联的session信息，通过 service_type 字段区分不同业务场景。遵循第一范式设计原则，已合并sessions表以优化查询性能。

| 列名 | 类型 | 约束 | 说明 |
|-----|------|------|------|
| project_id | VARCHAR | PRIMARY KEY | 项目唯一标识 |
| session_id | VARCHAR | NULL | 当前活跃会话 ID（可为空） |
| service_type | VARCHAR | NOT NULL | 服务类型 (RCoder/ComputerAgentRunner) |
| container_id | VARCHAR | NOT NULL | 关联的容器 ID |
| user_id | VARCHAR | NULL | ComputerAgentRunner 用户 ID (RCoder 模式为 NULL) |
| agent_status_code | INTEGER | NULL | Agent 状态码 (0=Active, 1=Idle, 2=Terminating) |
| agent_status_name | VARCHAR | NULL | Agent 状态描述 (Active/Idle/Terminating) |
| request_id | VARCHAR | NULL | 当前请求 ID |
| model_provider_json | VARCHAR | NULL | 模型配置 (JSON 序列化) |
| created_at | TIMESTAMP | NOT NULL | 项目创建时间 |
| last_activity | TIMESTAMP | NOT NULL | 项目最后活动时间 |
| session_created_at | TIMESTAMP | NULL | 会话创建时间（可为空） |
| session_last_activity | TIMESTAMP | NULL | 会话最后活动时间（可为空） |

```sql
-- 合并后的projects表（包含session信息）
CREATE TABLE projects (
    project_id VARCHAR PRIMARY KEY,
    session_id VARCHAR,  -- 从sessions表合并，允许NULL（无活跃session）
    service_type VARCHAR NOT NULL,  -- 'RCoder' 或 'ComputerAgentRunner'
    container_id VARCHAR NOT NULL,  -- 无外键约束，通过应用层保证一致性
    user_id VARCHAR,  -- ComputerAgentRunner 模式时使用，RCoder 模式为 NULL
    agent_status_code INTEGER,  -- Agent状态码 (0=Active, 1=Idle, 2=Terminating)
    agent_status_name VARCHAR,  -- Agent状态描述 (Active/Idle/Terminating)
    request_id VARCHAR,
    model_provider_json VARCHAR,
    created_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
    last_activity TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
    session_created_at TIMESTAMP,  -- session创建时间，可为空
    session_last_activity TIMESTAMP,  -- session最后活动时间，可为空

    -- 约束：RCoder 模式下 user_id 必须为 NULL，ComputerAgentRunner 模式下 user_id 必须不为 NULL
    CHECK (
        (service_type = 'RCoder' AND user_id IS NULL) OR
        (service_type = 'ComputerAgentRunner' AND user_id IS NOT NULL)
    ),

    -- 约束：agent_status_code 和 agent_status_name 要么都为NULL，要么都为非NULL且匹配
    CHECK (
        (agent_status_code IS NULL AND agent_status_name IS NULL) OR
        (agent_status_code IS NOT NULL AND agent_status_name IS NOT NULL AND
         ((agent_status_code = 0 AND agent_status_name = 'Active') OR
          (agent_status_code = 1 AND agent_status_name = 'Idle') OR
          (agent_status_code = 2 AND agent_status_name = 'Terminating')))
    )
);

-- 索引优化（重点支持session_id查询和状态筛选）
CREATE INDEX idx_projects_session_id ON projects(session_id);  -- 核心查询：SSE消息转发
CREATE INDEX idx_projects_container_id ON projects(container_id);
CREATE INDEX idx_projects_user_id ON projects(user_id);  -- ComputerAgentRunner 模式
CREATE INDEX idx_projects_agent_status_code ON projects(agent_status_code);  -- 状态查询优化
CREATE INDEX idx_projects_last_activity ON projects(last_activity);
CREATE INDEX idx_projects_service_type ON projects(service_type);
CREATE INDEX idx_projects_service_type_activity ON projects(service_type, last_activity);
CREATE INDEX idx_projects_status_activity ON projects(agent_status_code, last_activity);  -- 清理任务优化
```

### 3.2 轻量一致性与容错策略

#### 3.2.1 轻量一致性保证

大部分操作为轻量级更新，采用以下策略保证基本一致性：

1. **容器存在性检查**: 创建项目前检查容器是否存在（轻量查询）
2. **业务规则校验**: 通过 CHECK 约束保证基本的业务规则
3. **应用层级联**: 删除操作时手动清理关联数据，无需复杂事务

#### 3.2.2 业务规则约束

1. **RCoder 模式**: `service_type = 'RCoder'` 时，`user_id IS NULL`
2. **ComputerAgentRunner 模式**: `service_type = 'ComputerAgentRunner'` 时，`user_id IS NOT NULL`
3. **Agent状态一致性**: `agent_status_code` 和 `agent_status_name` 要么都为NULL，要么都为非NULL且相互匹配

#### 3.2.3 内存模式特性

使用 DuckDB 内存模式，每次容器重启都会获得全新的空数据库，这是设计的核心特性：

```rust
/// DuckDB 内存模式初始化
pub struct DuckDbMemoryStorage {
    connection: Arc<Mutex<Connection>>,
}

impl DuckDbMemoryStorage {
    /// 初始化内存数据库
    pub fn new() -> Result<Self, StorageError> {
        // 创建内存数据库连接
        let connection = Connection::open_in_memory()?;

        // 初始化表结构
        Self::init_tables(&connection)?;

        Ok(Self {
            connection: Arc::new(Mutex::new(connection)),
        })
    }

    /// 初始化数据库表结构
    fn init_tables(conn: &Connection) -> Result<(), StorageError> {
        // 创建 containers 表
        conn.execute(
            "CREATE TABLE containers (...)",
            [],
        )?;

        // 创建 projects 表（已包含session信息）
        conn.execute(
            "CREATE TABLE projects (...)",
            [],
        )?;

        // 创建索引
        Self::create_indexes(conn)?;

        Ok(())
    }
}
```

#### 3.2.4 原子性操作范围

**需要事务保证的操作**:
- `agent_status` 状态变更

**轻量级操作**（无需事务）:
- `last_activity` 时间更新
- 会话信息维护
- 基本 CRUD 操作

### 3.3 内存模式设计优势

#### 3.3.1 天然的容错性

- **无持久化故障**: 内存模式下不存在数据文件损坏的问题
- **重启即清理**: 每次容器重启自动获得干净的状态
- **简化部署**: 无需考虑数据库文件的备份和恢复

#### 3.3.2 性能优势

- **内存访问**: 数据直接在内存中，访问速度极快
- **无磁盘I/O**: 避免磁盘读写瓶颈
- **轻量事务**: 只有必要的状态变更使用事务，大部分操作零开销

#### 3.3.3 架构简化

- **无降级策略**: 不需要复杂的故障恢复机制
- **简化监控**: 无需监控数据库健康状态
- **部署友好**: 容器化环境下天然适合内存模式

### 3.4 Agent状态字段设计优化

#### 3.4.1 设计理念

将单一的 `agent_status` 字段拆分为 `agent_status_code` (数字) 和 `agent_status_name` (字符串) 的设计有以下优势：

1. **查询性能优化**: 数字状态码在索引和比较操作中更高效
2. **扩展性**: 支持未来增加更多状态而不需要修改现有数据
3. **国际化支持**: 状态名称可以根据语言环境显示不同的文本
4. **向后兼容**: 通过枚举提供类型安全，同时支持数据库层的灵活性

#### 3.4.2 枚举设计

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum AgentStatus {
    /// 活跃状态 - 正在处理请求
    Active = 0,
    /// 空闲状态 - 等待新请求
    Idle = 1,
    /// 正在终止
    Terminating = 2,
}
```

#### 3.4.3 数据库约束

```sql
-- 确保状态码和状态名的一致性
CHECK (
    (agent_status_code IS NULL AND agent_status_name IS NULL) OR
    (agent_status_code IS NOT NULL AND agent_status_name IS NOT NULL AND
     ((agent_status_code = 0 AND agent_status_name = 'Active') OR
      (agent_status_code = 1 AND agent_status_name = 'Idle') OR
      (agent_status_code = 2 AND agent_status_name = 'Terminating')))
)
```

#### 3.4.4 索引优化

```sql
-- 状态码索引 - 数字比较更快
CREATE INDEX idx_projects_agent_status_code ON projects(agent_status_code);

-- 复合索引 - 优化清理任务
CREATE INDEX idx_projects_status_activity ON projects(agent_status_code, last_activity);
```

#### 3.4.5 查询性能对比

```sql
-- 优化前：字符串比较
SELECT * FROM projects WHERE agent_status = 'Idle';

-- 优化后：数字比较（更快）
SELECT * FROM projects WHERE agent_status_code = 1;

-- 复合查询：状态+时间（索引更有效）
SELECT * FROM projects
WHERE agent_status_code = 1 AND last_activity < ?
```

### 3.5 DuckDB-RS 适配说明

接口设计完全基于 DuckDB-RS 的实际使用模式：

#### 3.5.1 连接管理适配

- **内存数据库**: 使用 `Connection::open_in_memory()` 初始化
- **并发访问**: 通过 `try_clone()` 支持多连接并发操作
- **连接生命周期**: 支持独立关闭，避免资源泄漏

```rust
/// 连接管理最佳实践
pub struct DuckDbStorage {
    /// 主连接 - 用于 DDL 和管理操作
    connection: Arc<Mutex<Connection>>,
}

impl DuckDbStorage {
    /// 创建内存数据库
    pub fn new() -> Result<Self, DuckDbError> {
        let connection = Connection::open_in_memory()
            .map_err(|e| DuckDbError::ConnectionError(e.to_string()))?;
        Ok(Self {
            connection: Arc::new(Mutex::new(connection)),
        })
    }

    /// 创建工作连接（用于并发查询）
    ///
    /// DuckDB-RS 的 try_clone() 创建共享同一数据库的新连接，
    /// 适合多线程并发访问场景
    pub fn create_worker_connection(&self) -> Result<WorkerConnection, DuckDbError> {
        let conn = self.connection.lock().unwrap();
        let cloned = conn.try_clone()
            .map_err(|e| DuckDbError::ConnectionCloneError(e.to_string()))?;
        Ok(WorkerConnection::new(cloned))
    }
}

/// 工作连接包装器
pub struct WorkerConnection {
    connection: Option<Connection>,
}

impl WorkerConnection {
    pub fn new(connection: Connection) -> Self {
        Self { connection: Some(connection) }
    }

    /// 执行查询
    pub fn execute<F, T>(&self, f: F) -> Result<T, DuckDbError>
    where
        F: FnOnce(&Connection) -> Result<T, duckdb::Error>,
    {
        let conn = self.connection.as_ref()
            .ok_or_else(|| DuckDbError::ConnectionError("连接已关闭".to_string()))?;
        f(conn).map_err(DuckDbError::from)
    }

    /// 显式关闭连接
    pub fn close(mut self) -> Result<(), DuckDbError> {
        self.connection.take();
        Ok(())
    }
}
```

#### 3.5.2 事务机制适配

- **事务创建**: 使用 `connection.transaction()` 创建事务对象
- **提交行为**: 支持 `DropBehavior::Commit` 和 `DropBehavior::Rollback`
- **作用域管理**: 事务对象离开作用域时自动处理提交/回滚

```rust
/// 事务管理示例
impl DuckDbStorage {
    /// 执行需要事务的状态更新
    pub fn update_status_atomic(
        &self,
        project_id: &str,
        status: AgentStatus,
    ) -> Result<(), DuckDbError> {
        let conn = self.connection.lock().unwrap();
        let tx = conn.transaction()
            .map_err(|e| DuckDbError::TransactionError(e.to_string()))?;

        // 设置自动提交行为
        // tx.set_drop_behavior(DropBehavior::Rollback);

        tx.execute(
            "UPDATE projects SET agent_status_code = ?, agent_status_name = ?, last_activity = CURRENT_TIMESTAMP WHERE project_id = ?",
            params![status.code(), status.name(), project_id],
        ).map_err(|e| DuckDbError::QueryError(e.to_string()))?;

        // 显式提交
        tx.commit().map_err(|e| DuckDbError::TransactionError(e.to_string()))?;
        Ok(())
    }
}
```

#### 3.5.3 批量操作适配（Appender API）

- **Appender API**: 提供高效的批量插入接口，比逐行 INSERT 快 10-100 倍
- **参数化查询**: 使用 `params!` 宏进行安全的参数绑定
- **批量执行**: 支持 `execute_batch()` 执行多条SQL语句

```rust
use duckdb::params;

/// 批量插入示例
impl ContainerRepositoryImpl {
    /// 使用 Appender 批量插入容器
    pub fn bulk_insert(&self, containers: &[ContainerRecord]) -> Result<(), DuckDbError> {
        let conn = self.connection.lock().unwrap();

        // 创建 Appender
        let mut appender = conn.appender("containers")
            .map_err(|e| DuckDbError::AppenderError(e.to_string()))?;

        for container in containers {
            appender.append_row(params![
                container.container_id,
                container.container_name,
                container.container_ip,
                container.internal_port as i32,
                container.external_port as i32,
                container.service_type.to_string(),
                container.status,
                container.service_url,
                container.created_at.to_rfc3339(),
                container.last_activity.to_rfc3339(),
            ]).map_err(|e| DuckDbError::AppenderError(e.to_string()))?;
        }

        // Appender 在 drop 时自动 flush
        Ok(())
    }
}

/// params! 宏使用示例
impl ProjectRepositoryImpl {
    pub fn upsert(&self, project: &ProjectRecord) -> Result<(), DuckDbError> {
        let conn = self.connection.lock().unwrap();

        // 使用 params! 宏进行参数绑定，防止 SQL 注入
        conn.execute(
            r#"
            INSERT INTO projects (project_id, session_id, service_type, container_id, user_id,
                                  agent_status_code, agent_status_name, request_id,
                                  model_provider_json, created_at, last_activity)
            VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
            ON CONFLICT (project_id) DO UPDATE SET
                session_id = excluded.session_id,
                agent_status_code = excluded.agent_status_code,
                agent_status_name = excluded.agent_status_name,
                request_id = excluded.request_id,
                model_provider_json = excluded.model_provider_json,
                last_activity = excluded.last_activity
            "#,
            params![
                project.project_id,
                project.session_id,
                project.service_type.to_string(),
                project.container_id,
                project.user_id,
                project.agent_status.as_ref().map(|s| s.code()),
                project.agent_status.as_ref().map(|s| s.name()),
                project.request_id,
                project.model_provider.as_ref().map(|mp| serde_json::to_string(mp).ok()).flatten(),
                project.created_at.to_rfc3339(),
                project.last_activity.to_rfc3339(),
            ],
        ).map_err(|e| DuckDbError::QueryError(e.to_string()))?;

        Ok(())
    }
}
```

#### 3.5.4 查询结果映射

- **预编译语句**: 使用 `prepare()` 创建可重用查询
- **结果映射**: 支持将查询结果映射到 Rust 结构体
- **类型安全**: 通过泛型参数确保类型安全

```rust
/// 查询结果映射示例
impl ProjectRepositoryImpl {
    /// 按 session_id 查询项目
    pub fn find_by_session_id(&self, session_id: &str) -> Result<Option<ProjectRecord>, DuckDbError> {
        let conn = self.connection.lock().unwrap();

        let mut stmt = conn.prepare(
            "SELECT * FROM projects WHERE session_id = ?"
        ).map_err(|e| DuckDbError::QueryError(e.to_string()))?;

        let mut rows = stmt.query(params![session_id])
            .map_err(|e| DuckDbError::QueryError(e.to_string()))?;

        if let Some(row) = rows.next()
            .map_err(|e| DuckDbError::QueryError(e.to_string()))?
        {
            Ok(Some(Self::row_to_project_record(row)?))
        } else {
            Ok(None)
        }
    }

    /// 行数据映射到 ProjectRecord
    fn row_to_project_record(row: &duckdb::Row) -> Result<ProjectRecord, DuckDbError> {
        let status_code: Option<i32> = row.get("agent_status_code")
            .map_err(|e| DuckDbError::RowMappingError(e.to_string()))?;
        let status = status_code.and_then(AgentStatus::from_code);

        let model_provider_json: Option<String> = row.get("model_provider_json")
            .map_err(|e| DuckDbError::RowMappingError(e.to_string()))?;
        let model_provider = model_provider_json
            .and_then(|json| serde_json::from_str(&json).ok());

        let service_type_str: String = row.get("service_type")
            .map_err(|e| DuckDbError::RowMappingError(e.to_string()))?;
        let service_type = service_type_str.parse::<ServiceType>()
            .map_err(|e| DuckDbError::RowMappingError(e.to_string()))?;

        let created_at_str: String = row.get("created_at")
            .map_err(|e| DuckDbError::RowMappingError(e.to_string()))?;
        let created_at = DateTime::parse_from_rfc3339(&created_at_str)
            .map_err(|e| DuckDbError::RowMappingError(e.to_string()))?
            .with_timezone(&Utc);

        let last_activity_str: String = row.get("last_activity")
            .map_err(|e| DuckDbError::RowMappingError(e.to_string()))?;
        let last_activity = DateTime::parse_from_rfc3339(&last_activity_str)
            .map_err(|e| DuckDbError::RowMappingError(e.to_string()))?
            .with_timezone(&Utc);

        Ok(ProjectRecord {
            project_id: row.get("project_id").map_err(|e| DuckDbError::RowMappingError(e.to_string()))?,
            session_id: row.get("session_id").map_err(|e| DuckDbError::RowMappingError(e.to_string()))?,
            service_type,
            container_id: row.get("container_id").map_err(|e| DuckDbError::RowMappingError(e.to_string()))?,
            user_id: row.get("user_id").map_err(|e| DuckDbError::RowMappingError(e.to_string()))?,
            agent_status: status,
            request_id: row.get("request_id").map_err(|e| DuckDbError::RowMappingError(e.to_string()))?,
            model_provider,
            created_at,
            last_activity,
        })
    }

    /// 查询所有项目（带类型过滤）
    pub fn find_all(&self, service_type: Option<ServiceType>) -> Result<Vec<ProjectRecord>, DuckDbError> {
        let conn = self.connection.lock().unwrap();

        let sql = match service_type {
            Some(_) => "SELECT * FROM projects WHERE service_type = ? ORDER BY last_activity DESC",
            None => "SELECT * FROM projects ORDER BY last_activity DESC",
        };

        let mut stmt = conn.prepare(sql)
            .map_err(|e| DuckDbError::QueryError(e.to_string()))?;

        let rows = match service_type {
            Some(st) => stmt.query(params![st.to_string()]),
            None => stmt.query([]),
        }.map_err(|e| DuckDbError::QueryError(e.to_string()))?;

        let mut projects = Vec::new();
        for row_result in rows.mapped(|row| Self::row_to_project_record(row)) {
            projects.push(row_result.map_err(|e| DuckDbError::RowMappingError(e.to_string()))??);
        }

        Ok(projects)
    }
}
```

#### 3.5.5 异步封装

DuckDB-RS 是同步 API，需要通过 `spawn_blocking` 封装为异步：

```rust
/// 异步封装示例
impl AsyncProjectRepository {
    pub async fn find_by_session_id(&self, session_id: String) -> Result<Option<ProjectRecord>, DuckDbError> {
        let storage = self.storage.clone();
        tokio::task::spawn_blocking(move || {
            storage.projects().find_by_session_id(&session_id)
        })
        .await
        .map_err(|e| DuckDbError::ConcurrencyError(e.to_string()))?
    }
}
```

### 3.6 表关系图（优化后：2表设计）

```
┌─────────────────────────────────────────────────────────────────────────────┐
│                              containers                                      │
│  ┌─────────────────────┐                                                    │
│  │ container_id (PK)   │  容器唯一标识                                       │
│  │ container_name      │  容器名称 (rcoder-agent-xxx / computer-agent-runner-xxx) │
│  │ container_ip        │  容器 IP 地址                                       │
│  │ internal_port       │  内部端口                                           │
│  │ external_port       │  外部端口                                           │
│  │ service_type        │  服务类型 (RCoder / ComputerAgentRunner)            │
│  │ status              │  容器状态                                           │
│  │ service_url         │  服务 URL                                           │
│  │ created_at          │  创建时间                                           │
│  │ last_activity       │  最后活动时间                                       │
│  └─────────────────────┘                                                    │
└─────────────────────────────────────────────────────────────────────────────┘
                                         │
                                         │ 逻辑关联 (应用层保证，无外键)
                                         ▼
┌─────────────────────────────────────────────────────────────────────────────┐
│                     projects (已合并 session 信息)                          │
│  ┌─────────────────────┐                                                    │
│  │ project_id (PK)     │  项目唯一标识                                       │
│  │ session_id          │  当前活跃会话 ID（合并自 sessions 表）               │
│  │ service_type        │  服务类型 (RCoder / ComputerAgentRunner)            │
│  │ container_id        │  关联的容器 ID ─────────────────────────────────────┤
│  │ user_id             │  用户 ID (ComputerAgentRunner 模式专用)             │
│  │ agent_status_code   │  Agent 状态码 (0=Active, 1=Idle, 2=Terminating)     │
│  │ agent_status_name   │  Agent 状态名称                                     │
│  │ request_id          │  当前请求 ID                                        │
│  │ model_provider_json │  模型配置 (JSON 序列化)                              │
│  │ created_at          │  项目创建时间                                       │
│  │ last_activity       │  项目最后活动时间                                   │
│  │ session_created_at  │  会话创建时间（合并自 sessions 表）                  │
│  │ session_last_activity│ 会话最后活动时间（合并自 sessions 表）              │
│  └─────────────────────┘                                                    │
└─────────────────────────────────────────────────────────────────────────────┘

核心查询路径（已优化）：
━━━━━━━━━━━━━━━━━━━━━━━━━━
session_id ──► projects ──► container_id
              (单表查询)

原有查询路径（3表 JOIN，已废弃）：
───────────────────────────────────────
session_id ──► sessions ──► project_id ──► projects ──► container_id
```

### 3.7 边界场景说明

本节说明 DuckDB 迁移中不涉及的组件和需要特殊处理的场景。

#### 3.7.1 gRPC 连接池（不迁移）

`GrpcChannelPool` 保持使用 DashMap，**不迁移到 DuckDB**。原因如下：

1. **连接对象特殊性**: gRPC Channel 是有状态的连接对象，无法序列化到数据库
2. **生命周期管理**: 连接池需要管理连接的创建、复用和销毁，这是内存操作的优势场景
3. **性能要求**: 每次请求都需要获取连接，DashMap 的 O(1) 查找是最优选择

```rust
// AppState 中保持不变
pub struct AppState {
    // ... 其他字段

    /// gRPC 连接池 - 保持使用 DashMap
    pub grpc_pool: Arc<GrpcChannelPool>,
}

// GrpcChannelPool 结构保持不变
pub struct GrpcChannelPool {
    channels: DashMap<String, GrpcChannel>,
    // ...
}
```

**清理时机**: 当容器被销毁时，需要同时清理对应的 gRPC 连接：

```rust
// 在容器清理流程中
async fn cleanup_container(container_id: &str, state: &AppState) {
    // 1. 从 DuckDB 删除容器和关联项目
    storage.cleanup_project(project_id).await?;

    // 2. 清理 gRPC 连接池（DashMap 操作）
    if let Some(container) = get_container_info(container_id) {
        let channel_key = format!("{}:{}", container.container_ip, container.internal_port);
        state.grpc_pool.remove(&channel_key);
    }

    // 3. 实际销毁 Docker 容器
    docker_manager.remove_container(container_id).await?;
}
```

#### 3.7.2 VNC 后端映射清理（ComputerAgentRunner 模式）

ComputerAgentRunner 模式下，每个用户容器可能有关联的 VNC 后端映射，需要在容器清理时一并处理：

```rust
// VNC 后端清理流程
async fn cleanup_computer_agent_container(user_id: &str, state: &AppState) {
    // 1. 从 DuckDB 获取用户关联的容器信息
    let projects = storage.projects().find_by_user_id(user_id)?;

    // 2. 清理 VNC 后端映射（如果使用 Pingora 代理）
    if let Some(pingora_service) = &state.pingora_service {
        for project in &projects {
            if let Some(container) = storage.containers().find_by_id(&project.container_id)? {
                // 移除 VNC 代理映射
                pingora_service.remove_vnc_backend(&container.container_id);
            }
        }
    }

    // 3. 清理 gRPC 连接
    // ... (同上)

    // 4. 从 DuckDB 删除用户相关数据
    storage.cleanup_user(user_id)?;
}
```

**VNC 映射不存入 DuckDB**: VNC 后端映射由 Pingora 代理服务内部管理，属于运行时路由配置，不需要持久化到数据库。

#### 3.7.3 清理任务保护期

新创建的容器有 **5 分钟保护期**，在保护期内不会被清理任务回收：

```rust
/// 查询可清理的闲置项目（带保护期）
fn find_projects_for_cleanup(
    &self,
    idle_threshold: Duration,        // 闲置阈值（如 30 分钟）
    protection_duration: Duration,   // 保护期（如 5 分钟）
    service_type: Option<ServiceType>,
) -> Result<Vec<ProjectRecord>, StorageError>;
```

对应的 SQL 查询：

```sql
-- 查询可清理的闲置项目
SELECT p.*, c.*
FROM projects p
JOIN containers c ON p.container_id = c.container_id
WHERE
    -- 闲置时间超过阈值
    p.last_activity < NOW() - INTERVAL ? SECONDS
    -- 容器创建时间超过保护期
    AND c.created_at < NOW() - INTERVAL ? SECONDS
    -- 状态为空闲或空
    AND (p.agent_status_code IS NULL OR p.agent_status_code = 1)
    -- 可选：按服务类型过滤
    AND (? IS NULL OR p.service_type = ?);
```

### 3.8 表关系说明

#### 数据模型特点

| 特性 | 说明 |
|-----|------|
| **表数量** | 2 个表（containers + projects） |
| **外键约束** | 无，通过应用层保证数据一致性 |
| **业务区分** | 通过 `service_type` 字段区分 RCoder 和 ComputerAgentRunner |
| **会话管理** | session 信息已合并到 projects 表 |

#### 业务场景映射

| 场景 | 容器标识 | 项目映射 | 会话映射 |
|-----|---------|---------|---------|
| **RCoder** | `project_id` | `project_id → container_id` | `session_id → project_id` (同表查询) |
| **ComputerAgentRunner** | `user_id` | `user_id → container_id` | `session_id → project_id` (同表查询) |

#### 设计优势

1. **查询性能**: SSE 消息转发从 3 表 JOIN 优化为单表查询
2. **维护简单**: 减少 60% 的表数量，降低维护复杂度
3. **一致性**: session 与 project 状态在同一记录中，无需跨表同步
4. **扩展性**: 新增服务类型只需修改枚举，无需改表结构

---

## 4. 接口设计

### 4.1 核心 Trait 定义

#### 4.1.1 存储层 Trait

```rust
/// DuckDB 内存存储管理器
///
/// 负责管理 DuckDB 内存数据库的生命周期和连接
pub trait MemoryStorageManager: Send + Sync {
    /// 初始化数据库，创建表结构
    fn initialize(&self) -> Result<(), StorageError>;

    /// 获取数据库连接
    fn get_connection(&self) -> Result<Connection, StorageError>;

    /// 关闭数据库（容器停止时调用）
    fn shutdown(&self) -> Result<(), StorageError>;

    /// 获取统计信息（用于监控）
    fn get_stats(&self) -> StorageStats;
}

/// 容器信息仓储接口
pub trait ContainerRepository: Send + Sync {
    /// 插入或更新容器信息
    fn upsert(&self, container: &ContainerRecord) -> Result<(), StorageError>;

    /// 根据容器 ID 查询
    fn find_by_id(&self, container_id: &str) -> Result<Option<ContainerRecord>, StorageError>;

    /// 根据服务类型查询所有容器
    fn find_by_service_type(&self, service_type: ServiceType) -> Result<Vec<ContainerRecord>, StorageError>;

    /// 删除容器记录
    fn delete(&self, container_id: &str) -> Result<bool, StorageError>;

    /// 检查容器是否存在
    fn exists(&self, container_id: &str) -> Result<bool, StorageError>;

    /// 查询闲置容器（用于清理任务）
    fn find_idle_containers(
        &self,
        idle_threshold: Duration,
        service_type: Option<ServiceType>,
    ) -> Result<Vec<ContainerRecord>, StorageError>;

    /// 获取所有容器ID集合（用于孤立数据检测）
    fn get_all_container_ids(&self) -> Result<HashSet<String>, StorageError>;

    /// 查询孤立容器（无关联项目的容器）
    ///
    /// 用于检测数据一致性问题，找出没有任何项目关联的容器
    fn find_orphan_containers(&self) -> Result<Vec<ContainerRecord>, StorageError>;

    /// 批量删除容器
    fn bulk_delete(&self, container_ids: &[String]) -> Result<usize, StorageError>;
}

/// 统一项目仓储接口（已合并session功能）
/// 包含项目和session的所有操作，大部分操作为轻量级操作，无需事务保证
pub trait ProjectRepository: Send + Sync {
    /// 插入或更新项目（轻量操作）
    fn upsert(&self, project: &ProjectRecord) -> Result<(), StorageError>;

    /// 根据项目 ID 查询
    fn find_by_id(&self, project_id: &str) -> Result<Option<ProjectRecord>, StorageError>;

    /// 根据容器 ID 查询项目
    fn find_by_container_id(&self, container_id: &str) -> Result<Vec<ProjectRecord>, StorageError>;

    /// 根据用户 ID 查询项目（ComputerAgentRunner 模式）
    fn find_by_user_id(&self, user_id: &str) -> Result<Vec<ProjectRecord>, StorageError>;

    /// 根据会话 ID 查询项目（核心查询：SSE消息转发）
    fn find_by_session_id(&self, session_id: &str) -> Result<Option<ProjectRecord>, StorageError>;

    /// 根据服务类型查询项目
    fn find_by_service_type(&self, service_type: ServiceType) -> Result<Vec<ProjectRecord>, StorageError>;

    /// 轻量更新项目最后活动时间（高频操作，无事务）
    fn update_activity(&self, project_id: &str) -> Result<(), StorageError>;

    /// 轻量更新会话信息（包含session_id和时间戳）
    fn update_session(&self, project_id: &str, session_id: Option<&str>, session_created_at: Option<DateTime<Utc>>) -> Result<(), StorageError>;

    /// 轻量更新会话最后活动时间（高频操作，无事务）
    fn update_session_activity(&self, session_id: &str) -> Result<(), StorageError>;

    /// 原子性更新 Agent 状态（需要事务保证）
    fn update_status_atomic(&self, project_id: &str, status: Option<AgentStatus>) -> Result<(), StorageError>;

    /// 按状态查询项目（支持状态码查询，性能更优）
    fn find_by_status(&self, status: AgentStatus) -> Result<Vec<ProjectRecord>, StorageError>;

    /// 查询非终止状态的项目（用于清理任务）
    fn find_active_and_idle(&self) -> Result<Vec<ProjectRecord>, StorageError>;

    /// 删除项目记录
    fn delete(&self, project_id: &str) -> Result<bool, StorageError>;

    /// 查询闲置项目（利用状态码索引优化性能）
    fn find_idle_projects(
        &self,
        idle_threshold: Duration,
        service_type: Option<ServiceType>,
    ) -> Result<Vec<ProjectRecord>, StorageError>;

    /// 获取所有项目
    fn find_all(&self, service_type: Option<ServiceType>) -> Result<Vec<ProjectRecord>, StorageError>;

    /// 统计项目数量
    fn count(&self) -> Result<usize, StorageError>;

    /// 按状态统计项目数量
    fn count_by_status(&self) -> Result<std::collections::HashMap<AgentStatus, usize>, StorageError>;

    /// 获取会话对应的容器ID（优化后的核心方法）
    fn get_container_id_by_session(&self, session_id: &str) -> Result<Option<String>, StorageError>;

    /// 批量插入项目（使用 DuckDB Appender API）
    fn bulk_insert(&self, projects: &[ProjectRecord]) -> Result<(), StorageError>;

    /// 获取所有项目ID集合（用于孤立容器检测）
    fn get_all_project_ids(&self) -> Result<HashSet<String>, StorageError>;

    /// 批量删除项目（用于孤立数据清理）
    fn bulk_delete(&self, project_ids: &[String]) -> Result<usize, StorageError>;

    /// 查询可清理的闲置项目（带保护期）
    ///
    /// # 参数
    /// - `idle_threshold`: 闲置时间阈值（如 30 分钟）
    /// - `protection_duration`: 新建容器保护期（如 5 分钟）
    /// - `service_type`: 可选的服务类型过滤
    fn find_projects_for_cleanup(
        &self,
        idle_threshold: Duration,
        protection_duration: Duration,
        service_type: Option<ServiceType>,
    ) -> Result<Vec<ProjectRecord>, StorageError>;
}

## SessionRepository 已合并到 ProjectRepository

由于session信息已合并到projects表，SessionRepository接口已废弃。所有session相关的操作都通过ProjectRepository完成，这样可以：

1. **减少表关联查询**: 从3表JOIN变为2表JOIN
2. **简化数据一致性**: session与project状态在一个事务中维护
3. **优化核心查询**: `session_id -> container_id` 直接查询，无需关联
```

#### 4.1.2 事务管理接口

基于 DuckDB-RS 的事务机制，设计更贴近实际使用的接口：

```rust
/// 事务管理器
///
/// 封装 DuckDB-RS 的事务行为
pub trait TransactionManager: Send + Sync {
    /// 执行事务操作
    /// 支持自动提交和回滚行为
    fn execute_transaction<F, T>(&self, f: F) -> Result<T, StorageError>
    where
        F: FnOnce(&Transaction) -> Result<T, StorageError>;

    /// 执行状态变更事务（强制提交）
    /// 专门用于 agent_status 等需要强一致性的操作
    fn execute_status_update<F, T>(&self, f: F) -> Result<T, StorageError>
    where
        F: FnOnce(&Transaction) -> Result<T, StorageError>;
}

/// 事务对象
/// 封装 DuckDB 事务，支持显式提交/回滚
pub trait Transaction {
    /// 提交事务
    fn commit(self) -> Result<(), StorageError>;

    /// 回滚事务
    fn rollback(self) -> Result<(), StorageError>;

    /// 设置丢弃行为（自动提交/回滚）
    fn set_drop_behavior(&mut self, behavior: DropBehavior);
}

/// 丢弃行为枚举
#[derive(Debug, Clone, Copy)]
pub enum DropBehavior {
    /// 自动提交
    Commit,
    /// 自动回滚
    Rollback,
}

#### 4.1.3 连接管理接口

基于 DuckDB-RS 的连接特性，设计连接管理接口：

```rust
/// 连接管理器
///
/// 管理 DuckDB 连接的生命周期和并发访问
pub trait ConnectionManager: Send + Sync {
    /// 获取主连接
    fn get_connection(&self) -> Result<&Connection, StorageError>;

    /// 创建工作连接（用于并发操作）
    fn create_worker_connection(&self) -> Result<WorkerConnection, StorageError>;

    /// 健康检查
    fn health_check(&self) -> Result<(), StorageError>;
}

/// 工作连接
///
/// 支持独立关闭的连接副本
pub struct WorkerConnection {
    connection: Option<Arc<Mutex<Connection>>>,
}

impl WorkerConnection {
    /// 执行查询
    pub fn execute<F, T>(&self, f: F) -> Result<T, StorageError>
    where
        F: FnOnce(&Connection) -> Result<T, StorageError>;

    /// 关闭连接
    pub fn close(self) -> Result<(), StorageError>;
}
```

#### 4.1.3 轻量存储门面接口

```rust
/// 轻量存储接口
///
/// 提供对所有仓储的统一访问，大部分操作无需事务
pub trait UnifiedStorage: Send + Sync {
    /// 获取容器仓储
    fn containers(&self) -> &dyn ContainerRepository;

    /// 获取项目仓储
    fn projects(&self) -> &dyn ProjectRepository;

    /// session功能已合并到projects仓储中

    /// 获取事务管理器（仅状态变更使用）
    fn transaction(&self) -> &dyn TransactionManager;

    /// 轻量项目创建：顺序执行，无强事务保证
    fn create_project_with_container(
        &self,
        project: &ProjectRecord,
        container: &ContainerRecord,
    ) -> Result<(), StorageError>;

    /// 轻量会话创建
    fn create_session_for_project(
        &self,
        session: &SessionRecord,
    ) -> Result<(), StorageError>;

    /// 原子性状态变更：使用事务保证状态变更的原子性
    fn update_project_status_atomic(
        &self,
        project_id: &str,
        new_status: Option<AgentStatus>,
    ) -> Result<(), StorageError>;

    /// 轻量项目清理
    fn cleanup_project(&self, project_id: &str) -> Result<CleanupResult, StorageError>;

    /// ComputerAgentRunner 用户清理
    fn cleanup_user(&self, user_id: &str) -> Result<CleanupResult, StorageError>;

    /// 根据 session_id 获取项目信息
    fn get_project_info_by_session(
        &self,
        session_id: &str,
    ) -> Result<Option<ProjectInfo>, StorageError>;

    /// 根据项目 ID 获取项目信息
    fn get_project_info_by_id(&self, project_id: &str) -> Result<Option<ProjectInfo>, StorageError>;

    /// 获取指定服务类型的所有项目
    fn get_projects_by_service_type(&self, service_type: ServiceType) -> Result<Vec<ProjectInfo>, StorageError>;

    /// 获取用户的所有项目
    fn get_user_projects(&self, user_id: &str) -> Result<Vec<ProjectInfo>, StorageError>;
}
```

### 4.2 数据记录结构

基于模块化设计原则，将数据结构按使用范围进行分类：

#### 4.2.1 shared_types 模块（公共结构体）

```rust
// crates/shared_types/src/storage.rs

/// 容器记录 - 公共结构体，多模块使用
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContainerRecord {
    pub container_id: String,
    pub container_name: String,
    pub container_ip: String,
    pub internal_port: u16,
    pub external_port: u16,
    pub service_type: ServiceType,
    pub status: String,
    pub service_url: String,
    pub created_at: DateTime<Utc>,
    pub last_activity: DateTime<Utc>,
}

/// Agent 状态枚举
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum AgentStatus {
    /// 活跃状态 - 正在处理请求
    Active = 0,
    /// 空闲状态 - 等待新请求
    Idle = 1,
    /// 正在终止
    Terminating = 2,
}

impl AgentStatus {
    /// 获取状态码
    pub fn code(&self) -> i32 {
        *self as i32
    }

    /// 获取状态名称
    pub fn name(&self) -> &'static str {
        match self {
            AgentStatus::Active => "Active",
            AgentStatus::Idle => "Idle",
            AgentStatus::Terminating => "Terminating",
        }
    }

    /// 从状态码创建枚举
    pub fn from_code(code: i32) -> Option<Self> {
        match code {
            0 => Some(AgentStatus::Active),
            1 => Some(AgentStatus::Idle),
            2 => Some(AgentStatus::Terminating),
            _ => None,
        }
    }

    /// 从状态名称创建枚举
    pub fn from_name(name: &str) -> Option<Self> {
        match name {
            "Active" => Some(AgentStatus::Active),
            "Idle" => Some(AgentStatus::Idle),
            "Terminating" => Some(AgentStatus::Terminating),
            _ => None,
        }
    }
}

/// 统一项目记录 - 公共结构体，业务逻辑层和存储层都需要
/// 遵循第一范式设计，通过 service_type 字段区分不同业务场景
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectRecord {
    pub project_id: String,
    pub service_type: ServiceType,
    pub container_id: String,
    pub user_id: Option<String>,  // ComputerAgentRunner 模式时使用，RCoder 模式为 None
    pub session_id: Option<String>,
    pub agent_status: Option<AgentStatus>,  // 内存中使用枚举，数据库中存储为code和name
    pub request_id: Option<String>,
    pub model_provider: Option<ModelProviderConfig>,
    pub created_at: DateTime<Utc>,
    pub last_activity: DateTime<Utc>,
}

/// 统一会话记录 - 公共结构体
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionRecord {
    pub session_id: String,
    pub project_id: String,
    pub container_id: String,
    pub service_type: ServiceType,
    pub created_at: DateTime<Utc>,
    pub last_activity: DateTime<Utc>,
}

/// 项目信息（统一的返回类型）- 公共结构体
#[derive(Debug, Clone)]
pub struct ProjectInfo {
    pub project: ProjectRecord,
    pub container: ContainerRecord,
}
```

#### 4.2.2 duckdb_manager 模块（专用结构体）

```rust
// crates/duckdb_manager/src/models.rs

/// 清理结果 - 存储层专用
#[derive(Debug, Clone)]
pub struct CleanupResult {
    pub containers_deleted: usize,
    pub projects_deleted: usize,
    pub sessions_deleted: usize,
}

/// 存储统计信息 - 存储层专用
#[derive(Debug, Clone, Default)]
pub struct StorageStats {
    pub total_containers: usize,
    pub total_projects: usize,
    pub active_sessions: usize,      // 有活跃 session_id 的项目数
    pub active_containers: usize,    // 状态为 Active 的项目关联的容器数
    pub idle_containers: usize,      // 状态为 Idle 的项目关联的容器数
    /// 按服务类型统计的项目数
    pub projects_by_service_type: std::collections::HashMap<ServiceType, usize>,
}

/// 数据库专用配置 - 存储层专用
#[derive(Debug, Clone)]
pub struct DuckDbConfig {
    pub max_connections: u32,
    pub connection_timeout: Duration,
    pub enable_wal_mode: bool,
}
```

### 4.3 错误类型

#### 4.3.1 shared_types 模块（公共错误类型）

```rust
// crates/shared_types/src/storage.rs

/// 存储层公共错误类型 - 多模块使用
#[derive(Debug, thiserror::Error)]
pub enum StorageError {
    #[error("连接错误: {0}")]
    ConnectionError(String),

    #[error("未找到: 实体 {entity}, ID {id}")]
    NotFound { entity: String, id: String },

    #[error("约束违反: {0}")]
    ConstraintViolation(String),
}
```

#### 4.3.2 duckdb_manager 模块（专用错误类型）

```rust
// crates/duckdb_manager/src/error.rs

/// DuckDB 专用错误类型 - 存储层内部使用
#[derive(Debug, thiserror::Error)]
pub enum DuckDbError {
    #[error("数据库连接错误: {0}")]
    ConnectionError(String),

    #[error("SQL 查询错误: {0}")]
    QueryError(String),

    #[error("事务错误: {0}")]
    TransactionError(String),

    #[error("配置错误: {0}")]
    ConfigError(String),

    #[error("数据迁移错误: {0}")]
    MigrationError(String),

    #[error("序列化错误: {0}")]
    SerializationError(String),

    #[error("初始化错误: {0}")]
    InitializationError(String),

    #[error("重复记录: {entity} with id {id}")]
    DuplicateRecord { entity: String, id: String },

    // === 新增的错误类型（基于 DuckDB-RS 实际使用） ===

    #[error("Appender 错误: {0}")]
    AppenderError(String),

    #[error("并发访问错误: {0}")]
    ConcurrencyError(String),

    #[error("连接克隆失败: {0}")]
    ConnectionCloneError(String),

    #[error("类型转换错误: 列 {column} 期望 {expected}, 实际 {actual}")]
    TypeConversionError {
        column: String,
        expected: String,
        actual: String,
    },

    #[error("查询结果映射错误: {0}")]
    RowMappingError(String),

    #[error("参数绑定错误: 参数 {index} - {message}")]
    ParameterBindError { index: usize, message: String },
}

impl From<duckdb::Error> for DuckDbError {
    fn from(err: duckdb::Error) -> Self {
        match err {
            duckdb::Error::QueryReturnedNoRows => {
                DuckDbError::QueryError("查询结果为空".to_string())
            }
            duckdb::Error::InvalidColumnType(idx, name, ty) => {
                DuckDbError::TypeConversionError {
                    column: name,
                    expected: format!("column {}", idx),
                    actual: format!("{:?}", ty),
                }
            }
            _ => DuckDbError::QueryError(err.to_string()),
        }
    }
}

/// 将 DuckDbError 转换为公共 StorageError
impl From<DuckDbError> for StorageError {
    fn from(err: DuckDbError) -> Self {
        match err {
            DuckDbError::ConnectionError(msg) => StorageError::ConnectionError(msg),
            DuckDbError::DuplicateRecord { entity, id } => {
                StorageError::ConstraintViolation(format!("重复的 {} 记录: {}", entity, id))
            }
            _ => StorageError::ConnectionError(err.to_string()),
        }
    }
}
```

### 4.4 结构体依赖关系说明

#### 4.4.1 公共结构体 (shared_types)

以下结构体被多个模块使用，应放在 `crates/shared_types` 中：

| 结构体 | 使用模块 | 说明 |
|--------|---------|------|
| `ContainerRecord` | rcoder, duckdb_manager | 容器基本信息 |
| `ProjectRecord` | rcoder, duckdb_manager | 项目状态信息 |
| `SessionRecord` | rcoder, duckdb_manager | 会话映射信息 |
| `ProjectInfo` | rcoder, duckdb_manager | 完整的项目信息 |
| `StorageError` | rcoder, duckdb_manager | 存储层公共错误 |

#### 4.4.2 专用结构体 (duckdb_manager)

以下结构体仅在存储层内部使用：

| 结构体 | 说明 |
|--------|------|
| `CleanupResult` | 清理操作结果统计 |
| `StorageStats` | 存储层统计信息 |
| `DuckDbConfig` | 数据库连接配置 |
| `DuckDbError` | 数据库专用错误 |

#### 4.4.3 依赖关系图

```
crates/rcoder
├── 使用 shared_types::ContainerRecord
├── 使用 shared_types::ProjectRecord
├── 使用 shared_types::StorageError
└── 调用 duckdb_manager 接口

crates/duckdb_manager
├── 定义专用结构体 (CleanupResult, StorageStats等)
├── 使用 shared_types 公共结构体
└── 实现基于 shared_types 的接口

crates/shared_types
└── 定义多模块共享的结构体和错误类型
```

---

## 5. 实现结构设计

### 5.1 模块架构

```
crates/
├── duckdb_manager/           # 新增：DuckDB 管理模块
│   ├── Cargo.toml           # 模块配置
│   └── src/
│       ├── lib.rs           # 库入口，导出公共接口
│       ├── error.rs         # 数据库错误类型定义
│       ├── models.rs        # 数据模型定义
│       ├── connection.rs    # 数据库连接管理
│       ├── schema.rs        # 表结构定义和初始化
│       ├── repositories/    # 仓储层实现
│       │   ├── mod.rs       # 仓储模块入口
│       │   ├── container.rs # 容器仓储实现
│       │   └── project.rs   # 统一项目仓储实现（已包含session功能）
│       ├── storage.rs       # 统一存储门面实现
│       └── manager.rs       # 存储管理器，全局实例管理
└── rcoder/                  # 业务层使用模块
    └── src/
        ├── storage/         # 适配层（可选）
        │   ├── mod.rs       # 适配器模块
        │   ├── adapters.rs  # DashMap 兼容适配器
        │   └── bridge.rs    # 与 duckdb_manager 的桥接
        └── ...              # 其他业务代码
```

### 5.2 duckdb_manager 模块职责

`crates/duckdb_manager` 作为独立的数据库管理模块，提供：

1. **数据访问接口**: 统一的仓储接口和数据模型，遵循第一范式设计
2. **连接管理**: DuckDB 连接的创建、维护和管理
3. **事务支持**: 跨表操作的事务管理
4. **数据模型定义**: 通过 service_type 字段区分不同业务场景，避免表分裂
5. **错误处理**: 统一的数据库错误类型和处理
6. **初始化管理**: 数据库表结构初始化和管理，维护数据完整性约束

### 5.3 rcoder 模块职责

`crates/rcoder` 作为业务层模块：

1. **业务逻辑**: 实现具体的业务规则和流程
2. **适配层**: 通过适配器提供与 DashMap 兼容的接口
3. **桥接调用**: 调用 `duckdb_manager` 提供的接口
4. **状态管理**: 管理应用状态，协调各个组件

### 5.4 核心实现结构

#### 5.4.1 duckdb_manager 核心结构

```rust
// lib.rs - 主要导出接口
pub mod error;
pub mod models;
pub mod repositories;
pub mod storage;

pub use storage::{StorageManager, ContainerRepository, /* ... */};
pub use manager::{init_storage, get_storage};
```

```rust
/// 存储管理器接口（已优化）
pub trait StorageManager: Send + Sync {
    fn containers(&self) -> &dyn ContainerRepository;
    fn projects(&self) -> &dyn ProjectRepository; // 已包含session功能
    fn transaction(&self) -> &dyn TransactionManager;
}

/// 全局存储管理
pub struct DuckDbManager {
    storage: Arc<DuckDbStorageManager>,
}

impl DuckDbManager {
    pub fn new() -> Result<Self, StorageError>;
    pub fn get_storage(&self) -> Arc<dyn StorageManager>;
}

/// 轻量数据操作实现示例
impl StorageManager for DuckDbManager {
    /// 轻量项目创建（大部分操作无需事务）
    fn create_project(&self, project: &ProjectRecord) -> Result<(), StorageError> {
        // 1. 轻量检查容器是否存在
        if self.containers().find_by_id(&project.container_id)?.is_none() {
            return Err(StorageError::NotFound {
                entity: "Container".to_string(),
                id: project.container_id.clone(),
            });
        }

        // 2. 业务规则通过 CHECK 约束保证，无需额外检查

        // 3. 创建项目（轻量操作）
        self.projects().upsert(project)
    }

    /// 原子性状态更新（需要事务保证）
    fn update_project_status_atomic(&self, project_id: &str, status: Option<AgentStatus>) -> Result<(), StorageError> {
        // 使用事务保证状态变更的原子性
        self.transaction().execute_status_update(|| {
            self.projects().update_status_atomic(project_id, status)
        })
    }

    /// 内存模式初始化：每次启动都是新数据库
    fn initialize_memory_database(&self) -> Result<(), StorageError> {
        info!("初始化 DuckDB 内存数据库");
        // 创建内存数据库连接
        // 初始化表结构和索引
        // 返回成功表示初始化完成
        Ok(())
    }
}
```

#### 5.4.2 rcoder 适配层结构

```rust
/// rcoder 模块中的适配器
pub mod storage {
    use duckdb_manager::{StorageManager, /* ... */};

    /// DashMap 兼容适配器
    pub struct RCoderProjectAdapter {
        storage: Arc<dyn StorageManager>,
    }

    impl RCoderProjectAdapter {
        pub fn get(&self, project_id: &str) -> Option<Arc<ProjectAndContainerInfo>>;
        pub fn insert(&self, project_id: String, info: Arc<ProjectAndContainerInfo>);
        // ... 其他 DashMap 兼容方法
    }
}
```

### 5.5 依赖关系

```
crates/rcoder
    ↓ (使用接口)
crates/duckdb_manager
    ↓ (封装)
duckdb crate
```

- `duckdb_manager` 只依赖 `duckdb` crate 和基础类型
- `rcoder` 依赖 `duckdb_manager` 提供的接口
- 其他模块通过 `rcoder` 提供的适配器使用数据

---

## 6. 迁移方案

### 6.1 迁移步骤

#### 阶段一：并行运行（保守策略）

1. **添加 DuckDB 依赖**: 在 `Cargo.toml` 中添加 `duckdb` crate
2. **实现存储层**: 完成所有 Trait 的 DuckDB 实现
3. **双写模式**: 在业务层同时写入 DashMap 和 DuckDB
4. **读取验证**: 定期对比两个存储的数据一致性

#### 阶段二：切换读取

1. **切换读取源**: 将读取操作切换到 DuckDB
2. **保留 DashMap 写入**: 作为备份和回滚方案
3. **监控验证**: 确保功能正常运行

#### 阶段三：完全迁移

1. **移除 DashMap 写入**: 只保留 DuckDB 写入
2. **移除 DashMap 字段**: 从 AppState 中移除 DashMap
3. **清理代码**: 删除旧的 DashMap 相关代码

### 6.2 兼容性适配

为了平滑迁移，提供与原 DashMap 操作兼容的适配层：

#### 6.2.1 统一项目适配器

```rust
/// 统一项目适配器
///
/// 模拟原有的 project_and_agent_map DashMap 的行为
pub struct ProjectAdapter {
    storage: Arc<dyn UnifiedStorage>,
}

impl ProjectAdapter {
    /// 模拟 DashMap.get() 操作
    pub fn get(&self, key: &str) -> Option<Arc<ProjectAndContainerInfo>> {
        // 先尝试按项目ID查找
        if let Ok(Some(project_info)) = self.storage.get_project_info_by_id(key) {
            return Some(Arc::new(self.build_project_and_container_info(project_info)));
        }

        // 如果没找到，尝试按用户ID查找（ComputerAgentRunner模式）
        if let Ok(projects) = self.storage.get_user_projects(key) {
            if !projects.is_empty() {
                // 返回用户的第一个项目作为代表
                return Some(Arc::new(self.build_user_container_info(key, &projects[0])));
            }
        }

        None
    }

    /// 模拟 DashMap.insert() 操作
    pub fn insert(&self, key: String, info: Arc<ProjectAndContainerInfo>) -> Result<(), StorageError> {
        // 根据 ProjectAndContainerInfo 的内容判断是哪种类型的插入
        let project_record = self.extract_project_record(&info)?;
        let container_record = self.extract_container_record(&info)?;

        self.storage.create_project_with_container(&project_record, &container_record)
    }

    /// 模拟 DashMap.entry() 操作
    pub fn entry(&self, key: String) -> ProjectEntry;

    /// 模拟 DashMap.remove() 操作
    pub fn remove(&self, key: &str) -> Option<Arc<ProjectAndContainerInfo>>;

    /// 模拟 DashMap.iter() 操作
    pub fn iter(&self) -> impl Iterator<Item = (String, Arc<ProjectAndContainerInfo>)>;

    /// 模拟 DashMap.contains_key() 操作
    pub fn contains_key(&self, key: &str) -> bool;
}
```

#### 6.2.3 会话适配器

```rust
/// 会话映射适配器
///
/// 模拟 sessions 和 session_to_container_id DashMap 的行为
pub struct SessionAdapter {
    storage: Arc<dyn UnifiedStorage>,
}

impl SessionAdapter {
    /// 模拟 sessions DashMap
    pub fn get_project(&self, session_id: &str) -> Option<Arc<ProjectAndContainerInfo>>;

    /// 模拟 session_to_container_id DashMap
    pub fn get_container_id(&self, session_id: &str) -> Option<String>;
}
```

#### 6.2.4 AppState 适配器

```rust
/// 适配后的 AppState
pub struct AdaptedAppState {
    /// 统一项目映射适配器（替代原来的 project_and_agent_map）
    pub projects: ProjectAdapter,

    /// 会话映射适配器
    pub sessions: SessionAdapter,

    /// 会话到容器ID映射适配器
    pub session_to_container_id: SessionAdapter,

    /// 其他原有字段保持不变
    pub config: AppConfig,
    pub pingora_service: Option<Arc<PingoraProxyService>>,
    pub grpc_pool: Arc<GrpcChannelPool>,
}
```

---

## 7. 查询优化

### 7.1 常用查询 SQL

#### 7.1.1 统计查询（无时间范围限制）

```sql
-- 统计各服务类型的容器数量
SELECT service_type, COUNT(*) as container_count
FROM containers
GROUP BY service_type;

-- 统计 RCoder 模式的项目数量
SELECT COUNT(*) as rcoder_project_count
FROM projects
WHERE service_type = 'RCoder';

-- 统计 ComputerAgentRunner 模式的项目数量
SELECT COUNT(*) as computer_project_count
FROM projects
WHERE service_type = 'ComputerAgentRunner';

-- 按服务类型统计项目数量
SELECT service_type, COUNT(*) as project_count
FROM projects
GROUP BY service_type;
```

#### 7.1.2 闲置清理查询（带时间范围和保护期）

```sql
-- 通用闲置项目查询（带保护期）
SELECT p.*, c.*
FROM projects p
JOIN containers c ON p.container_id = c.container_id
WHERE p.last_activity < NOW() - INTERVAL ? SECONDS           -- 闲置阈值
  AND c.created_at < NOW() - INTERVAL ? SECONDS              -- 保护期
  AND (p.agent_status_code IS NULL OR p.agent_status_code = 1)  -- Idle 或空状态
  AND (? IS NULL OR p.service_type = ?);                     -- 可选服务类型过滤

-- RCoder 模式：闲置项目查询
SELECT p.*, c.*
FROM projects p
JOIN containers c ON p.container_id = c.container_id
WHERE p.service_type = 'RCoder'
  AND p.last_activity < NOW() - INTERVAL ? SECONDS
  AND c.created_at < NOW() - INTERVAL ? SECONDS              -- 5分钟保护期
  AND (p.agent_status_code IS NULL OR p.agent_status_code = 1);

-- ComputerAgentRunner 模式：闲置项目查询
SELECT p.*, c.*
FROM projects p
JOIN containers c ON p.container_id = c.container_id
WHERE p.service_type = 'ComputerAgentRunner'
  AND p.last_activity < NOW() - INTERVAL ? SECONDS
  AND c.created_at < NOW() - INTERVAL ? SECONDS              -- 5分钟保护期
  AND (p.agent_status_code IS NULL OR p.agent_status_code = 1);
```

#### 7.1.3 业务查询

```sql
-- 根据 session_id 查找项目和容器信息（单表优化后）
SELECT p.*, c.*
FROM projects p
JOIN containers c ON p.container_id = c.container_id
WHERE p.session_id = ?;

-- 获取 RCoder 模式的所有项目
SELECT p.*, c.*
FROM projects p
JOIN containers c ON p.container_id = c.container_id
WHERE p.service_type = 'RCoder'
ORDER BY p.last_activity DESC;

-- 获取 ComputerAgentRunner 模式的所有项目
SELECT p.*, c.*
FROM projects p
JOIN containers c ON p.container_id = c.container_id
WHERE p.service_type = 'ComputerAgentRunner'
ORDER BY p.last_activity DESC;

-- 获取指定用户的所有项目（ComputerAgentRunner 模式）
SELECT p.*, c.*
FROM projects p
JOIN containers c ON p.container_id = c.container_id
WHERE p.user_id = ?
ORDER BY p.last_activity DESC;
```

#### 7.1.4 维护查询

```sql
-- 查找孤立容器（没有关联项目的容器）
SELECT c.*
FROM containers c
LEFT JOIN projects p ON c.container_id = p.container_id
WHERE p.container_id IS NULL;

-- 查找没有容器的项目（数据一致性检查）
SELECT p.*
FROM projects p
LEFT JOIN containers c ON p.container_id = c.container_id
WHERE c.container_id IS NULL;

-- 查找 session 已过期但项目仍存在的记录
SELECT p.*
FROM projects p
WHERE p.session_id IS NOT NULL
  AND p.session_last_activity < NOW() - INTERVAL ? SECONDS;
```

### 7.2 索引策略

基于数据量较小的约束，索引策略以查询效率和维护简单性为优先：

#### 7.2.1 主要索引

| 表名 | 索引列 | 用途 | 优先级 |
|-----|-------|------|-------|
| **containers** | `container_id` (PK) | 容器查找 | 高 |
| | `service_type` | 按类型统计 | 中 |
| | `last_activity` | 闲置检测 | 中 |
| **projects** | `project_id` (PK) | 项目查找 | 高 |
| | `container_id` | 容器关联 | 高 |
| | `session_id` | 会话查找（SSE消息转发核心路径） | 高 |
| | `user_id` | 用户关联（ComputerAgentRunner模式） | 中 |
| | `last_activity` | 闲置检测 | 中 |
| | `service_type` | 类型筛选 | 中 |
| | `agent_status_code` | 状态查询 | 中 |
| | `(agent_status_code, last_activity)` | 清理任务复合查询 | 中 |
| | `(service_type, last_activity)` | 按类型闲置检测 | 低 |

#### 7.2.2 索引设计原则

1. **主键索引**: 所有表的主键自动建立索引
2. **关联字段索引**: 重要关联字段建立索引，保证关联查询性能
3. **查询索引**: 针对频繁查询字段建立索引
4. **简化策略**: 数据量小的情况下，避免过度索引，减少维护成本

---

## 8. 性能考量

### 8.1 轻量性能特点

基于数据量小和轻量事务要求的约束，设计重点关注以下性能特点：

#### 8.1.1 优势场景
- **高频轻量更新**: `last_activity` 时间戳更新，无事务开销
- **简单查询**: 基于主键或少量索引的快速查询
- **统计查询**: 全表统计，无需复杂的时间范围过滤
- **内存模式**: DuckDB 内存数据库的快速访问特性

#### 8.1.2 关键性能点
- **事务仅用于状态变更**: 只有 `agent_status` 更新需要事务，其他操作轻量
- **状态码索引优化**: 数字状态码查询比字符串更高效
- **复合索引**: 状态+时间的复合索引优化清理任务
- **索引优化**: 基于数据量小，精简索引设计
- **并发友好**: 大部分操作无需锁，减少竞争

### 8.2 性能优化策略

#### 8.2.1 查询优化

1. **预编译语句**: 对常用查询预编译，提高执行效率
2. **批量操作**: 使用事务批量处理多条记录
3. **异步包装**: 虽然数据量小，仍使用 `spawn_blocking` 避免阻塞 Tokio 线程
4. **结果缓存**: 对于统计查询，可以考虑内存缓存结果

#### 8.2.2 索引优化

基于数据量小但查询频繁的特点：

- **核心索引**: 主键、关联字段建立索引
- **查询索引**: `session_id`、`last_activity` 等查询频繁字段
- **简化策略**: 避免过度索引，平衡查询和维护成本

#### 8.2.3 内存和存储优化

1. **内存模式**: 充分利用 DuckDB 内存模式特性
2. **数据清理**: 及时清理过期数据，控制内存占用
3. **简单设计**: 无需复杂的分页、压缩等大数据优化策略

#### 8.2.4 业务层优化

1. **异步更新**: 活动时间更新异步处理，避免影响主流程
2. **延迟写入**: 非关键更新可以延迟批量提交
3. **读写分离**: 考虑读写操作的并发优化

### 8.2 优化策略

1. **连接池**: 使用连接池避免频繁创建连接
2. **预编译语句**: 缓存常用 SQL 的预编译语句
3. **批量操作**: 使用事务批量处理多条记录
4. **异步包装**: 使用 `spawn_blocking` 包装同步 DuckDB 调用

---

## 9. 测试计划

### 9.1 单元测试

#### 9.1.1 仓储层测试
- **ContainerRepository**: 容器 CRUD 操作测试
- **ProjectRepository**: 统一项目管理（支持RCoder和ComputerAgentRunner两种模式）
- **ProjectRepository**: 项目和会话统一 CRUD 操作测试

#### 9.1.2 业务逻辑测试
- 事务操作的原子性测试（跨表操作）
- 错误处理和边界条件测试
- 数据一致性约束测试

### 9.2 集成测试

#### 9.2.1 业务场景测试
- **RCoder 模式**: 完整的项目创建到清理流程
- **ComputerAgentRunner 模式**: 用户容器创建到项目管理的完整流程
- **混合模式**: 两种模式并存时的相互影响测试

#### 9.2.2 适配层测试
- DashMap 兼容适配器的功能测试
- 双写模式下的数据一致性测试
- 迁移过程中的数据同步测试

#### 9.2.3 清理任务测试
- 闲置检测逻辑的准确性测试
- 清理操作的完整性测试（级联删除）
- 保护期机制的正确性测试

### 9.3 性能测试

#### 9.3.1 基础性能测试
- **单操作性能**: CRUD 操作的响应时间测试
- **并发性能**: 正常业务负载下的并发读写测试
- **统计查询性能**: 全表统计查询的响应时间测试

#### 9.3.2 业务场景性能测试
- **频繁更新场景**: 活动时间更新的性能测试
- **清理任务场景**: 闲置检测和清理操作的性能测试
- **混合负载场景**: 正常业务操作的综合性能测试

#### 9.3.3 与 DashMap 对比测试
- **功能对比**: 确保所有操作功能正确
- **性能对比**: 正常数据量下的性能对比测试
- **内存占用对比**: 相同数据量下的内存使用情况对比

---

## 10. 风险评估

### 10.1 轻量设计风险评估

| 风险 | 影响程度 | 缓解措施 |
|-----|---------|---------|
| 状态变更原子性丢失 | 高 | 明确标识需要事务的操作 |
| 模块接口不匹配 | 中 | 定义清晰的轻量接口 |
| 跨模块依赖管理 | 低 | 简单的依赖关系 |
| 轻量操作一致性 | 中 | 业务层保证基本一致性 |
| DuckDB 兼容性问题 | 低 | 版本锁定 + 充分测试 |
| 内存模式连接问题 | 低 | 每次启动都是新数据库，连接问题影响小 |

### 10.2 回滚方案

1. 保留 DashMap 代码至少一个版本周期
2. 通过配置开关控制使用哪个存储后端
3. 准备快速回滚脚本和文档

---

## 11. 依赖管理

### 11.1 模块依赖配置

#### 11.1.1 crates/duckdb_manager/Cargo.toml

```toml
[package]
name = "duckdb_manager"
version = "0.1.0"
edition = "2021"

[dependencies]
# 基础依赖
serde = { version = "1.0", features = ["derive"] }
chrono = { version = "0.4", features = ["serde"] }
thiserror = "1.0"
tokio = { version = "1.0", features = ["sync"] }

# DuckDB 相关
duckdb = { version = "1.1", features = ["bundled"] }

# 共享类型 - 必须依赖，包含公共结构体和错误类型
shared_types = { path = "../shared_types" }

[lib]
name = "duckdb_manager"
path = "src/lib.rs"
```

#### 11.1.2 crates/rcoder/Cargo.toml (更新)

```toml
[dependencies]
# 共享类型 - 包含公共结构体 (ContainerRecord, ProjectRecord, StorageError等)
shared_types = { path = "../shared_types" }

# Docker 管理器
docker_manager = { path = "../docker_manager" }

# 其他原有依赖...

# 新增：DuckDB 存储管理器 - 提供存储接口实现
duckdb_manager = { path = "../duckdb_manager" }
```

### 11.2 Feature 说明

- `bundled`: 内置 DuckDB 库，无需系统安装
- **模块分离**: `duckdb_manager` 作为独立 crate，只包含数据库相关逻辑
- **接口依赖**: `rcoder` 通过接口调用 `duckdb_manager`，不直接依赖 `duckdb`

---

## 12. 时间规划

### 12.1 开发阶段规划

| 阶段 | 任务 | 预计时间 | 主要交付物 |
|-----|------|---------|----------|
| 1 | 需求分析与设计评审 | 2 天 | 完整的数据库设计文档 |
| 2 | duckdb_manager 模块搭建 | 4 天 | 新模块创建、基础结构、依赖配置 |
| 3 | 数据模型与接口定义 | 3 天 | 数据结构、Trait 定义、错误类型 |
| 4 | 数据库核心实现 | 5 天 | 仓储实现、连接管理、事务支持 |
| 5 | rcoder 适配层实现 | 4 天 | DashMap 兼容适配器、桥接调用 |
| 6 | 单元测试 | 4 天 | 两个模块的完整测试覆盖 |
| 7 | 集成测试 | 4 天 | 跨模块集成测试、接口验证 |
| 8 | 性能测试与优化 | 3 天 | 性能基准测试、查询优化 |
| 9 | 灰度迁移 | 3 天 | 双写模式、数据一致性验证 |
| 10 | 完全迁移 | 2 天 | 移除 DashMap、清理遗留代码 |

**总计**: 约 30 个工作日 (增加模块分离设计，时间略有增加)

### 12.2 里程碑

#### 里程碑 1: 核心框架完成 (第 5 天)
- ✅ 数据库表结构设计完成
- ✅ 所有 Trait 定义完成
- ✅ 核心数据结构实现
- ✅ 单元测试框架搭建

#### 里程碑 2: 仓储层完成 (第 10 天)
- ✅ 所有 Repository 实现完成
- ✅ 事务支持实现
- ✅ 基础 CRUD 操作测试通过
- ✅ 性能基准测试完成

#### 里程碑 3: 适配层完成 (第 13 天)
- ✅ DashMap 兼容适配器实现
- ✅ 业务逻辑适配完成
- ✅ 集成测试通过

#### 里程碑 4: 测试验证完成 (第 20 天)
- ✅ 完整测试覆盖
- ✅ 性能优化完成
- ✅ 与现有系统集成验证

#### 里程碑 5: 生产就绪 (第 30 天)
- ✅ 灰度迁移完成
- ✅ 完全迁移完成
- ✅ 监控和运维文档完成

### 12.3 风险控制

| 风险点 | 应对措施 | 时间缓冲 |
|-------|---------|---------|
| DuckDB 性能不达标 | 提前进行性能测试，有备选方案 | +2 天 |
| 业务逻辑适配复杂 | 分阶段实现，先 RCoder 后 ComputerAgentRunner | +3 天 |
| 测试覆盖不足 | 制定详细测试计划，自动化测试 | +2 天 |
| 迁移过程数据不一致 | 实现数据校验工具，双写模式验证 | +3 天 |

---

## 13. 模块架构设计

### 13.1 crates/duckdb_manager 模块设计

#### 13.1.1 模块职责

`crates/duckdb_manager` 是专门的数据库管理模块，职责如下：

1. **数据访问抽象**: 提供统一的仓储接口，封装所有数据库操作
2. **连接生命周期管理**: 负责 DuckDB 连接的创建、维护和关闭
3. **事务管理**: 支持跨表操作的原子性事务
4. **数据模型定义**: 定义所有数据结构和类型
5. **错误处理**: 统一的数据库错误类型和处理逻辑
6. **模式管理**: 数据库表结构初始化和管理

#### 13.1.2 接口设计原则

```rust
// lib.rs - 主要的公共接口
pub mod error;
pub mod models;
pub mod repositories;
pub mod storage;

// 核心接口导出
pub use storage::{StorageManager, ContainerRepository, /* ... */};
pub use manager::{init_storage, get_storage};
```

#### 13.1.3 仓储接口示例

```rust
/// 容器仓储接口
pub trait ContainerRepository: Send + Sync {
    fn upsert(&self, container: &ContainerRecord) -> Result<(), StorageError>;
    fn find_by_id(&self, container_id: &str) -> Result<Option<ContainerRecord>, StorageError>;
    fn find_by_service_type(&self, service_type: ServiceType) -> Result<Vec<ContainerRecord>, StorageError>;
    fn count(&self) -> Result<usize, StorageError>;
    fn bulk_insert(&self, containers: &[ContainerRecord]) -> Result<(), StorageError>;
    // ... 其他方法
}
```

#### 13.1.4 存储管理器接口

```rust
/// 统一存储管理器接口
pub trait StorageManager: Send + Sync {
    fn containers(&self) -> &dyn ContainerRepository;
    fn projects(&self) -> &dyn ProjectRepository; // 已合并所有功能
    fn transaction(&self) -> &dyn TransactionManager;
}
```

### 13.2 crates/rcoder 适配层设计

#### 13.2.1 适配器模式

`crates/rcoder` 通过适配器模式提供与现有代码兼容的接口：

```rust
/// DashMap 兼容适配器
pub struct RCoderProjectAdapter {
    storage: Arc<dyn StorageManager>,  // 调用 duckdb_manager 接口
}

impl RCoderProjectAdapter {
    /// 模拟 DashMap.get() 操作
    pub fn get(&self, project_id: &str) -> Option<Arc<ProjectAndContainerInfo>> {
        // 内部调用 duckdb_manager 的接口
        // 转换数据格式后返回
    }
}
```

#### 13.2.2 桥接设计

```rust
/// 桥接模块：连接业务逻辑和数据访问
pub mod bridge {
    use duckdb_manager::StorageManager;

    pub struct StorageBridge {
        manager: Arc<dyn StorageManager>,
    }

    impl StorageBridge {
        // 业务特定的复合操作（通过 service_type 区分业务场景）
        pub fn create_project_with_session(&self, /* ... */) -> Result<(), BridgeError>;
    }
}
```

### 13.3 依赖关系与隔离

#### 13.3.1 依赖层次

```
crates/rcoder (业务层)
    ↓ (接口调用)
crates/duckdb_manager (数据访问层)
    ↓ (直接依赖)
duckdb crate + shared_types
```

#### 13.3.2 隔离原则

1. **接口隔离**: `rcoder` 只依赖 `duckdb_manager` 的接口，不依赖具体实现
2. **数据隔离**: 数据库操作完全封装在 `duckdb_manager` 中
3. **错误隔离**: 数据库错误转换为业务错误，避免泄露实现细节
4. **测试隔离**: 各模块可独立测试，通过接口 mock 解耦

#### 13.3.3 优势

- **职责分离**: 数据库操作与业务逻辑分离
- **可维护性**: 修改数据库实现不影响业务代码
- **可测试性**: 接口化设计便于单元测试和集成测试
- **可扩展性**: 可轻松更换底层数据库实现

---

## 14. 设计合理性分析

### 14.1 DuckDB方案的合理性评估

基于实际使用场景分析，我们的DuckDB设计是合理的选择：

#### 14.1.1 场景匹配度

| 需求特点 | DashMap | 简单内存结构 | SQLite内存 | **DuckDB内存** | 评估 |
|---------|---------|-------------|------------|----------------|------|
| **并发访问** | ✅ 原生支持 | ❌ 需要锁 | ⚠️ 有限支持 | ✅ 连接克隆 | DuckDB优 |
| **复杂查询** | ❌ 仅精确查找 | ❌ 仅精确查找 | ✅ SQL查询 | ✅ SQL查询 | DuckDB/SQLite优 |
| **事务保证** | ⚠️ 应用层保证 | ❌ 无 | ✅ ACID | ✅ ACID | SQLite/DuckDB优 |
| **统计功能** | ❌ 手动实现 | ❌ 手动实现 | ✅ SQL聚合 | ✅ SQL聚合 | DuckDB/SQLite优 |
| **内存模式** | ✅ | ✅ | ✅ | ✅ | 都支持 |
| **开发复杂度** | ⚠️ 高（业务逻辑复杂） | ✅ 低 | ⚠️ 中 | ⚠️ 中 | 内存结构最简单 |
| **维护成本** | ✅ 低 | ✅ 低 | ⚠️ 中 | ⚠️ 中 | DashMap最简单 |

#### 14.1.2 DuckDB的核心优势

1. **SQL查询能力**: 支持复杂的统计和筛选，而不需要应用层实现
2. **事务支持**: 只在需要时使用事务，保证重要操作的原子性
3. **类型安全**: 通过Repository模式提供类型安全的数据访问
4. **并发友好**: 通过连接克隆支持并发访问
5. **生态成熟**: DuckDB-RS有完善的Rust绑定和活跃的社区

#### 14.1.3 替代方案对比

##### 方案A: 继续使用DashMap
```rust
// 优点：简单直接，性能极好
pub struct AppState {
    pub sessions: DashMap<String, Arc<ProjectAndContainerInfo>>,
    pub project_and_agent_map: DashMap<String, Arc<ProjectAndContainerInfo>>,
    pub session_to_container_id: DashMap<String, String>,
}
```
**优点**:
- 性能最佳（内存操作，无序列化开销）
- 实现最简单，无额外依赖
- 并发访问原生支持

**缺点**:
- 统计查询需要手动遍历所有数据
- 复杂查询能力有限
- 类型安全依赖人工保证

##### 方案B: 简单内存HashMap + 锁
```rust
// 优点：控制力强，资源占用少
pub struct InMemoryStorage {
    projects: RwLock<HashMap<String, ProjectRecord>>,
    containers: RwLock<HashMap<String, ContainerRecord>>,
    sessions: RwLock<HashMap<String, SessionRecord>>,
}
```
**优点**:
- 完全控制，无外部依赖
- 内存占用最小
- 性能良好

**缺点**:
- 需要手动实现并发控制
- 统计查询效率低（需要遍历）
- 错误处理复杂

##### 方案C: SQLite内存模式
```sql
-- SQLite内存数据库
.open :memory:
```
**优点**:
- 轻量级，SQL查询能力
- 文件格式兼容性好
- 生态完善

**缺点**:
- DuckDB在分析查询上有优势
- Rust绑定可能不如DuckDB-RS成熟

#### 14.1.4 为什么DuckDB是最佳选择

1. **功能平衡**: 在性能、功能、复杂度间取得最佳平衡
2. **渐进迁移**: 可以逐步从DashMap迁移，不需要大爆炸式重构
3. **扩展性**: 为未来可能的持久化需求留有空间
4. **学习成本**: 基于SQL的查询语言，团队容易理解

### 14.2 设计优化建议

#### 14.2.1 当前设计的合理改进

1. **简化Repository接口**: 移除一些不常用的方法
2. **优化索引策略**: 基于实际查询模式调整索引
3. **连接池优化**: 评估是否需要连接池

#### 14.2.2 可能的进一步优化

1. **异步接口**: 如果业务层需要异步，可以考虑r2d2连接池
2. **查询缓存**: 对频繁查询添加内存缓存层
3. **批量更新**: 进一步优化批量操作的性能

## 15. 最终结论

### 15.1 设计决策总结

经过全面分析，我们的DuckDB内存模式设计并结合表合并优化是**优秀且高性能**的选择：

#### ✅ 正确的决策

1. **内存模式**: 完全符合容器重启清空数据的需求
2. **轻量事务**: 只有`agent_status`变更需要事务，其他操作无开销
3. **表合并优化**: projects和sessions表合并，大幅提升查询性能
4. **状态字段优化**: 状态码+状态名的设计提升查询性能和扩展性
5. **Repository模式**: 提供了良好的抽象和类型安全
6. **模块分离**: duckdb_manager与业务逻辑解耦

#### 🎯 核心优势

1. **高性能查询**: SSE消息转发等核心查询减少JOIN操作，性能显著提升
2. **渐进式迁移**: 可以平滑从DashMap过渡
3. **功能完备**: 支持所有现有需求，包括统计查询
4. **架构简化**: 从5表优化到2表（containers+projects），维护成本大幅降低
5. **状态字段优化**: 状态码+状态名的设计提升查询性能和扩展性
5. **维护友好**: SQL查询直观，调试容易

#### 📊 与替代方案的对比

| 方案 | 复杂度 | 功能 | 性能 | 维护成本 | 推荐指数 |
|-----|--------|------|------|----------|----------|
| **DuckDB内存** | 中 | 高 | 高 | 中 | ⭐⭐⭐⭐⭐ |
| DashMap继续使用 | 低 | 低 | 高 | 低 | ⭐⭐⭐ |
| 简单内存结构 | 低 | 低 | 高 | 低 | ⭐⭐ |
| SQLite内存 | 中 | 高 | 中 | 中 | ⭐⭐⭐⭐ |

### 15.2 实施建议

1. **从小开始**: 先实现核心CRUD操作，验证性能
2. **逐步迁移**: 可以先迁移部分功能，逐步替换DashMap
3. **监控指标**: 添加基本的性能监控（查询耗时、连接数等）
4. **回滚计划**: 保留DashMap代码一段时间，确保可以快速回滚

### 15.3 总结

DuckDB内存模式的设计是经过深思熟虑的解决方案，它在功能、性能、复杂度之间取得了良好的平衡。相比简单的内存结构，它提供了更强的查询能力和更好的可维护性；相比复杂的持久化方案，它保持了轻量和高效。

这个设计既解决了当前的技术债务，又为未来的扩展留下了空间，是一个务实且前瞻性的选择。

---

**文档版本**: 1.4 (补充边界场景、孤立容器检测、错误类型扩展、DuckDB-RS实现细节)
**最后更新**: 2025-12-19
**状态**: ✅ 设计完善，等待实施

### 14.1 重新设计的核心改进

基于用户反馈，这次重新设计主要解决了以下问题：

#### 原始设计问题：
1. **违反第一范式**: 将业务相似的容器管理数据分散到多个表中
2. **维护复杂性**: `rcoder_projects` 和 `computer_users` 表差异很小，却需要分别维护
3. **扩展困难**: 新增业务场景需要新增表结构

#### 重新设计解决方案：
1. **第一范式优化**: 合并为统一的 `projects` 表，通过 `service_type` 字段区分业务场景
2. **表合并优化**: 将 `sessions` 表合并到 `projects` 表，从 3 个表减少到 2 个表
3. **查询性能提升**: SSE消息转发等核心查询减少一次JOIN操作
4. **状态字段优化**: 状态码+状态名的设计提升查询性能和扩展性
5. **无外键约束**: 通过应用层保证数据一致性，避免数据库级联操作的复杂性
6. **维护成本降低**: 统一的仓储接口和业务逻辑，减少重复代码
7. **扩展性提升**: 新增服务类型和状态只需修改枚举，无需改表结构

#### 具体改进对比：

| 方面 | 原始设计 | 重新设计 | 改进效果 |
|-----|---------|---------|---------|
| **表数量** | 5个表 (containers, rcoder_projects, computer_users, computer_projects, sessions) | 2个表 (containers, projects) | 减少60%的表数量 |
| **状态字段** | 单字符串字段 | 状态码+状态名双字段 | 查询性能提升，扩展性增强 |
| **业务区分** | 表级区分 | 字段级区分 | 更符合第一范式 |
| **外键约束** | 有外键约束 | 无外键约束，应用层保证 | 减少数据库复杂性，增加应用层控制 |
| **维护复杂度** | 高 (需维护多个相似表+外键+关联) | 低 (统一表结构+应用层逻辑) | 大幅降低维护成本 |
| **扩展性** | 差 (需新增表) | 好 (只需修改枚举) | 显著提升扩展性 |

### 14.2 轻量设计原则与约束影响

本设计以 **数据量较小**、**轻量事务** 和 **模块架构分离** 的重要约束为基础，遵循以下轻量设计原则：

1. **轻量事务理念**: 只有 `agent_status` 变更需要强事务保证，其他操作均为轻量级
2. **模块职责分离**: 数据库操作与业务逻辑完全分离，`duckdb_manager` 专注数据访问
3. **简洁性优先**: 基于数据量小的约束，避免过度设计和复杂优化
4. **功能完整性**: 确保满足所有业务需求，包括统计查询的全数据覆盖
5. **内存模式优势**: 使用内存模式，每次重启都是全新状态，无需持久化容错处理

#### 约束对设计的影响：

- **事务策略**: 大部分操作无需事务，`agent_status` 变更使用轻量事务
- **第一范式设计**: 统一项目表，通过 `service_type` 字段区分业务场景，避免表分裂
- **无外键约束**: 通过 CHECK 约束和应用层保证数据一致性，增加灵活性但简化维护
- **模块架构**: 创建独立的 `duckdb_manager` 模块，封装所有数据库操作
- **接口设计**: 定义轻量的仓储接口，区分事务和非事务操作
- **索引策略**: 简化为主，核心查询字段建立索引即可
- **查询设计**: 包含全表统计查询，无需时间范围过滤
- **数据一致性**: 通过 CHECK 约束保证业务规则，应用层保证基本关联完整性
- **DuckDB-RS 适配**: 接口设计完全兼容 DuckDB-RS 的连接管理和事务机制
- **结构体分类**: 公共结构体集中管理，专用结构体模块隔离，提高代码复用性
- **维护成本**: 大幅降低系统复杂度和维护成本，提高代码可维护性

---

## 15. 实施指南

### 15.1 crates/duckdb_manager 模块创建

#### 15.1.1 创建模块目录结构

```bash
cd crates/
mkdir -p duckdb_manager/src/repositories
```

#### 15.1.2 创建 Cargo.toml

```toml
# crates/duckdb_manager/Cargo.toml
[package]
name = "duckdb_manager"
version = "0.1.0"
edition = "2021"

[dependencies]
serde = { version = "1.0", features = ["derive"] }
chrono = { version = "0.4", features = ["serde"] }
thiserror = "1.0"
tokio = { version = "1.0", features = ["sync"] }
duckdb = { version = "1.1", features = ["bundled"] }
shared_types = { path = "../shared_types" }
```

#### 15.1.3 主要文件结构

```
crates/duckdb_manager/src/
├── lib.rs              # 库入口，导出接口
├── error.rs            # 错误定义
├── models.rs           # 数据模型
├── connection.rs       # 连接管理
├── schema.rs           # 表结构定义
├── repositories/
│   ├── mod.rs          # 仓储模块入口
│   ├── container.rs    # 容器仓储
│   └── project.rs      # 统一项目仓储（已包含session功能）
├── storage.rs          # 统一存储接口
└── manager.rs          # 全局管理器
```

#### 15.1.4 lib.rs 示例

```rust
// crates/duckdb_manager/src/lib.rs
pub mod error;
pub mod models;
pub mod repositories;
pub mod storage;
mod connection;
mod schema;
mod manager;

// 主要接口导出
pub use storage::{
    StorageManager, ContainerRepository, ProjectRepository,
    TransactionManager
};
pub use manager::{init_storage, get_storage};
pub use error::StorageError;
```

### 15.2 rcoder 模块适配

#### 15.2.1 更新依赖

在 `crates/rcoder/Cargo.toml` 中添加：

```toml
[dependencies]
# ... 现有依赖
duckdb_manager = { path = "../duckdb_manager" }
```

#### 15.2.2 创建适配层

```rust
// crates/rcoder/src/storage/mod.rs
pub mod adapters;
pub mod bridge;

// 导出适配器（统一的项目适配器，通过 service_type 区分业务场景）
pub use adapters::{ProjectAdapter, ContainerAdapter};
pub use bridge::StorageBridge;
```

#### 15.2.3 AppState 更新

```rust
// crates/rcoder/src/router.rs
use crate::storage::{ProjectAdapter, ContainerAdapter};

pub struct AdaptedAppState {
    // 原有的字段保持不变
    pub config: AppConfig,
    pub pingora_service: Option<Arc<PingoraProxyService>>,
    pub grpc_pool: Arc<GrpcChannelPool>,  // gRPC 连接池保持使用 DashMap

    // 替换原来的 DashMap（统一适配器）
    pub projects: ProjectAdapter,         // 统一项目适配器（包含 session 信息）
    pub containers: ContainerAdapter,     // 容器适配器
}
```

---

## 16. 附录

### 13.1 参考资料

- [DuckDB 官方文档](https://duckdb.org/docs/)
- [DuckDB Rust Crate](https://crates.io/crates/duckdb)
- [DuckDB GitHub](https://github.com/duckdb/duckdb)

### 15.1 系统约束

| 约束类型 | 具体要求 | 对设计的影响 |
|---------|---------|-------------|
| **数据规模** | 数据量较小，无需考虑大数据量优化 | 简化索引策略，优先考虑简洁性 |
| **统计查询** | 统计所有数据，不受时间范围限制 | 查询设计中包含全表统计场景 |
| **查询模式** | 主要为精确查询和全表统计 | 避免过度复杂的范围查询优化 |
| **模块架构** | 使用 crates/duckdb_manager 封装数据库操作 | 接口化设计，业务逻辑与数据访问分离 |
| **外键约束** | 禁止使用数据库外键 | 通过应用层保证数据一致性 |
| **外键约束** | 禁止使用数据库外键 | 通过应用层保证数据一致性 |

### 15.2 术语表

| 术语 | 说明 |
|-----|------|
| DashMap | Rust 的并发 HashMap 实现 |
| DuckDB | 嵌入式分析型数据库 |
| 内存模式 | 数据库数据仅存储在内存中 |
| Repository | 仓储模式，封装数据访问逻辑 |
| RAII | Resource Acquisition Is Initialization |

