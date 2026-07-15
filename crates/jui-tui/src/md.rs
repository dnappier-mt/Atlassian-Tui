use pulldown_cmark::{CodeBlockKind, Event, HeadingLevel, Options, Parser, Tag, TagEnd};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};

/// Render Markdown into ratatui `Line`s with light styling — headings, bold/italic,
/// inline code, fenced code blocks, lists, and blockquotes. No syntax highlighting
/// inside code blocks (the whole block is colored uniformly).
pub fn render_markdown(input: &str) -> Vec<Line<'static>> {
    let mut opts = Options::empty();
    opts.insert(Options::ENABLE_STRIKETHROUGH);
    opts.insert(Options::ENABLE_TABLES);

    let mut state = State::default();
    let parser = Parser::new_ext(input, opts);
    for event in parser {
        match event {
            Event::Start(tag) => state.start(tag),
            Event::End(end) => state.end(end),
            Event::Text(t) => state.text(t.into_string()),
            Event::Code(c) => state.inline_code(c.into_string()),
            Event::SoftBreak => state.push_space(),
            Event::HardBreak => state.flush_line(),
            Event::Rule => state.rule(),
            Event::Html(_) | Event::InlineHtml(_) => {} // ignore raw HTML
            Event::TaskListMarker(checked) => {
                let marker = if checked { "[x] " } else { "[ ] " };
                state.text(marker.to_string());
            }
            Event::FootnoteReference(_) => {}
            Event::InlineMath(_) | Event::DisplayMath(_) => {}
        }
    }
    state.flush_line();
    state.lines
}

#[derive(Default)]
struct State {
    lines: Vec<Line<'static>>,
    /// Spans accumulating into the in-progress visual line.
    cur: Vec<Span<'static>>,
    /// Style stack — current style is the last entry.
    style_stack: Vec<Style>,
    /// Currently inside a fenced code block (each `Text` event is one line).
    in_code_block: bool,
    /// Indent prefix to emit at the start of every new line (for nested lists).
    list_stack: Vec<ListKind>,
    /// Current heading level when inside a heading.
    heading: Option<HeadingLevel>,
    /// Current blockquote depth.
    quote_depth: u8,
}

#[derive(Clone, Copy)]
enum ListKind {
    Bullet,
    Ordered(u64),
}

impl State {
    fn cur_style(&self) -> Style {
        *self.style_stack.last().unwrap_or(&Style::default())
    }

    fn push_style(&mut self, s: Style) {
        let merged = self.cur_style().patch(s);
        self.style_stack.push(merged);
    }

    fn pop_style(&mut self) {
        self.style_stack.pop();
    }

    fn push_span(&mut self, content: String) {
        if content.is_empty() {
            return;
        }
        let style = self.cur_style();
        self.cur.push(Span::styled(content, style));
    }

    fn push_space(&mut self) {
        self.push_span(" ".into());
    }

    fn text(&mut self, t: String) {
        if self.in_code_block {
            // Code block: each line of the input becomes its own visual line.
            for (i, ln) in t.split('\n').enumerate() {
                if i > 0 {
                    self.flush_line();
                }
                if !ln.is_empty() {
                    self.push_span(ln.to_string());
                }
            }
        } else {
            // Collapse explicit newlines inside paragraphs into spaces; pulldown emits
            // soft breaks for line wraps already.
            let cleaned = t.replace('\n', " ");
            self.push_span(cleaned);
        }
    }

    fn inline_code(&mut self, c: String) {
        let style = Style::default()
            .fg(Color::Yellow)
            .bg(Color::Rgb(36, 36, 50));
        self.cur.push(Span::styled(c, style));
    }

    fn flush_line(&mut self) {
        let mut spans = std::mem::take(&mut self.cur);
        if spans.is_empty() {
            self.lines.push(Line::from(""));
            return;
        }
        // Prepend any list / blockquote indent prefix.
        if let Some(prefix) = self.line_prefix() {
            spans.insert(0, prefix);
        }
        self.lines.push(Line::from(spans));
    }

