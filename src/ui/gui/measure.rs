//! How big everything in the window's body is: a line of code, a file's
//! header, a comment's card as its text wraps. The view scrolls by these
//! heights and the rows are drawn at them, so the two always agree.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::sync::Arc;

use gpui::{FontWeight, LineFragment, Pixels, SharedString, TextSystem, px};
use ratatui::style::Modifier;

use super::super::diffview::{DiffView, Measure, Row};

/// Room between the window's sides and what is in it.
pub const PAD_X: f32 = 14.0;
/// Room inside a card, and between cards and their neighbours.
pub const CARD_PAD_X: f32 = 12.0;
pub const CARD_PAD_Y: f32 = 8.0;
pub const CARD_MARGIN: f32 = 6.0;
/// The coloured edge down a card's left side.
pub const CARD_EDGE: f32 = 3.0;
/// A card's border, drawn in sight under the cursor and clear elsewhere.
pub const CARD_BORDER: f32 = 1.0;
/// Room above and below the description.
pub const DESC_PAD: f32 = 14.0;
/// Room inside the comment editor, and its row of buttons.
pub const EDITOR_PAD: f32 = 10.0;

/// The fonts and their sizes, and the heights rows are built from.
#[derive(Clone, Debug)]
pub struct Metrics {
    pub mono: SharedString,
    pub ui: SharedString,
    pub mono_size: Pixels,
    pub ui_size: Pixels,
    /// A line of code.
    pub line: f32,
    /// A line of text in the proportional font.
    pub ui_line: f32,
    /// A character of code.
    pub cell: f32,
    pub header: f32,
    pub hunk: f32,
    /// The space between files.
    pub gap: f32,
}

impl Metrics {
    pub fn new(mono: SharedString, mono_size: f32, ui: SharedString, cell: f32) -> Self {
        let ui_size = (mono_size + 1.0).round();
        let line = (mono_size * 1.4).round();
        let ui_line = (ui_size * 1.45).round();
        Self {
            mono,
            ui,
            mono_size: px(mono_size),
            ui_size: px(ui_size),
            line,
            ui_line,
            cell,
            header: (ui_line + 14.0).max(line + 12.0),
            hunk: line + 10.0,
            gap: (line * 1.25).round(),
        }
    }

    /// The width of the column a mark's letter goes in, left of the
    /// line numbers.
    pub fn badge_w(&self) -> f32 {
        (self.cell * 2.0 + 12.0).round()
    }

    /// Where, from the body's left edge, a line's parts start: the old and
    /// new numbers (one column for a file as it is), and after the +/−
    /// marker, the code.
    pub fn columns(&self, digits: usize, plain: bool) -> Columns {
        let number = (self.cell * digits as f32 + 12.0).round();
        let old = PAD_X + self.badge_w();
        let new = if plain { old } else { old + number };
        let marker = new + number;
        Columns {
            old,
            new,
            number,
            code: marker + (self.cell * 2.0).round(),
        }
    }

    /// Where cards start, from the body's left edge.
    pub fn card_left(&self) -> f32 {
        PAD_X + self.badge_w()
    }
}

#[derive(Clone, Copy, Debug)]
pub struct Columns {
    pub old: f32,
    pub new: f32,
    pub number: f32,
    pub code: f32,
}

/// The comment editor, open on a row.
#[derive(Clone, Debug, PartialEq)]
pub struct Editor {
    pub row: usize,
    /// It takes the place of the row's text (editing it) rather than
    /// opening below the row.
    pub replace: bool,
    pub text: String,
}

/// Row heights for the view, in whole pixels.
pub struct Heights {
    pub m: Metrics,
    text: Arc<TextSystem>,
    /// The body's width, which decides where text wraps.
    width: Cell<f32>,
    pub editor: RefCell<Option<Editor>>,
    /// Lines a text takes, by text, width and weight.
    wrapped: RefCell<HashMap<(String, u32, bool), usize>>,
}

impl Heights {
    pub fn new(m: Metrics, text: Arc<TextSystem>) -> Self {
        Self {
            m,
            text,
            width: Cell::new(800.0),
            editor: RefCell::new(None),
            wrapped: RefCell::new(HashMap::new()),
        }
    }

    pub fn set_width(&self, width: f32) {
        if self.width.get() != width {
            self.width.set(width);
            self.wrapped.borrow_mut().clear();
        }
    }

    pub fn width(&self) -> f32 {
        self.width.get()
    }

    /// How wide a card's text may run.
    pub fn card_text_w(&self) -> f32 {
        (self.width()
            - self.m.card_left()
            - PAD_X
            - 2.0 * CARD_PAD_X
            - CARD_EDGE
            - 2.0 * CARD_BORDER)
            .max(40.0)
    }

