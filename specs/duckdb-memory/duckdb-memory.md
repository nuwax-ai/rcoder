# Instruction

## Project Alpha

https://github.com/duckdb/duckdb 这个是官方 duckdb 的github 仓库,我想使用duckdb的内存模式,用于存储数据,跟随主容器 crates/rcoder 模块,当容器重启,数据就全部重置了,用于记录 {user_id, project_id} 对应的容器信息,更新容器的最后使用时间等,当业务服务闲置,就自动销毁对应的容器,当前都是使用 Dashmap 来记录存储数据信息,我想用 duckdb 来替代 Dashmap. 比如对应 crates/rcoder/src/router.rs 里的 AppState结构体中的字段: "pub project_and_agent_map: DashMap<String, Arc<ProjectAndContainerInfo>>," ,还有"session_to_container_id","sessions" ,"session_to_container_id"的信息维护,都是使用 Dashmap 来维护,我想用 duckdb 来替代 Dashmap.

我希望通过内存数据库,通过设计良好的数据模型,来替代当前的 Dashmap,实现相同的功能.

- @crates/shared_types/src/service_type.rs 根据 ServiceType 枚举,分2个业务场景,对应容器有区别
- 清理容器,核心是根据对应的agent是否闲置,定期清理容器,确保资源及时回收,所以会定期根据请求,或者agent是否在执行任务,刷新容器最后的时间,用于清理容器来提供依据. 这个是当前已有的业务逻辑
- duckdb 使用内存模式,不需要持久化数据,跟随主容器 rcoder 启动的时候,使用空数据状态的数据库
- duckdb 的数据库使用,数据不会太多,不需要限制时间范围,比如统计当前容器数量,统计所有的数据
- duckdb数据库,使用新的模块: crates/duckdb_manager, 封装好数据库的操作接口,以lib库的形式,给 crates/rcoder 模块使用,避免其他模块内部有业务无关的数据库操作
- 数据库库表之间,禁止外键的使用
- 公共的结构体,可以放在 crates/shared_types 模块里定义

按照这个想法，帮我生成详细的需求和设计文档，输出为中文。涉及到代码部分，不要写详细的具体实现，只需要写 trait 和 结构体 struct