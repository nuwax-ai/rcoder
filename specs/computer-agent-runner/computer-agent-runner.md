# Instruction

## project alpha

我要做一个带有虚拟远程桌面的agentic ai，agent 可以在这个带有远程桌面的docker容器里，操作浏览器，搜索访问自己需要的网络资料，用户还可以通过 vnc 远程虚拟桌面，查看操作使用，最终agent在docker容器里挂载的目录下，按照用户的 prompt 的要求，完成复杂任务。

我现在初步想法点，大概如下： 

* agent 复用当前 `crates/agent_runner` 模块，使用的docker容器是： docker/rcoder-agent-runner/Dockerfile 通过这个构建出来的容器 ，通过 `ServiceType::ComputerAgentRunner` 来创建使用我们的新的docker 容器，进行使用。

* `crates/rcoder` 模块的 `crates/rcoder/src/router.rs` 的http router ，参考现有使用的agent_runner 模块的http接口("/agent"前缀的接口,"/chat")，我们统一增加新接口，前缀是 "/computer" 的http接口对外服务，内部还是调用 `crates/agent_runner` 模块在 `ServiceType::ComputerAgentRunner` docker 容器里运行。

1） "/computer/chat" 接口，入参需要增加字段 `user_id` 用户id，必填,根据 `user_id` 来检查有无对应的 docker容器，一个 `user_id`对应一个docker容器（一一对应，如果没有，则自动动态创建，和现在的 `ServiceType::RCoder`创建容器方式一样），然后动态创建的子容器，是根据入参 `user_id` ,`project_id` 来进行挂载。容器挂载路径： /app/computer-project-workspace/{user_id} 。
挂载目录示意： /app/computer-project-workspace/{user_id}/{project_id}

如果 `user_id`  对应的docker容器已经启动，容器里可以有多个agent服务运行（根据 `project_id` 启动对应的agent 服务，当前  `crates/agent_runner`  模块，是支持在一个docker容器里，按照 `project_id` 启动对应的agent 服务）

注： 我在本地测试用的docker compose配置文件（docker/docker-compose.yml），有挂载 `computer-project-workspace` 目录到容器里，你可以去看下这个配置文件


* `docker/rcoder-agent-runner/Dockerfile` docker镜像配置文件本身，有用 noVnc 提供vnc服务，用于连接虚拟远程桌面。 但用户访问是通过 `crates/rcoder` 模块 所在的主容器，来访问vnc服务的，不能直接让用户访问内部动态创建的子容器服务，我这里想法是，可以通过  pingora 来提供反向透明代理服务，按照我定义的路径规则，透明代理vnc服务。
我设想的vnc路径规则是： "/computer/desktop/{user_id}/{project_id}" ，这样 pingora 就知道要访问的是哪个子容器的vnc服务了，可以进行透明代理操作。

然后关于vnc服务，我还有些不清楚的点，如果以后vnc 的虚拟远程桌面，需要粘贴复制，是不是应该进一步封装成接口，做一些处理，而不是仅仅只使用 pingora 透明代理了？

* `ServiceType::ComputerAgentRunner` docker 容器里的agent，在当前的默认mcp配置下，增加mcp工具： Chrome DevTools MCP， 对应github仓库地址是： https://github.com/ChromeDevTools/chrome-devtools-mcp?tab=readme-ov-file

mcp参考json配置： 
```
{
  "mcpServers": {
    "chrome-devtools": {
      "command": "npx",
      "args": ["-y", "chrome-devtools-mcp@latest"]
    }
  }
}
```

`crates/agent_config/configs/default_agents.json` 这个是 `ServiceType::RCoder` 默认使用的agent配置，我们的新功能 `ServiceType::ComputerAgentRunner` 也可以使用这个配置吗？ 但需要额外增加 "Chrome DevTools MCP" 这个浏览器操作的mcp工具。 `ServiceType::ComputerAgentRunner`对应的docker容器里，已经有提前安装好的 chromium 浏览器，并设置开发里 9200 端口，用于开发调试使用。
* 现有的接口 "/agent/stop" 是停止对应的 project_id 对应的容器，但是新接口： "/computer/agent/stop"对应的逻辑，是根据 `user_id` ,`project_id`  停止对应的agent服务，就行了。

* 现有根据 agent是否闲置的规则需要有区分枚举`ServiceType`，之前因为一个  `project_id` 对应一个docker容器，现在`ServiceType::ComputerAgentRunner` 对应容器，是根据 `user_id` 对应一个docker容器， `user_id` 可能有多个  `project_id` 对应的agent 在运行，需要 `user_id`下的所有  `project_id` 对应的agent 都是闲置状态（复用现有闲置时间规则），才能销毁对应的 docker 容器。


* 目前`crates/rcoder` 通过 gRPC和 `crates/agent_runner` 模块通信，如果失败，不要回退成 http通信，`crates/agent_runner` 模块里的http接口，后面是要彻底删除的。`crates/agent_runner` 模块只用gRPC通信。


### vnc虚拟远程桌面，手动测试

开发结束后，在帮我开发一个html网页，放在 `fixtures`目录下，方便我打开网页，输入本地的ip和端口，来测试验证 vnc虚拟桌面。 


按照这个想法，帮我生成详细的需求和设计文档，输出为中文。涉及到代码部分，不要写详细的具体实现，只需要写 trait 和 结构体 struct