    fn line_prefix(&self) -> Option<Span<'static>> {
        let depth = self.list_stack.len();
        if depth == 0 && self.quote_depth == 0 {
            return None;
        }
        let mut prefix = String::new();
        for _ in 0..self.quote_depth {
            prefix.push_str("│ ");
        }
        for _ in 0..depth.saturating_sub(1) {
            prefix.push_str("  ");
        }
        let style = if self.quote_depth > 0 {
            Style::default().fg(Color::DarkGray)
        } else {
            Style::default()
        };
        Some(Span::styled(prefix, style))
    }

    fn start(&mut self, tag: Tag) {
        match tag {
            Tag::Paragraph => {}
            Tag::Heading { level, .. } => {
                self.heading = Some(level);
                let color = match level {
                    HeadingLevel::H1 => Color::Magenta,
                    HeadingLevel::H2 => Color::Cyan,
                    HeadingLevel::H3 => Color::Green,
                    _ => Color::Yellow,
                };
                self.push_style(Style::default().fg(color).add_modifier(Modifier::BOLD));
            }
            Tag::BlockQuote(_) => {
                self.quote_depth = self.quote_depth.saturating_add(1);
                self.push_style(
                    Style::default()
                        .fg(Color::DarkGray)
                        .add_modifier(Modifier::ITALIC),
                );
            }
            Tag::CodeBlock(kind) => {
                self.flush_line();
                self.in_code_block = true;
                let lang = if let CodeBlockKind::Fenced(l) = kind {
                    l.into_string()
                } else {
                    String::new()
                };
                let header_style = Style::default().fg(Color::DarkGray);
                let header = if lang.is_empty() {
                    "─── code ───".to_string()
                } else {
                    format!("─── {lang} ───")
                };
                self.lines
                    .push(Line::from(Span::styled(header, header_style)));
                self.push_style(
                    Style::default()
                        .fg(Color::Yellow)
                        .bg(Color::Rgb(28, 28, 38)),
                );
            }
            Tag::List(start) => {
                self.flush_line();
                let kind = match start {
                    Some(n) => ListKind::Ordered(n),
                    None => ListKind::Bullet,
                };
                self.list_stack.push(kind);
            }
            Tag::Item => {
                self.flush_line();
                let bullet = match self.list_stack.last_mut() {
                    Some(ListKind::Bullet) => "• ".to_string(),
                    Some(ListKind::Ordered(n)) => {
                        let s = format!("{n}. ");
                        *n += 1;
                        s
                    }
                    None => "• ".to_string(),
                };
                let bullet_style = Style::default().fg(Color::Cyan);
                self.cur.push(Span::styled(bullet, bullet_style));
            }
            Tag::Emphasis => self.push_style(Style::default().add_modifier(Modifier::ITALIC)),
            Tag::Strong => self.push_style(Style::default().add_modifier(Modifier::BOLD)),
            Tag::Strikethrough => {
                self.push_style(Style::default().add_modifier(Modifier::CROSSED_OUT))
            }
            Tag::Link { .. } => self.push_style(
                Style::default()
                    .fg(Color::Blue)
                    .add_modifier(Modifier::UNDERLINED),
            ),
            Tag::Image { .. } => self.push_style(Style::default().fg(Color::Magenta)),
            Tag::Table(_) | Tag::TableHead | Tag::TableRow | Tag::TableCell => {}
            Tag::FootnoteDefinition(_) => {}
            _ => {}
        }
    }

    fn end(&mut self, end: TagEnd) {
        match end {
            TagEnd::Paragraph => self.flush_line(),
            TagEnd::Heading(_) => {
                self.heading = None;
                self.pop_style();
                self.flush_line();
                // Spacer line under headings.
                self.lines.push(Line::from(""));
            }
            TagEnd::BlockQuote(_) => {
                self.quote_depth = self.quote_depth.saturating_sub(1);
                self.pop_style();
                self.flush_line();
            }
            TagEnd::CodeBlock => {
                self.flush_line();
                self.pop_style();
                self.in_code_block = false;
                self.lines.push(Line::from(Span::styled(
                    "──────────",
                    Style::default().fg(Color::DarkGray),
                )));
            }
            TagEnd::List(_) => {
                self.list_stack.pop();
                self.flush_line();
            }
            TagEnd::Item => self.flush_line(),
            TagEnd::Emphasis | TagEnd::Strong | TagEnd::Strikethrough => self.pop_style(),
            TagEnd::Link | TagEnd::Image => self.pop_style(),
            TagEnd::TableCell | TagEnd::TableHead | TagEnd::TableRow => {}
            TagEnd::Table => self.flush_line(),
            _ => {}
        }
    }

    fn rule(&mut self) {
        self.flush_line();
        self.lines.push(Line::from(Span::styled(
            "─".repeat(60),
            Style::default().fg(Color::DarkGray),
        )));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn smoke() {
        let md =
            "# Hi\n\nSome **bold** and `code`.\n\n- one\n- two\n\n```rust\nfn main() {}\n```\n";
        let lines = render_markdown(md);
        assert!(!lines.is_empty());
    }
}
