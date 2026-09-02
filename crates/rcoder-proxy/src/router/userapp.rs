//! userApp 域路由注册：统一代理入口（从 router.rs 拆出；pattern 原样、注册顺序不变）。
//!
//! 全族统一形态 `/api/v1/userapp/proxy/{tool}/{stage}/{user_id}/{app_id}/{*path}`
//! （tool→stage→user_id→app_id 段序；与 REST 面 `/api/v1/userapp/...` 同根，
//! `proxy` 段标识 Pingora 代理面）：
//! - `app` = pingap 应用流量（容器内统一入口 9080；prod 走 app_backends 注册表
//!   + 单端口回退，dev 走注册表定位开发容器）；user_id 不参与解析，未来归属鉴权锚点；
//! - `ttyd`/`dbx`/`vnc`/`audio`/`ime` = 容器内工具服务（定位语义见 route_type.rs；
//!   vnc/audio/ime 仅 dev——prod 无对应服务）。
//! stage 段 dev/prod 全族统一（切环境只改一段）。

use matchit::Router;

use super::insert_route;
use crate::route_type::RouteType;

/// userApp 域全部路由（共 18 条；兜底与通配成对注册——matchit 的 `{*path}`
/// 至少 1 字符）。
pub(super) fn insert_userapp_routes(
    router: &mut Router<RouteType>,
) -> Result<(), crate::ProxyError> {
    // 应用流量族（tool=app）：/api/v1/userapp/proxy/app/{stage}/{user_id}/{app_id}
    for (prefix, route) in [
        (
            "/api/v1/userapp/proxy/app/prod/{user_id}/{app_id}/{*path}",
            RouteType::ProdAppProxy,
        ),
        (
            "/api/v1/userapp/proxy/app/prod/{user_id}/{app_id}",
            RouteType::ProdAppProxy,
        ),
        (
            "/api/v1/userapp/proxy/app/dev/{user_id}/{app_id}/{*path}",
            RouteType::DevAppProxy,
        ),
        (
            "/api/v1/userapp/proxy/app/dev/{user_id}/{app_id}",
            RouteType::DevAppProxy,
        ),
    ] {
        insert_route(router, prefix, route)?;
    }

    // 开发域终端/桌面代理族（与 computer 族对称；{user_id}/{app_id} 双段——
    // user_id 是懒创建显式 owner 档）。
    for (prefix, route) in [
        (
            "/api/v1/userapp/proxy/ttyd/dev/{user_id}/{app_id}/{*path}",
            RouteType::DevTtydProxy,
        ),
        (
            "/api/v1/userapp/proxy/ttyd/dev/{user_id}/{app_id}",
            RouteType::DevTtydProxy,
        ),
        (
            "/api/v1/userapp/proxy/vnc/dev/{user_id}/{app_id}/{*path}",
            RouteType::DevVncProxy,
        ),
        (
            "/api/v1/userapp/proxy/vnc/dev/{user_id}/{app_id}",
            RouteType::DevVncProxy,
        ),
        (
            "/api/v1/userapp/proxy/audio/dev/{user_id}/{app_id}/{*path}",
            RouteType::DevAudioProxy,
        ),
        (
            "/api/v1/userapp/proxy/audio/dev/{user_id}/{app_id}",
            RouteType::DevAudioProxy,
        ),
        (
            "/api/v1/userapp/proxy/ime/dev/{user_id}/{app_id}/{*path}",
            RouteType::DevImeProxy,
        ),
        (
            "/api/v1/userapp/proxy/ime/dev/{user_id}/{app_id}",
            RouteType::DevImeProxy,
        ),
    ] {
        insert_route(router, prefix, route)?;
    }

    // 生产域工具族（运行容器，部署后的生产环境；原 `/runtime` 静态段已随
    // 路径风格统一退役，pgweb 已全面退役）。
    for (prefix, route) in [
        (
            "/api/v1/userapp/proxy/ttyd/prod/{user_id}/{app_id}/{*path}",
            RouteType::RuntimeTtydProxy,
        ),
        (
            "/api/v1/userapp/proxy/ttyd/prod/{user_id}/{app_id}",
            RouteType::RuntimeTtydProxy,
        ),
    ] {
        insert_route(router, prefix, route)?;
    }

    // DBX 数据库 Web GUI 两阶段（dev=开发容器 / prod=运行容器，均直连 :4224）。
    for (prefix, route) in [
        (
            "/api/v1/userapp/proxy/dbx/dev/{user_id}/{app_id}/{*path}",
            RouteType::DevDbxProxy,
        ),
        (
            "/api/v1/userapp/proxy/dbx/dev/{user_id}/{app_id}",
            RouteType::DevDbxProxy,
        ),
        (
            "/api/v1/userapp/proxy/dbx/prod/{user_id}/{app_id}/{*path}",
            RouteType::ProdDbxProxy,
        ),
        (
            "/api/v1/userapp/proxy/dbx/prod/{user_id}/{app_id}",
            RouteType::ProdDbxProxy,
        ),
    ] {
        insert_route(router, prefix, route)?;
    }

    Ok(())
}
