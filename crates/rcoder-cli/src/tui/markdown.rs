//! Markdown → ratatui Text 渲染器
//!
//! 使用 pulldown-cmark 解析 Markdown，转换为 ratatui 的 Text/Span 结构。

use pulldown_cmark::{Event, Options, Parser, Tag, TagEnd};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};

/// 将 Markdown 文本渲染为 ratatui Text
pub fn render_markdown(input: &str) -> Text<'static> {
    let mut opts = Options::empty();
    opts.insert(Options::ENABLE_STRIKETHROUGH);
    opts.insert(Options::ENABLE_TABLES);

    let parser = Parser::new_ext(input, opts);
    let mut renderer = MarkdownRenderer::new();
    renderer.render(parser);
    renderer.into_text()
}

struct MarkdownRenderer {
    lines: Vec<Line<'static>>,
    current_line: Vec<Span<'static>>,
    // Style stack
    bold: bool,
    italic: bool,
    strikethrough: bool,
    // Block state
    in_code_block: bool,
    code_block_lang: String,
    heading_level: u8,
    list_depth: usize,
    list_counters: Vec<Option<u64>>, // None = unordered, Some(n) = ordered at n
    // Link state: URL 栈，支持嵌套链接
    link_urls: Vec<String>,
    // Blockquote 嵌套深度
    blockquote_depth: usize,
}

impl MarkdownRenderer {
    fn new() -> Self {
        Self {
            lines: Vec::new(),
            current_line: Vec::new(),
            bold: false,
            italic: false,
            strikethrough: false,
            in_code_block: false,
            code_block_lang: String::new(),
            heading_level: 0,
            list_depth: 0,
            list_counters: Vec::new(),
            link_urls: Vec::new(),
            blockquote_depth: 0,
        }
    }

    fn render(&mut self, parser: Parser) {
        for event in parser {
            self.handle_event(event);
        }
        self.flush_line();
    }

    fn handle_event(&mut self, event: Event) {
        match event {
            Event::Start(tag) => self.handle_start(tag),
            Event::End(tag) => self.handle_end(tag),
            Event::Text(text) => self.handle_text(&text),
            Event::Code(code) => {
                self.current_line.push(Span::styled(
                    code.to_string(),
                    Style::default()
                        .fg(Color::Cyan)
                        .bg(Color::Rgb(40, 40, 40)),
                ));
            }
            Event::SoftBreak | Event::HardBreak => {
                self.flush_line();
            }
            Event::Rule => {
                self.flush_line();
                self.lines.push(Line::from(Span::styled(
                    "─".repeat(40),
                    Style::default().fg(Color::DarkGray),
                )));
            }
            _ => {}
        }
    }

    fn handle_start(&mut self, tag: Tag) {
        match tag {
            Tag::Heading { level, .. } => {
                self.flush_line();
                self.heading_level = level as u8;
                let prefix = "#".repeat(self.heading_level as usize);
                self.current_line.push(Span::styled(
                    format!("{} ", prefix),
                    Style::default()
                        .fg(Color::Yellow)
                        .add_modifier(Modifier::BOLD),
                ));
            }
            Tag::Paragraph
                if !self.lines.is_empty() || !self.current_line.is_empty() =>
            {
                self.flush_line();
            }
            Tag::CodeBlock(kind) => {
                self.flush_line();
                self.in_code_block = true;
                self.code_block_lang = match kind {
                    pulldown_cmark::CodeBlockKind::Fenced(lang) => lang.to_string(),
                    pulldown_cmark::CodeBlockKind::Indented => String::new(),
                };
                if !self.code_block_lang.is_empty() {
                    self.lines.push(Line::from(Span::styled(
                        format!("  ┌─ {} ", self.code_block_lang),
                        Style::default().fg(Color::DarkGray),
                    )));
                }
            }
            Tag::Emphasis => self.italic = true,
            Tag::Strong => self.bold = true,
            Tag::Strikethrough => self.strikethrough = true,
            Tag::List(start) => {
                self.list_depth += 1;
                self.list_counters.push(start);
            }
            Tag::BlockQuote(_) => {
                self.flush_line();
                self.blockquote_depth += 1;
            }
            Tag::Item => {
                self.flush_line();
                let indent = "  ".repeat(self.list_depth.saturating_sub(1));
                let marker = if let Some(counter) = self.list_counters.last_mut() {
                    match counter {
                        Some(n) => {
                            let m = format!("{}. ", n);
                            *n += 1;
                            m
                        }
                        None => "• ".to_string(),
                    }
                } else {
                    "• ".to_string()
                };
                self.current_line.push(Span::styled(
                    format!("{}{}", indent, marker),
                    Style::default().fg(Color::Yellow),
                ));
            }
            Tag::Link { dest_url, .. } => {
                self.link_urls.push(dest_url.to_string());
                self.current_line.push(Span::styled(
                    "[".to_string(),
                    Style::default().fg(Color::Blue),
                ));
            }
            _ => {}
        }
    }

