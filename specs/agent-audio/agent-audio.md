# Instructions

## Project Alpha
我使用的docker容器的配置文件: docker/rcoder-agent-runner/Dockerfile ,现在的vnc远程桌面,播放视频,没有声音,我新增了一个音频流传输方案,还有输入法我想使用客户端的输入法来输入,比如客户端使用虚拟桌面,客户端是支持中文输入法,在客户端输入中文,可以透传到novnc的虚拟桌面里.

我之前是通过新的端口,来提供对应的服务,但这个是在子容器里的,需要通过pingora 代理到 rcoder 主容器里,不对客户端暴露子容器的ip和端口信息,在rcoder模块里,通过pingora来提供服务.

我的pingora 代理模块,当前已有透明代理服务看: crates/rcoder-proxy/src/router.rs ,需要新增音频通道,还有输入法的通道,来进行透明代理使用吧.

注: 如何路由代理到子容器,可以根据 `{user_id}` 和 `{project_id}` 来区分不同的容器,这样就可以确认路由代理到哪个子容器了,现有的业务,是有实现的,可以参考服用
