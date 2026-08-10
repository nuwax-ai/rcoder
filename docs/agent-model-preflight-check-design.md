# Agent 启动前模型可用性预检

> 状态:**已实现**(2026-08-10)
> 类型:体验优化 / Fail-Fast
> 优先级:中(模型故障时大幅改善错误体验)
> 创建:2026-08-08,来源于 nuwax-k8s-prod 线上排查(project 1670983 等)

---

## 1. 背景(为什么做)

线上曾出现模型代理后端异常(对所有模型稳定返回 `500 {"error":{"message":"null"}}`),但用户侧的表现很不友好:

- **空等很久**:claude-code 内部对 5xx 会**自动重试 10 次**,指数退避累计约 **3 分钟**才最终报错;期间用户只看到"转圈/无响应"。
- **错误提示不友好**:模型故障有时被掩盖成 `Agent initialization timeout (60s)`(建会话超时)或裸的 gRPC 错误,用户无法定位是"模型坏了",只能反复重试或重启 Agent 计算机。
- **重试无效且加剧**:用户重试每次都新建会话(冷启动),又触发同样的等待/超时。

**根因**:agent 启动链路里,没有一个"提前确认模型可用"的关口。模型坏了要等 claude-code 内部重试耗尽、或等建会话超时才暴露,错误信息还失真。

**目标**:在 **agent 第一次启动(新建会话)之前**,主动做一次轻量模型探活。模型明确不可用时,**立即返回友好错误**,不启动 agent、不让用户空等。采取 **fail-open** 策略 —— 拿不准就放行。

---

## 2. 方案概述

| 维度 | 设计 |
|------|------|
| 检查时机 | agent_runner `handle_chat_core` 中,**仅"新建会话"(session_id 为空)**时探活;resume 直接跳过 |
| 检查方法 | genai 库发 `max_tokens:1` 极小推理请求,看返回码 |
| 协议适配 | 按 `api_protocol`+`wire_api` 自动选 adapter(Anthropic/OpenAI Chat/OpenAI Responses) |
| 结果缓存 | moka TTL 缓存(10min),只有"可用"才写;缓存命中零网络开销 |
| 失败处理 | 明确不可用(5xx / 连接拒绝 / DNS / TLS)→ 返回 `ERR_MODEL_UNAVAILABLE`,不启动 agent |
| 超时/拿不准 | **fail-open**:超时 / 429 / 401 / 403 / 4xx → 放行,只 warn 日志 |
| 开销 | 首次探活 ~1-2s;缓存命中后续零开销 |

---

## 3. 详细设计(已实现)

### 3.1 架构:独立 crate `model_probe`

```
crates/model_probe/
├── Cargo.toml          deps: genai 0.6.5, moka, shared_types, reqwest
├── src/
│   ├── lib.rs          ProbeResult 枚举 + pub use check_model_available
│   ├── probe.rs        编排:缓存 + 探活 + build_adapter
│   ├── classify.rs     错误分级(纯函数):genai Result → ProbeResult
│   └── endpoint.rs     URL 规范化(纯函数):双协议 normalize_endpoint
└── tests/integration.rs  集成测试(#[ignore],需 .env.local)
```

agent_runner 只依赖 `model_probe`,不直接依赖 genai —— 隔离干净。

### 3.2 协议选择(build_adapter)

按 `api_protocol` + `wire_api` 选择 genai adapter:

| `api_protocol` | `wire_api` | adapter | 端点 |
|----------------|-----------|---------|------|
| `anthropic`(或 None 默认) | (忽略) | `Anthropic` | POST `/v1/messages` |
| `openai` | `None`(默认) | **`OpenAIResp`** | POST `/responses` |
| `openai` | `"chat"` | `OpenAI` | POST `/chat/completions` |
| `openai` | `"response"` | `OpenAIResp` | POST `/responses` |

> `wire_api` 默认是 Responses API(与 `ModelProviderConfig` 字段文档一致:"response 表示 Responses API(默认)")。

