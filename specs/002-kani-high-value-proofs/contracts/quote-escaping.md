# Contract: 引号/转义闭合（QuoteEscaping）

**Theme**: QuoteEscaping  
**Subjects**:
- `shared_types::pg_utils::pg_shell_quote(value) -> String`
- `shared_types::pg_utils::pg_quote_ident(name) -> String`
- `shared_types::pg_utils::pg_escape_literal(value) -> String`
- `shared_types::pg_utils::validate_pg_identifier(name) -> Result<(), String>`

## Properties

### P1. Shell single-quote closure
```text
∀ value:
  pg_shell_quote(value) = "'" ++ escaped ++ "'"
  ∧ escaped 中不出现未闭合的单引号
  ⇒ 将结果放入 sh 词法时不产生第二条命令/单词
```
可检验形式：`quote(value)` 去掉首尾 `'` 后，把 `'\''` 还原，得到的字符串 == `value`（转义是双射）。

### P2. Ident double-quote closure
```text
∀ name:
  pg_quote_ident(name) = "\"" ++ name.replace("\"", "\"\"") ++ "\""
  ⇒ 与 SQL 标准双引号标识符词法一致（内部 " 均成对）
```

### P3. Literal escape pairing
```text
∀ value:
  pg_escape_literal(value) 中，' 仅以 '' 成对出现
```

### P4. Identifier whitelist totality
```text
∀ name:
  validate_pg_identifier(name) = Ok
  ⇔ 1 ≤ |name| ≤ 63 ∧ name[0] ∈ [A-Za-z_] ∧ name[1..] ⊆ [A-Za-z0-9_]
```

### P5. No panic
```text
∀ value/name (任意字节/UTF-8):
  不 panic
```

## Bounded domain

- `value`: `[u8; 12]` 或 16，允许 `'`、`\`、`"`、空格、`;`、`$`、换行（NUL 可 assume 排除若 API 约定 C 字符串）。  
- `name`: `[u8; 8]`。

## Counterexample playback

失败必须生成含注入样式的回归单测（修复前红）。

## Non-goals

- 真实 `psql`/shell 执行验证（属 E2E）。  
- `standard_conforming_strings=off` 的 `\` 歧义（代码注释已声明 PG14+ 默认 on）。
