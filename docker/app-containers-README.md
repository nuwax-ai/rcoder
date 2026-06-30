# 应用容器说明

本目录包含三个应用容器，用于支持不同类型的后端应用服务。

## 目录结构

```
docker/
├── java-app/           # Java + PostgreSQL + Node + Nginx + etcd
├── python-app/         # Python + PostgreSQL + Node + Nginx + etcd
└── typescript-app/     # TypeScript + PostgreSQL + Node + Nginx + etcd
```

## 应用容器对比

| 特性 | java-app | python-app | typescript-app |
|------|----------|------------|----------------|
| 主要语言 | Java 17 | Python 3.11 | TypeScript/Node.js 18 |
| 应用框架 | Spring Boot/Tomcat | Flask/FastAPI | Express/Nest.js/Next.js |
| 数据库 | PostgreSQL 15 | PostgreSQL 15 | PostgreSQL 15 |
| 前端服务 | Node.js 18 | Node.js 18 | Node.js 18 |
| 反向代理 | Nginx | Nginx | Nginx |
| 服务注册 | etcd 3.5.17 | etcd 3.5.17 | etcd 3.5.17 |
| Java 诊断 | Arthas 3.7.2 | - | - |
| 进程管理 | Supervisor | Supervisor | Supervisor |
| 应用端口 | 8080 | 5000 | 3000 |
| Node.js 端口 | 3000 | 3000 | 3001 |
| PostgreSQL 端口 | 5432 | 5432 | 5432 |
| etcd 端口 | 2379/2380 | 2379/2380 | 2379/2380 |

## 快速开始

### 1. 构建镜像

```bash
# 进入对应目录
cd docker/java-app    # 或 python-app, typescript-app

# 构建镜像
docker build -t rcoder-java-app .
```

### 2. 启动容器

```bash
# 使用 docker-compose
docker-compose up -d

# 或使用 docker run
docker run -d \
  --name rcoder-java-app \
  -p 8080:80 \
  -p 5432:5432 \
  -p 2379:2379 \
  -v ./src:/app/src \
  rcoder-java-app
```

### 3. 访问应用

- **HTTP**: http://localhost:8080
- **健康检查**: http://localhost:8080/health
- **etcd 健康检查**: http://localhost:8080/etcd/health
- **API**: http://localhost:8080/api/
- **PostgreSQL**: localhost:5432
- **etcd**: localhost:2379

## 目录挂载

| 容器路径 | 说明 |
|---------|------|
| `/app/src` | 应用代码 |
| `/app/config` | 配置文件 |
| `/app/data/postgresql` | PostgreSQL 数据 |
| `/app/data/etcd` | etcd 数据 |
| `/var/log/supervisor` | 服务日志 |

## 环境变量

所有容器共享以下环境变量：

```bash
# PostgreSQL
POSTGRES_USER=appuser
POSTGRES_PASSWORD=apppassword
POSTGRES_DB=appdb

# etcd
ETCD_NAME=etcd-node-1
ETCD_ENDPOINTS=http://localhost:2379

# 应用
APP_ENV=production
LOG_LEVEL=info
```

## 服务管理

容器使用 Supervisor 管理多个服务：

```bash
# 查看服务状态
docker exec rcoder-java-app supervisorctl status

# 重启单个服务
docker exec rcoder-java-app supervisorctl restart java-app
docker exec rcoder-java-app supervisorctl restart etcd

# 查看日志
docker exec rcoder-java-app tail -f /var/log/supervisor/java-app.log
docker exec rcoder-java-app tail -f /var/log/supervisor/etcd.log
```

## 架构说明

每个容器包含以下服务：

```
┌─────────────────────────────────────────┐
│               Nginx (80)                │
│         (反向代理 + 静态文件)            │
└───────────────┬─────────────────────────┘
                │
    ┌───────────┴───────────┐
    │                       │
┌───▼───┐             ┌────▼────┐
│ 主应用 │             │ Node.js │
│ (8080) │             │ (3000)  │
└───┬───┘             └────┬────┘
    │                       │
    └───────────┬───────────┘
                │
    ┌───────────┼───────────┐
    │           │           │
┌───▼───┐ ┌────▼────┐ ┌────▼────┐
│Postgres│ │  etcd   │ │Supervisor│
│ (5432) │ │ (2379)  │ │ (进程)   │
└────────┘ └─────────┘ └─────────┘
```

## etcd 使用说明

### 1. 健康检查

```bash
# 通过 Nginx 代理检查
curl http://localhost:8080/etcd/health

# 直接检查
curl http://localhost:2379/health
```

### 2. 查看集群状态

```bash
# 通过 Nginx 代理
curl http://localhost:8080/etcd/status

# 使用 etcdctl
docker exec rcoder-java-app etcdctl endpoint status --endpoints=http://localhost:2379
```

### 3. 服务注册示例

```bash
# 注册服务
docker exec rcoder-java-app etcdctl put /services/java-app '{"host":"localhost","port":8080}'

# 查询服务
docker exec rcoder-java-app etcdctl get /services/java-app

# 监听变更
docker exec rcoder-java-app etcdctl watch /services/
```

