# Introduction

## Project Alpha

我使用的ACP协议,来使用的agent ,目前agent使用的是"claude-code-acp","claude-code-acp"是zed公司
对 claude code 工具的ACP协议封装,我去看官方 https://github.com/zed-industries/claude-code-acp 仓库里有"--resume" 的相关逻辑, 使用的anthropic的ts sdk 来对接的api(ts版本的官方文档是: https://platform.claude.com/docs/en/agent-sdk/typescript ). 

rust版本的acp的官方源码(https://github.com/agentclientprotocol/rust-sdk),我下载到本地当前项目下的: tmp/rust-sdk 里了;

crates/agent_abstraction/src/compat/claude_code_launcher.rs 是创建agent session的逻辑,是我启动 "claude-code-acp"的部分代码.

我现在通过acp协议,和agent对话,如果agent停止后,下一次,我想通过 resume 参数,来继续之前的上下文对话记录,继续和agent对话,但当前我不知道怎么使用 resume 参数,来继续之前的上下文记录,来继续和agent对话,我需要先了解resume参数的使用方法,然后才能继续和agent对话。 

注: 本地参考的源码,可以看项目下的: tmp 目录下,只能用于查阅参考,禁止依赖引入使用
- tmp/claude-code-acp 官方的ts写的acp协议,封装的claude code ,支持acp协议调用agent
- tmp/rust-sdk 官方ACP协议的rust版本的sdk
