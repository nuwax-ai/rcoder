//! ttyd 二进制帧协议常量
//!
//! ttyd（libwebsockets）用自定义二进制帧：首字节是命令码，后接 payload。
//! 定义对照 ttyd 源码 `src/server.h`。中间层默认**原样透传** Message 字节，
//! 不重新解析帧语义（降低引入协议 bug 的风险）；这里的常量供日志/识别使用。

// 中间层原样透传 Message 字节、不解析帧语义，故这些协议常量暂未被引用；
// 保留供日志识别与未来扩展（如命令审计、PAUSE/RESUME 流控处理）。
#![allow(dead_code)]

// ============================================================================
// 客户端 → 服务端（client message，5 种）
// ============================================================================

/// 终端输入（原始字节）
pub const INPUT: u8 = b'0'; // 0x30
/// 终端尺寸变更，payload 为 JSON `{columns, rows}`
pub const RESIZE_TERMINAL: u8 = b'1'; // 0x31
/// 客户端请求暂停输出（流控：前端暂时不消费输出，让 ttyd 停止从 PTY 读取）
pub const PAUSE: u8 = b'2'; // 0x32
/// 客户端请求恢复输出（配合 [`PAUSE`]）
pub const RESUME: u8 = b'3'; // 0x33
/// 首条消息，payload 为 JSON `{columns, rows}`，ttyd 收到后才 fork bash
pub const JSON_DATA: u8 = b'{'; // 0x7B

// ============================================================================
// 服务端 → 客户端（server message，3 种）
// ============================================================================

/// 终端输出（原始字节，含 ANSI 转义）
pub const OUTPUT: u8 = b'0'; // 0x30
/// 窗口标题（字符串）
pub const SET_WINDOW_TITLE: u8 = b'1'; // 0x31
/// 偏好设置（JSON）
pub const SET_PREFERENCES: u8 = b'2'; // 0x32

#[cfg(test)]
mod tests {
    use super::*;

    /// 客户端命令码与 ttyd 源码 `src/server.h` 一致（防止手抖改错常量）
    #[test]
    fn client_command_codes_match_ttyd() {
        assert_eq!(INPUT, 0x30);
        assert_eq!(RESIZE_TERMINAL, 0x31);
        assert_eq!(PAUSE, 0x32);
        assert_eq!(RESUME, 0x33);
        assert_eq!(JSON_DATA, 0x7B);
    }

    /// 服务端命令码与 ttyd 源码 `src/server.h` 一致
    #[test]
    fn server_command_codes_match_ttyd() {
        assert_eq!(OUTPUT, 0x30);
        assert_eq!(SET_WINDOW_TITLE, 0x31);
        assert_eq!(SET_PREFERENCES, 0x32);
    }
}
