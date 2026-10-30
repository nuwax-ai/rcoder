# Vendored pingora-proxy 0.9.0（RCoder 受控补丁）

来源：crates.io `pingora-proxy 0.9.0`（registry checksum
`2467265ad39b56230b56149f1c03ca510f46c4bf4a25d6d4b7345e0f06cc46e7`），
经 `[patch.crates-io]` 路径补丁接入（与 `vendor/pingora-core` 同一维护方式，
见其 README 的背景说明：Pingora 0.9 无"只等上游最终响应头"旋钮）。

除下列差异外，源码与上游 0.9.0 一致；升级 Pingora 版本时必须先重放或评估
本补丁（及 `vendor/pingora-core` 的配套差异）。

## 为什么需要补丁 pingora-proxy（影响说明）

H2 驱动（`proxy_h2.rs::proxy_1to2`）为并发上传与读响应，经
`take_request_body_writer()` 拿走裸 `SendStream`，DATA/END_STREAM 全部经该
流直发，不经过 `Http2Session` 的会话 API——core 侧的首头 deadline 因此观察
不到上传 EOS（独立复审已实证：无体请求预算正常触发，上传形态无界）。core
补丁为此提供 `Http2Session::request_end_stream_signal()`（`Notify` 句柄）；
本补丁让驱动在**真实 END_STREAM 发出处**（`send_body_to2` 成功写出
`end=true` 后）通知该信号，使首头预算在 H2 上传形态下同样自 EOS 起算。

## 精确差异（1 个文件）

`src/proxy_h2.rs`：

1. `proxy_1to2`：`take_request_body_writer` 后取
   `client_session.request_end_stream_signal()`，传给
   `bidirection_down_to_up`（新增参数）。
2. `bidirection_down_to_up`：信号透传至两处 `send_body_to2` 调用
   （重试缓冲回放与常规 body 路径）。
3. `send_body_to2`：新增末参；`write_body(.., end=true)` 成功后
   `end_stream_signal.notify_one()`（无预算配置时为无害空操作）。bodyless
   路径（`write_request_header(end=true)` / `write_request_body(empty, true)`
   / `finish_request_body`）在 core 侧的 `mark_end_stream` 内自行通知。

## 验证锚点

- 本目录自身单测：58/58 通过（`cargo nextest run --manifest-path
  vendor/pingora-proxy/Cargo.toml --lib`）。
- 行为反例在 `vendor/pingora-core` 的 h2 测试：
  `detached_upload_end_stream_still_bounds_header_deadline`（复刻驱动的
  writer 分离——take → 裸流发 END_STREAM → 通知信号 → 黑洞，预算内有界）
  与 `header_deadline_timeout_isolates_the_stream`（并发流不受影响、超时流
  被服务端以 CANCEL 复位）。
- 本 manifest 尾部的 `[patch.crates-io] pingora-core = ../pingora-core` 仅供
  本目录独立测试解析配套补丁的 core；workspace 消费用根 manifest 的 patch。

## 锁文件副作用

无（上游 lockfile 保留；独立测试时 cargo 会按 path patch 记录本地 core）。
