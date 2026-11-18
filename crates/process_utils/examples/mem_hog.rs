//! e2e 用的"吃内存命令"：分配并逐字节触碰，然后常驻。
//!
//! 故意做成**独立二进制**：看门狗的保护规则 4 是 `exe == root.exe`，
//! 如果 hog 和编排器是同一个可执行文件，它会被永久保护、测不出淘汰。

fn main() {
    let mb: usize = std::env::args()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(150);
    let bytes = mb * 1024 * 1024;
    let mut buf = Vec::with_capacity(bytes);
    for i in 0..bytes {
        buf.push((i % 251) as u8);
    }
    println!("hog {mb}MB ready pid={}", std::process::id());
    std::thread::sleep(std::time::Duration::from_secs(120));
    drop(buf);
}
