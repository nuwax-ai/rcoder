# Instruction

## Project Alpha

https://github.com/duckdb/duckdb 这个是官方 duckdb 的github 仓库,我想使用duckdb的内存模式,用于存储数据,跟随主容器 crates/rcoder 模块,当容器重启,数据就全部重置了,用于记录 {user_id, project_id} 对应的容器信息,更新容器的最后使用时间等,当业务服务闲置,就自动销毁对应的容器,当前都是使用 Dashmap 来记录存储数据信息,我想用 duckdb 来替代 Dashmap. 比如对应 crates/rcoder/src/router.rs 里的 AppState结构体中的字段: "pub project_and_agent_map: DashMap<String, Arc<ProjectAndContainerInfo>>," ,还有"session_to_container_id","sessions" ,"session_to_container_id"的信息维护,都是使用 Dashmap 来维护,我想用 duckdb 来替代 Dashmap.

我希望通过内存数据库,通过设计良好的数据模型,来替代当前的 Dashmap,实现相同的功能.

按照这个想法，帮我生成详细的需求和设计文档，输出为中文。涉及到代码部分，不要写详细的具体实现，只需要写 trait 和 结构体 struct