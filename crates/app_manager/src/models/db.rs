//! 数据库管理模型（app-runtime 镜像单容器自带 PG；rcoder exec psql 操作）

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

/// 重置 PG 密码请求（rcoder exec 容器内 psql ALTER USER，本地 trust 认证绕过当前密码）
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct ResetDbPasswordRequest {
    /// 新密码（PG 密码，非空）
    pub password: String,
}

/// 新建 PG 库请求
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct CreateDatabaseRequest {
    /// 新数据库名（PG 标识符规则：字母/数字/下划线，首字符字母/下划线）
    pub database: String,
    /// owner 用户名（可选；默认不指定 = 连接用户 $POSTGRES_USER）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub owner: Option<String>,
}

/// database 目录 SQL 自动执行报告（发布 activate 后执行；失败仅收集不阻断）
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct DatabaseSqlReport {
    /// 执行成功的 SQL 文件（相对 code 根路径，按执行序）
    pub executed: Vec<String>,
    /// 执行失败的 SQL 文件与错误摘要（`{rel}: {stderr}`）
    pub failed: Vec<String>,
}
