//! TUI 应用核心
//!
//! App 结构体持有所有状态，运行事件循环。
//!
//! ## Prompt 生命周期管理
//!
//! 核心挑战：`send_prompt_and_wait` 在 spawned task 中运行，其完成/失败
//! 与事件循环是异步的。如果 prompt 超时后用户立即提交新 prompt，
//! 旧 prompt 的 stale `PromptEnded` 事件可能污染新 prompt 的状态。
//!
//! 解决方案：**prompt generation 计数器**
//! - 每次提交 prompt 时递增 `prompt_generation`
//! - Spawned task 捕获提交时的 generation 值
//! - `PromptEnded` 和 `ResetWaiting` 事件携带 generation
//! - 事件循环只在 generation 匹配时处理这些事件
//!
//! 安全性保证：用户只能在 `waiting == false` 时提交新 prompt，
//! 而 `waiting` 只在 `PromptEnded` 处理后才变为 false。
//! 因此不存在同一时刻有两个 prompt 并发的情况。

use std::future::Future;
use std::pin::pin;
use std::sync::Arc;
use std::task::Context;
use std::time::Duration;

use agent_abstraction::AcpClient;
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyModifiers};
use futures::task::noop_waker;
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;

use crate::registry::SimpleSessionRegistry;
use crate::tui::chat::ChatState;
use crate::tui::composer::Composer;
use crate::tui::event::{AppEvent, PermissionOption, PromptGeneration};
use crate::tui::notifier::TuiSessionNotifier;
use crate::tui::terminal::{self, TuiTerminal};
use crate::tui::ui;

/// AcpClient 类型别名（TUI 模式专用）
pub type Client = AcpClient<TuiSessionNotifier, SimpleSessionRegistry>;

/// 权限弹窗状态
pub struct PermissionOverlay {
    pub tool_name: String,
    pub options: Vec<PermissionOption>,
    pub selected_index: usize,
    pub response_tx: Option<oneshot::Sender<Option<String>>>,
}

/// 事件处理结果
enum EventResult {
    Continue,
    Exit(i32),
}

/// TUI 应用
pub struct App {
    /// 聊天状态（消息历史 + 流式响应）
    pub chat: ChatState,
    /// 输入框状态
    pub composer: Composer,
    /// 应用事件接收端
    event_rx: mpsc::UnboundedReceiver<AppEvent>,
    /// 应用事件发送端（clone 给 spawned tasks）
    event_tx: mpsc::UnboundedSender<AppEvent>,
    /// Agent 客户端
    client: Arc<Client>,
    /// 终端实例
    terminal: TuiTerminal,
    /// 是否启用 Markdown 渲染
    pub use_markdown: bool,
    /// 权限弹窗覆盖层
    pub permission_overlay: Option<PermissionOverlay>,
    /// 项目 ID（状态栏显示）
    pub project_id: String,
    /// 会话 ID（状态栏显示）
    pub session_id: String,

    // ── Prompt 生命周期 ──

    /// Prompt 代际计数器
    ///
    /// 每次调用 `handle_submit` 时递增。Spawned task 捕获当前值，
    /// 用于在 `PromptEnded` / `ResetWaiting` 事件中区分 stale 事件。
    prompt_generation: PromptGeneration,

    /// 当前 submit task 的 JoinHandle
    ///
    /// 用于在 Ctrl+C 取消时 abort 旧 task，防止 stale 事件。
    submit_task: Option<JoinHandle<()>>,

    /// 等待 Agent 响应期间用户输入的排队 prompt
    ///
    /// 用户在 `waiting == true` 时按 Enter，文本存入此字段而非立即提交。
    /// `PromptEnded` 处理后自动调用 `handle_submit` 提交排队内容。
    /// 取消当前 prompt 时清空队列。
    pending_prompt: Option<String>,

    /// 静默模式：抑制 Diagnostics 事件（进程生命周期信息等）
    quiet: bool,
}