    fn handle_end(&mut self, tag: TagEnd) {
        match tag {
            TagEnd::Heading(_) => {
                self.flush_line();
                self.heading_level = 0;
            }
            TagEnd::Paragraph => {
                self.flush_line();
            }
            TagEnd::CodeBlock => {
                self.flush_line();
                self.in_code_block = false;
                if !self.code_block_lang.is_empty() {
                    self.lines.push(Line::from(Span::styled(
                        "  └────".to_string(),
                        Style::default().fg(Color::DarkGray),
                    )));
                    self.code_block_lang.clear();
                }
                self.flush_line();
            }
            TagEnd::Emphasis => self.italic = false,
            TagEnd::Strong => self.bold = false,
            TagEnd::Strikethrough => self.strikethrough = false,
            TagEnd::List(_) => {
                self.list_counters.pop();
                self.list_depth = self.list_depth.saturating_sub(1);
            }
            TagEnd::BlockQuote(_) => {
                self.flush_line();
                self.blockquote_depth = self.blockquote_depth.saturating_sub(1);
            }
            TagEnd::Item => {
                self.flush_line();
            }
            TagEnd::Link => {
                self.current_line.push(Span::styled(
                    "]".to_string(),
                    Style::default().fg(Color::Blue),
                ));
                if let Some(url) = self.link_urls.pop() {
                    self.current_line.push(Span::styled(
                        format!("({})", url),
                        Style::default()
                            .fg(Color::Blue)
                            .add_modifier(Modifier::UNDERLINED),
                    ));
                }
            }
            _ => {}
        }
    }

    fn handle_text(&mut self, text: &str) {
        if self.in_code_block {
            // Code block: render each line with code styling
            for line in text.split('\n') {
                self.lines.push(Line::from(Span::styled(
                    format!("  │ {}", line),
                    Style::default()
                        .fg(Color::Green)
                        .bg(Color::Rgb(30, 30, 30)),
                )));
            }
            // Remove trailing empty line from split
            if text.ends_with('\n') {
                self.lines.pop();
            }
        } else {
            let style = self.current_style();
            self.current_line.push(Span::styled(text.to_string(), style));
        }
    }

