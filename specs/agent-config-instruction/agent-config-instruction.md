# Instructions

## project alpha 需求和设计文档

### “/chat”接口增加可选配置入参,入参如下: 
1. `system_prompt` 系统提示词,可选
2. `user_prompt` 用户提示词,可选
3. `agent_config` agent配置文件,包含agent使用的mcp配置,自定义agent配置文件,可选
参考默认配置json文件: @crates/agent_config/configs/default_agents.json ，对应的结构体 `AgentServersConfig`。
如果不传,使用默认的agent,传了就使用给的agent配置来启动agent服务进行使用。

细节补充说明：
* 如果 `system_prompt`,`user_prompt`都传值了，非空字符串，但 `agent_config` 里对应的同名字段也有配置（同名字段下有`template`字段，定义的是提示词模板），认入参 `system_prompt`,`user_prompt`的为准，覆盖`agent_config` 里的同名配置
* `user_prompt` 提示词模板文本内容， 可以注入的变量 `{user_prompt}`，用户"{}"花括号匹配，把”/chat”入参字段：`prompt`,注入进去，替换变量 `{user_prompt}`


按照这个想法，帮我生成详细的需求和设计文档，放在 @specs/agent-config-instruction-spec.md 文件中，输出为中文。

## implementation plan

按照 @specs/agent-config-instruction-spec.md 中的需求和设计文档，生成一个详细的实现计划，放在 @specs/02-agent-config-instruction-spec-plan.md 中，输出中文
