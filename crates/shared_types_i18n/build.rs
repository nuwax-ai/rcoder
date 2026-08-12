// rust-i18n 在编译期用 i18n!("locales") 宏把 yml 嵌入二进制。
// cargo 默认不监听 yml 变更 → 改翻译后不重编译 → 翻译不生效。
// 显式声明 locales/ 为 rerun-if-changed 源, 确保翻译更新被捕获。
fn main() {
    println!("cargo::rerun-if-changed=locales");
}