    fn current_style(&self) -> Style {
        let mut style = Style::default();
        if self.heading_level > 0 {
            style = style
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD);
        }
        if self.bold {
            style = style.add_modifier(Modifier::BOLD);
        }
        if self.italic {
            style = style.add_modifier(Modifier::ITALIC);
        }
        if self.strikethrough {
            style = style.add_modifier(Modifier::CROSSED_OUT);
        }
        style
    }

    fn flush_line(&mut self) {
        if !self.current_line.is_empty() {
            let spans = std::mem::take(&mut self.current_line);
            if self.blockquote_depth > 0 {
                // 块引用：添加前缀，内容保留原有样式（代码、链接等）并叠加斜体
                let prefix = "│ ".repeat(self.blockquote_depth);
                let mut prefixed = vec![Span::styled(
                    prefix,
                    Style::default().fg(Color::DarkGray),
                )];
                for span in spans {
                    let style = span
                        .style
                        .patch(Style::default().add_modifier(Modifier::ITALIC));
                    prefixed.push(Span::styled(span.content, style));
                }
                self.lines.push(Line::from(prefixed));
            } else {
                self.lines.push(Line::from(spans));
            }
        }
    }

    fn into_text(self) -> Text<'static> {
        Text::from(self.lines)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::style::Modifier;

    /// Helper: extract all spans from rendered Text
    fn all_spans<'a>(text: &'a Text<'a>) -> Vec<&'a Span<'a>> {
        text.lines.iter().flat_map(|l| l.spans.iter()).collect()
    }

    /// Helper: check if any span contains the given substring
    fn contains_text(text: &Text<'_>, needle: &str) -> bool {
        text.lines.iter().any(|l| l.spans.iter().any(|s| s.content.contains(needle)))
    }

    #[test]
    fn plain_text() {
        let text = render_markdown("hello world");
        assert!(contains_text(&text, "hello world"));
    }

    #[test]
    fn heading() {
        let text = render_markdown("# Title");
        assert!(contains_text(&text, "Title"));
        assert!(contains_text(&text, "#"));
    }

    #[test]
    fn bold_text() {
        let text = render_markdown("**bold**");
        let spans = all_spans(&text);
        let bold_span = spans
            .iter()
            .find(|s| s.content.contains("bold"))
            .expect("should contain 'bold'");
        assert!(bold_span.style.add_modifier.contains(Modifier::BOLD));
    }

    #[test]
    fn code_inline() {
        let text = render_markdown("use `println!` here");
        assert!(contains_text(&text, "println!"));
    }

    #[test]
    fn code_block_with_lang() {
        let text = render_markdown("```rust\nlet x = 1;\n```");
        assert!(contains_text(&text, "rust")); // lang label
        assert!(contains_text(&text, "let x = 1;")); // code content
        assert!(contains_text(&text, "┌─")); // open marker
        assert!(contains_text(&text, "└────")); // close marker
    }

    #[test]
    fn link_url_preserved() {
        let text = render_markdown("[Rust](https://rust-lang.org)");
        assert!(contains_text(&text, "Rust"));
        assert!(contains_text(&text, "https://rust-lang.org"));
    }

    #[test]
    fn blockquote_has_prefix() {
        let text = render_markdown("> quoted text");
        assert!(contains_text(&text, "│")); // blockquote prefix
        assert!(contains_text(&text, "quoted text"));
    }

    #[test]
    fn nested_blockquote_prefix() {
        let text = render_markdown("> > nested");
        assert!(contains_text(&text, "│ │")); // depth=2 prefix
    }

    #[test]
    fn unordered_list() {
        let text = render_markdown("- item one\n- item two");
        assert!(contains_text(&text, "•"));
        assert!(contains_text(&text, "item one"));
        assert!(contains_text(&text, "item two"));
    }

    #[test]
    fn ordered_list() {
        let text = render_markdown("1. first\n2. second");
        assert!(contains_text(&text, "1."));
        assert!(contains_text(&text, "first"));
    }

    #[test]
    fn horizontal_rule() {
        let text = render_markdown("---");
        assert!(contains_text(&text, "─")); // rule character
    }

    #[test]
    fn strikethrough() {
        let text = render_markdown("~~deleted~~");
        let spans = all_spans(&text);
        let st_span = spans
            .iter()
            .find(|s| s.content.contains("deleted"))
            .expect("should contain 'deleted'");
        assert!(st_span.style.add_modifier.contains(Modifier::CROSSED_OUT));
    }

    #[test]
    fn empty_input() {
        let text = render_markdown("");
        assert!(text.lines.is_empty());
    }

    #[test]
    fn blockquote_preserves_inline_styles() {
        let text = render_markdown("> **bold in quote**");
        let spans = all_spans(&text);
        let bold_span = spans
            .iter()
            .find(|s| s.content.contains("bold in quote"))
            .expect("should contain bold text in quote");
        // Should have both BOLD (from **) and ITALIC (from blockquote)
        assert!(bold_span.style.add_modifier.contains(Modifier::BOLD));
        assert!(bold_span.style.add_modifier.contains(Modifier::ITALIC));
    }
}
