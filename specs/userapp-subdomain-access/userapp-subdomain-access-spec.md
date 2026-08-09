# UserApp 内置 PG/pgweb · 需求文档(Spec)

> 状态:**v3 草案**(2026-07-21),待 review
> 相关:`crates/rcoder/src/app_manager/`、`docker/app-runtime-base/`、`build-agent-docker/build_config/app-runtime-base/`
>
> **v3 简化**:pingora **完全不改**,复用现有 path 路由 `/proxy/apps/{app_id}/{port}/`;pgweb 固定端口;子域名/host/DNS **全部不管**(外部系统负责)。

---

## 1. 背景与目标

### 1.1 用户场景
UserApp 部署用户应用(前端 + 后端),用户需要:
1. 应用通过 HTTP 对外访问
2. 数据库(PostgreSQL)支持
3. 开发者通过 pgweb(Web UI)查看/操作数据库

### 1.2 本期方案:单容器 + 复用 path 路由
把 **PG + pgweb 打包进应用运行时镜像**,与应用**同容器**:
- 应用、PG、pgweb 在同一个 UserApp 容器
- **pgweb 固定端口 8081**,通过 pingora 现有 path 路由访问
- 应用走自己的 HTTP 端口,同样 path 路由
- **PG 容器内 localhost,不对外**
- 子域名 → pingora path 的映射由**外部系统**负责,rcoder 不管

### 1.3 关键简化(相对 v2)
- ❌ 不做 pingora host 路由(用现有 path 路由)
- ❌ 不做子域名生成 / host 配置 / DNS(rcoder 不管)
- ✅ pgweb 固定端口 8081(约定)
- ✅ pingora **零改动**

---

## 2. 目标与非目标

### 2.1 本期目标(✅ 做)
1. **镜像改造**:`app-runtime-base` 加 PostgreSQL 16 + pgweb,supervisor 随容器启动(本地 + 生产两处同步)
2. **单容器**:应用 + PG + pgweb 同一个 UserApp 容器
3. **复用 path 路由**:应用端口 + pgweb 8081 都通过现有 `/proxy/apps/{app_id}/{port}/` 访问

### 2.2 非目标(❌ 不做)
- ❌ pingora 改造(host 路由、新路由表)
- ❌ 子域名 / host 生成 / DNS(rcoder 不管,外部系统负责)
- ❌ TLS / HTTPS
- ❌ PG 的 TCP 对外(PG 只容器内 localhost)
- ❌ 多容器(应用与 PG 不分离)

---

## 3. 架构

### 3.1 单容器进程模型
```
UserApp 容器(app-runtime-* 镜像,base 带 PG + pgweb)
├─ PostgreSQL       :5432   (localhost,容器内;PGDATA=/app/data/pg 持久化)
├─ pgweb            :8081   (HTTP,固定端口,--host 0.0.0.0,连 localhost PG)
└─ 用户应用         :{用户端口}  (用户 command,连 localhost PG)

入口:start-app.sh(supervisor 统一拉起 PG + pgweb + exec 用户 command)
```

### 3.2 访问链路(复用现有 path 路由)
```
【应用访问】
  外部 → /proxy/apps/{app_id}/{应用端口}/  → pingora(现有 path 路由)→ app:{应用端口}

【pgweb 访问(开发者)】
  浏览器 → /proxy/apps/{app_id}/8081/  → pingora(现有 path 路由)→ app:8081
                                       → pgweb → localhost:5432 PG → 操作

【子域名(可选,外部系统自己搞)】
  外部网关/Java:{app_id}.nuwax.com → rewrite → pingora /proxy/apps/{app_id}/{port}/
  (rcoder 不管这层映射)
```

### 3.3 职责边界
| 职责 | 负责 |
|---|---|
| **pingora path 路由**(现有) | rcoder(不改) |
| **子域名/host/DNS** | **外部系统**(rcoder 不管) |
| PG + pgweb 运行 | UserApp 容器(镜像内置) |
| 用户应用运行 | UserApp 容器(用户 command) |

---

## 4. 镜像改造(app-runtime-base)

### 4.1 改造内容
base 镜像加:
- **PostgreSQL 16**(apt 官方 repo)
- **pgweb**(Go 二进制,GitHub release)
- **start-app.sh**:supervisor 启动 PG + pgweb + exec 用户 command

### 4.2 关键设计:ENTRYPOINT 包装(单容器多进程协调)
当前 base `CMD start-ttyd.sh` 会被 UserApp command 覆盖(ttyd 不起,之前实测确认)。改为 **ENTRYPOINT 模式**:
```dockerfile
ENTRYPOINT ["/usr/local/bin/start-app.sh"]   # 固定入口:启 PG + pgweb
CMD ["sleep","infinity"]                       # 默认(被 UserApp command 覆盖)
```
- `start-app.sh`(supervisor):启动 postgres + pgweb,最后 `exec "$@"` 执行用户 command
- UserApp 的 `command`(用户应用)作为 CMD args 传给 ENTRYPOINT,**不覆盖 ENTRYPOINT**
- → PG + pgweb 随容器自动起,**用户应用也照常跑**

> ⚠️ 实施时验证 rcoder DockerRuntime 用 docker `command`(覆盖 CMD,不覆盖 ENTRYPOINT)。

### 4.3 PG 数据持久化(必须,否则容器删数据丢)
PostgreSQL 默认 `PGDATA=/var/lib/postgresql/data`,**不在 UserApp 持久化 workspace**。容器删/重建 → 数据全丢(对 DB 是灾难)。

