# 如何添加新的排除目录

在 `project_zip.rs` 中，排除目录列表被维护为一个常量数组：

```rust
/// 需要排除的目录名称列表
const EXCLUDED_DIRECTORIES: &[&str] = &[
    "node_modules",
    // 可以在这里添加更多需要排除的目录名称
];
```

## 添加新的排除目录

如果你想要添加更多需要排除的目录，只需在数组中添加新的字符串即可：

```rust
const EXCLUDED_DIRECTORIES: &[&str] = &[
    "node_modules",
    "target",          // Rust 构建目录
    "dist",            // 前端构建目录
    "build",           // 通用构建目录
    ".git",            // Git 目录
    ".idea",           // IntelliJ IDEA 目录
    "vendor",          // Composer vendor 目录
    // 可以继续添加更多...
];
```

## 示例：添加常见的排除目录

```rust
const EXCLUDED_DIRECTORIES: &[&str] = &[
    // Node.js 相关
    "node_modules",
    "npm-debug.log",

    // Rust 相关
    "target",
    "Cargo.lock",

    // 前端构建目录
    "dist",
    "build",
    "out",

    // 版本控制
    ".git",
    ".svn",
    ".hg",

    // IDE 配置
    ".idea",
    ".vscode",
    ".vs",

    // 缓存目录
    ".cache",
    "tmp",
    "temp",

    // 其他
    "vendor",
    "bower_components",
    "logs",
];
```

## 测试新的排除目录

所有的排除目录都会在单元测试中自动验证，确保它们被正确排除。