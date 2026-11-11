//! Safe, deliberately lightweight terminal Markdown rendering.
use pulldown_cmark::{CodeBlockKind, Event, Options, Parser, Tag, TagEnd};
use ratatui::{
    style::{Color, Modifier, Style},
    text::{Line, Span, Text},
};

/// Remove terminal controls, complete ANSI control sequences and Unicode bidi
/// controls. Tabs become four spaces; newlines are retained.
fn sanitize(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\u{9b}' {
            for c in chars.by_ref() {
                if ('\u{40}'..='\u{7e}').contains(&c) {
                    break;
                }
            }
            continue;
        }
        if matches!(c, '\u{90}' | '\u{98}' | '\u{9d}' | '\u{9e}' | '\u{9f}') {
            while let Some(c) = chars.next() {
                if c == '\u{7}' || c == '\u{9c}' {
                    break;
                }
                if c == '\u{1b}' && chars.peek() == Some(&'\\') {
                    chars.next();
                    break;
                }
            }
            continue;
        }
        if c == '\u{1b}' {
            match chars.peek().copied() {
                Some('[') => {
                    chars.next();
                    for c in chars.by_ref() {
                        if ('\u{40}'..='\u{7e}').contains(&c) {
                            break;
                        }
                    }
                }
                Some(']') | Some('P') | Some('X') | Some('^') | Some('_') => {
                    chars.next();
                    while let Some(c) = chars.next() {
                        if c == '\u{7}' || c == '\u{9c}' {
                            break;
                        }
                        if c == '\u{1b}' && chars.peek() == Some(&'\\') {
                            chars.next();
                            break;
                        }
                    }
                }
                Some(c) if (' '..='/').contains(&c) => {
                    while chars.peek().is_some_and(|c| (' '..='/').contains(c)) {
                        chars.next();
                    }
                    if chars.peek().is_some_and(|c| ('0'..='~').contains(c)) {
                        chars.next();
                    }
                }
                Some(c) if ('0'..='~').contains(&c) => {
                    chars.next();
                }
                _ => {}
            }
            continue;
        }
        match c {
            '\n' => out.push('\n'),
            '\t' => out.push_str("    "),
            '\u{061c}'
            | '\u{200e}'
            | '\u{200f}'
            | '\u{202a}'..='\u{202e}'
            | '\u{2066}'..='\u{206f}' => {}
            c if c.is_control() => {}
            c => out.push(c),
        }
    }
    out
}

pub fn plain_text(text: &str) -> Text<'static> {
    Text::from(sanitize(text))
}

#[derive(Default)]
struct Output {
    lines: Vec<Line<'static>>,
    spans: Vec<Span<'static>>,
}

impl Output {
    fn push(&mut self, text: &str, style: Style) {
        // Entity decoding happens inside the Markdown parser. Sanitize again at
        // the output boundary so e.g. &#27; cannot reintroduce terminal controls.
        let safe = sanitize(text);
        for (i, part) in safe.split('\n').enumerate() {
            if i > 0 {
                self.newline();
            }
            if !part.is_empty() {
                self.spans.push(Span::styled(part.to_owned(), style));
            }
        }
    }
    fn newline(&mut self) {
        self.lines.push(Line::from(std::mem::take(&mut self.spans)));
    }
    fn boundary(&mut self) {
        if !self.spans.is_empty() {
            self.newline();
        }
    }
    fn wiki_text(&mut self, text: &str, style: Style) {
        let mut rest = text;
        while let Some(start) = rest.find("[[") {
            let Some(end) = rest[start + 2..].find("]]").map(|i| i + start + 2) else {
                break;
            };
            let body = &rest[start + 2..end];
            if body.contains('\n') {
                break;
            }
            self.push(&rest[..start], style);
            let (target, label) = body.split_once('|').unwrap_or((body, body));
            self.push(
                &format!("[{label}]"),
                style.fg(Color::Magenta).add_modifier(Modifier::UNDERLINED),
            );
            if target != label {
                self.push(&format!(" ({target})"), style.fg(Color::DarkGray));
            }
            rest = &rest[end + 2..];
        }
        self.push(rest, style);
    }
}

