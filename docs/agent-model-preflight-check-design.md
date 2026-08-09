# Agent 启动前模型可用性预检(代办)

> 状态:**待实现**(后续版本开发)
> 类型:体验优化 / Fail-Fast
> 优先级:中(模型故障时大幅改善错误体验)
> 创建:2026-08-08,来源于 nuwax-k8s-prod 线上排查(project 1670983 等)

---

## 1. 背景(为什么做)

线上曾出现模型代理后端异常(对所有模型稳定返回 `500 {"error":{"message":"null"}}`),但用户侧的表现很不友好:

- **空等很久**:claude-code 内部对 5xx 会**自动重试 10 次**,指数退避累计约 **3 分钟**才最终报错;期间用户只看到"转圈/无响应"。
- **错误提示不友好**:模型故障有时被掩盖成 `Agent initialization timeout (60s)`(建会话超时)或裸的 gRPC 错误,用户无法定位是"模型坏了",只能反复重试或重启 Agent 计算机。
- **重试无效且加剧**:用户重试每次都新建会话(冷启动),又触发同样的等待/超时;超时后清理不干净还会泄漏进程(见 [[container-not-destroyed-keepalive]] 同类清理问题)。

**根因**:agent 启动链路里,没有一个"提前确认模型可用"的关口。模型坏了要等 claude-code 内部重试耗尽、或等建会话超时才暴露,错误信息还失真。

**目标**:在 **agent 第一次启动(新建会话)之前**,主动做一次轻量模型探活。模型明确不可用时,**立即返回友好错误**,不启动 agent、不让用户空等。agent 一旦启动成功(说明模型可用),后续对话不再探活。

> 注:本特性是"提前发现明显故障"的锦上添花,**不是**严格鉴权,采取 **fail-open** 策略(见 §4.3)。

---

## 2. 方案概述

| 维度 | 设计 |
|------|------|
| 检查时机 | agent_runner 处理 chat 时,**仅"新建会话"(session_id 为空)且 agent 尚未启动**时探活一次;复用已有会话(resume)直接跳过 |
| 检查方法 | 用请求自带的 `model_config` 发一个极小推理请求(`max_tokens:1`),看返回码 |
| 结果缓存 | 探活通过 / agent 会话建立成功 → 标记 `(api_base, model)` 已验证;TTL(如 10 min)内不重复探活 |
| 失败处理 | 明确不可用(**5xx** / 连接拒绝 / DNS / TLS 失败)→ 返回 `ERR_MODEL_UNAVAILABLE` 友好错误,**不启动 agent** |
| 超时/拿不准 | **fail-open**:探活超时(>5s)、429 限流、**401/403**(防怪网关 auth 误杀)、网络抖动 → 放行让 agent 自己试,只记 warn 日志 |
| 开销 | 模型正常时多一次 ~几十 ms~1s 的小请求;有缓存,后续对话零开销 |

---

## 方案对比(社区调研)

