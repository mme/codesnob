//! Light markdown for commit messages and PR descriptions: headings and
//! emphasis styled, lists bulleted, code dimmed, HTML dropped, everything
//! wrapped to the width of the terminal.

use pulldown_cmark::{CodeBlockKind, Event, Options, Parser, Tag, TagEnd};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};

use super::Palette;

struct Writer<'p> {
    width: usize,
    p: &'p Palette,
    out: Vec<Line<'static>>,
    /// Words of the paragraph being built: (text, style, space before).
    words: Vec<(String, Style, bool)>,
    styles: Vec<Style>,
    /// Open lists: the next number for ordered ones.
    lists: Vec<Option<u64>>,
    /// Prefix for the first line of the next paragraph, e.g. "• ".
    bullet: Option<String>,
    in_code: bool,
    pending_space: bool,
}

impl Writer<'_> {
    fn style(&self) -> Style {
        self.styles.last().copied().unwrap_or_default()
    }

    fn indent(&self) -> usize {
        self.lists.len() * 2
    }

    fn blank(&mut self) {
        if self.out.last().is_some_and(|l| l.width() > 0) {
            self.out.push(Line::default());
        }
    }

    fn text(&mut self, text: &str, style: Style) {
        let mut first = true;
        for (i, part) in text.split(' ').enumerate() {
            if i > 0 {
                self.pending_space = true;
            }
            if part.is_empty() {
                continue;
            }
            let space = self.pending_space && !(first && self.words.is_empty());
            self.words.push((part.to_string(), style, space));
            self.pending_space = false;
            first = false;
        }
    }

    /// Wrap the collected words into lines.
    fn flush(&mut self) {
        if self.words.is_empty() {
            self.bullet = None;
            return;
        }
        let indent = self.indent();
        let first_prefix = match self.bullet.take() {
            Some(b) => format!("{}{b}", " ".repeat(indent.saturating_sub(2))),
            None => " ".repeat(indent),
        };
        let hang = " ".repeat(indent);
        let limit = self.width.max(indent + 10);
        let mut line: Vec<Span<'static>> = vec![Span::styled(
            first_prefix.clone(),
            Style::default().fg(self.p.hunk_fg),
        )];
        let mut used = first_prefix.chars().count();
        for (word, style, space) in std::mem::take(&mut self.words) {
            let w = word.chars().count();
            let gap = usize::from(space);
            if used + gap + w > limit && used > hang.len() {
                self.out.push(Line::from(std::mem::take(&mut line)));
                line.push(Span::raw(hang.clone()));
                used = hang.len();
            } else if space {
                line.push(Span::raw(" "));
                used += 1;
            }
            line.push(Span::styled(word, style));
            used += w;
        }
        self.out.push(Line::from(line));
        self.pending_space = false;
    }
}

