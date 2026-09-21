//! gpui-kit 应用装配（复刻 gpui-kit examples/ai_recipes 的 bootstrap 模式）：
//! `gpui_kit::init` 先行 → open_window 包 `Root` → 状态视图含三层 overlay
//! layer 渲染。

use gpui_kit::component::Root;
use gpui_kit::{
    AppContext as _, Context, IntoElement, ParentElement as _, Render, SharedString, Styled as _,
    Window, WindowOptions, div,
};

use crate::engine;

/// 窗口标题栏下展示的主端口（与 rcoder config 默认对齐；env RCODER_PORT
/// 在服务侧生效，探活端口取同源 env）。
fn service_port() -> u16 {
    std::env::var("RCODER_PORT")
        .ok()
        .and_then(|value| value.trim().parse::<u16>().ok())
        .unwrap_or(8086)
}

pub fn run() {
    // 完整 rcoder 服务先起（tokio runtime 线程；tracing/遥测由服务组合初始化，
    // gpui 不初始化 tracing）。
    let _service = engine::spawn_rcoder_service();

    gpui_kit::application()
        .with_assets(gpui_kit::assets::Assets)
        .run(|cx| {
            gpui_kit::init(cx);
            cx.spawn(async move |cx| {
                cx.open_window(WindowOptions::default(), |window, cx| {
                    let view = cx.new(|cx| StatusView::new(window, cx));
                    cx.new(|cx| Root::new(view, window, cx))
                })
                .expect("failed to open window");
            })
            .detach();
        });
}

/// 骨架状态视图：引擎状态卡（运行时形态/健康探活/端口/~/.rcoder 提示）。
struct StatusView {
    runtime_label: SharedString,
    health: SharedString,
    port: u16,
    _poll_task: gpui_kit::Task<()>,
}

impl StatusView {
    fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let runtime_label: SharedString = if cfg!(feature = "deploy-host") {
            "deploy-host 宿主机形态"
        } else {
            "容器编译形态（宿主机运行需 --features deploy-host）"
        }
        .into();
        let port = service_port();
        let poll_task = cx.spawn_in(window, async move |this, cx| {
            loop {
                // 同步 TCP 探活放 background executor（避免阻塞 UI 线程）
                let health = cx
                    .background_executor()
                    .spawn(async move { engine::tcp_health_probe(port) })
                    .await;
                let text = match health {
                    Ok(()) => format!("rcoder 服务健康（127.0.0.1:{port}/health）"),
                    Err(reason) => format!("服务未就绪：{reason}"),
                };
                let _ = this.update(cx, |view, cx| {
                    view.health = text.into();
                    cx.notify();
                });
                cx.background_executor()
                    .timer(std::time::Duration::from_secs(2))
                    .await;
            }
        });
        Self {
            runtime_label,
            health: "启动中…".into(),
            port,
            _poll_task: poll_task,
        }
    }
}

impl Render for StatusView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        use gpui_kit::component::ActiveTheme as _;
        div()
            .flex()
            .flex_col()
            .size_full()
            .p_6()
            .gap_4()
            .bg(cx.theme().background)
            .text_color(cx.theme().foreground)
            .child("RCoder 桌面客户端（骨架）")
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .child(format!("运行时：{}", self.runtime_label))
                    .child(format!("状态：{}", self.health))
                    .child(format!(
                        "主端口：{}（UI 数据面进程内直读为后续项）",
                        self.port
                    ))
                    .child("目录约定：~/.rcoder（deploy-host 形态）"),
            )
            .children(Root::render_dialog_layer(_window, cx))
            .children(Root::render_sheet_layer(_window, cx))
            .children(Root::render_notification_layer(_window, cx))
    }
}