> 本节回答"社区一般怎么做、我们为什么选 preflight"。来源:[LiteLLM health-check routing](https://docs.litellm.ai/docs/proxy/health_check_routing)、[LiteLLM reliability/fallback](https://docs.litellm.ai/docs/proxy/reliability)、[Portkey fallback](https://portkey.ai/docs/product/ai-gateway/fallbacks)、[Portkey failover 设计](https://portkey.ai/blog/how-to-design-a-reliable-fallback-system-for-llm-apps-using-an-ai-gateway/)。

生产 LLM 网关处理"模型不可用"有三种典型策略:

| 策略 | 代表 | 机制 | 代价 |
|---|---|---|---|
| **A. 重试+降级(reactive)** | LiteLLM / Portkey **默认** | 不 preflight;请求失败→重试 `num_retries` 次→降级到备用模型组。LiteLLM 原话:"先路由,失败后才摘除" | 首个请求会失败/重试;需要**有降级目标** |
| **B. preflight 探活** | Kong / Bifrost | 启动前(或后台轮询)探活,坏了直接摘 | 延迟、误报、消耗限流配额、状态过期 |
| **C. 在线快速失败** | 部分自研网关 | 让请求跑,但监听其流;连续 5xx 立即取消 + 友好报错 | 实现复杂(要解析流、取消) |

**为什么我们选 B(preflight)而非 A**:
- LiteLLM/Portkey 选 A 的前提是**能降级到别的模型**。我们**一个会话只配一个模型,无降级目标** —— 纯重试(LiteLLM 式)只会让用户干等 claude-code 的 10 次重试(~3 分钟)。对我们而言,提前探活 → 立即友好报错,收益远大于 LiteLLM 那种场景。
- 我们场景是"单会话首次启动探一次 + 缓存",不是"后台持续轮询所有部署",所以 B 的"限流消耗/状态过期"代价被最小化(每会话最多一次,有缓存)。

**B 的固有风险及缓解**:
| 风险 | 缓解(已写入设计) |
|---|---|
| 误报(探活说坏、实际好)→ 挡正常用户 | **只拦 5xx + 连接/DNS/TLS 失败**;401/403/429/超时全 fail-open(§3.3) |
| 首次 chat 多等(超时 5s) | 超时压到 5s;缓存命中后零开销(§3.4) |
| 状态过期 | 短 TTL(10min)+ 探活不缓存"不可用"(§3.4) |
| 持续故障 | 后续可加"连续失败上报告警"(§7) |

**备选 C(在线快速失败)**:不在本期,但记录为备选 —— 让 agent 正常起,但解析其输出流,检测到连续模型 5xx 就取消并友好报错。优点是零 preflight 延迟/零误报;缺点是要侵入 SSE/gRPC 流解析 + 取消逻辑,复杂度高。**若 preflight 上线后仍有"探活通过但对话时模型挂"的窗口**,再考虑叠加 C。

> 结论:本期采 **B(preflight,max_tokens:1 单次 + 缓存 + 只拦 5xx/不可达)**,契合我们"单模型、无降级、要快速友好报错"的场景。

---

## 3. 详细设计

### 3.1 检查时机(在 agent_runner chat_handler)

插入点:`crates/agent_runner/src/service/chat_handler.rs::handle_chat_core`(line ~183),"新建会话"分支附近(line ~496 `session_id is None` / line ~574 `create new session` 之前)。

```
chat 请求进来
  ├─ session_id 非空(resume) → 跳过探活,直接复用会话
  └─ session_id 为空(新建)
       ├─ 缓存命中 (api_base,model) 已验证且未过期 → 跳过探活
       └─ 否则 → 探活
            ├─ 明确不可用 → 返回 ERR_MODEL_UNAVAILABLE(不创建会话)
            └─ 可用 / 拿不准(fail-open) → 继续建会话;建成功后写缓存
```

> 只在"新建会话"探活,对应需求里"agent 第一次启动才检查"。会话建好后 agent 常驻,后续对话(resume)不再探活。

### 3.2 探活实现(用 genai 库,不手写协议)

> **数据来源(已核对源码 `crates/shared_types/src/model/model_provider.rs`)**:`ModelProviderConfig` 字段是 `base_url` / `api_key` / `requires_openai_auth` / `default_model` / `api_protocol: Option<String>` / `wire_api`。**实际对话的 model 名来自请求体**(如 `qwen3.8-max-preview`),不是 provider 配置的 `default_model` —— 探活用**请求里的 model 名** + provider 的 `base_url`/`api_key`。若请求或 model_config 为 None → **跳过探活**(无端点可探)。

**库选型**:[genai](https://github.com/jeremychone/rust-genai)(`0.7.x`,默认 `rustls-tls`,reqwest 0.13,依赖足迹与 rcoder 基本重合,无新重型依赖)。选它因为:① 原生支持 anthropic/openai 双协议(不用我们维护协议分支);② `ServiceTarget` 原生支持自定义网关端点(你们走代理网关,非官方端点);③ `webc::Error` 能区分 HTTP 状态 vs 网络超时(支撑 fail-open 分级);④ 原生覆盖 anthropic/openai/deepseek/bigmodel(智谱)/aliyun/qwen_cloud/moonshot 等你们用的 provider。

> `Cargo.toml`:`genai = { version = "0.7", default-features = true }`(默认 rustls-tls)。无需按 provider 切 feature(adapter 全在一个 crate 内)。
> **强制 adapter**:用 `Client::builder().with_adapter_kind(adapter)` **按 `api_protocol` 绑死** adapter(anthropic→`AdapterKind::Anthropic`,否则 OpenAI),**不让 genai 按 model 名自动路由**(自定义网关的 model 名如 `qwen3.8-max-preview` 会让 genai 误判 provider)。

**探活接口**:社区标准 = `max_tokens:1` 的 liveness ping(AgentWASP `_run_boot_sequence` / LiteLLM 深度冒烟都用这个),不用 `GET /v1/models`(那个只验"端点活着",验不出推理链路坏;线上故障时 `/v1/models` 也 500,但万一"列模型正常、推理坏"只有推理 ping 能发现)。1 token 成本可忽略。

**探活代码骨架**(源码已核实 API,字段名已核对):

```rust
use genai::chat::{ChatMessage, ChatRequest, ChatOptions};
use genai::resolver::{AuthData, Endpoint};
use genai::{AdapterKind, Client, ModelIden, ServiceTarget};

// provider = &ModelProviderConfig, model_name = 请求里的模型名(如 "qwen3.8-max-preview")
let adapter = match provider.get_api_protocol() {
    ModelApiProtocol::Anthropic => AdapterKind::Anthropic,
    ModelApiProtocol::OpenAi    => AdapterKind::OpenAi,
};
let target = ServiceTarget {
    endpoint: Endpoint::owned(&provider.base_url),
    auth:     AuthData::Key(provider.api_key.clone()),
    model:    ModelIden::new(adapter, model_name),
};
// 注入带短超时的 reqwest(整体 5s;fail-open 阈值,见 §3.3)
let client = Client::builder()
    .with_adapter_kind(adapter)           // 绑死,避免按 model 名误路由
    .with_reqwest(reqwest::Client::builder().timeout(Duration::from_secs(5)).build()?)
    .build();
let req = ChatRequest::new(vec![ChatMessage::user("hi")]);
let result = client.exec_chat(model_name, req,
    Some(&ChatOptions::default().with_max_tokens(1))).await;
classify(result)  // → Available / Unavailable / Inconclusive
```

> `adapter_kind`:`api_protocol == "anthropic"` → `AdapterKind::Anthropic`,否则按 base_url/model 归到 OpenAI 或对应原生 adapter(智谱→`bigmodel`、deepseek→`deepseek` 等);自定义网关用 `with_adapter_kind` 绑定到对应协议 adapter。首版可先只支持 anthropic + openai 两类,覆盖绝大部分场景。

**fail-open 分级**(靠 `webc::Error` 两个变体,源码已核实):

```rust
match result {
    Ok(_) => ProbeResult::Available,                       // 2xx → 可用
    Err(e) => match extract_webc_error(&e) {
        // HTTP 状态码分支
        Some(webc::Error::ResponseFailedStatus { status, .. }) =>
            if status.is_server_error() {
                ProbeResult::Unavailable(format!("HTTP {}", status))   // 仅 5xx → 拦截(最明确)
            } else {
                ProbeResult::Inconclusive                               // 401/403/429/其他4xx → 全放行
            },
        // 网络层分支(reqwest 自带判定)
        Some(webc::Error::Reqwest(r)) =>
            if r.is_timeout() { ProbeResult::Inconclusive }             // 超时 → 放行(可能慢)
            else { ProbeResult::Unavailable(format!("connect: {r}")) }, // 连接拒绝/DNS/TLS → 拦截
        _ => ProbeResult::Inconclusive,                                  // 其他(JSON解析等)→ 放行
    }
}
```

> 铁律:任何 `Inconclusive` 都 **fail-open 放行**(只 warn 日志),不拦截。探活只挡"明确故障"。

### 3.3 判定规则(关键)

| 探活返回 | 判定 | 动作 | 说明 |
|---|---|---|---|
| HTTP 2xx | 可用 | 放行 + 写缓存 | |
| **HTTP 5xx**(含本次线上的 `500 {"message":"null"}`) | **不可用** | 拦截,返回友好错误 | 最明确的"服务端坏"信号 |
| **连接拒绝 / DNS / TLS 失败** | **不可用** | 拦截 | 端点明确不可达 |
| 探活超时(>5s) | 拿不准 | **fail-open 放行** | 可能只是慢,别误杀 |
| HTTP 429(限流) | 拿不准 | **fail-open 放行** | 瞬时限流不代表不可用 |
| **HTTP 401 / 403** | 拿不准 | **fail-open 放行** | ⚠️ 见下"误报风险" |

**⚠️ 为什么 401/403 也放行(防误杀,与早期版本不同)**:怪异网关可能对探活请求的 auth 头格式(`x-api-key` vs `Authorization: Bearer`)或附加 header(`anthropic-version` 等)与真实 agent(claude-code)要求不一致 → 探活拿到 401/403,但真实 agent 用同 key 实际能跑。**若拦 401/403 会误杀正常用户**。所以只拦"无歧义的服务端故障"(5xx)+ "端点不可达"(连接/DNS/TLS),认证类一律放行让 agent 自己试。线上要解决的是 500,不是 401。

**铁律:拿不准就放行。** 探活只是"提前发现明显故障",宁可漏报(让 agent 跑)不可误报(挡住正常请求)。

### 3.4 缓存

- 结构:`DashMap<String, Instant>`,key = `format!("{}|{}", api_base, model)`。
- 位置:agent_runner 的 AppState(或 SessionCache 旁)。
- 写入:探活通过,**或** agent 会话成功建立后(双重保险 —— 会话能建说明模型一定可用)。
- TTL:10 分钟(可配)。过期后下次新建会话再探活一次。
- 失效:探活拦截(不可用)时**不写**缓存(让用户下次还能立刻看到错误,而不是被缓存"已验证"放行)。

### 3.5 错误码与文案

新增错误码 `ERR_MODEL_UNAVAILABLE`:

- `crates/shared_types_i18n/src/error_codes.rs`:`pub const ERR_MODEL_UNAVAILABLE: &str = "ERR_MODEL_UNAVAILABLE";`,HTTP 503,`is_retryable = true`(模型恢复后用户重试即可)。
- 三语文案 `crates/shared_types_i18n/locales/{zh-CN,en,ja-JP}.yml`,key `error.model_unavailable`:
  - zh-CN:`模型服务暂不可用(无法连接模型或返回错误)。请检查模型配置或稍后重试;如持续失败请联系管理员。`
  - en:`Model service is unavailable (connection failed or returned an error). Check model config or retry later; contact admin if it persists.`
  - 可细分 `error.model_auth_invalid`(401/403)、`error.model_unreachable`(连接失败)。

### 3.6 返回方式

- HTTP chat 路径:探活失败 → 直接 `HttpResult::error_with_message(ERR_MODEL_UNAVAILABLE, locale, ...)`,不进入建会话流程。
- SSE 路径:发一个错误 event 后关闭流(复用现有 SSE 错误注入机制,见 `session_stream_registry` 的 `make_connection_error_event` 模式)。
- 日志:`warn!("[MODEL_PROBE] model unavailable, blocking agent start: api_base={}, model={}, reason={}", ...)`;fail-open 时也 warn。

---

### 3.7 并发去重(single-flight,必做)

突发场景:用户连点 / 前端重试,N 个 chat 同时进来建新会话(同一 `base_url+model`),若各打各的探活 → N 次重复请求 + N 倍延迟/限流消耗。

**解法**:探活用 single-flight(单飞)—— 同 key 探活进行中时,后到的请求**共享这一次探活结果**,不重复发。
- 实现:`DashMap<String, Arc<SharedProbe>>`,key 同缓存键;`SharedProbe` 内部放一个 `OnceCell<ProbeResult>` 或 `tokio::sync::broadcast` / `Notify`。
- 第一个请求发起探活并持有 `SharedProbe`;并发请求 await 同一个;探活完成后写缓存、清 single-flight 条目。
- 与缓存(§3.4)配合:先查缓存→未命中查 single-flight→都没有才发起新探活。

### 3.8 首次 chat 延迟代价(坦诚)

- 模型正常:探活是一次 `max_tokens:1` 请求,典型 **200ms~1s**,加在**首次**新建会话前;缓存命中后后续对话零开销。
- 模型慢但可用:探活可能跑到超时 **5s**(fail-open),即首次 chat 最多多等 5s。这是 preflight 的固有代价 —— 换"模型真坏时秒级友好报错(省 60s~3min)"。
- 超时阈值取 **5s**(而非 8s)以压低 happy-path 代价;5s 对"探活 ping"已足够宽裕(正常推理 ping <1s)。
- 若未来想完全消除 happy-path 延迟,改走 §"方案对比" 备选 C(在线快速失败),但复杂度更高。

---

## 4. 涉及文件 / 改动点

| # | 文件 | 改动 |
|---|------|------|
| 1 | `crates/agent_runner/src/service/model_probe.rs`(**新增**) | 探活核心逻辑:`probe_model(&ModelProviderConfig) -> ProbeResult`(枚举:Available / Unavailable(reason) / Inconclusive)。含超时、协议分支、判定规则 |
| 2 | `crates/agent_runner/src/service/chat_handler.rs` | `handle_chat_core` 新建会话分支前调 `probe_model`;缓存命中跳过;失败返回 `ERR_MODEL_UNAVAILABLE`;成功/会话建立后写缓存 |
| 3 | `crates/agent_runner/src/service/mod.rs` | `pub mod model_probe;` 导出 |
| 4 | agent_runner AppState(或 SessionCache) | 加 `model_probe_cache: DashMap<String, Instant>` + TTL |
| 5 | `crates/shared_types_i18n/src/error_codes.rs` | 加 `ERR_MODEL_UNAVAILABLE`(503, retryable);加入 `is_retryable_code` 白名单;`http_status_for_code` 映射 |
| 6 | `crates/shared_types_i18n/locales/{zh-CN,en,ja-JP}.yml` | 三语 `error.model_unavailable`(及可选 `model_auth_invalid` / `model_unreachable`) |
| 7 | (可选)`crates/agent_runner` config | 探活超时 / TTL / 开关 `model_probe_enabled`(默认 true,可关) |

> 跨 crate 复用提示:若 rcoder 侧也想在转发前探活(更早拦截),可把 `model_probe` 下沉到 `shared_types` 或新建共享 crate。首版建议只在 agent_runner 做(离 agent 启动最近,model_config 现成)。

---

## 5. 完成标准

- [ ] `cargo check --features kubernetes --workspace` + `cargo clippy --features kubernetes --workspace` 零新增 warning(禁 `unwrap`/`expect`,用 `.context()`)
- [ ] 探活**明确不可用**(5xx / 连接拒绝 / DNS / TLS 失败)时,chat 立即返回 `ERR_MODEL_UNAVAILABLE`,**不启动 agent、不建会话**(<1s 返回,不等 60s/3min)
- [ ] 探活**超时/429/401/403**(拿不准)时 **fail-open 放行**(不拦截),warn 日志可见 —— 防止怪网关 auth 格式差异导致误杀
- [ ] resume 会话(session_id 非空)**不触发**探活
- [ ] 首次探活通过后,TTL 内重复新建会话**不再探活**(缓存命中)
- [ ] 三语文案齐备;SSE 路径也能收到友好错误
- [ ] 单测:覆盖 `ProbeResult` 各判定分支(2xx/5xx/401/超时/429/连接失败);缓存 TTL 命中/过期
- [ ] (部署后验证)模型 500 时,用户 <2s 看到"模型服务暂不可用",而非空等

---

## 6. 风险与约束

1. **fail-open 必须守住**:探活任何拿不准的情况都放行,否则会误杀正常请求(模型偶发慢/限流)。这是本特性的安全阀。
2. **不要变成严格鉴权**:探活只挡"明显故障",不做模型能力校验、不做额度管理。
3. **协议分支要准**:`requires_openai_auth` / `api_protocol` 决定走 anthropic 还是 openai 端点/认证头,搞反会误判。参考现有模型 env 渲染逻辑(`agent_abstraction` 的 model_env)。
4. **密钥是敏感信息**:探活日志**不要**打印完整 api_key(脱敏,参考现有 `ak-0***` 打印)。
5. **缓存别误缓存"不可用"**:只有"可用"才写缓存;不可用不写,保证下次还能立刻报错。
6. **不要在 resume 路径探活**:已有会话说明模型之前可用,别加延迟。

---

## 7. 后续可选增强(非本期)

- **健康度缓存共享**:多个 user 用同一 model provider 时,共享探活结果(按 api_base+model 维度,而非 per-session),减少重复探活。
- **退避告警**:短期内同 provider 多次探活失败 → 上报告警(模型后端可能整体故障)。
- **rcoder 侧前置探活**:在 rcoder 转发 chat 到 agent_runner 之前就探活,避免无谓的容器/会话编排(需把 model_probe 下沉到共享 crate)。
- **结合初始化超时治理**:本特性解决"模型坏导致空等";另需单独治理"新 pod agent 全栈首次冷加载 >60s 建会话超时"(调大 `acp_session_create_timeout` 或 pod 启动预热 agent)—— 两者是不同问题,本特性不覆盖后者。

---

## 附:本次线上排查的关键证据(供实现时参考)

- 故障模型:`http://47.109.194.91:18086/api/proxy/model`,对 `qwen3.8-max-preview` / `deepseek-v4-flash` 均 `500 {"error":{"message":"null"}}`(6 种姿势 curl 全 500,13~28ms 秒回)。
- claude-code 对 5xx 自动重试 10 次,退避 0.56s→39.7s,累计 ~178s(~3 分钟)。
- 模型坏 → 对话(prompt)阶段重试耗尽后失败;部分场景掩盖为 `Agent initialization timeout (60s)`。
- 换可用模型(如智谱 `https://open.bigmodel.cn/api/anthropic` glm-4.6)整链路 6s 跑通 → 链路本身没问题,纯模型后端故障。
