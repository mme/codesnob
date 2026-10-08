//! Syntax highlighting via syntect, mapped to ratatui styles.

use std::collections::HashMap;

use ratatui::style::{Color, Modifier, Style};
use syntect::easy::HighlightLines;
use syntect::highlighting::{FontStyle, Theme, ThemeSet};
use syntect::parsing::SyntaxSet;

/// Styled byte ranges for one line.
pub type Spans = Vec<(Style, usize, usize)>;

pub struct Highlighter {
    syntaxes: SyntaxSet,
    theme: Theme,
    cache: HashMap<(usize, u8), Vec<Spans>>,
}

impl Highlighter {
    pub fn new(theme_name: &str) -> Result<Self, String> {
        let syntaxes = two_face::syntax::extra_newlines();
        let themes: ThemeSet = two_face::theme::extra().into();
        let theme = themes.themes.get(theme_name).cloned().ok_or_else(|| {
            let names: Vec<&str> = themes.themes.keys().map(String::as_str).collect();
            format!(
                "unknown theme {theme_name:?}; available: {}",
                names.join(", ")
            )
        })?;
        Ok(Self {
            syntaxes,
            theme,
            cache: HashMap::new(),
        })
    }

    pub fn theme_names() -> Vec<String> {
        let themes: ThemeSet = two_face::theme::extra().into();
        themes.themes.keys().cloned().collect()
    }

    pub fn clear(&mut self) {
        self.cache.clear();
    }

    /// Highlight every line of a file once and cache the result.
    pub fn lines(&mut self, key: (usize, u8), path: &str, lines: &[String]) -> &[Spans] {
        if !self.cache.contains_key(&key) {
            let spans = self.compute(path, lines);
            self.cache.insert(key, spans);
        }
        &self.cache[&key]
    }

    fn compute(&self, path: &str, lines: &[String]) -> Vec<Spans> {
        let name = path.rsplit('/').next().unwrap_or(path);
        let ext = name.rsplit_once('.').map(|(_, e)| e).unwrap_or("");
        let syntax = self
            .syntaxes
            .find_syntax_by_extension(name)
            .or_else(|| self.syntaxes.find_syntax_by_extension(ext))
            .or_else(|| {
                lines
                    .first()
                    .and_then(|l| self.syntaxes.find_syntax_by_first_line(l))
            });
        let Some(syntax) = syntax else {
            return lines.iter().map(|_| Vec::new()).collect();
        };
        let mut hl = HighlightLines::new(syntax, &self.theme);
        let mut out = Vec::with_capacity(lines.len());
        let mut buf = String::new();
        for line in lines {
            buf.clear();
            buf.push_str(line);
            buf.push('\n');
            let mut spans = Vec::new();
            if let Ok(regions) = hl.highlight_line(&buf, &self.syntaxes) {
                let mut pos = 0;
                for (style, text) in regions {
                    let end = (pos + text.len()).min(line.len());
                    if end > pos {
                        spans.push((to_style(style), pos, end));
                    }
                    pos = end;
                }
            }
            out.push(spans);
        }
        out
    }
}

/// bat's convention: alpha 0 means "ANSI palette index in r",
/// alpha 1 means "terminal default".
fn to_color(c: syntect::highlighting::Color) -> Option<Color> {
    match c.a {
        0 => Some(Color::Indexed(c.r)),
        1 => None,
        _ => Some(Color::Rgb(c.r, c.g, c.b)),
    }
}

fn to_style(s: syntect::highlighting::Style) -> Style {
    let mut style = Style::default();
    if let Some(fg) = to_color(s.foreground) {
        style = style.fg(fg);
    }
    if s.font_style.contains(FontStyle::BOLD) {
        style = style.add_modifier(Modifier::BOLD);
    }
    if s.font_style.contains(FontStyle::ITALIC) {
        style = style.add_modifier(Modifier::ITALIC);
    }
    if s.font_style.contains(FontStyle::UNDERLINE) {
        style = style.add_modifier(Modifier::UNDERLINED);
    }
    style
}