impl App {
    pub fn new(
        client: Arc<Client>,
        event_tx: mpsc::UnboundedSender<AppEvent>,
        event_rx: mpsc::UnboundedReceiver<AppEvent>,
        terminal: TuiTerminal,
        use_markdown: bool,
        _verbose: u8,
        quiet: bool,
    ) -> Self {
        let project_id = client.project_id().to_string();
        let session_id = client.session_id().to_string();
        Self {
            chat: ChatState::new(),
            composer: Composer::new(),
            event_rx,
            event_tx,
            client,
            terminal,
            use_markdown,
            permission_overlay: None,
            project_id,
            session_id,
            prompt_generation: 0,
            submit_task: None,
            pending_prompt: None,
            quiet,
        }
    }

    /// 运行 TUI 应用主循环
    pub async fn run(mut self) -> i32 {
        let exit_code = self.run_inner().await;

        // 恢复终端
        let _ = terminal::restore();

        // Abort 任何残留的 submit task
        if let Some(handle) = self.submit_task.take() {
            handle.abort();
        }

        // 停止 Agent
        self.stop_agent().await;

        exit_code
    }

    /// 停止 Agent 进程
    ///
    /// `Arc::try_unwrap` 需要所有 clone 都已释放才能成功。
    /// Spawned tasks（send_prompt_and_wait / cancel）持有 Arc clone，
    /// 需要先等待它们完成。
    async fn stop_agent(self) {
        // 让 spawned tasks 有机会完成（cancel 操作通常很快）
        tokio::task::yield_now().await;
        tokio::time::sleep(Duration::from_millis(100)).await;

        let arc = self.client;
        match Arc::try_unwrap(arc) {
            Ok(client) => {
                if let Err(e) = client.stop().await {
                    eprintln!("Agent 停止时出错: {}", e);
                }
            }
            Err(arc) => {
                // 再等一次（send_prompt_and_wait 可能需要 cancel 后才能返回）
                tokio::time::sleep(Duration::from_millis(500)).await;
                match Arc::try_unwrap(arc) {
                    Ok(client) => {
                        if let Err(e) = client.stop().await {
                            eprintln!("Agent 停止时出错: {}", e);
                        }
                    }
                    Err(_) => {
                        // Spawned tasks 仍持有 Arc（如 agent 无响应导致 cancel 也卡住）。
                        // Agent 进程会在所有 Arc 引用释放、lifecycle guard drop 后自动清理。
                        eprintln!("警告: 后台任务未完成，Agent 将延迟停止");
                    }
                }
            }
        }
    }

    /// 内部事件循环
    ///
    /// 使用 `tokio::select!` 将事件接收与重绘定时器解耦：
    /// - `biased` 优先 drain 所有 pending 事件（含 `try_recv` 批量合并）
    /// - 33ms interval（~30fps）触发重绘，仅当 `dirty` 时实际绘制
    ///
    /// 好处：高频 `AgentText`（每个 token 一次）不会逐次全量重绘，
    /// 而是合并到同一帧渲染，消除闪烁、降低 CPU 占用。
    async fn run_inner(&mut self) -> i32 {
        self.spawn_event_poller();

        let mut redraw_interval = tokio::time::interval(Duration::from_millis(33));
        redraw_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let mut dirty = true; // 首帧强制绘制

        loop {
            tokio::select! {
                biased;

                // ── 事件处理（非阻塞 drain）──
                result = self.event_rx.recv() => {
                    match result {
                        Some(evt) => {
                            match self.handle_event(evt) {
                                EventResult::Continue => dirty = true,
                                EventResult::Exit(code) => return code,
                            }
                            // 批量排空所有 pending 事件，合并同一 tick 内的多次更新
                            while let Ok(evt) = self.event_rx.try_recv() {
                                match self.handle_event(evt) {
                                    EventResult::Continue => dirty = true,
                                    EventResult::Exit(code) => return code,
                                }
                            }
                        }
                        None => return 0, // channel 关闭（所有 sender 已 drop）
                    }
                }

                // ── 定时重绘（~30fps）──
                _ = redraw_interval.tick() => {
                    if dirty {
                        self.redraw();
                        dirty = false;
                    }
                }
            }
        }
    }

