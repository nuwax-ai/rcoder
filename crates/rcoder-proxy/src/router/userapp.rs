//! userApp 域路由注册：应用流量族 + dev/prod 工具族 + dbx（从 router.rs 拆出；
//! pattern 原样、注册顺序不变）。

use matchit::Router;

use super::insert_route;
use crate::route_type::RouteType;

/// userApp 域全部路由（共 20 条；兜底与通配成对注册——matchit 的 `{*path}`
/// 至少 1 字符）。
pub(super) fn insert_userapp_routes(
    router: &mut Router<RouteType>,
) -> Result<(), crate::ProxyError> {
    // userApp 应用流量族（免端口，业务域前缀 userapp——/proxy 为跨业务共享命名空间）：
    // /proxy/userapp/{stage}/{user_id}/{app_id}/{path} -> 内部固定拨 pingap 统一入口
    // APP_ENTRY_PORT(9080)（prod 走 app_backends 注册表 + 单端口回退；dev 走注册表
    // 定位开发容器）；user_id 不参与解析, 未来归属鉴权锚点。
    // stage 段 dev/prod 与 /userapp/{dev,prod} 工具族语义统一——切环境只改一段。
    for (prefix, route) in [
        (
            "/proxy/userapp/prod/{user_id}/{app_id}/{*path}",
            RouteType::ProdAppProxy,
        ),
        (
            "/proxy/userapp/prod/{user_id}/{app_id}",
            RouteType::ProdAppProxy,
        ),
        (
            "/proxy/userapp/dev/{user_id}/{app_id}/{*path}",
            RouteType::DevAppProxy,
        ),
        (
            "/proxy/userapp/dev/{user_id}/{app_id}",
            RouteType::DevAppProxy,
        ),
    ] {
        insert_route(router, prefix, route)?;
    }

    // userApp 开发域终端/桌面代理族（与 computer 族对称；{user_id}/{app_id} 双段——
    // user_id 是懒创建显式 owner 档）。
    for (prefix, route) in [
        (
            "/userapp/dev/ttyd/{user_id}/{app_id}/{*path}",
            RouteType::DevTtydProxy,
        ),
        (
            "/userapp/dev/ttyd/{user_id}/{app_id}",
            RouteType::DevTtydProxy,
        ),
        (
            "/userapp/dev/vnc/{user_id}/{app_id}/{*path}",
            RouteType::DevVncProxy,
        ),
        (
            "/userapp/dev/vnc/{user_id}/{app_id}",
            RouteType::DevVncProxy,
        ),
        (
            "/userapp/dev/audio/{user_id}/{app_id}/{*path}",
            RouteType::DevAudioProxy,
        ),
        (
            "/userapp/dev/audio/{user_id}/{app_id}",
            RouteType::DevAudioProxy,
        ),
        (
            "/userapp/dev/ime/{user_id}/{app_id}/{*path}",
            RouteType::DevImeProxy,
        ),
        (
            "/userapp/dev/ime/{user_id}/{app_id}",
            RouteType::DevImeProxy,
        ),
    ] {
        insert_route(router, prefix, route)?;
    }

    // userApp 生产域工具族（运行容器，部署后的生产环境）。
    // stage 段 prod 与开发域工具族对称（{user_id}/{app_id} 双段同构；
    // 原 `/runtime` 静态段随路径风格统一退役）。
    for (prefix, route) in [
        (
            "/userapp/prod/ttyd/{user_id}/{app_id}/{*path}",
            RouteType::RuntimeTtydProxy,
        ),
        (
            "/userapp/prod/ttyd/{user_id}/{app_id}",
            RouteType::RuntimeTtydProxy,
        ),
    ] {
        insert_route(router, prefix, route)?;
    }

    // DBX 数据库 Web GUI 两阶段（dev=开发容器 / prod=运行容器，均直连 :4224）——
    // 归入工具族 stage 前缀风格（{user_id}/{app_id} 双段与 ttyd/vnc/audio/ime 同一形态）。
    for (prefix, route) in [
        (
            "/userapp/dev/dbx/{user_id}/{app_id}/{*path}",
            RouteType::DevDbxProxy,
        ),
        (
            "/userapp/dev/dbx/{user_id}/{app_id}",
            RouteType::DevDbxProxy,
        ),
        (
            "/userapp/prod/dbx/{user_id}/{app_id}/{*path}",
            RouteType::ProdDbxProxy,
        ),
        (
            "/userapp/prod/dbx/{user_id}/{app_id}",
            RouteType::ProdDbxProxy,
        ),
    ] {
        insert_route(router, prefix, route)?;
    }

    Ok(())
}