    /// How wide the description's text may run.
    pub fn desc_text_w(&self) -> f32 {
        (self.width() - 2.0 * (PAD_X + DESC_PAD)).max(40.0)
    }

    /// How many lines `text` takes in the proportional font at `width`,
    /// each line break starting a new one.
    pub fn lines(&self, text: &str, width: f32, bold: bool) -> usize {
        let key = (text.to_string(), width as u32, bold);
        if let Some(n) = self.wrapped.borrow().get(&key) {
            return *n;
        }
        let mut font = gpui::font(self.m.ui.clone());
        if bold {
            font.weight = FontWeight::BOLD;
        }
        let mut wrapper = self.text.line_wrapper(font, self.m.ui_size);
        let n = text
            .split('\n')
            .map(|p| {
                1 + wrapper
                    .wrap_line(&[LineFragment::text(p)], px(width))
                    .count()
            })
            .sum();
        self.wrapped.borrow_mut().insert(key, n);
        n
    }

    /// How tall the text of an editor holding `text` is, `width` wide.
    pub fn field_h(&self, text: &str, width: f32) -> f32 {
        self.lines(text, width, false).max(2) as f32 * self.m.ui_line
    }

    /// The editor under a card's header, in place of its text: the text
    /// box and its buttons.
    pub fn editor_h(&self, text: &str) -> f32 {
        6.0 + self.field_h(text, self.card_text_w()) + 8.0 + self.m.ui_line
    }

    /// The editor's own card below a row: its text box and its buttons,
    /// within its padding and border.
    pub fn editor_card_h(&self, text: &str) -> f32 {
        let field = self.field_h(text, self.card_text_w());
        2.0 * EDITOR_PAD + field + 8.0 + self.m.ui_line + 8.0
    }

    /// The height of row `idx`, in pixels.
    pub fn row_h(&self, view: &DiffView, idx: usize) -> f32 {
        let m = &self.m;
        let Some(row) = view.row(idx) else {
            return m.line;
        };
        let editor = self.editor.borrow();
        let editor = editor.as_ref().filter(|e| e.row == idx);
        // A new comment or reply opens in a card of its own below.
        let below = editor
            .filter(|e| !e.replace)
            .map_or(0.0, |e| self.editor_card_h(&e.text) + 2.0 * CARD_MARGIN);
        let last_of_card = |idx: usize| !matches!(view.row(idx + 1), Some(Row::Reply(..)));
        let base = match row {
            Row::Desc(i) => {
                let line = &view.desc()[i];
                let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
                let bold = line
                    .spans
                    .iter()
                    .any(|s| s.style.add_modifier.contains(Modifier::BOLD));
                let n = self.lines(&text, self.desc_text_w(), bold).max(1);
                let mut h = n as f32 * m.ui_line;
                if i == 0 {
                    h += DESC_PAD;
                }
                if !matches!(view.row(idx + 1), Some(Row::Desc(_))) {
                    h += DESC_PAD;
                }
                h
            }
            Row::Mark(i) => {
                let mark = &view.marks()[i];
                let body = match editor.filter(|e| e.replace) {
                    Some(e) => self.editor_h(&e.text),
                    None => match &mark.note {
                        Some(note) => {
                            4.0 + self.lines(note, self.card_text_w(), false) as f32 * m.ui_line
                        }
                        None => 0.0,
                    },
                };
                // A card's last row keeps a margin below, unless the editor
                // opens there with margins of its own.
                let bottom = if last_of_card(idx) && below == 0.0 {
                    CARD_MARGIN
                } else {
                    0.0
                };
                CARD_MARGIN + 2.0 * (CARD_PAD_Y + CARD_BORDER) + m.ui_line + body + bottom
            }
            Row::Reply(i, r) => {
                let reply = &view.marks()[i].replies[r];
                // The editor sits 6 below the header, the text 2.
                let body = match editor.filter(|e| e.replace) {
                    Some(e) => self.editor_h(&e.text),
                    None => {
                        2.0 + self.lines(&reply.text, self.card_text_w(), false) as f32 * m.ui_line
                    }
                };
                let bottom = if last_of_card(idx) && below == 0.0 {
                    CARD_MARGIN
                } else {
                    0.0
                };
                2.0 * (CARD_PAD_Y + CARD_BORDER) + m.ui_line + body + bottom
            }
            Row::File(_) => m.header,
            Row::Hunk(..) => m.hunk,
            Row::Line(..) => m.line,
            Row::Note(_) => m.line + 4.0,
            Row::Blank => m.gap,
        };
        (base + below).round()
    }
}

impl Measure for Heights {
    fn line(&self) -> usize {
        self.m.line as usize
    }

    fn height(&self, view: &DiffView, idx: usize) -> usize {
        self.row_h(view, idx) as usize
    }
}
