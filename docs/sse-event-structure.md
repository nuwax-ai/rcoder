# SSE 进度事件结构文档

## 📖 概览

本文档说明如何在 OpenAPI Swagger 文档中查看和理解 SSE (Server-Sent Events) 进度流的事件结构。

## 🔍 在 OpenAPI 文档中查看

### 1. 访问 Swagger UI

启动服务后访问：
```
http://localhost:8087/api/docs
```

### 2. 查看 SSE 接口

在 Swagger UI 中找到以下接口：

- **Agent 进度流**: `GET /agent/progress/{session_id}`
- **Computer Agent 进度流**: `GET /computer/agent/progress/{session_id}`

### 3. 查看事件结构

在接口文档的 "Responses" 部分：

1. 点击 **200 响应** 展开详情
2. 在 **description** 中查看：
   - 📡 SSE 事件格式说明
   - 🔄 各种事件类型示例
   - 💡 JavaScript 使用示例

3. 在 **Schemas** 部分查找 `ProgressEventDoc` 获取完整的字段定义

## 📊 事件结构说明

### ProgressEventDoc

通过 SSE 流推送的核心事件结构：

```typescript
interface ProgressEventDoc {
  // 消息主类型
  message_type: "SessionPromptStart" | "SessionPromptEnd" | "AgentSessionUpdate" | "Heartbeat";
  
  // 消息子类型（作为 SSE 的 event 字段）
  sub_type: string;  // agent_message_chunk, tool_call, end_turn 等
  
  // ACP 消息的完整 JSON 载荷（作为 SSE 的 data 字段）
  payload: object;
  
  // 可选的请求 ID
  request_id?: string;
  
  // 时间戳（Unix 毫秒）
  timestamp: number;
}
```

### 常见子类型 (sub_type)

| sub_type | 说明 | payload 示例 |
|----------|------|-------------|
| `agent_message_chunk` | AI 响应文本片段 | `{"content":{"type":"text","text":"Hello"},"index":0}` |
| `agent_thought_chunk` | AI 思考过程片段 | `{"thinking":"正在分析...","is_complete":false}` |
| `tool_call` | 工具调用事件 | `{"tool_name":"read_file","tool_input":{"path":"test.rs"},"status":"started"}` |
| `tool_result` | 工具执行结果 | `{"tool_name":"read_file","tool_output":"...","status":"success"}` |
| `plan` | 执行计划 | `{"steps":[...],"current_step":1}` |
| `end_turn` | 对话轮次结束 | `{"reason":"complete","final_message":"Done"}` |
| `cancelled` | 任务被取消 | `{"reason":"user_request"}` |
| `error` | 错误事件 | `{"code":"EXECUTION_ERROR","message":"..."}` |

## 📡 SSE 原始格式

实际通过网络传输的 SSE 消息格式：

```
event: agent_message_chunk
data: {"content":{"type":"text","text":"正在分析您的请求..."},"index":0}

event: tool_call
data: {"tool_name":"read_file","tool_input":{"path":"src/main.rs"},"status":"started"}

event: tool_result
data: {"tool_name":"read_file","tool_output":"fn main() {...}","status":"success"}

event: end_turn
data: {"reason":"complete","final_message":"任务已完成"}
```

## 💻 客户端使用示例

### JavaScript / TypeScript

```typescript
// 1. 建立 SSE 连接
const eventSource = new EventSource('/agent/progress/session_123');

// 2. 监听特定事件类型
eventSource.addEventListener('agent_message_chunk', (event) => {
  const data = JSON.parse(event.data);
  console.log('AI 响应:', data.content.text);
  // 更新 UI 显示 AI 响应
});

eventSource.addEventListener('tool_call', (event) => {
  const data = JSON.parse(event.data);
  console.log(`工具调用: ${data.tool_name}`, data.tool_input);
  // 显示工具调用状态
});

eventSource.addEventListener('tool_result', (event) => {
  const data = JSON.parse(event.data);
  console.log(`工具结果: ${data.tool_name}`, data.tool_output);
  // 显示工具执行结果
});

eventSource.addEventListener('end_turn', (event) => {
  const data = JSON.parse(event.data);
  console.log('任务完成:', data.final_message);
  eventSource.close();  // 关闭连接
});

// 3. 错误处理
eventSource.onerror = (error) => {
  console.error('连接错误:', error);
  eventSource.close();
};

// 4. 监听所有消息（可选）
eventSource.onmessage = (event) => {
  console.log('收到消息:', event.data);
};
```

### Python

