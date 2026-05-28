//! 多行输入框组件
//!
//! 支持多行文本编辑、光标在行间移动、换行符插入和提交。
//!
//! ## 键盘映射
//! - Enter → 提交整个输入
//! - Alt+Enter / Shift+Enter / Ctrl+J → 在当前光标处插入换行
//! - Up/Down → 在行间移动（边界处返回 false，调用方用于滚动聊天）
//! - Home/End → 当前行的行首/行尾
//!
//! ## 数据结构
//! `lines: Vec<String>` 存储每行文本（不含换行符），
//! `cursor_row`/`cursor_col` 定位光标（col 为字符索引，非字节偏移）。

/// 输入框状态
pub struct Composer {
    /// 每行文本（不含换行符）
    pub lines: Vec<String>,
    /// 当前行号（0-based，从顶部）
    pub cursor_row: usize,
    /// 当前列号（字符索引，在当前行内）
    pub cursor_col: usize,
}

impl Composer {
    pub fn new() -> Self {
        Self {
            lines: vec![String::new()],
            cursor_row: 0,
            cursor_col: 0,
        }
    }

    /// 行数
    #[allow(dead_code)]
    pub fn line_count(&self) -> usize {
        self.lines.len()
    }

    /// 显示高度（行内容 + 2 行 border），最大 10 行
    pub fn display_height(&self) -> u16 {
        const MAX_DISPLAY_HEIGHT: u16 = 10;
        (self.lines.len() as u16 + 2).min(MAX_DISPLAY_HEIGHT)
    }

    /// 是否有内容（任意行非空）
    pub fn has_content(&self) -> bool {
        self.lines.iter().any(|l| !l.is_empty())
    }

    /// 是否为空（单行且无内容）
    pub fn is_empty(&self) -> bool {
        self.lines.len() == 1 && self.lines[0].is_empty()
    }

    /// 兼容旧接口：返回整个输入内容（所有行用换行符连接）
    #[allow(dead_code)]
    pub fn buffer(&self) -> String {
        self.lines.join("\n")
    }

    // ── 编辑操作 ──

    /// 在光标处插入单个字符
    pub fn insert_char(&mut self, c: char) {
        let byte_pos = self.byte_offset(self.cursor_row, self.cursor_col);
        self.lines[self.cursor_row].insert(byte_pos, c);
        self.cursor_col += 1;
    }

    /// 在光标处插入字符串（粘贴，可能含换行符）
    pub fn insert_str(&mut self, s: &str) {
        if !s.contains('\n') {
            let byte_pos = self.byte_offset(self.cursor_row, self.cursor_col);
            self.lines[self.cursor_row].insert_str(byte_pos, s);
            self.cursor_col += s.chars().count();
        } else {
            // 多行粘贴：在当前光标处拆分并插入多行
            let paste_lines: Vec<&str> = s.split('\n').collect();
            let byte_pos = self.byte_offset(self.cursor_row, self.cursor_col);

            // 当前行在光标处拆分为 before / after
            let after = self.lines[self.cursor_row][byte_pos..].to_string();
            self.lines[self.cursor_row].truncate(byte_pos);

            // 第一行粘贴内容追加到当前行（before 部分）
            self.lines[self.cursor_row].push_str(paste_lines[0]);

            // 中间行作为新行插入
            for (i, line) in paste_lines.iter().enumerate().skip(1).take(paste_lines.len() - 2) {
                self.lines.insert(self.cursor_row + i, (*line).to_string());
            }

            // 最后一行 + after 拼接为新行
            let last_idx = paste_lines.len() - 1;
            let last_line = format!("{}{}", paste_lines[last_idx], after);
            self.lines.insert(self.cursor_row + last_idx, last_line);

            // 光标移动到最后一行末尾（粘贴内容部分）
            self.cursor_row += last_idx;
            self.cursor_col = paste_lines[last_idx].chars().count();
        }
    }

    /// 在光标处插入换行（拆分当前行）
    pub fn insert_newline(&mut self) {
        let byte_pos = self.byte_offset(self.cursor_row, self.cursor_col);
        let rest = self.lines[self.cursor_row][byte_pos..].to_string();
        self.lines[self.cursor_row].truncate(byte_pos);
        self.cursor_row += 1;
        self.lines.insert(self.cursor_row, rest);
        self.cursor_col = 0;
    }

    /// 删除光标前一个字符（Backspace）
    pub fn backspace(&mut self) {
        if self.cursor_col > 0 {
            // 当前行内删除
            self.cursor_col -= 1;
            let byte_pos = self.byte_offset(self.cursor_row, self.cursor_col);
            let next_byte_pos = self.byte_offset(self.cursor_row, self.cursor_col + 1);
            self.lines[self.cursor_row].drain(byte_pos..next_byte_pos);
        } else if self.cursor_row > 0 {
            // 当前行首 → 合并到上一行
            let current = self.lines.remove(self.cursor_row);
            self.cursor_row -= 1;
            self.cursor_col = self.lines[self.cursor_row].chars().count();
            self.lines[self.cursor_row].push_str(&current);
        }
    }

