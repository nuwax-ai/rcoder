# RCoder 反向代理功能使用指南

## 概述

RCoder 现在支持基于端口参数的反向代理功能，可以在 Docker 容器环境中通过统一端口访问多个前端应用服务。

## 功能特性

### 🔗 端口路由
- **查询参数方式**: `?port=3000` - 通过 URL 参数指定目标端口
- **路径方式**: `/proxy/3000/path` - 通过 URL 路径指定目标端口
- **默认端口**: 未指定端口时使用默认后端端口

### 🚀 启动方式
```bash
# 启用反向代理（默认监听 8080 端口）
./rcoder --enable-proxy

# 指定代理端口
./rcoder --enable-proxy --proxy-port 9000

# 指定默认后端端口
./rcoder --enable-proxy --default-backend-port 3000
```

## 使用场景

### 🐳 Docker 容器环境
在 Docker 容器中，可以暴露一个代理端口（如 8080），通过该端口访问容器内的多个服务：

```yaml
# docker-compose.yml
version: '3.8'
services:
  rcoder-app:
    build: .
    ports:
      - "8080:8080"  # 代理端口
      - "3000:3000"  # 主服务端口
    command: ./rcoder --enable-proxy
```

### 📱 多前端应用
访问运行在不同端口的前端应用：

```bash
# 访问端口 3000 的服务
curl http://localhost:8080?port=3000

# 访问端口 8081 的服务
curl http://localhost:8080/proxy/8081/api/data

# 默认访问端口 3000（未指定端口）
curl http://localhost:8080/
```

### 🌐 Web 应用集成
在前端应用中使用代理：

```javascript
// 通过查询参数访问不同服务
const apiPort = new URLSearchParams(window.location.search).get('port') || '3000';
const apiUrl = `http://localhost:8080?port=${apiPort}`;

// 或通过路径方式
const serviceUrl = `http://localhost:8080/proxy/${port}/api/endpoint`;
```

## 配置选项

### 命令行参数
| 参数 | 默认值 | 说明 |
|------|--------|------|
| `--enable-proxy` | false | 启用反向代理 |
| `--proxy-port` | 8080 | 代理服务监听端口 |
| `--default-backend-port` | 3000 | 默认后端服务端口 |

### 配置文件
```yaml
# config.yml
default_agent: codex
projects_dir: ./project_workspace
port: 3000
proxy_config:
  listen_port: 8080
  default_backend_port: 3000
  backend_host: "127.0.0.1"
  port_param: "port"
```

## 请求示例

### 1. 查询参数方式
```bash
# 基本用法
curl "http://localhost:8080?port=3000"

# 带路径的请求
curl "http://localhost:8080/api/users?port=3001"

# 带多个参数
curl "http://localhost:8080/api/data?type=json&port=3002"
```

### 2. 路径方式
```bash
# 基本用法
curl "http://localhost:8080/proxy/3000/"

# 带路径的请求
curl "http://localhost:8080/proxy/3001/api/users"

# 复杂路径
curl "http://localhost:8080/proxy/3002/api/v1/data?format=json"
```

### 3. POST 请求
```bash
# JSON 数据提交
curl -X POST "http://localhost:8080/api/submit?port=3000" \
  -H "Content-Type: application/json" \
  -d '{"name": "test", "value": 123}'

# 表单数据提交
curl -X POST "http://localhost:8080/proxy/3001/form" \
  -F "file=@test.txt" \
  -F "name=test"
```

## 响应头信息

代理服务会自动添加以下响应头：
- `X-Forwarded-Proto`: 请求协议
- `X-Port-Proxy`: 代理标识
- `X-Forwarded-For`: 客户端 IP（如果可用）
- `X-Real-IP`: 真实客户端 IP（如果可用）

## 错误处理

### 常见错误响应
```json
// 代理失败（502 Bad Gateway）
{
  "error": "代理失败: Connection refused"
}

// 后端服务不存在
{
  "error": "未找到端口 9999 对应的后端服务"
}
```

### 调试信息
查看日志获取详细错误信息：
```bash
# 查看代理日志
tail -f logs/rcoder.$(date +%Y-%m-%d) | grep "代理"
```

## 性能考虑

### 优化建议
1. **连接复用**: 代理会复用 HTTP 连接以提高性能
2. **日志级别**: 生产环境可调整日志级别减少开销
3. **健康检查**: 确保后端服务正常运行

### 监控指标
- 请求转发数量
- 响应时间
- 错误率
- 并发连接数

## 故障排除

### 常见问题

#### 1. 代理服务无法启动
```bash
# 检查端口是否被占用
netstat -tulpn | grep 8080

# 使用不同端口
./rcoder --enable-proxy --proxy-port 9090
```

#### 2. 后端服务无法访问
```bash
# 检查后端服务是否运行
curl http://localhost:3000

# 检查防火墙设置
sudo ufw status
```

#### 3. 请求参数丢失
确保正确传递端口参数：
```bash
# 错误示例（port 参数位置错误）
curl "http://localhost:8080/port=3000"

# 正确示例
curl "http://localhost:8080?port=3000"
```

## 开发和测试

### 本地开发环境
```bash
# 启动多个后端服务进行测试
python -m http.server 3000 &  # 端口 3000
python -m http.server 3001 &  # 端口 3001

# 启动代理服务
./rcoder --enable-proxy

# 测试代理功能
curl "http://localhost:8080?port=3000"
curl "http://localhost:8080/proxy/3001"
```

### 集成测试
```bash
# 使用 curl 测试脚本
#!/bin/bash
for port in 3000 3001 3002; do
  echo "Testing port $port..."
  curl -w "Status: %{http_code}\n" "http://localhost:8080?port=$port"
  echo "---"
done
```

## 安全注意事项

1. **端口限制**: 建议限制可代理的端口范围
2. **访问控制**: 在生产环境中添加认证机制
3. **日志审计**: 记录所有代理请求以便审计
4. **防火墙**: 配置适当的防火墙规则

---

通过这个反向代理功能，你可以轻松地在 Docker 容器中管理多个前端服务，提供统一的访问入口。