**解法**(钉进方案):start-app.sh 里把 `PGDATA` 指到 workspace 持久化目录:
```bash
export PGDATA=/app/data/pg     # /app/data 是 UserApp workspace(持久化)
initdb -D "$PGDATA" ...        # 首启初始化
postgres -D "$PGDATA" ...      # 常驻
```
→ PG 数据落在 `/app/data/pg`,随 UserApp workspace 持久化(bind mount / PVC),**容器删数据保留**。

### 4.4 两处同步改造(保持一致)
| 位置 | 用途 | 改动 |
|---|---|---|
| `rcoder/docker/app-runtime-base/` | **本地开发测试** | Dockerfile + start-app.sh |
| `build-agent-docker/build_config/app-runtime-base/` | **生产** | 同样改动 |

### 4.5 环境变量
| 变量 | 用途 | 默认 |
|---|---|---|
| `POSTGRES_USER` | PG 用户 | app |
| `POSTGRES_PASSWORD` | PG 密码 | (必填) |
| `POSTGRES_DB` | PG 初始库 | app |
| `PGDATA` | PG 数据目录 | `/app/data/pg`(固定,持久化) |

### 4.6 影响范围(待确认 §8.1)
base 改动 → 所有运行时镜像(node/java/python/go/rust)**继承,都带 PG + pgweb**。镜像各 +~300MB。可用 env 开关(待定)让不需要的关掉。

---

## 5. UserApp 改造(最小)

### 5.1 放开 HTTP 端口数限制
当前 `service.rs` 校验 HTTP ≤1。改为允许**应用 + pgweb** 多 HTTP 端口(或 rcoder 自动注册 pgweb)。

### 5.2 pgweb 端口注册(二选一,§8.2 待定)
- **方案 A(自动,推荐)**:rcoder 约定运行时镜像带 pgweb 8081,create 时自动注册 `(app_id, 8081) → app:8081` 到 pingora。用户 ports 只声明应用端口。
- **方案 B(显式)**:用户 ports 里显式声明 `{name:"pgweb", port:8081, expose_type:Http}`。

### 5.3 access 返回(复用现有 path)
```jsonc
{
  "external": {
    "http": "/proxy/apps/{app_id}/{应用端口}",   // 应用(现有)
    "pgweb": "/proxy/apps/{app_id}/8081"          // pgweb(新增,或自动)
  }
}
```

---

## 6. pingora(不改)
- 完全复用现有 path 路由 `/proxy/apps/{app_id}/{port}/`
- 只要 UserApp 把 (app_id, 8081) 注册进 `app_backends`(现有 register_pingora_backends 机制),pingora 自动路由
- **零代码改动**

---

## 7. 访问方式总结
| 访问 | URL(path) |
|---|---|
| 用户应用 | `/proxy/apps/{app_id}/{应用端口}/` |
| pgweb(开发者) | `/proxy/apps/{app_id}/8081/` |

外部要子域名的话,自己把 `{app_id}.nuwax.com` 映射到上述 path(rcoder 不管)。

---

## 8. 待确认 / 决策点

### 8.1 PG 加在 base vs 专门镜像 ⭐ ✅ 已决策
- **方案 A(改 base)**:✅ 已落地 —— 统一运行时镜像 `app-runtime`（rust:1.97/Debian 底）内置 PG + pgweb，所有应用都带（不再按语言拆镜像）。
- ~~**方案 B(专门镜像)**:`app-runtime-node-pg` 等,按需。~~ 已弃用（多语言合并为单一 `app-runtime`）。

### 8.2 pgweb 端口注册方式
- A(自动,推荐):rcoder 自动注册 8081
- B(显式):用户 ports 声明

### 8.3 ENTRYPOINT 兼容性
rcoder DockerRuntime 是否正确处理 ENTRYPOINT(用户 command 不覆盖 ENTRYPOINT)?实施时验证(§4.2)。

### 8.4 启动顺序
supervisor 按 PG → pgweb → 应用 启动;应用连 PG 失败自动重试(PG initdb 需时间)。

---

## 9. 实施步骤(review 后执行)

1. **镜像改造**(本地 + 生产两处同步):
   - base Dockerfile 加 PG 16 + pgweb
   - 新增 start-app.sh(supervisor:PG + pgweb + exec 用户 command,PGDATA=/app/data/pg)
   - 改 ENTRYPOINT/CMD 模式
2. **本地构建 + 测试**:`bash build.sh node`,跑容器验证 PG + pgweb + 用户应用三进程都起、PG 数据持久化
3. **UserApp 小改**:放开 HTTP 端口数 + pgweb 8081 注册(自动或显式)+ access 返回 pgweb path
4. **端到端测试**:部署 app(node + PG + pgweb),通过 `/proxy/apps/{app_id}/8081/` 访问 pgweb 操作 PG
5. **文档**:handbook 新增「带数据库的应用部署」章节

> pingora **第 4 步零改动**(复用现有 path)。

---

## 附:需用户拍板的决策清单

- [ ] **8.1** PG 加 base(所有运行时带)确认?加 ENABLE_POSTGRES 开关?
- [ ] **8.2** pgweb 8081 自动注册(推荐)vs 用户显式声明?
- [ ] **PG 持久化**(§4.3):确认 PGDATA=/app/data/pg 钉进方案?(强烈建议)
- [ ] 确认本期范围 = 单容器(应用+PG+pgweb)+ 复用 path 路由,pingora 不改,无 TLS,无 TCP 对外,不管子域名/DNS