    /// 重绘整个 TUI 界面
    ///
    /// 解构 `App` 字段以分离借用（`terminal` 需 `&mut`，其余需 `&`）。
    fn redraw(&mut self) {
        let App {
            ref chat,
            ref composer,
            ref permission_overlay,
            ref pending_prompt,
            use_markdown,
            ref project_id,
            ref session_id,
            ..
        } = *self;
        if let Err(e) = self.terminal.draw(|frame| {
            ui::draw(
                frame,
                chat,
                composer,
                pending_prompt.as_deref(),
                permission_overlay.as_ref(),
                use_markdown,
                project_id,
                session_id,
            )
        }) {
            eprintln!("渲染失败: {}", e);
        }
    }

    /// 处理单个应用事件
    fn handle_event(&mut self, evt: AppEvent) -> EventResult {
        match evt {
            // ── 退出 ──
            AppEvent::Exit => return EventResult::Exit(0),

            // ── 键盘事件 ──
            AppEvent::Key(key) => {
                if self.permission_overlay.is_some() {
                    self.handle_permission_key(key);
                } else if let Some(exit_code) = self.handle_key(key) {
                    return EventResult::Exit(exit_code);
                }
            }

            // ── 终端事件 ──
            AppEvent::Resize => {} // 重绘时自动处理
            AppEvent::Paste(text) => self.composer.insert_str(&text),

            // ── Agent 事件 ──
            AppEvent::AgentText(text) => {
                self.chat.push_agent_text(&text);
            }
            AppEvent::AgentThought(text) => {
                tracing::debug!("thought: {}", text);
            }
            AppEvent::ToolCall { title, status } => {
                self.chat.push_tool_call(&title, &status);
            }
            AppEvent::PromptStarted { .. } => {
                // waiting 已在 handle_submit 中设置，此处为冗余保护
                self.chat.waiting = true;
            }

            // PromptEnded 来自 notifier（所有 prompt 共用），
            // 需要检查是否是当前 prompt 的事件。
            //
            // Guard: `waiting == true` 时处理，`false` 时视为 stale 忽略。
            //
            // 原理：notifier 总是先发送 PromptEnded 再调用 signal_completion()，
            // 因此正常情况下 PromptEnded 在 FIFO 通道中先于 spawned task 的
            // ResetWaiting 到达。当 `waiting == false` 时，说明 ResetWaiting
            // 已先行处理（prompt 超时/失败），此时的 PromptEnded 是 stale 事件。
            //
            // 已知残留竞态：Ctrl+L 后立即提交新 prompt，旧 PromptEnded 可能在
            // 新 prompt 的 AgentText 之后到达（跨 sender 无 FIFO 保证）。此场景
            // 极为罕见（需 ~33ms 内完成 Ctrl+L → 输入 → Enter），且 current_response
            // 通常为空，commit_response 的 is_empty() 守卫阻止了无效提交。
            AppEvent::PromptEnded { error, .. } if self.chat.waiting => {
                self.chat.commit_response(self.use_markdown);
                if let Some(err) = error {
                    self.chat
                        .push_system_message(&format!("Error: {}", err));
                }
                // 自动提交排队 prompt（如有）
                if let Some(pending) = self.pending_prompt.take() {
                    self.handle_submit(pending);
                }
            }

            // Stale PromptEnded（waiting 已被 ResetWaiting 或 Ctrl+L 清除）
            AppEvent::PromptEnded { .. } => {
                tracing::debug!("忽略 stale PromptEnded（waiting == false）");
            }

            // ── 权限请求 ──
            AppEvent::PermissionRequest {
                tool_name,
                options,
                response_tx,
                ..
            } => {
                self.permission_overlay = Some(PermissionOverlay {
                    tool_name,
                    options,
                    selected_index: 0,
                    response_tx: Some(response_tx),
                });
            }

            // ── 诊断信息 ──
            AppEvent::Diagnostics(msg) => {
                if !self.quiet {
                    self.chat.push_system_message(&msg);
                }
            }

            // ── 带 generation 的 ResetWaiting ──
            // 仅当 generation 匹配当前 prompt 时才重置 waiting 状态
            AppEvent::ResetWaiting(event_gen) => {
                if event_gen == self.prompt_generation {
                    self.chat.waiting = false;
                }
                // 否则是 stale 事件（来自已超时的旧 prompt），安全忽略
            }
        }
        EventResult::Continue
    }

