# Vendored pingora-core 0.9.0（RCoder 受控补丁）

来源：crates.io `pingora-core 0.9.0`
（registry checksum `7fd4d1ddc6d6dbc1a97164ce13c301980c25c35422aab87b512c4c8b5123729b`），
经 `[patch.crates-io]` 路径补丁接入（与 `vendor/toasty-driver-postgresql` 同一维护方式）。
除下列差异外，源码与上游 0.9.0 逐字节一致；升级 Pingora 版本时必须先重放或评估本补丁
（H2 侧需与 `vendor/pingora-proxy` 的配套差异一起评估）。

## 为什么补丁（影响说明）

Pingora 0.9 没有"只等待上游最终响应头"的超时旋钮：`read_timeout` 同时管
响应头与正文、且每次 read 重置——直接设置会把正常 SSE/下载在 60s 空闲处
误杀，慢滴响应头也永远凑不齐超时。UserApp "请求已发往上游但等不到响应头"
的挂起问题需要**阶段预算**（绝对 deadline、请求 EOS 后起算、100/103 不算、
101 即止、头到即取消、重试共享剩余预算）。

## 精确差异（4 个文件）

1. `src/upstreams/peer.rs`
   - `PeerOptions` 新增 `pub response_header_timeout: Option<Duration>`，
     `PeerOptions::new()` 默认 `None`。默认 None = 其他消费者行为不变。

2. `src/protocols/http/v1/client.rs`
   - 新增 `pub const RESPONSE_HEADER_TIMEDOUT: ErrorType = ErrorType::new("ResponseHeaderTimedout")`。
   - `HttpSession` 新增 `pub response_header_timeout`（配置）与私有
     `response_header_deadline: Option<tokio::time::Instant>`（已武装的绝对期限）。
   - `new_with_options` 从 peer options 复制该配置。
   - `read_response()`：仅在**请求已完整写出**（`body_writer.finished()`——
     上传耗时不计入预算）且首次进入时武装 deadline；每次 socket read 同受
     per-read `read_timeout`（重置）与绝对 deadline（不重置、1xx/零碎头不
     重新起表）约束；deadline 耗尽返回 `RESPONSE_HEADER_TIMEDOUT` 错误，
     与普通 `ReadTimedout` 区分。归属判定：`read_timeout=None` 时该 read
     的唯一约束就是 deadline（`pingora_timeout` 的 10ms tick 轮次有微小
     取整抖动），定时器触发即归 deadline；`read_timeout=Some` 时仅当时钟
     确已越过绝对 deadline 才归 deadline，较小的 per-read 超时保持
     `ReadTimedout`。仅作用于响应头阶段：最终头（含 101）解析后，正文/
     升级读保持既有超时。

3. `src/protocols/http/v2/client.rs`
   - `Http2Session` 同款两个字段；`read_response_header()` 在请求流已发
     END_STREAM 时武装绝对 deadline 并约束等待；超时仅结束本流（错误返回
     后 stream 被释放/复位，连接其他流不受影响——回归测试覆盖）。
   - **END_STREAM 可观察性**（独立复审修复）：H2 驱动经
     `take_request_body_writer()` 拿走裸 `SendStream` 直发 DATA/END_STREAM，
     会话侧 `ended` 不再翻转——新增 `end_stream_signal: Arc<Notify>` 与
     `pub fn request_end_stream_signal()`；会话 API 的三处 EOS
     （`write_request_header(end)`/`write_request_body(end)`/
     `finish_request_body`）经 `mark_end_stream()` 置位并 `notify_one()`；
     驱动（vendored pingora-proxy）在真实 END_STREAM 写出后通知同一信号。
     `read_response_header()` 未武装且 `ended` 仍为假时，先在
     "END_STREAM 信号 / 响应头自身 / per-read 超时" 三者上等待（poll_fn
     取消安全），信号到达即武装、进入有界等待——上传耗时不计入预算，
     且早到的响应头立即正常返回。
   - 测试（`tests_h2`）：`detached_upload_end_stream_still_bounds_header_deadline`
     与 `header_deadline_timeout_isolates_the_stream`（真实 h2 双工 + 复刻
     驱动的 writer 分离）。

4. `src/connectors/http/v2.rs`
   - `new_http_session` 与 `reused_http_session` 把 peer option 复制到新
     spawn 的 H2 stream session（H1 经 `new_with_options` 已覆盖）。

## 行为边界

- 默认 `None`：不设置该字段的调用方（含 app-cli 独立 workspace）行为与
  上游 0.9.0 完全一致。
- 超时错误不设置 retry 标记：不自动重放；跨重试的预算共享由使用方
  （rcoder-proxy）以绝对 deadline 递减传入。
- H2 语义成立的前提：驱动在 END_STREAM 写出后调用
  `request_end_stream_signal().notify_one()`（vendored pingora-proxy 已接
  线；上游 pingora-proxy 未接线时，H2 上传形态预算不武装——不产生假语义）。

## 锁文件副作用

pingora-core 声明 `zstd = "0"`；接入路径补丁后 workspace 重解析将其 zstd
从 0.14.3 统一到 0.13.3（与 pingora-header-serde 共用，"0" 语义下两者均
合法）。`vendor/pingora-core/Cargo.lock` 保留上游发布形态（仅影响本目录
独立测试的解析，workspace 消费时忽略）。升级/移除本补丁时核对
`Cargo.lock` 中 pingora-core 的 zstd 解析。
