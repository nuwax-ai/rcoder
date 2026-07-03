use anyhow::Result;

/// 从 Option 中取出 stdio 句柄，失败时返回错误
pub(crate) fn take_stdio<T>(opt: &mut Option<T>, name: &str) -> Result<T> {
    opt.take()
        .ok_or_else(|| anyhow::anyhow!("[SACP] Failed to get subprocess {}", name))
}