### 4. 配置中心示例

```bash
# 存储配置
docker exec rcoder-java-app etcdctl put /config/java-app/db-host localhost

# 获取配置
docker exec rcoder-java-app etcdctl get /config/java-app/db-host
```

## 健康检查

所有容器都包含健康检查机制：

```bash
# 检查健康状态
curl http://localhost:8080/health

# 返回示例
{
  "status": "healthy",
  "services": {
    "java": "running",
    "node": "running",
    "postgres": "running",
    "etcd": "running"
  }
}
```

## 故障排查

### 1. 查看容器日志

```bash
docker logs rcoder-java-app
```

### 2. 进入容器调试

```bash
docker exec -it rcoder-java-app /bin/bash
```

### 3. 检查服务状态

```bash
docker exec rcoder-java-app supervisorctl status
```

### 4. 查看 PostgreSQL 日志

```bash
docker exec rcoder-java-app cat /var/log/supervisor/postgresql.log
```

### 5. 查看 etcd 日志

```bash
docker exec rcoder-java-app cat /var/log/supervisor/etcd.log
```

### 6. 检查 etcd 数据

```bash
docker exec rcoder-java-app etcdctl get --prefix / --keys-only
```

## 内置排查工具

所有容器都预装了常用排查工具：

### 网络工具
| 命令 | 用途 | 示例 |
|------|------|------|
| `curl` | HTTP 请求测试 | `curl http://localhost/health` |
| `wget` | 下载工具 | `wget http://example.com/file` |
| `nslookup` / `dig` | DNS 查询 | `nslookup google.com` |
| `ping` | 网络连通性测试 | `ping 8.8.8.8` |
| `telnet` | 端口测试 | `telnet localhost 5432` |
| `traceroute` | 路由追踪 | `traceroute google.com` |
| `tcpdump` | 网络抓包 | `tcpdump -i eth0 port 80` |
| `netstat` | 网络连接查看 | `netstat -tlnp` |

### 系统工具
| 命令 | 用途 | 示例 |
|------|------|------|
| `ps` | 进程查看 | `ps aux` |
| `htop` | 进程监控 | `htop` |
| `top` | 系统资源监控 | `top` |
| `lsof` | 文件描述符查看 | `lsof -p <pid>` |
| `strace` | 系统调用追踪 | `strace -p <pid>` |
| `vmstat` | 虚拟内存统计 | `vmstat 1` |
| `iostat` | IO 统计 | `iostat -x 1` |
| `iotop` | IO 监控 | `iotop` |

### 文件工具
| 命令 | 用途 | 示例 |
|------|------|------|
| `vim` / `nano` | 文本编辑 | `vim /app/config/app.conf` |
| `tree` | 目录树查看 | `tree /app/src` |
| `jq` | JSON 处理 | `echo '{"a":1}' \| jq .` |
| `less` | 分页查看 | `less /var/log/app.log` |
| `tar` / `zip` | 压缩解压 | `tar czf backup.tar.gz /app/data` |
| `rsync` | 文件同步 | `rsync -av /src/ /dest/` |

## Arthas (Java 诊断工具)

Java 容器 (`java-app`) 预装了 [Arthas](https://arthas.aliyun.com/doc/)，这是阿里开源的 Java 诊断工具。

### 启动 Arthas

```bash
# 进入容器
docker exec -it rcoder-java-app /bin/bash

# 启动 Arthas (自动识别 Java 进程)
arthas

# 或指定 PID
java -jar /usr/local/bin/arthas-boot.jar <pid>
```

### 常用命令

```bash
# 查看仪表盘
dashboard

# 查看线程信息
thread
thread -n 3          # 查看最忙的 3 个线程
thread <id>          # 查看指定线程堆栈

# 查看 JVM 信息
jvm
memory               # 查看内存使用
sysprop              # 查看系统属性

# 方法调用追踪
trace com.example.MyClass myMethod
watch com.example.MyClass myMethod '{params, returnObj, throwExp}'
stack com.example.MyClass myMethod

# 反编译
jad com.example.MyClass

# 热更新代码
mc /tmp/MyClass.java -d /tmp
sc -d com.example.MyClass
redefine /tmp/com/example/MyClass.class

# 查看方法执行耗时
monitor com.example.MyClass myMethod -c 5

# 获取 Spring Context
ognl '@org.springframework.context.ApplicationContext@getBean("myBean")'
```

### Arthas Web Console

Arthas 还提供 Web Console，可以通过浏览器访问：

```bash
# 启动 Arthas 时会显示 Web Console 地址
# 默认端口是 8563
# 可以通过 Nginx 代理访问
```

## 注意事项

1. **数据持久化**: PostgreSQL 和 etcd 数据默认存储在 `./data/` 目录
2. **端口冲突**: 确保宿主机端口未被占用
3. **权限问题**: 某些操作可能需要 root 权限
4. **资源限制**: 生产环境建议设置内存和 CPU 限制
5. **etcd 单节点**: 当前配置为单节点模式，适合开发测试，生产环境建议部署集群
6. **Arthas 安全**: Arthas 仅用于开发/测试环境，生产环境请谨慎使用
