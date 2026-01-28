# Instractions

## Project Alpha

我现在使用的官方版本: agent-client-protocol = { version = "0.9.3", features = ["unstable"] } , 目前官方acp协议有个问题,agent端,只能在 LocalSet 下运行,因为没实现 Send trait ,我现在发现一个基于官方acp协议重新封装的acp库: https://github.com/symposium-dev/symposium-acp ,对acp协议封装的更好,我想把acp协议从官方的,切换到 symposium-acp  来使用,对应最新版本是: sacp = "10.1.0" , 需要更新 crates/agent_runner , crates/agent_abstraction 等模块