/// Render `md` to lines no wider than `width`.
pub fn render(md: &str, width: usize, p: &Palette) -> Vec<Line<'static>> {
    let mut w = Writer {
        width,
        p,
        out: Vec::new(),
        words: Vec::new(),
        styles: Vec::new(),
        lists: Vec::new(),
        bullet: None,
        in_code: false,
        pending_space: false,
    };
    let code = Style::default().fg(p.hunk_fg);
    let opts = Options::ENABLE_STRIKETHROUGH | Options::ENABLE_TASKLISTS | Options::ENABLE_TABLES;
    for event in Parser::new_ext(md, opts) {
        match event {
            Event::Start(tag) => match tag {
                Tag::Heading { .. } => {
                    w.flush();
                    w.blank();
                    let s = w.style().add_modifier(Modifier::BOLD);
                    w.styles.push(s);
                }
                Tag::Paragraph => w.flush(),
                Tag::List(start) => {
                    w.flush();
                    if w.lists.is_empty() {
                        w.blank();
                    }
                    w.lists.push(start);
                }
                Tag::Item => {
                    w.flush();
                    let bullet = match w.lists.last_mut() {
                        Some(Some(n)) => {
                            let b = format!("{n}. ");
                            *n += 1;
                            b
                        }
                        _ => "• ".to_string(),
                    };
                    w.bullet = Some(bullet);
                }
                Tag::Emphasis => {
                    let s = w.style().add_modifier(Modifier::ITALIC);
                    w.styles.push(s);
                }
                Tag::Strong => {
                    let s = w.style().add_modifier(Modifier::BOLD);
                    w.styles.push(s);
                }
                Tag::Strikethrough => {
                    let s = w.style().add_modifier(Modifier::CROSSED_OUT);
                    w.styles.push(s);
                }
                Tag::Link { .. } => {
                    let s = w.style().add_modifier(Modifier::UNDERLINED);
                    w.styles.push(s);
                }
                Tag::CodeBlock(kind) => {
                    w.flush();
                    w.blank();
                    w.in_code = true;
                    if let CodeBlockKind::Fenced(lang) = kind
                        && !lang.is_empty()
                    {
                        w.out.push(Line::from(Span::styled(
                            format!("{}  {lang}", " ".repeat(w.indent())),
                            Style::default().fg(p.note_fg),
                        )));
                    }
                }
                Tag::BlockQuote(_) => {
                    w.flush();
                    let s = w.style().fg(p.note_fg);
                    w.styles.push(s);
                }
                Tag::TableCell => w.pending_space = true,
                Tag::HtmlBlock => {}
                _ => {}
            },
            Event::End(tag) => match tag {
                TagEnd::Heading(_) => {
                    w.flush();
                    w.styles.pop();
                    w.blank();
                }
                TagEnd::Paragraph => {
                    w.flush();
                    if w.lists.is_empty() {
                        w.blank();
                    }
                }
                TagEnd::List(_) => {
                    w.flush();
                    w.lists.pop();
                    if w.lists.is_empty() {
                        w.blank();
                    }
                }
                TagEnd::Item => w.flush(),
                TagEnd::Emphasis
                | TagEnd::Strong
                | TagEnd::Strikethrough
                | TagEnd::Link
                | TagEnd::BlockQuote(_) => {
                    if matches!(tag, TagEnd::BlockQuote(_)) {
                        w.flush();
                    }
                    w.styles.pop();
                }
                TagEnd::CodeBlock => {
                    w.in_code = false;
                    w.blank();
                }
                TagEnd::TableRow | TagEnd::TableHead => w.flush(),
                TagEnd::TableCell => {
                    let s = Style::default().fg(p.note_fg);
                    w.text(" │", s);
                }
                TagEnd::Table => w.blank(),
                _ => {}
            },
            Event::Text(t) => {
                if w.in_code {
                    let pad = " ".repeat(w.indent() + 2);
                    for line in t.lines() {
                        w.out.push(Line::from(Span::styled(
                            format!("{pad}{line}"),
                            Style::default().fg(p.note_fg),
                        )));
                    }
                } else {
                    let s = w.style();
                    w.text(&t, s);
                }
            }
            Event::Code(t) => {
                let s = code.patch(w.style());
                let s = s.fg(p.hunk_fg);
                w.text(&t, s);
            }
            Event::SoftBreak => w.pending_space = true,
            Event::HardBreak => w.flush(),
            Event::Rule => {
                w.flush();
                w.out.push(Line::from(Span::styled(
                    "─".repeat(w.width.min(40)),
                    Style::default().fg(p.note_fg),
                )));
            }
            Event::TaskListMarker(done) => {
                let s = w.style();
                w.text(if done { "[x]" } else { "[ ]" }, s);
            }
            // Raw HTML (often template comments in PR bodies) is not shown.
            Event::Html(_) | Event::InlineHtml(_) => {}
            _ => {}
        }
    }
    w.flush();
    while w.out.last().is_some_and(|l| l.width() == 0) {
        w.out.pop();
    }
    w.out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(md: &str, width: usize) -> Vec<String> {
        render(md, width, &Palette::dark())
            .iter()
            .map(|l| l.to_string().trim_end().to_string())
            .collect()
    }

    #[test]
    fn paragraphs_wrap_and_lists_hang() {
        let out = text(
            "First line of a long paragraph that keeps going.\n\n- one item that wraps around\n- two",
            24,
        );
        assert_eq!(
            out,
            [
                "First line of a long",
                "paragraph that keeps",
                "going.",
                "",
                "• one item that wraps",
                "  around",
                "• two",
            ]
        );
    }

    #[test]
    fn html_comments_are_dropped_and_code_is_kept() {
        let out = text("<!-- template -->\nFix it.\n\n```rust\nlet x = 1;\n```", 40);
        assert_eq!(out, ["Fix it.", "", "  rust", "  let x = 1;"]);
    }

    #[test]
    fn soft_breaks_reflow() {
        let out = text("A commit body wrapped\nat seventy-two columns.", 80);
        assert_eq!(out, ["A commit body wrapped at seventy-two columns."]);
    }
}
