1. Always Response in 中文
2. 禁止使用模拟响应逻辑,比如为了简化逻辑,就直接使用模拟结果,是禁止的.
3. 禁止写 unsafe 代码
4. AgentSideConnection ,ClientSideConnection 没有实现 Send trait, 必须在 LocalSt and spawn_local 中使用