/// Render local text only: no HTML interpretation, image fetching or URL opening.
/// Tables are simple rows; unsupported extensions degrade to readable text.
pub fn render_markdown(text: &str) -> Text<'static> {
    // Sanitize before parsing, so even code, link destinations and HTML are safe.
    let safe = sanitize(text);
    let mut out = Output::default();
    let mut styles = vec![Style::default()];
    let mut lists: Vec<Option<u64>> = Vec::new();
    let mut links = Vec::new();
    let mut code_depth = 0usize;
    let mut pending = String::new();
    let mut options = Options::empty();
    options
        .insert(Options::ENABLE_TABLES | Options::ENABLE_STRIKETHROUGH | Options::ENABLE_TASKLISTS);
    for event in Parser::new_ext(&safe, options) {
        if let Event::Text(text) = &event {
            pending.push_str(text);
            continue;
        }
        let style = *styles.last().unwrap();
        if !pending.is_empty() {
            if code_depth > 0 {
                out.push(&pending, style);
            } else {
                out.wiki_text(&pending, style);
            }
            pending.clear();
        }
        match event {
            Event::Start(tag) => {
                let mut next = style;
                match tag {
                    Tag::Heading { level, .. } => {
                        out.boundary();
                        out.push(
                            &format!("{} ", "#".repeat(level as usize)),
                            style.fg(Color::Cyan),
                        );
                        next = next.fg(Color::Cyan).add_modifier(Modifier::BOLD);
                    }
                    Tag::Paragraph => {}
                    Tag::Strong => next = next.add_modifier(Modifier::BOLD),
                    Tag::Emphasis => next = next.add_modifier(Modifier::ITALIC),
                    Tag::Strikethrough => next = next.add_modifier(Modifier::CROSSED_OUT),
                    Tag::CodeBlock(kind) => {
                        out.boundary();
                        if let CodeBlockKind::Fenced(language) = kind {
                            if !language.is_empty() {
                                out.push(&format!("```{language}"), style.fg(Color::DarkGray));
                                out.newline();
                            }
                        }
                        code_depth += 1;
                        next = next.fg(Color::Yellow);
                    }
                    Tag::BlockQuote(_) => {
                        out.boundary();
                        out.push("│ ", style.fg(Color::DarkGray));
                    }
                    Tag::List(start) => {
                        out.boundary();
                        lists.push(start);
                    }
                    Tag::Item => {
                        out.boundary();
                        out.push(&"  ".repeat(lists.len().saturating_sub(1)), style);
                        let prefix = match lists.last_mut() {
                            Some(Some(number)) => {
                                let p = format!("{number}. ");
                                *number = number.saturating_add(1);
                                p
                            }
                            _ => "• ".to_owned(),
                        };
                        out.push(&prefix, style);
                    }
                    Tag::Link { dest_url, .. } => {
                        links.push(dest_url.into_string());
                        next = next.fg(Color::Blue).add_modifier(Modifier::UNDERLINED);
                    }
                    Tag::Image { .. } => {
                        out.push("[image: ", style.fg(Color::DarkGray));
                    }
                    Tag::Table(_) | Tag::TableHead | Tag::TableRow => out.boundary(),
                    Tag::TableCell => out.push("| ", style.fg(Color::DarkGray)),
                    _ => {}
                }
                styles.push(next);
            }
            Event::End(tag) => {
                if styles.len() > 1 {
                    styles.pop();
                }
                match tag {
                    TagEnd::Paragraph
                    | TagEnd::Heading(_)
                    | TagEnd::BlockQuote(_)
                    | TagEnd::Item
                    | TagEnd::Table
                    | TagEnd::TableHead
                    | TagEnd::TableRow => out.boundary(),
                    TagEnd::CodeBlock => {
                        code_depth = code_depth.saturating_sub(1);
                        out.boundary();
                    }
                    TagEnd::List(_) => {
                        lists.pop();
                        out.boundary();
                    }
                    TagEnd::Link => {
                        if let Some(target) = links.pop() {
                            out.push(
                                &format!(" ({target})"),
                                Style::default().fg(Color::DarkGray),
                            );
                        }
                    }
                    TagEnd::Image => out.push("]", style.fg(Color::DarkGray)),
                    TagEnd::TableCell => out.push(" ", style),
                    _ => {}
                }
            }
            Event::Code(code) => out.push(&code, style.fg(Color::Yellow)),
            Event::Html(html) | Event::InlineHtml(html) => {
                out.push(&html, style.fg(Color::DarkGray))
            }
            Event::SoftBreak => out.push(" ", style),
            Event::HardBreak => out.newline(),
            Event::Rule => {
                out.boundary();
                out.push("────────", style.fg(Color::DarkGray));
                out.newline();
            }
            Event::TaskListMarker(done) => out.push(if done { "[x] " } else { "[ ] " }, style),
            Event::FootnoteReference(label) => out.push(&format!("[{label}]"), style),
            Event::InlineMath(math) | Event::DisplayMath(math) => out.push(&math, style),
            Event::Text(_) => unreachable!(),
        }
    }
    if !pending.is_empty() {
        out.wiki_text(&pending, *styles.last().unwrap());
    }
    out.boundary();
    Text::from(out.lines)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn string(text: Text<'_>) -> String {
        text.lines
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }
    #[test]
    fn controls_and_ansi_are_removed() {
        let input = "a\x1b[31mred\x1b[0m\x00\x07\u{202e}\u{2066}\tend\nnext\x1b]8;;https://bad\x07link\x1b]8;;\x07";
        assert_eq!(string(plain_text(input)), "ared    end\nnextlink");
        assert!(!string(render_markdown(input)).contains('\x1b'));
    }
    #[test]
    fn headings_code_and_text_survive() {
        let rendered = render_markdown("# Title\n\nSome **bold** and *italic*. [[Other|label]]\n\n```rust\nlet x = \"[[literal]]\";\n```\n\n[label](other.md)");
        let text = string(rendered.clone());
        assert!(text.contains("# Title"));
        assert!(text.contains("Some bold and italic."));
        assert!(text.contains("[label] (Other)"));
        assert!(text.contains("let x = \"[[literal]]\";"));
        assert!(text.contains("label (other.md)"));
        assert!(rendered.lines[0]
            .spans
            .iter()
            .any(|s| s.style.add_modifier.contains(Modifier::BOLD)));
    }
    #[test]
    fn html_is_inert_and_tabs_expanded() {
        assert_eq!(string(plain_text("one\ttwo\nthree")), "one    two\nthree");
        assert!(string(render_markdown("<script>alert('x')</script>")).contains("<script>"));
    }

    #[test]
    fn decoded_entities_and_c1_sequences_are_safe() {
        let text = string(render_markdown("safe &#27;[31mred &#x202e; text"));
        assert!(!text.contains('\x1b'));
        assert!(!text.contains('\u{202e}'));
        assert!(!text.contains("[31m"));
        assert_eq!(string(plain_text("a\u{9b}31mb\u{9b}0m")), "ab");
    }

    #[test]
    fn list_prefix_stays_with_its_text() {
        let text = string(render_markdown("- first\n- second\n\n> quote"));
        assert!(text.contains("• first\n• second"));
        assert!(text.contains("│ quote"));
    }
}
