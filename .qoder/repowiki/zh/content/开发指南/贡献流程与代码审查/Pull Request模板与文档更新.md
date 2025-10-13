# Pull Request模板与文档更新

<cite>
**本文档中引用的文件**  
- [README.md](file://README.md)
- [config.yml](file://config.yml)
- [crates/nuwax_parser/README.md](file://crates/nuwax_parser/README.md)
</cite>

## 目录
1. [Pull Request 核心要求](#pull-request-核心要求)
2. [PR 模板建议](#pr-模板建议)
3. [文档更新规范](#文档更新规范)
4. [nuwax_parser 组件文档更新指南](#nuwax_parser-组件文档更新指南)
5. [文档质量要求](#文档质量要求)

## Pull Request 核心要求

所有 Pull Request 必须遵循标准化流程，确保代码变更的可追溯性、可维护性和一致性。PR 不仅是代码提交，更是技术沟通的重要载体，必须包含完整的变更说明、实现细节和影响评估。

**Section sources**
- [README.md](file://README.md#L588-L608)

## PR 模板建议

为确保 PR 信息完整，建议使用以下结构化模板：

```markdown
### 变更目的
简要说明本次变更的背景、动机和解决的问题。例如：修复了文件同步时的哈希验证漏洞，或新增了对特定文件类型的支持。

### 实现方案
详细描述技术实现路径，包括：
- 核心算法或逻辑变更
- 关键函数或模块的修改
- 数据结构或接口的调整
- 与现有系统的集成方式

### 测试方法
说明验证变更正确性的测试策略：
- 单元测试覆盖情况
- 集成测试场景
- 手动测试步骤
- 边界条件和异常处理验证

### 影响范围评估
评估变更对系统其他部分的影响：
- **API变更**：如有接口修改，必须说明兼容性处理方案（如版本控制、默认值、迁移路径）
- **性能影响**：评估对系统性能的潜在影响
- **安全影响**：分析可能引入的安全风险
- **依赖变更**：列出新增或更新的依赖项

### 相关截图或日志片段
提供关键执行结果的可视化证据：
- 功能演示截图
- 调试日志输出
- 性能对比数据
- 错误处理示例
```

**Section sources**
- [README.md](file://README.md#L588-L608)

## 文档更新规范

任何功能增改都必须同步更新相关文档，确保文档与代码实现保持一致。文档更新范围包括但不限于：

- **crate 级别 README**：更新功能描述、使用示例和 API 参考
- **公共 API 注释**：完善函数、结构体和方法的文档注释
- **配置说明**：在 `config.yml` 示例中添加新配置项说明
- **快速开始指南**：更新安装和使用步骤

```yaml
# rcoder 配置文件
# 该文件在首次启动时自动生成

# 默认使用的 AI 代理类型 (Codex/Claude/Proxy)
default_agent: Codex

# 项目工作目录
projects_dir: ./project_workspace

# 主服务端口
port: 3000

# Pingora 反向代理配置
proxy_config:
  # 代理服务监听端口 (用于接收外部请求)
  listen_port: 8080
  # 默认后端服务端口 (当请求未指定端口时使用)
  default_backend_port: 3000
  # 后端服务主机地址
  backend_host: "127.0.0.1"
  # URL 中端口参数的名称 (用于从路径中提取端口号)
  port_param: "port"
  # 健康检查配置
  health_check:
    enabled: true
    interval_seconds: 5
    timeout_seconds: 1
    healthy_threshold: 2
    unhealthy_threshold: 3
```

**Diagram sources**
- [config.yml](file://config.yml#L1-L30)

**Section sources**
- [config.yml](file://config.yml#L1-L30)
- [README.md](file://README.md#L396-L435)

## nuwax_parser 组件文档更新指南

`nuwax_parser` 作为独立的文件解析和同步工具包，其文档更新需遵循特定流程：

1. **更新独立文档**：修改 `crates/nuwax_parser/README.md` 中的相关内容
   - 功能特性描述
   - 使用示例代码
   - API 参考信息
   - 配置说明

2. **同步主文档**：确保主 `README.md` 中的集成说明与最新实现一致
   - 架构概览中的组件描述
   - 快速开始指南中的使用示例
   - API 文档中的集成说明

```rust
// Axum 集成示例
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
```

**Diagram sources**
- [crates/nuwax_parser/README.md](file://crates/nuwax_parser/README.md#L296-L345)

**Section sources**
- [crates/nuwax_parser/README.md](file://crates/nuwax_parser/README.md#L1-L528)
- [README.md](file://README.md#L1-L652)

## 文档质量要求

文档更新必须满足以下质量标准：

- **语言清晰**：使用准确、简洁的技术语言，避免歧义
- **示例可运行**：提供的代码示例必须经过验证，能够直接运行
- **与代码一致**：文档内容必须与最新代码实现完全匹配
- **结构完整**：包含必要的标题、段落和列表，便于阅读
- **格式规范**：遵循统一的 Markdown 格式标准

特别强调，文档不仅是使用指南，更是系统设计的重要组成部分，必须与代码变更同步演进，确保团队成员能够准确理解和使用系统功能。

**Section sources**
- [README.md](file://README.md#L11-L41)
- [crates/nuwax_parser/README.md](file://crates/nuwax_parser/README.md#L1-L528)