    /// 启动终端事件轮询后台任务
    ///
    /// 使用 crossterm 的 poll+read 在 spawn_blocking 中运行，
    /// 将键盘/粘贴/resize 事件通过 channel 转发到事件循环。
    fn spawn_event_poller(&self) {
        let tx = self.event_tx.clone();
        tokio::task::spawn_blocking(move || {
            loop {
                match event::poll(Duration::from_millis(50)) {
                    Ok(true) => {
                        match event::read() {
                            Ok(Event::Key(key)) => {
                                if tx.send(AppEvent::Key(key)).is_err() {
                                    break;
                                }
                            }
                            Ok(Event::Resize(_, _)) => {
                                if tx.send(AppEvent::Resize).is_err() {
                                    break;
                                }
                            }
                            Ok(Event::Paste(text)) => {
                                if tx.send(AppEvent::Paste(text)).is_err() {
                                    break;
                                }
                            }
                            Ok(_) => {} // 忽略其他事件（mouse 等）
                            Err(_) => break,
                        }
                    }
                    Ok(false) => {} // 超时，继续轮询
                    Err(_) => break,
                }
            }
            // Poller 退出（通常是终端错误）→ 发送 Exit 防止应用挂死
            let _ = tx.send(AppEvent::Exit);
        });
    }

    /// 处理键盘事件（主界面）
    ///
    /// 返回 Some(exit_code) 表示需要退出
    fn handle_key(&mut self, key: KeyEvent) -> Option<i32> {
        match key.code {
            // Alt+Enter / Shift+Enter: 在当前光标处插入换行（多行输入）
            KeyCode::Enter
                if key.modifiers.contains(KeyModifiers::ALT)
                    || key.modifiers.contains(KeyModifiers::SHIFT) =>
            {
                self.composer.insert_newline();
            }

            // Ctrl+J: 插入换行（终端对 Alt+Enter 支持不一致时的备选）
            KeyCode::Char('j') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.composer.insert_newline();
            }

            // Enter: 提交 prompt（空闲时）或排队（Agent 运行期间）
            KeyCode::Enter if !self.chat.waiting && self.composer.has_content() => {
                let text = self.composer.submit();
                self.handle_submit(text);
            }

            // Enter during waiting: 排队 prompt，Agent 完成后自动提交
            KeyCode::Enter if self.chat.waiting && self.composer.has_content() => {
                let text = self.composer.submit();
                self.pending_prompt = Some(text);
            }

            // Ctrl+C: 取消当前 prompt 或退出
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                if self.chat.waiting {
                    self.handle_cancel();
                } else {
                    return Some(130);
                }
            }

            // Esc: 退出
            KeyCode::Esc if !self.chat.waiting => {
                return Some(0);
            }

