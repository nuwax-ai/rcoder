# Instruction

## Project Alpha

@crates/rcoder/src/router.rs
我需要新增接口:
1. 获取当前容器数量,这个接口不需要入参 
2. 入参给 user_id, project_id , 启动容器(如果对应容器已经启动,则do nothing),不需要启动agent服务,方便我使用 noVNC 来远程虚拟桌面使用.

这2个新接口,返回结构要用 HttpResult 来包装, @crates/shared_types/src/model/http_result.rs

按照这个想法，帮我生成详细的需求和设计文档，输出为中文。涉及到代码部分，不要写详细的具体实现，只需要写 trait 和 结构体 struct