```python
import sseclient
import requests
import json

# 1. 建立 SSE 连接
response = requests.get(
    'http://localhost:8087/agent/progress/session_123',
    stream=True,
    headers={'Accept': 'text/event-stream'}
)

client = sseclient.SSEClient(response)

# 2. 处理事件
for event in client.events():
    if event.event == 'agent_message_chunk':
        data = json.loads(event.data)
        print(f"AI 响应: {data['content']['text']}")
    
    elif event.event == 'tool_call':
        data = json.loads(event.data)
        print(f"工具调用: {data['tool_name']}")
    
    elif event.event == 'end_turn':
        data = json.loads(event.data)
        print(f"任务完成: {data['final_message']}")
        break
```

### Rust

```rust
use eventsource_stream::Eventsource;
use futures_util::StreamExt;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let client = reqwest::Client::new();
    let mut stream = client
        .get("http://localhost:8087/agent/progress/session_123")
        .send()
        .await?
        .bytes_stream()
        .eventsource();

    while let Some(event) = stream.next().await {
        match event {
            Ok(event) => {
                match event.event.as_str() {
                    "agent_message_chunk" => {
                        let data: serde_json::Value = serde_json::from_str(&event.data)?;
                        println!("AI 响应: {}", data["content"]["text"]);
                    }
                    "tool_call" => {
                        let data: serde_json::Value = serde_json::from_str(&event.data)?;
                        println!("工具调用: {}", data["tool_name"]);
                    }
                    "end_turn" => {
                        println!("任务完成");
                        break;
                    }
                    _ => {}
                }
            }
            Err(e) => {
                eprintln!("错误: {}", e);
                break;
            }
        }
    }

    Ok(())
}
```

## 🎯 完整工作流程

```
1. 发起对话
   POST /chat
   ↓
   返回 { session_id: "session_123" }

2. 建立 SSE 连接
   GET /agent/progress/session_123
   ↓
   EventSource 连接建立

3. 接收进度事件
   event: agent_message_chunk
   data: {...}
   ↓
   event: tool_call
   data: {...}
   ↓
   event: tool_result
   data: {...}
   ↓
   event: end_turn
   data: {...}

4. 关闭连接
   eventSource.close()
```

## 🔧 在 Swagger UI 中测试

虽然 Swagger UI 本身不支持直接测试 SSE 流，但你可以：

1. 使用浏览器开发者工具的 Network 标签
2. 使用 `curl` 命令：
   ```bash
   curl -N http://localhost:8087/agent/progress/session_123
   ```
3. 使用专门的 SSE 测试工具

## 📚 相关文档

- [OpenAPI 规范](http://localhost:8087/api/docs)
- [ACP (Agent Client Protocol) 协议文档](../specs/)
- [gRPC 架构文档](./grpc-architecture.md)

## ❓ FAQ

### Q: 为什么在 OpenAPI 中看不到具体的 data 字段？

A: 因为 SSE 是流式响应，OpenAPI 无法像普通 JSON 响应那样定义结构。我们通过以下方式提供文档：

1. **ProgressEventDoc** schema：定义了事件的完整结构
2. **接口 description**：提供详细的事件格式说明和示例
3. **本文档**：提供完整的使用指南

### Q: 如何知道有哪些可能的 sub_type？

A: 查看：
1. Swagger UI 中的接口描述部分
2. `ProgressEventDoc` schema 的 `sub_type` 字段注释
3. 本文档的"常见子类型"表格

### Q: payload 的具体结构在哪里定义？

A: `payload` 是透传的 ACP (Agent Client Protocol) 消息，具体结构取决于 `sub_type`。每种 `sub_type` 的 payload 结构在接口文档的示例中都有说明。

### Q: 如何处理错误？

A: SSE 流可能会发送 `error` 事件，同时监听 `EventSource.onerror` 来处理连接错误：

```javascript
eventSource.addEventListener('error', (event) => {
  const data = JSON.parse(event.data);
  console.error('业务错误:', data.code, data.message);
});

eventSource.onerror = (error) => {
  console.error('连接错误:', error);
};
```

## 📝 总结

- ✅ **在 Swagger UI 中查看**: `ProgressEventDoc` schema 和接口描述
- ✅ **事件格式**: SSE 标准格式，event = sub_type, data = payload
- ✅ **客户端支持**: JavaScript, Python, Rust 等都有成熟的 SSE 客户端库
- ✅ **类型安全**: 可以根据 `ProgressEventDoc` 定义生成 TypeScript 类型

