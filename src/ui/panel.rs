//! The bottom-docked panel: one component for every list (commands now;
//! files, commits and locations later). Type to filter, arrows to move,
//! Enter to choose, Esc to close. It looks and behaves the same in every mode.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use nucleo_matcher::pattern::{CaseMatching, Normalization, Pattern};
use nucleo_matcher::{Config, Matcher, Utf32Str};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};

use super::Palette;
use super::commands::Action;

#[derive(Clone, Copy, PartialEq, Eq, Debug, Hash)]
pub enum Kind {
    Commands,
    Files,
    Commits,
    Open,
    Locations,
}

impl Kind {
    pub fn title(self) -> &'static str {
        match self {
            Kind::Commands => "Commands",
            Kind::Files => "Files",
            Kind::Commits => "Commits",
            Kind::Open => "Open",
            Kind::Locations => "Locations",
        }
    }

    /// Lists whose order means something keep it; the rest put recent first.
    fn recent_first(self) -> bool {
        match self {
            Kind::Commands => true,
            Kind::Files | Kind::Commits | Kind::Open | Kind::Locations => false,
        }
    }

    /// Moving the selection previews the choice behind the panel.
    pub fn previews(self) -> bool {
        matches!(self, Kind::Files | Kind::Commits)
    }
}

pub struct Item {
    /// A short colored prefix that is shown but not searched, like a status letter.
    pub tag: Option<(String, Color)>,
    pub text: String,
    /// Right-aligned: a shortcut, a count, a date.
    pub hint: String,
    pub action: Action,
    /// Stable identity for recency ordering.
    pub key: String,
}

pub enum Outcome {
    /// Keep the panel open.
    Stay,
    Close,
    Choose(Action, String),
    /// Enter in a list that takes typed input (Open): the input, and the
    /// highlighted row if any.
    Submit {
        input: String,
        selected: Option<Action>,
    },
    /// Tab in a list that takes typed input: complete the input.
    Complete,
    Quit,
}

pub struct Panel {
    pub kind: Kind,
    input: String,
    items: Vec<Item>,
    /// Indices into `items`, in display order.
    shown: Vec<usize>,
    selected: usize,
    offset: usize,
    /// Item keys, most recently used first.
    recent: Vec<String>,
    /// Shown on the input line in place of the viewer's status, e.g. why a
    /// list is short.
    pub note: Option<String>,
    matcher: Matcher,
}

impl Panel {
    /// `select` names the item to start on, by key.
    pub fn new(kind: Kind, items: Vec<Item>, recent: Vec<String>, select: Option<&str>) -> Self {
        let mut p = Self {
            kind,
            input: String::new(),
            items,
            shown: Vec::new(),
            selected: 0,
            offset: 0,
            recent,
            note: None,
            matcher: Matcher::new(Config::DEFAULT),
        };
        p.refilter();
        if let Some(key) = select
            && let Some(pos) = p.shown.iter().position(|&i| p.items[i].key == key)
        {
            p.selected = pos;
        }
        p
    }

    pub fn input(&self) -> &str {
        &self.input
    }

    pub fn set_input(&mut self, input: String) {
        self.input = input;
        self.refilter();
    }

    /// Pasted text goes into the input, newlines dropped.
    pub fn paste(&mut self, text: &str) {
        self.input
            .extend(text.chars().filter(|c| *c != '\n' && *c != '\r'));
        self.refilter();
    }

    /// The action of the highlighted row.
    pub fn selected_action(&self) -> Option<Action> {
        self.shown.get(self.selected).map(|&i| self.items[i].action)
    }

    fn recency(&self, i: usize) -> usize {
        if !self.kind.recent_first() {
            return usize::MAX;
        }
        self.recent
            .iter()
            .position(|k| *k == self.items[i].key)
            .unwrap_or(usize::MAX)
    }