> **严格探测,不做 fallback**:探活严格使用配置指定的协议。如果 Responses API 返回 4xx,说明该协议不匹配 → fail-open 放行(Inconclusive),不尝试 Chat Completions。因为 Agent 实际使用的协议由配置决定,探活结果必须与之一致 —— 探活 Chat 成功但 Agent 用 Responses 会导致假阳性。

### 3.3 URL 规范化(normalize_endpoint)

genai 的 adapter URL 拼接不带 `/` 分隔符,需按协议规范化:

- **Anthropic**:genai 追加 `messages`(不带 `/v1/`),而 claude-code SDK 追加 `/v1/messages` → endpoint 需含 `/v1/`
- **OpenAI**:genai 用 `Url::join("chat/completions")` 或 `Url::join("responses")` → 需 trailing `/`

```rust
fn normalize_endpoint(base_url: &str, protocol: ModelApiProtocol) -> String {
    let base = base_url.trim_end_matches('/');
    match protocol {
        ModelApiProtocol::Anthropic => {
            if base.ends_with("/v1") { format!("{base}/") }     // 已含 /v1
            else { format!("{base}/v1/") }                       // 补 /v1/
        }
        ModelApiProtocol::OpenAI => format!("{base}/"),          // 只补 trailing /
    }
}
```

### 3.4 缓存(moka TTL)

```rust
static MODEL_PROBE_CACHE: LazyLock<Cache<String, ()>> = LazyLock::new(|| {
    Cache::builder().time_to_live(Duration::from_secs(600)).max_capacity(500).build()
});
```

- key = `{normalized_endpoint}|{model}|{adapter}`,value = `()`(仅标记"已验证")
- moka 自动按 TTL 过期,无需手写 Instant 比较
- 只有 `Available` 才写缓存(不可用/拿不准不写,保证下次还能立刻报错)

### 3.5 错误分级(classify)

靠 `genai::Error::WebModelCall { webc_error, .. }` 内的 `webc::Error`:

| webc::Error 变体 | 判定 | 动作 |
|------------------|------|------|
| `ResponseFailedStatus { 5xx }` | **Unavailable** | 拦截 |
| `ResponseFailedStatus { 4xx }`(含 401/403/429) | **Inconclusive** | fail-open 放行 |
| `Reqwest(r)` + `is_timeout()`(含连接超时) | **Inconclusive** | 放行(超时优先,可能只是慢) |
| `Reqwest(r)` + `is_connect()` 且非 timeout | **Unavailable** | 拦截(连接拒绝/DNS/TLS) |
| `Reqwest(r)` + 其他(body/redirect 等) | **Inconclusive** | 放行(拿不准) |
| 其他(JSON 解析等) | **Inconclusive** | 放行 |

> 铁律:任何 Inconclusive 都 fail-open 放行。探活只挡"明确故障"。

### 3.6 检查时机(agent_runner chat_handler)

```
handle_chat_core(input, context):
  ├─ 阶段0: 模型探活(仅 session_id 为空 + model_config 存在)
  │   └─ probe::run_model_probe(&input, &project_id, &session_id)
  │       ├─ Some(blocked) → return blocked(拦截,不启动 agent)
  │       └─ None → 继续(fail-open 或可用)
  ├─ 阶段1: prepare_session
  ├─ 阶段2: dispatch_task
  └─ 阶段3: finalize_response
```

`run_model_probe` 在 `chat_handler/probe.rs`,返回 `Option<ChatHandlerOutput>` —— Some 表示拦截,None 表示继续。

### 3.7 错误码与文案

`ERR_MODEL_UNAVAILABLE`(可重试),三语文案:

| locale | key | 文案 |
|--------|-----|------|
| zh-CN | `error.model_unavailable` | 模型服务暂不可用(无法连接或返回错误)。请检查模型配置或稍后重试。 |
| en-US | `error.model_unavailable` | Model service is unavailable (connection failed or returned an error). Check model config or retry later. |
| zh-TW | `error.model_unavailable` | 模型服務暫不可用(無法連接或返回錯誤)。請檢查模型配置或稍後重試。 |

> agent_runner 无 locale 上下文,首版用 `get_i18n_message_default`(DEFAULT_LOCALE)。