    /// 删除光标后一个字符（Delete）
    pub fn delete(&mut self) {
        let char_count = self.lines[self.cursor_row].chars().count();
        if self.cursor_col < char_count {
            // 当前行内删除
            let byte_pos = self.byte_offset(self.cursor_row, self.cursor_col);
            let next_byte_pos = self.byte_offset(self.cursor_row, self.cursor_col + 1);
            self.lines[self.cursor_row].drain(byte_pos..next_byte_pos);
        } else if self.cursor_row + 1 < self.lines.len() {
            // 当前行尾 → 合并下一行到当前行
            let next = self.lines.remove(self.cursor_row + 1);
            self.lines[self.cursor_row].push_str(&next);
        }
    }

    // ── 光标移动 ──

    /// 光标左移
    pub fn move_left(&mut self) {
        if self.cursor_col > 0 {
            self.cursor_col -= 1;
        }
    }

    /// 光标右移
    pub fn move_right(&mut self) {
        let len = self.lines[self.cursor_row].chars().count();
        if self.cursor_col < len {
            self.cursor_col += 1;
        }
    }

    /// 光标上移一行
    ///
    /// 返回 `true` 表示在 composer 内成功移动；
    /// 返回 `false` 表示已在首行，调用方可转为滚动聊天。
    pub fn move_up(&mut self) -> bool {
        if self.cursor_row > 0 {
            self.cursor_row -= 1;
            self.cursor_col = self
                .cursor_col
                .min(self.lines[self.cursor_row].chars().count());
            true
        } else {
            false
        }
    }

    /// 光标下移一行
    ///
    /// 返回 `true` 表示在 composer 内成功移动；
    /// 返回 `false` 表示已在末行，调用方可转为滚动聊天。
    pub fn move_down(&mut self) -> bool {
        if self.cursor_row + 1 < self.lines.len() {
            self.cursor_row += 1;
            self.cursor_col = self
                .cursor_col
                .min(self.lines[self.cursor_row].chars().count());
            true
        } else {
            false
        }
    }

    /// 移动到当前行行首
    pub fn move_home(&mut self) {
        self.cursor_col = 0;
    }

    /// 移动到当前行行尾
    pub fn move_end(&mut self) {
        self.cursor_col = self.lines[self.cursor_row].chars().count();
    }

    // ── 提交 ──

    /// 清空并提交：返回所有行用换行符连接的完整文本
    pub fn submit(&mut self) -> String {
        let text = self.lines.join("\n");
        self.lines = vec![String::new()];
        self.cursor_row = 0;
        self.cursor_col = 0;
        text
    }

    /// 清空输入
    #[allow(dead_code)]
    pub fn clear(&mut self) {
        self.lines = vec![String::new()];
        self.cursor_row = 0;
        self.cursor_col = 0;
    }

    // ── 内部工具 ──