            // Ctrl+L: 清屏（取消当前 prompt + 清空聊天历史）
            KeyCode::Char('l') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                if self.chat.waiting {
                    self.handle_cancel();
                }
                self.chat.messages.clear();
                self.chat.current_response.clear();
                self.chat.scroll_offset = 0;
                self.chat.auto_scroll = true;
                self.chat.waiting = false;
                // 递增 generation 使旧 task 的 stale 事件失效
                self.prompt_generation += 1;
            }

            // Backspace
            KeyCode::Backspace => {
                self.composer.backspace();
            }

            // Delete
            KeyCode::Delete => {
                self.composer.delete();
            }

            // 方向键: 光标移动 / 滚动
            KeyCode::Left => self.composer.move_left(),
            KeyCode::Right => self.composer.move_right(),
            // Up/Down: 多行 composer 时在行间移动，边界处穿透到聊天滚动
            KeyCode::Up if !self.composer.move_up() => {
                self.chat.scroll_up(3);
            }
            KeyCode::Down if !self.composer.move_down() => {
                self.chat.scroll_down(3);
            }
            KeyCode::Up => {}   // 在 composer 内成功移动，无需滚动
            KeyCode::Down => {} // 在 composer 内成功移动，无需滚动

            // Home/End: 当前行的行首/行尾
            KeyCode::Home => self.composer.move_home(),
            KeyCode::End => self.composer.move_end(),

            // PgUp/PgDn
            KeyCode::PageUp => self.chat.scroll_up(20),
            KeyCode::PageDown => self.chat.scroll_down(20),

            // q: 空闲时退出
            KeyCode::Char('q') if !self.chat.waiting && self.composer.is_empty() => {
                return Some(0);
            }

            // 普通字符输入
            KeyCode::Char(c) => {
                self.composer.insert_char(c);
            }

            _ => {}
        }
        None
    }

    /// 处理权限弹窗中的键盘事件
    fn handle_permission_key(&mut self, key: KeyEvent) {
        let overlay = match self.permission_overlay.as_mut() {
            Some(o) => o,
            None => return,
        };

        match key.code {
            // Esc: 取消
            KeyCode::Esc => {
                if let Some(overlay) = self.permission_overlay.take()
                    && let Some(tx) = overlay.response_tx
                {
                    let _ = tx.send(None);
                }
            }

            // Enter: 确认选择
            KeyCode::Enter => {
                if let Some(mut overlay) = self.permission_overlay.take() {
                    let option_id =
                        overlay.options.get(overlay.selected_index).map(|o| o.id.clone());
                    if let Some(tx) = overlay.response_tx.take() {
                        let _ = tx.send(option_id);
                    }
                }
            }

            // 数字键快速选择
            KeyCode::Char(c) if c.is_ascii_digit() => {
                if let Some(idx) = c.to_digit(10) {
                    let idx = idx as usize;
                    if idx >= 1 && idx <= overlay.options.len() {
                        let option_id = overlay.options[idx - 1].id.clone();
                        if let Some(mut overlay) = self.permission_overlay.take()
                            && let Some(tx) = overlay.response_tx.take()
                        {
                            let _ = tx.send(Some(option_id));
                        }
                    }
                }
            }

            // j/Down: 向下移动选择
            KeyCode::Char('j') | KeyCode::Down
                if overlay.selected_index + 1 < overlay.options.len() =>
            {
                overlay.selected_index += 1;
            }

            // k/Up: 向上移动选择
            KeyCode::Char('k') | KeyCode::Up => {
                overlay.selected_index = overlay.selected_index.saturating_sub(1);
            }

            _ => {}
        }
    }

    /// 提交 prompt 到 Agent
    ///
    /// 关键设计：
    /// 1. 递增 `prompt_generation`，使旧 task 的 stale 事件失效
    /// 2. 调用 `drain_notify_permit()` 消费 `Notify` 上可能残留的 permit
    ///    （上一次超时后 permit 未被消费，会导致下次 `send_prompt_and_wait` 立即返回）
    /// 3. 清空 `current_response`，防止旧 prompt 的残留文本被 `PromptEnded` 提交
    /// 4. 存储 `JoinHandle`，用于退出时 abort 和取消时清理
    fn handle_submit(&mut self, text: String) {
        self.chat.push_user_message(&text);
        self.chat.waiting = true;

        // 递增 generation，使任何旧 prompt 的 stale 事件失效
        self.prompt_generation += 1;
        let current_gen = self.prompt_generation;

        // 消费 Notify 上可能残留的 stale permit。
        //
        // 场景：上一次 send_prompt_and_wait 超时后，tokio::select! 丢弃了 notified()
        // future，但随后 notifier 调用 notify_one() 存储了一个 permit（无人消费）。
        // 如果不清除，下一次 send_prompt_and_wait 的 notified() 会立即消费该 permit
        // 并返回 Ok，而实际上 Agent 还没处理完当前 prompt。
        //
        // 注意：notify_waiters() 无法解决此问题——它只唤醒当前 waiter，不消费
        // 已存储的 permit。必须通过 poll Notified future 来消费。
        if let Some(signal) = self.client.completion_signal() {
            Self::drain_notify_permit(&signal.notify);
        }

        // 清空旧 prompt 的流式响应。
        // FIFO 保证：所有旧 AgentText 事件在 PromptEnded 之前到达（已在通道中排队）。
        // 因此此处 current_response 中的文本一定来自旧 prompt，安全清除。
        self.chat.current_response.clear();

        let client = Arc::clone(&self.client);
        let tx = self.event_tx.clone();

        // Abort 旧的 submit task（如果还在运行）
        if let Some(old_handle) = self.submit_task.take() {
            old_handle.abort();
        }

        let handle = tokio::spawn(async move {
            if let Err(e) = client.send_prompt_and_wait(&text).await {
                // 失败时发送带 generation 的 ResetWaiting。
                // 只有当 generation 匹配当前 prompt 时，事件循环才会处理。
                // 这防止了旧 prompt 超时后的 ResetWaiting 影响新 prompt 的 waiting 状态。
                //
                // 注意：PromptEnded 通常在 send_prompt_and_wait 返回前已被事件循环处理
                // （notifier 在 signal_completion 前先发送 PromptEnded），
                // 所以大多数情况下 waiting 已经是 false。
                // ResetWaiting 处理的是 Agent 完全无响应的边界情况。
                let _ = tx.send(AppEvent::ResetWaiting(current_gen));
                let _ = tx.send(AppEvent::Diagnostics(format!(
                    "Prompt 失败: {}",
                    e
                )));
            }
        });

        self.submit_task = Some(handle);
    }

    /// 消费 `Notify` 上可能残留的 stale permit
    ///
    /// `tokio::sync::Notify::notify_one()` 在无 waiter 时会存储一个 permit。
    /// 如果 `send_prompt_and_wait` 因超时被 `tokio::select!` 丢弃了 `notified()` future，
    /// 而 notifier 随后调用 `notify_one()` 存储了 permit，下次 `notified()` 会立即返回。
    ///
    /// 此函数通过创建并 poll 一个 `Notified` future 来消费残留 permit：
    /// - `Poll::Ready` → 消费了 stale permit
    /// - `Poll::Pending` → 无 stale permit，无需处理
    ///
    /// `Notified<'_>` 包含 `PhantomPinned`（非 `Unpin`），需用 `pin!` 宏栈上 pin。
    /// Future drop 时自动从 waiter list 中移除。
    fn drain_notify_permit(notify: &tokio::sync::Notify) {
        let mut notified = pin!(notify.notified());
        let waker = noop_waker();
        let mut cx = Context::from_waker(&waker);
        let _ = notified.as_mut().poll(&mut cx);
    }

    /// 取消当前 prompt 执行
    ///
    /// 不直接设置 `waiting = false`，而是依赖以下两条路径之一：
    /// 1. Agent 处理 cancel → 发送 `PromptEnded` → 事件循环设置 `waiting = false`
    /// 2. `cancel()` 返回后（成功或超时）→ 发送 `ResetWaiting` 兜底
    ///
    /// 这样即使 agent 无响应（cancel 超时 10s），`waiting` 也能被正确清除。
    fn handle_cancel(&mut self) {
        // Abort 旧的 submit task，防止其 stale 事件到达
        if let Some(handle) = self.submit_task.take() {
            handle.abort();
        }

        // 递增 generation，使旧 task（如果被 abort 前已发送事件）的 stale 事件失效
        self.prompt_generation += 1;
        let cancel_gen = self.prompt_generation;

        // 清空流式响应缓冲区，防止旧 prompt 的 stale AgentText 事件
        // 被后续的 PromptEnded 错误提交到消息历史
        self.chat.current_response.clear();

        // 清空排队 prompt（取消时不应自动提交）
        self.pending_prompt = None;

        let client = Arc::clone(&self.client);
        let tx = self.event_tx.clone();

        tokio::spawn(async move {
            let _ = client.cancel().await;
            // 无论 cancel 成功还是超时，都发送 ResetWaiting 作为兜底
            let _ = tx.send(AppEvent::ResetWaiting(cancel_gen));
        });
    }
}