---

## 4. 库选型:genai 0.6.5

用 crates.io 稳定版 `genai = "0.6.5"`(非 beta)。核心 API:`ServiceTarget { endpoint, auth, model }` + `exec_chat(target, req, options)`,绕过 genai 名称路由。

依赖冲突风险:低。`derive_more 2.1.1` / `reqwest 0.13` / `tokio 1` 均已在 workspace。`serde_json preserve_order` 早已被 file-server 启用,genai 不引入新影响。

社区方案对比见附录(原调研保留)。

---

## 5. 涉及文件

| 文件 | 改动 |
|------|------|
| `crates/model_probe/`(**新增 crate**) | 4 模块 + 集成测试,24 单测 |
| `crates/agent_runner/src/service/chat_handler/` | 拆分为 mod.rs + types.rs + probe.rs + prepare.rs + dispatch.rs + finalize.rs |
| `crates/agent_runner/Cargo.toml` | 加 `model_probe = { workspace = true }` |
| `Cargo.toml`(workspace) | 加 `genai = "0.6.5"` + `model_probe` path |
| `crates/shared_types_i18n/src/error_codes.rs` | `ERR_MODEL_UNAVAILABLE` 常量 + i18n key + retryable + 单测 |
| `crates/shared_types_i18n/locales/{zh-CN,en-US,zh-TW}.yml` | 三语 `error.model_unavailable` |
| `crates/shared_types/src/lib.rs` + `shared_types_i18n/src/lib.rs` | re-export `ERR_MODEL_UNAVAILABLE` |

---

## 6. 完成标准(全部达成 ✅)

- [x] `cargo clippy --features kubernetes --workspace` 零 warning
- [x] 探活**明确不可用**(5xx / 连接拒绝 / DNS / TLS)→ 返回 `ERR_MODEL_UNAVAILABLE`,不启动 agent
- [x] 探活**超时/429/401/403**(拿不准)→ fail-open 放行
- [x] resume(session_id 非空)不触发探活
- [x] TTL 内重复新建会话不再探活(moka 缓存命中)
- [x] 三语文案齐备
- [x] 单测:24 个(build_adapter 5 + endpoint 8 + classify 9 + probe 边界 2)+ 集成测试 5 个
- [x] 集成验证:GLM(Anthropic)Available + DeepSeek(OpenAI Responses)Available + 缓存命中 + 不可达拦截

---

## 7. 后续可选增强(非本期)

- **single-flight 并发去重**:同 key 并发探活共享一次结果(暂缓 —— 缓存 + fail-open 已足够)
- **退避告警**:短期多次探活失败 → 上报告警
- **rcoder 侧前置探活**:转发前就探活,避免无谓容器/会话编排

---

## 附:社区方案调研(设计阶段)

生产 LLM 网关处理"模型不可用"有三种策略:

| 策略 | 代表 | 代价 |
|---|---|---|
| **A. 重试+降级** | LiteLLM / Portkey | 首个请求失败/重试;需有降级目标 |
| **B. preflight 探活** | Kong / Bifrost | 延迟、误报、消耗限流配额 |
| **C. 在线快速失败** | 部分自研 | 实现复杂(解析流、取消) |

我们选 B(preflight),因为:单会话只配一个模型,无降级目标;纯重试让用户干等 3 分钟。我们场景是"单会话首次探一次 + 缓存",B 的"限流消耗/状态过期"代价最小化。

来源:[LiteLLM health-check routing](https://docs.litellm.ai/docs/proxy/health_check_routing)、[Portkey failover 设计](https://portkey.ai/blog/how-to-design-a-reliable-fallback-system-for-llm-apps-using-an-ai-gateway/)。

---

## 附:线上排查关键证据

- 故障模型:`http://47.109.194.91:18086/api/proxy/model`,对所有模型 `500 {"error":{"message":"null"}}`(6 种姿势 curl 全 500)。
- claude-code 对 5xx 自动重试 10 次,累计 ~178s(~3 分钟)。
- 换可用模型(如智谱 GLM)整链路 6s 跑通 → 链路本身没问题,纯模型后端故障。
