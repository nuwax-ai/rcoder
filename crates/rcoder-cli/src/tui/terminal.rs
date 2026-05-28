//! 终端初始化与恢复
//!
//! 管理 alternate screen、raw mode、panic hook 等终端状态。
//! 退出时必须恢复原始终端状态，否则用户终端会异常。

use crossterm::{
    event::{DisableBracketedPaste, EnableBracketedPaste},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;
use std::io::{self, stdout, Stdout};

/// 终端类型别名
pub type TuiTerminal = Terminal<CrosstermBackend<Stdout>>;

/// 初始化终端：进入 alternate screen + raw mode + bracketed paste
pub fn init() -> io::Result<TuiTerminal> {
    // 设置 panic hook 确保 panic 时恢复终端
    setup_panic_hook();

    enable_raw_mode()?;
    let mut out = stdout();
    execute!(out, EnterAlternateScreen, EnableBracketedPaste)?;

    let backend = CrosstermBackend::new(out);
    let terminal = Terminal::new(backend)?;
    Ok(terminal)
}

/// 恢复终端：退出 alternate screen + 关闭 raw mode
pub fn restore() -> io::Result<()> {
    disable_raw_mode()?;
    execute!(stdout(), LeaveAlternateScreen, DisableBracketedPaste)?;
    Ok(())
}

/// 安装 panic hook，确保 panic 时恢复终端状态
fn setup_panic_hook() {
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |panic_info| {
        // 先恢复终端，再打印 panic 信息
        let _ = disable_raw_mode();
        let _ = execute!(stdout(), LeaveAlternateScreen, DisableBracketedPaste);
        default_hook(panic_info);
    }));
}