    /// 将指定行的字符索引转为字节偏移
    fn byte_offset(&self, row: usize, col: usize) -> usize {
        self.lines[row]
            .char_indices()
            .nth(col)
            .map(|(i, _)| i)
            .unwrap_or(self.lines[row].len())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_composer_is_empty() {
        let c = Composer::new();
        assert!(c.is_empty());
        assert!(!c.has_content());
        assert_eq!(c.cursor_row, 0);
        assert_eq!(c.cursor_col, 0);
    }

    #[test]
    fn insert_char_single_line() {
        let mut c = Composer::new();
        for ch in "hello".chars() {
            c.insert_char(ch);
        }
        assert_eq!(c.lines, vec!["hello"]);
        assert_eq!(c.cursor_col, 5);
        assert!(c.has_content());
    }

    #[test]
    fn insert_char_utf8() {
        let mut c = Composer::new();
        for ch in "你好".chars() {
            c.insert_char(ch);
        }
        assert_eq!(c.lines, vec!["你好"]);
        assert_eq!(c.cursor_col, 2); // 字符索引，非字节
    }

    #[test]
    fn backspace_within_line() {
        let mut c = Composer::new();
        c.insert_str("abc");
        c.backspace();
        assert_eq!(c.lines, vec!["ab"]);
        assert_eq!(c.cursor_col, 2);
    }

    #[test]
    fn backspace_at_line_start_merges() {
        let mut c = Composer::new();
        c.insert_str("hello");
        c.insert_newline();
        c.insert_str("world");
        // cursor at row=1, col=5
        assert_eq!(c.cursor_row, 1);
        assert_eq!(c.cursor_col, 5);

        c.move_home(); // move to col=0 on row=1
        assert_eq!(c.cursor_col, 0);

        c.backspace(); // merge line 1 into line 0
        assert_eq!(c.lines, vec!["helloworld"]);
        assert_eq!(c.cursor_row, 0);
        assert_eq!(c.cursor_col, 5); // cursor at join point
    }

    #[test]
    fn delete_within_line() {
        let mut c = Composer::new();
        c.insert_str("abc");
        c.move_home();
        c.delete();
        assert_eq!(c.lines, vec!["bc"]);
        assert_eq!(c.cursor_col, 0);
    }

    #[test]
    fn delete_at_line_end_merges() {
        let mut c = Composer::new();
        c.insert_str("hello");
        c.insert_newline();
        c.insert_str("world");
        c.move_home(); // row=1, col=0
        c.move_up();   // row=0, col=min(0, 5)=0
        c.move_end();  // row=0, col=5 (end of "hello")

        c.delete(); // merge line 1 into line 0
        assert_eq!(c.lines, vec!["helloworld"]);
        assert_eq!(c.cursor_row, 0);
        assert_eq!(c.cursor_col, 5);
    }

    #[test]
    fn insert_newline_splits_line() {
        let mut c = Composer::new();
        c.insert_str("helloworld");
        // move to col=5
        for _ in 0..5 {
            c.move_left();
        }
        assert_eq!(c.cursor_col, 5);

        c.insert_newline();
        assert_eq!(c.lines, vec!["hello", "world"]);
        assert_eq!(c.cursor_row, 1);
        assert_eq!(c.cursor_col, 0);
    }

    #[test]
    fn insert_str_multiline_paste() {
        let mut c = Composer::new();
        c.insert_str("hello");
        // cursor at col=2
        c.move_home();
        c.move_right();
        c.move_right();
        assert_eq!(c.cursor_col, 2);

        c.insert_str("a\nb\nc");
        // "he" + "a" on line 0, "b" on line 1, "c" + "llo" on line 2
        assert_eq!(c.lines, vec!["hea", "b", "cllo"]);
        assert_eq!(c.cursor_row, 2);
        assert_eq!(c.cursor_col, 1); // after "c"
    }

    #[test]
    fn move_up_down_return_values() {
        let mut c = Composer::new();
        c.insert_str("line0");
        c.insert_newline();
        c.insert_str("line1");
        c.insert_newline();
        c.insert_str("line2");
        // cursor at row=2

        // move_up within bounds returns true
        assert!(c.move_up());
        assert_eq!(c.cursor_row, 1);

        assert!(c.move_up());
        assert_eq!(c.cursor_row, 0);

        // move_up at top returns false
        assert!(!c.move_up());
        assert_eq!(c.cursor_row, 0);

        // move_down within bounds returns true
        assert!(c.move_down());
        assert_eq!(c.cursor_row, 1);

        assert!(c.move_down());
        assert_eq!(c.cursor_row, 2);

        // move_down at bottom returns false
        assert!(!c.move_down());
        assert_eq!(c.cursor_row, 2);
    }

    #[test]
    fn move_up_clamps_col() {
        let mut c = Composer::new();
        c.insert_str("long line");
        c.insert_newline();
        c.insert_str("hi");
        // cursor at row=1, col=2

        assert!(c.move_up());
        assert_eq!(c.cursor_row, 0);
        // col should be clamped to min(2, 9) = 2
        assert_eq!(c.cursor_col, 2);
    }

    #[test]
    fn move_home_end() {
        let mut c = Composer::new();
        c.insert_str("hello world");
        assert_eq!(c.cursor_col, 11);

        c.move_home();
        assert_eq!(c.cursor_col, 0);

        c.move_end();
        assert_eq!(c.cursor_col, 11);
    }

    #[test]
    fn submit_returns_text_and_resets() {
        let mut c = Composer::new();
        c.insert_str("hello");
        c.insert_newline();
        c.insert_str("world");

        let text = c.submit();
        assert_eq!(text, "hello\nworld");
        assert!(c.is_empty());
        assert_eq!(c.cursor_row, 0);
        assert_eq!(c.cursor_col, 0);
    }

    #[test]
    fn display_height_capped() {
        let mut c = Composer::new();
        assert_eq!(c.display_height(), 3); // 1 line + 2 border

        for i in 0..20 {
            c.insert_str(&format!("line{}", i));
            c.insert_newline();
        }
        // 21 lines + 2 = 23, capped at 10
        assert_eq!(c.display_height(), 10);
    }

    #[test]
    fn is_empty_vs_has_content() {
        let mut c = Composer::new();
        assert!(c.is_empty());

        c.insert_char('a');
        assert!(!c.is_empty());
        assert!(c.has_content());

        c.submit();
        assert!(c.is_empty());
        assert!(!c.has_content());
    }
}