    fn refilter(&mut self) {
        let mut order: Vec<(u32, usize, usize)> = Vec::new(); // (score, recency, index)
        if self.input.is_empty() {
            for i in 0..self.items.len() {
                order.push((0, self.recency(i), i));
            }
        } else {
            let pattern = Pattern::parse(&self.input, CaseMatching::Ignore, Normalization::Smart);
            let mut buf = Vec::new();
            for i in 0..self.items.len() {
                let hay = Utf32Str::new(&self.items[i].text, &mut buf);
                if let Some(score) = pattern.score(hay, &mut self.matcher) {
                    order.push((score, self.recency(i), i));
                }
            }
        }
        order.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)).then(a.2.cmp(&b.2)));
        self.shown = order.into_iter().map(|(_, _, i)| i).collect();
        self.selected = 0;
        self.offset = 0;
    }

    pub fn move_by(&mut self, delta: isize) {
        if self.shown.is_empty() {
            return;
        }
        let last = self.shown.len() as isize - 1;
        self.selected = (self.selected as isize + delta).clamp(0, last) as usize;
    }

    pub fn key(&mut self, k: KeyEvent, rows: usize) -> Outcome {
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
        let alt = k.modifiers.contains(KeyModifiers::ALT);
        match (k.code, ctrl) {
            (KeyCode::Char('c'), true) => return Outcome::Quit,
            (KeyCode::Esc, _) => return Outcome::Close,
            (KeyCode::Enter, _) if self.kind == Kind::Open => {
                return Outcome::Submit {
                    input: self.input.clone(),
                    selected: self.selected_action(),
                };
            }
            (KeyCode::Tab, _) if self.kind == Kind::Open => return Outcome::Complete,
            (KeyCode::Enter, _) => {
                return match self.shown.get(self.selected) {
                    Some(&i) => Outcome::Choose(self.items[i].action, self.items[i].key.clone()),
                    None => Outcome::Stay,
                };
            }
            (KeyCode::Down, _) | (KeyCode::Tab, _) | (KeyCode::Char('n'), true) => self.move_by(1),
            (KeyCode::Up, _) | (KeyCode::BackTab, _) | (KeyCode::Char('p'), true) => {
                self.move_by(-1)
            }
            (KeyCode::PageDown, _) => self.move_by(rows.max(1) as isize),
            (KeyCode::PageUp, _) => self.move_by(-(rows.max(1) as isize)),
            (KeyCode::Backspace, _) => {
                self.input.pop();
                self.refilter();
            }
            (KeyCode::Char('u'), true) => {
                self.input.clear();
                self.refilter();
            }
            (KeyCode::Char('w'), true) => {
                let trimmed = self.input.trim_end().len();
                let cut = self.input[..trimmed].rfind(' ').map(|i| i + 1).unwrap_or(0);
                self.input.truncate(cut);
                self.refilter();
            }
            (KeyCode::Char(c), false) if !alt => {
                self.input.push(c);
                self.refilter();
            }
            _ => {}
        }
        Outcome::Stay
    }

    /// Rows the panel wants for an area of `height` lines above the status bar.
    pub fn height_for(height: u16) -> u16 {
        let want = (height / 3).clamp(6, 14);
        want.min(height.saturating_sub(3))
    }

    /// Draws the panel and returns where the text cursor belongs.
    pub fn render(&mut self, area: Rect, buf: &mut Buffer, p: &Palette, info: &str) -> (u16, u16) {
        let bar = Style::default().bg(p.header_bg);
        let dim = Style::default().fg(p.note_fg);

        // Input line: mode title, query, and dim context on the right.
        let input_area = Rect::new(area.x, area.y, area.width, 1);
        buf.set_style(input_area, bar);
        let title = format!(" {} ", self.kind.title());
        let line = Line::from(vec![
            Span::styled(title.clone(), bar.add_modifier(Modifier::BOLD)),
            Span::styled("› ", bar.fg(p.hunk_fg)),
            Span::styled(self.input.clone(), bar),
        ]);
        let used = buf.set_line(area.x, area.y, &line, area.width).0;
        let info = self.note.as_deref().unwrap_or(info);
        let iw = info.chars().count() as u16 + 1;
        if area.right() > used + iw + 2 {
            buf.set_span(
                area.right() - iw,
                area.y,
                &Span::styled(info.to_string(), bar.fg(p.note_fg)),
                iw,
            );
        }
        let cursor = (used.min(area.right().saturating_sub(1)), area.y);

        // The list.
        let rows = area.height.saturating_sub(1) as usize;
        if self.selected < self.offset {
            self.offset = self.selected;
        } else if self.selected >= self.offset + rows {
            self.offset = self.selected + 1 - rows;
        }
        if self.shown.is_empty() {
            let text = match self.kind {
                Kind::Open if self.items.is_empty() => "  type a path, Tab completes",
                Kind::Open => "  Enter opens the path",
                _ => "  no matches",
            };
            buf.set_span(area.x, area.y + 1, &Span::styled(text, dim), area.width);
        }
        for (n, &i) in self.shown.iter().skip(self.offset).take(rows).enumerate() {
            let y = area.y + 1 + n as u16;
            let row = Rect::new(area.x, y, area.width, 1);
            let selected = self.offset + n == self.selected;
            let style = if selected {
                Style::default()
                    .bg(p.header_bg)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default()
            };
            buf.set_style(row, style);
            let marker = if selected { "› " } else { "  " };
            let item = &self.items[i];
            let mut spans = vec![Span::styled(marker, style.fg(p.hunk_fg))];
            if let Some((tag, color)) = &item.tag {
                spans.push(Span::styled(format!("{tag} "), style.fg(*color)));
            }
            spans.push(Span::styled(item.text.clone(), style));
            buf.set_line(area.x, y, &Line::from(spans), area.width);
            let hw = item.hint.chars().count() as u16 + 2;
            if !item.hint.is_empty() && area.width > hw + 10 {
                buf.set_span(
                    area.right() - hw,
                    y,
                    &Span::styled(item.hint.clone(), style.fg(p.note_fg)),
                    hw,
                );
            }
        }
        cursor
    }
}
