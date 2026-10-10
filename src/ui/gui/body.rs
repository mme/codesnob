//! The body: the view's rows as native elements, each at the height
//! `measure` gives it, scrolled by the pixel; the file header that sticks
//! at the top; the scroll bar; and what the mouse does over them.

use crossterm::event::KeyCode;
use gpui::prelude::*;
use gpui::{
    Context, Div, FontStyle, FontWeight, HighlightStyle, Hsla, MouseButton, MouseDownEvent, Pixels,
    Point, SharedString, StrikethroughStyle, StyledText, UnderlineStyle, div, px,
};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::Span;

use super::measure::{
    CARD_BORDER, CARD_EDGE, CARD_MARGIN, CARD_PAD_X, CARD_PAD_Y, Columns, DESC_PAD, EDITOR_PAD,
    PAD_X,
};
use super::select::{self, Point as TextPoint, Selection};
use super::{Viewer, hsla};
use crate::diff::{Kind as LineKind, Status};
use crate::ui::Compose;
use crate::ui::diffview::{Author, Row};

/// A row as drawn, for the mouse.
pub struct Drawn {
    /// Its top and height, in the window.
    pub top: f32,
    pub h: f32,
    pub row: usize,
    /// Where its code starts, on a line of code.
    pub code_x: Option<f32>,
}

/// What a row shows, gathered while the view is borrowed.
enum Part {
    Desc {
        spans: Vec<Span<'static>>,
        first: bool,
        last: bool,
    },
    File(FileHead),
    Hunk {
        range: String,
        func: Option<String>,
        plain: bool,
    },
    Line(LinePart),
    Mark {
        label: String,
        author: Author,
        note: Option<String>,
        stale: bool,
        re: Option<String>,
        last: bool,
        seq: usize,
    },
    Reply {
        label: String,
        author: Author,
        text: String,
        last: bool,
    },
    Note(&'static str),
    Blank,
}

struct FileHead {
    fi: usize,
    of: usize,
    path: String,
    old_path: Option<String>,
    status: Status,
    added: usize,
    removed: usize,
    binary: bool,
    plain: bool,
    lines: usize,
}

struct LinePart {
    kind: LineKind,
    old_no: Option<usize>,
    new_no: Option<usize>,
    pieces: Vec<(String, Style, bool)>,
    /// The mark over it: its author and letter, and whether this is its
    /// first line.
    mark: Option<(Author, String, bool)>,
    marking: bool,
    target: bool,
    cols: Columns,
}

impl Viewer {
    pub(super) fn body_view(&mut self, cx: &mut Context<Self>) -> Div {
        self.drawn.clear();
        let body_h = f32::from(self.body.size.height);
        let body = div()
            .relative()
            .w_full()
            .h(px(body_h))
            .flex_none()
            .overflow_hidden();
        if let Some(notice) = self.core.notice() {
            return body.child(self.empty(&notice, body_h));
        }
        let m = self.m.clone();
        let heights = self.heights.clone();
        let editor = heights.editor.borrow().clone();
        let selection = self.selection;
        // Gather what the rows in sight show.
        let mut rows: Vec<(usize, f32, f32, Part)> = Vec::new();
        let sticky;
        let cursor;
        {
            let super::Core { locations, hl, .. } = &mut self.core;
            let Some(loc) = locations.first() else {
                return body;
            };
            let v = &loc.view;
            cursor = v.cursor();
            sticky = v.sticky_file().map(|fi| file_head(v, fi));
            let digits = v.number_width();
            // Rows from the top, the first partly scrolled out of sight; the
            // header that sticks floats over them.
            let mut y = -(v.top_hidden() as f32);
            let mut idx = v.scroll;
            while y < body_h && idx < v.row_count() {
                let h = heights.row_h(v, idx);
                let part = match v.row(idx) {
                    Some(Row::Desc(i)) => Part::Desc {
                        spans: v.desc()[i].spans.clone(),
                        first: i == 0,
                        last: !matches!(v.row(idx + 1), Some(Row::Desc(_))),
                    },
                    Some(Row::File(fi)) => Part::File(file_head(v, fi)),
                    Some(Row::Hunk(fi, hi)) => {
                        let f = &v.files[fi];
                        let h = &f.hunks[hi];
                        let range = if f.plain {
                            let end = h.new_start + h.new_count.saturating_sub(1);
                            format!("Lines {}–{end}", h.new_start)
                        } else {
                            format!(
                                "@@ -{},{} +{},{} @@",
                                h.old_start, h.old_count, h.new_start, h.new_count
                            )
                        };
                        Part::Hunk {
                            range,
                            func: h.func.clone(),
                            plain: f.plain,
                        }
                    }
                    Some(Row::Line(fi, hi, li)) => {
                        let f = &v.files[fi];
                        let l = &f.hunks[hi].lines[li];
                        let target = f.plain
                            && v.target
                                .as_ref()
                                .is_some_and(|(p, n)| *p == f.path && l.new_no == Some(*n));
                        Part::Line(LinePart {
                            kind: l.kind,
                            old_no: l.old_no,
                            new_no: l.new_no,
                            pieces: v.code_pieces(fi, hi, li, hl, 0),
                            mark: v
                                .mark_over(fi, hi, li)
                                .map(|(mk, first)| (mk.author, mk.label.clone(), first)),
                            marking: v.marking.is_some_and(|(a, b)| (a..=b).contains(&idx)),
                            target,
                            cols: m.columns(digits, f.plain),
                        })
                    }
                    Some(Row::Mark(i)) => {
                        let mk = &v.marks()[i];
                        Part::Mark {
                            label: mk.label.clone(),
                            author: mk.author,
                            note: mk.note.clone(),
                            stale: mk.stale,
                            re: mk.re.clone(),
                            last: mk.replies.is_empty(),
                            seq: mk.seq,
                        }
                    }
                    Some(Row::Reply(i, r)) => {
                        let mk = &v.marks()[i];
                        let reply = &mk.replies[r];
                        Part::Reply {
                            label: reply.label.clone(),
                            author: reply.author,
                            text: reply.text.clone(),
                            last: r + 1 == mk.replies.len(),
                        }
                    }
                    Some(Row::Note(text)) => Part::Note(text),
                    Some(Row::Blank) | None => Part::Blank,
                };
                rows.push((idx, y, h, part));
                y += h;
                idx += 1;
            }
        }

        // The next file's header pushes the floating one up as it comes.
        let push = rows
            .iter()
            .find(|(_, y, _, p)| matches!(p, Part::File(_)) && *y > 0.0)
            .map(|(_, y, ..)| *y)
            .filter(|y| *y < m.header)
            .map_or(0.0, |y| y - m.header);
        let top = f32::from(self.body.origin.y);
        let mut out = body.cursor_text();
        for (idx, y, h, part) in rows {
            let here = idx == cursor;
            let below = editor
                .as_ref()
                .filter(|e| e.row == idx && !e.replace)
                .map(|_| self.editor_card(cx));
            let in_place = editor.as_ref().is_some_and(|e| e.row == idx && e.replace);
            let code_x = match &part {
                Part::Line(l) => Some(l.cols.code),
                _ => None,
            };
            let selected = selection.and_then(|s| selected_cols(&s, idx, &part));
            let el: gpui::AnyElement = match part {
                Part::Desc { spans, first, last } => {
                    self.desc_row(&spans, first, last).into_any_element()
                }
                Part::File(f) => self.file_header(&f, here).into_any_element(),
                Part::Hunk { range, func, plain } => {
                    self.hunk_row(range, func, plain, here).into_any_element()
                }
                Part::Line(l) => self.line_row(idx, l, here, selected, cx),
                Part::Mark {
                    label,
                    author,
                    note,
                    stale,
                    re,
                    last,
                    seq,
                } => {
                    let text = if in_place {
                        CardText::Editor(self.field.clone())
                    } else {
                        CardText::Text(note)
                    };
                    self.card(
                        idx,
                        CardHead {
                            label,
                            author,
                            stale,
                            re,
                            seq: Some(seq),
                        },
                        text,
                        true,
                        last && below.is_none(),
                        here,
                        cx,
                    )
                }
                Part::Reply {
                    label,
                    author,
                    text,
                    last,
                } => {
                    let text = if in_place {
                        CardText::Editor(self.field.clone())
                    } else {
                        CardText::Text(Some(text))
                    };
                    self.card(
                        idx,
                        CardHead {
                            label,
                            author,
                            stale: false,
                            re: None,
                            seq: None,
                        },
                        text,
                        false,
                        last && below.is_none(),
                        here,
                        cx,
                    )
                }
                Part::Note(text) => div()
                    .h(px(self.m.line + 4.0))
                    .pl(px(self.m.card_left()))
                    .flex()
                    .items_center()
                    .italic()
                    .text_color(self.fg(self.core.palette.note_fg))
                    .child(text)
                    .into_any_element(),
                Part::Blank => div().into_any_element(),
            };
            let row = div()
                .absolute()
                .top(px(y))
                .left_0()
                .right_0()
                .h(px(h))
                .flex()
                .flex_col()
                .child(el)
                .children(below);
            out = out.child(row);
            self.drawn.push(Drawn {
                top: top + y,
                h,
                row: idx,
                code_x,
            });
        }
        if let Some(f) = sticky {
            let fi = f.fi;
            out = out.child(
                div()
                    .id("sticky")
                    .absolute()
                    .top(px(push))
                    .left_0()
                    .right_0()
                    .shadow_sm()
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, _, _, cx| {
                            cx.stop_propagation();
                            if let Some(loc) = this.core.cur_mut() {
                                loc.view.scroll_to_file(fi);
                            }
                            cx.notify();
                        }),
                    )
                    .child(self.file_header(&f, false)),
            );
        }
        out.children(self.scrollbar(cx))
            .children(self.hscrollbar(cx))
    }

    // ---- rows -----------------------------------------------------------------

    fn desc_row(&self, spans: &[Span<'static>], first: bool, last: bool) -> Div {
        let mut row = div()
            .w_full()
            .px(px(PAD_X + DESC_PAD))
            .whitespace_normal()
            .child(self.styled(spans, Style::default(), None));
        if first {
            row = row.pt(px(DESC_PAD));
        }
        if last {
            row = row.pb(px(DESC_PAD));
        }
        row
    }

    fn file_header(&self, f: &FileHead, here: bool) -> Div {
        let p = &self.core.palette;
        let chrome = hsla(self.theme.chrome);
        let bg = if here {
            self.fg(p.cursor_over(Color::Rgb(
                (self.theme.chrome >> 16) as u8,
                (self.theme.chrome >> 8) as u8,
                self.theme.chrome as u8,
            )))
        } else {
            chrome
        };
        let dim = hsla(self.theme.fg).opacity(0.6);
        let (tag, color) = match f.status {
            Status::Added => ("Added", Color::Green),
            Status::Deleted => ("Deleted", Color::Red),
            Status::Modified => ("Modified", Color::Yellow),
            Status::Renamed => ("Renamed", Color::Magenta),
        };
        let color = self.fg(color);
        let (dir, name) = match f.path.rfind('/') {
            Some(i) => (f.path[..=i].to_string(), f.path[i + 1..].to_string()),
            None => (String::new(), f.path.clone()),
        };
        let mut head = div()
            .h(px(self.m.header))
            .w_full()
            .px(px(PAD_X))
            .flex()
            .items_center()
            .bg(bg)
            .border_b_1()
            .border_color(hsla(self.theme.selection))
            .whitespace_nowrap()
            .overflow_hidden();
        if !f.plain {
            head = head.child(
                div()
                    .mr(px(10.0))
                    .px(px(7.0))
                    .rounded_sm()
                    .bg(color.opacity(0.16))
                    .text_color(color)
                    .text_size(self.m.ui_size * 0.85)
                    .font_weight(FontWeight::SEMIBOLD)
                    .child(tag),
            );
        }
        if let Some(old) = &f.old_path {
            head = head.child(div().mr(px(6.0)).text_color(dim).child(format!("{old} →")));
        }
        head = head
            .child(div().text_color(dim).child(dir))
            .child(div().font_weight(FontWeight::SEMIBOLD).child(name));
        let stats = if f.binary {
            div().ml(px(12.0)).text_color(dim).child("binary")
        } else if f.plain {
            let n = f.lines;
            div()
                .ml(px(12.0))
                .text_color(dim)
                .child(format!("{n} line{}", if n == 1 { "" } else { "s" }))
        } else {
            div()
                .ml(px(12.0))
                .flex()
                .font_family(self.m.mono.clone())
                .text_size(self.m.mono_size)
                .child(
                    div()
                        .text_color(self.fg(Color::Green))
                        .child(format!("+{}", f.added)),
                )
                .child(
                    div()
                        .ml(px(6.0))
                        .text_color(self.fg(Color::Red))
                        .child(format!("−{}", f.removed)),
                )
        };
        head.child(stats).child(div().flex_1()).child(
            div()
                .text_color(hsla(self.theme.fg).opacity(0.55))
                .text_size(self.m.ui_size * 0.9)
                .child(format!("{} of {}", f.fi + 1, f.of)),
        )
    }

    fn hunk_row(&self, range: String, func: Option<String>, plain: bool, here: bool) -> Div {
        let p = &self.core.palette;
        let hunk = self.fg(p.hunk_fg);
        let bg = if here {
            self.fg(p.cursor_bg)
        } else {
            hunk.opacity(0.07)
        };
        let mut row = div()
            .h(px(self.m.hunk))
            .w_full()
            .pl(px(self.m.card_left()))
            .flex()
            .items_center()
            .bg(bg)
            .whitespace_nowrap()
            .overflow_hidden();
        row = if plain {
            row.child(div().text_color(hunk).child(range))
        } else {
            row.child(
                div()
                    .font_family(self.m.mono.clone())
                    .text_size(self.m.mono_size)
                    .text_color(hunk.opacity(0.85))
                    .child(range),
            )
        };
        if let Some(func) = func {
            row = row.child(
                div()
                    .ml(px(12.0))
                    .text_color(self.fg(p.note_fg))
                    .child(func.trim().to_string()),
            );
        }
        row
    }

    fn line_row(
        &self,
        idx: usize,
        l: LinePart,
        here: bool,
        selected: Option<(usize, usize)>,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let p = &self.core.palette;
        let m = &self.m;
        let line_bg = match l.kind {
            LineKind::Add => Some(p.add_bg),
            LineKind::Del => Some(p.del_bg),
            LineKind::Context => None,
        };
        // Highlighted context lines take the highlight's tint, as in the
        // terminal; changed lines keep their color.
        let line_bg = match (line_bg, &l.mark, l.marking) {
            (None, _, true) => Some(p.comment_bg),
            (None, Some((author, ..)), _) => Some(p.mark_colors(*author).0),
            (bg, ..) => bg,
        };
        let bg = match (line_bg, here) {
            (bg, true) => Some(self.fg(p.cursor_over(bg.unwrap_or(Color::Reset)))),
            (Some(bg), false) => self.color(bg),
            (None, false) => None,
        };
        let emph = match l.kind {
            LineKind::Add => self.color(p.add_emph),
            LineKind::Del => self.color(p.del_emph),
            LineKind::Context => None,
        };
        let numbers = if here {
            self.fg(p.cursor_fg)
        } else {
            self.fg(p.gutter_fg)
        };
        let group = SharedString::from(format!("line-{idx}"));
        let number = |n: Option<usize>| {
            div()
                .w(px(l.cols.number))
                .pr(px(8.0))
                .flex()
                .justify_end()
                .text_color(numbers)
                .when(here, |d| d.font_weight(FontWeight::BOLD))
                .child(n.map(|n| n.to_string()).unwrap_or_default())
        };
        let mut row = div()
            .id(("line", idx))
            .group(group.clone())
            .relative()
            .h(px(m.line))
            .w_full()
            .flex()
            .items_center()
            .font_family(m.mono.clone())
            .text_size(m.mono_size)
            .line_height(px(m.line))
            .whitespace_nowrap()
            .overflow_hidden()
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, ev: &MouseDownEvent, _, cx| this.down_on_line(idx, ev, cx)),
            );
        if let Some(bg) = bg {
            row = row.bg(bg);
        } else {
            row = row.hover(|s| s.bg(hsla(self.theme.fg).opacity(0.04)));
        }
        // The mark's edge, and its letter on its first line; elsewhere a
        // + to comment, on hover.
        let edge = match (&l.mark, l.marking) {
            (_, true) => Some(self.fg(p.comment_fg)),
            (Some((author, ..)), _) => Some(self.fg(p.mark_colors(*author).1)),
            _ => None,
        };
        if let Some(edge) = edge {
            row = row.child(
                div()
                    .absolute()
                    .left(px(PAD_X * 0.5))
                    .top_0()
                    .bottom_0()
                    .w(px(CARD_EDGE))
                    .bg(edge),
            );
        }
        let badge = match &l.mark {
            Some((author, label, true)) => {
                let color = self.fg(p.mark_colors(*author).1);
                self.badge(label.clone(), color)
            }
            _ => div()
                .px(px(4.0))
                .rounded_sm()
                .bg(hsla(self.theme.accent))
                .text_color(hsla(self.theme.bg))
                .font_weight(FontWeight::BOLD)
                .invisible()
                .group_hover(group, |s| s.visible())
                .child("+"),
        };
        row = row.child(
            div()
                .ml(px(PAD_X))
                .w(px(m.badge_w()))
                .flex()
                .items_center()
                .child(badge),
        );
        if l.cols.new != l.cols.old {
            row = row.child(number(l.old_no));
        }
        row = row.child(number(l.new_no));
        let (marker, marker_color) = match l.kind {
            _ if l.target => ("▶", self.fg(p.hunk_fg)),
            LineKind::Add => ("+", self.fg(Color::Green)),
            LineKind::Del => ("−", self.fg(Color::Red)),
            LineKind::Context => ("", numbers),
        };
        row = row.child(
            div()
                .w(px(m.cell * 2.0))
                .text_color(marker_color)
                .font_weight(FontWeight::BOLD)
                .child(marker),
        );
        let text = code_text(
            &l.pieces,
            |style, emphasized| {
                let mut h = self.highlight(style, Style::default());
                if emphasized {
                    h.background_color = emph;
                }
                h
            },
            selected,
            hsla(self.theme.selection),
        );
        row.child(
            div()
                .flex_1()
                .min_w_0()
                .overflow_hidden()
                .child(div().ml(px(-self.hpx)).child(text)),
        )
        .into_any_element()
    }

    pub(super) fn badge(&self, label: String, color: Hsla) -> Div {
        div()
            .px(px(5.0))
            .rounded_sm()
            .bg(color)
            .text_color(hsla(self.theme.bg))
            .font_weight(FontWeight::BOLD)
            .text_size(self.m.ui_size * 0.85)
            .line_height(px(self.m.ui_line * 0.95))
            .font_family(self.m.ui.clone())
            .child(label)
    }

    #[allow(clippy::too_many_arguments)]
    fn card(
        &self,
        idx: usize,
        head: CardHead,
        text: CardText,
        first: bool,
        last: bool,
        here: bool,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let p = &self.core.palette;
        let (tint, ink) = p.mark_colors(head.author);
        let tint_c = if here { p.cursor_over(tint) } else { tint };
        let (bg, ink) = (self.fg(tint_c), self.fg(ink));
        let who = match (head.author, &head.re) {
            (Author::Agent, _) => "Agent".to_string(),
            (Author::User, Some(re)) => format!("You, on {re}"),
            (Author::User, None) => "You".to_string(),
        };
        let group = SharedString::from(format!("card-{idx}"));
        let action = |id: &'static str, label: &'static str, key: KeyCode| {
            div()
                .id((id, idx))
                .ml(px(6.0))
                .px(px(8.0))
                .rounded_sm()
                .border_1()
                .border_color(ink.opacity(0.4))
                .text_color(ink)
                .text_size(self.m.ui_size * 0.9)
                .cursor_pointer()
                .hover(|s| s.bg(ink.opacity(0.12)))
                .child(label)
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |this, _, _, cx| {
                        cx.stop_propagation();
                        this.press_on(idx, key, cx);
                    }),
                )
        };
        // What Enter, e and x do on this row, as buttons.
        let mut actions = div()
            .flex()
            .invisible()
            .group_hover(group.clone(), |s| s.visible());
        actions = match head.author {
            Author::Agent => actions.child(action("reply", "Reply", KeyCode::Enter)),
            Author::User => actions
                .child(action("edit", "Edit", KeyCode::Char('e')))
                .child(action("delete", "Delete", KeyCode::Char('x'))),
        };
        let mut header = div()
            .h(px(self.m.ui_line))
            .flex()
            .items_center()
            .whitespace_nowrap()
            .child(self.badge(head.label, ink))
            .child(
                div()
                    .ml(px(8.0))
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(ink)
                    .child(who),
            );
        if head.stale {
            header = header.child(
                div()
                    .ml(px(8.0))
                    .px(px(6.0))
                    .rounded_sm()
                    .border_1()
                    .border_color(ink.opacity(0.5))
                    .text_size(self.m.ui_size * 0.85)
                    .text_color(ink)
                    .child("lines gone"),
            );
        }
        let header = header.child(div().flex_1()).child(actions);
        let body = match text {
            CardText::Text(Some(t)) => Some(
                div()
                    .mt(px(if head.seq.is_some() { 4.0 } else { 2.0 }))
                    .w(px(self.heights.card_text_w()))
                    .whitespace_normal()
                    .child(t),
            ),
            CardText::Text(None) => None,
            CardText::Editor(field) => Some(self.editor_body(field, head.seq.is_some(), cx)),
        };
        let mut card = div()
            .id(("card", idx))
            .group(group)
            .relative()
            .ml(px(self.m.card_left()))
            .mr(px(PAD_X))
            .pl(px(CARD_PAD_X + CARD_EDGE))
            .pr(px(CARD_PAD_X))
            .py(px(CARD_PAD_Y))
            .bg(bg)
            .border(px(CARD_BORDER))
            .border_color(if here { ink } else { gpui::transparent_black() })
            .flex()
            .flex_col()
            .child(edge(ink))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, _, _, cx| {
                    if let Some(loc) = this.core.cur_mut() {
                        loc.view.click_row(idx);
                    }
                    this.selection = None;
                    cx.notify();
                }),
            )
            .child(header)
            .children(body);
        if first {
            card = card.mt(px(CARD_MARGIN)).rounded_tr_md();
        }
        if last {
            card = card.mb(px(CARD_MARGIN)).rounded_br_md();
        }
        div().w_full().child(card).into_any_element()
    }

    /// The editor in its own card, below the lines or the comment it
    /// writes about.
    fn editor_card(&self, cx: &mut Context<Self>) -> Div {
        div().w_full().child(
            div()
                .ml(px(self.m.card_left()))
                .mr(px(PAD_X))
                .my(px(CARD_MARGIN))
                .p(px(EDITOR_PAD))
                .bg(hsla(self.theme.bg))
                .border_1()
                .border_color(hsla(self.theme.accent))
                .rounded_md()
                .shadow_md()
                .child(self.editor_body(self.field.clone(), false, cx)),
        )
    }

    /// The text box and its buttons.
    fn editor_body(
        &self,
        field: gpui::Entity<super::field::TextField>,
        editing: bool,
        cx: &mut Context<Self>,
    ) -> Div {
        let dim = self.fg(self.core.palette.note_fg);
        let save = match (&self.core.composing, editing) {
            (_, true) => "Save",
            (
                Some(Compose::Reply {
                    editing: Some(_), ..
                }),
                _,
            ) => "Save",
            (Some(Compose::Reply { .. }), _) => "Reply",
            _ => "Comment",
        };
        let button =
            |id: &'static str, label: &'static str, primary: bool, cx: &mut Context<Self>| {
                let accent = hsla(self.theme.accent);
                let key = if primary {
                    KeyCode::Enter
                } else {
                    KeyCode::Esc
                };
                div()
                    .id(id)
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, _, _, cx| {
                            cx.stop_propagation();
                            this.press(key, cx);
                        }),
                    )
                    .ml(px(8.0))
                    .px(px(10.0))
                    .rounded_sm()
                    .cursor_pointer()
                    .when(primary, |d| {
                        d.bg(accent)
                            .text_color(hsla(self.theme.bg))
                            .font_weight(FontWeight::SEMIBOLD)
                    })
                    .when(!primary, |d| {
                        d.border_1()
                            .border_color(dim.opacity(0.6))
                            .hover(|s| s.bg(dim.opacity(0.15)))
                    })
                    .child(label)
            };
        div()
            .w_full()
            .flex()
            .flex_col()
            .child(
                div()
                    .mt(px(6.0))
                    .w(px(self.heights.card_text_w()))
                    .child(field),
            )
            .child(
                div()
                    .mt(px(8.0))
                    .h(px(self.m.ui_line))
                    .flex()
                    .items_center()
                    .whitespace_nowrap()
                    .child(
                        div()
                            .text_color(dim)
                            .text_size(self.m.ui_size * 0.9)
                            .child("Enter saves · Shift-Enter new line · Esc cancels"),
                    )
                    .child(div().flex_1())
                    .child(button("editor-cancel", "Cancel", false, cx))
                    .child(button("editor-save", save, true, cx)),
            )
    }

    // ---- the scroll bar ---------------------------------------------------------

    /// Where the view is in all its rows.
    pub(super) fn extent(&self) -> Option<Extent> {
        let v = &self.core.cur()?.view;
        let p = &self.core.palette;
        let mut total = 0.0;
        let mut offset = 0.0;
        let mut marks = Vec::new();
        for r in 0..v.row_count() {
            if r == v.scroll {
                offset = total + v.top_hidden() as f32;
            }
            if let Some(Row::Mark(i)) = v.row(r) {
                let color = self.fg(p.mark_colors(v.marks()[i].author).1);
                marks.push((total, color));
            }
            total += self.heights.row_h(v, r);
        }
        Some(Extent {
            offset,
            total,
            marks,
        })
    }

    /// The scroll bar, floating over the right edge: in sight while the
    /// view scrolls or the pointer is over it, fading out after; thicker
    /// under the pointer. Ticks mark where the marks are.
    fn scrollbar(&self, cx: &mut Context<Self>) -> Option<gpui::Stateful<Div>> {
        let Extent {
            offset,
            total,
            marks,
        } = self.extent()?;
        let track = f32::from(self.body.size.height);
        if total <= track {
            return None;
        }
        let thumb_h = (track / total * track).max(32.0);
        let thumb_y =
            (offset / (total - track).max(1.0) * (track - thumb_h)).clamp(0.0, track - thumb_h);
        let fg = hsla(self.theme.fg);
        let dragging = self.thumb_grab.is_some();
        let shown = if dragging { 1.0 } else { self.bar_shown() };
        let mut bar = div()
            .id("scrollbar")
            .group("scrollbar")
            .absolute()
            .top_0()
            .bottom_0()
            .right_0()
            .w(px(16.0))
            .cursor_default()
            .opacity(shown)
            .hover(|s| s.opacity(1.0))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, ev: &MouseDownEvent, _, cx| {
                    cx.stop_propagation();
                    this.coast = None;
                    let y = f32::from(ev.position.y - this.body.origin.y);
                    let grab = if (thumb_y..thumb_y + thumb_h).contains(&y) {
                        y - thumb_y
                    } else {
                        thumb_h / 2.0
                    };
                    this.thumb_grab = Some(grab + f32::from(this.body.origin.y));
                    this.drag_thumb(f32::from(ev.position.y) - this.thumb_grab.unwrap_or(0.0));
                    cx.notify();
                }),
            );
        for (at, color) in marks {
            bar = bar.child(
                div()
                    .absolute()
                    .right(px(1.0))
                    .w(px(3.0))
                    .h(px(2.0))
                    .top(px(at / total * track))
                    .rounded_sm()
                    .bg(color.opacity(0.9)),
            );
        }
        Some(
            bar.child(
                div()
                    .absolute()
                    .right(px(4.0))
                    .top(px(thumb_y + 2.0))
                    .w(px(if dragging { 8.0 } else { 6.0 }))
                    .h(px((thumb_h - 4.0).max(8.0)))
                    .rounded_full()
                    .bg(fg.opacity(if dragging { 0.5 } else { 0.28 }))
                    .group_hover("scrollbar", |s| s.w(px(8.0)).bg(fg.opacity(0.42))),
            ),
        )
    }

    /// The bottom scroll bar, under the code, when a line is wider than its
    /// room: like the right one, in sight while scrolling or under the
    /// pointer.
    fn hscrollbar(&self, cx: &mut Context<Self>) -> Option<gpui::Stateful<Div>> {
        if self.hmax <= 0.0 {
            return None;
        }
        let (left, track) = self.htrack();
        let thumb_w = (self.hroom / (self.hroom + self.hmax) * track).max(32.0);
        let thumb_x = (self.hpx / self.hmax * (track - thumb_w)).clamp(0.0, track - thumb_w);
        let fg = hsla(self.theme.fg);
        let dragging = self.hgrab.is_some();
        let shown = if dragging { 1.0 } else { self.hbar_shown() };
        Some(
            div()
                .id("hscrollbar")
                .group("hscrollbar")
                .absolute()
                .bottom_0()
                .left(px(left))
                .w(px(track))
                .h(px(14.0))
                .cursor_default()
                .opacity(shown)
                .hover(|s| s.opacity(1.0))
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |this, ev: &MouseDownEvent, _, cx| {
                        cx.stop_propagation();
                        let x = f32::from(ev.position.x) - left;
                        let grab = if (thumb_x..thumb_x + thumb_w).contains(&x) {
                            x - thumb_x
                        } else {
                            thumb_w / 2.0
                        };
                        this.hgrab = Some(grab + left);
                        this.drag_hthumb(f32::from(ev.position.x) - this.hgrab.unwrap_or(0.0));
                        cx.notify();
                    }),
                )
                .child(
                    div()
                        .absolute()
                        .bottom(px(4.0))
                        .left(px(thumb_x + 2.0))
                        .w(px((thumb_w - 4.0).max(8.0)))
                        .h(px(if dragging { 8.0 } else { 6.0 }))
                        .rounded_full()
                        .bg(fg.opacity(if dragging { 0.5 } else { 0.28 }))
                        .group_hover("hscrollbar", |s| s.h(px(8.0)).bg(fg.opacity(0.42))),
                ),
        )
    }

    /// Where the bottom scroll bar's track starts and how long it is: under
    /// the code, short of the right one.
    fn htrack(&self) -> (f32, f32) {
        let digits = self.core.cur().map_or(3, |l| l.view.number_width());
        let left = self.m.columns(digits, false).code;
        let width = f32::from(self.body.size.width);
        (left, (width - left - 16.0).max(32.0))
    }

    /// Scroll sideways so the bottom thumb's left is at `x` in the window.
    pub(super) fn drag_hthumb(&mut self, x: f32) {
        let (left, track) = self.htrack();
        let thumb_w = (self.hroom / (self.hroom + self.hmax) * track).max(32.0);
        let room = (track - thumb_w).max(1.0);
        self.hpx = ((x - left).clamp(0.0, room) / room) * self.hmax;
        self.hscrolled = Some(std::time::Instant::now());
    }

    /// Scroll so the thumb's top is at `y`, from the body's top.
    pub(super) fn drag_thumb(&mut self, y: f32) {
        let Some(Extent { offset, total, .. }) = self.extent() else {
            return;
        };
        let track = f32::from(self.body.size.height);
        let thumb_h = (track / total * track).max(28.0);
        let room = (track - thumb_h).max(1.0);
        let target = (y.clamp(0.0, room) / room) * (total - track).max(0.0);
        self.glide(target - offset);
        self.scrolled_now();
    }

    /// While the editor is open, keep it in sight: scroll its row up when
    /// it would end below the body.
    pub(super) fn keep_editor_in_sight(&mut self) {
        let Some(editor) = self.heights.editor.borrow().clone() else {
            return;
        };
        let Some(loc) = self.core.cur() else { return };
        let v = &loc.view;
        // Below the header floating over the top, and above the bottom; its
        // top first, when it is taller than the room.
        let cover = v.cover() as f32;
        let y = v.y_of(editor.row) as f32;
        let bottom = y + self.heights.row_h(v, editor.row);
        let body = f32::from(self.body.size.height);
        let by = if y < cover {
            y - cover
        } else if bottom > body {
            (bottom - body).min(y - cover)
        } else {
            0.0
        };
        if by.abs() >= 1.0 {
            self.glide(by);
        }
    }

    // ---- the mouse ------------------------------------------------------------

    fn down_on_line(&mut self, idx: usize, ev: &MouseDownEvent, cx: &mut Context<Self>) {
        cx.stop_propagation();
        // A click stops the view coasting.
        self.coast = None;
        if self.core.panel.is_some() {
            return;
        }
        let x = f32::from(ev.position.x);
        // The column left of the numbers comments on the line.
        if x < PAD_X + self.m.badge_w() && ev.click_count == 1 {
            self.press_on(idx, KeyCode::Enter, cx);
            return;
        }
        if let Some(loc) = self.core.cur_mut() {
            loc.view.click_row(idx);
        }
        let on_char = ev.click_count == 2;
        self.selection = self.point_at(ev.position, false, on_char).map(|p| {
            let text = self.row_text(p.row);
            match ev.click_count {
                1 => Selection::at(p),
                2 => {
                    let (start, end) = select::word(&text, p.col);
                    Selection {
                        anchor: TextPoint {
                            row: p.row,
                            col: start,
                        },
                        head: TextPoint {
                            row: p.row,
                            col: end,
                        },
                    }
                }
                _ => Selection {
                    anchor: TextPoint { row: p.row, col: 0 },
                    head: TextPoint {
                        row: p.row,
                        col: text.chars().count(),
                    },
                },
            }
        });
        self.selecting = ev.click_count == 1 && self.selection.is_some();
        if ev.click_count > 1 {
            self.offer_primary(cx);
        }
        cx.notify();
    }

    /// The text under `pos`: the boundary between characters nearest it,
    /// or with `on_char` the character it is on. Outside the rows drawn,
    /// the nearest row when `clamp`, else nothing.
    pub(super) fn point_at(
        &self,
        pos: Point<Pixels>,
        clamp: bool,
        on_char: bool,
    ) -> Option<TextPoint> {
        let y = f32::from(pos.y);
        let d = match self
            .drawn
            .iter()
            .find(|d| (d.top..d.top + d.h).contains(&y))
        {
            Some(d) => d,
            None if clamp && y < self.drawn.first()?.top => self.drawn.first()?,
            None if clamp => self.drawn.last()?,
            None => return None,
        };
        let text_len = self.row_text(d.row).chars().count();
        let col = match d.code_x {
            Some(code_x) => {
                let x = (f32::from(pos.x) - code_x + self.hpx) / self.m.cell;
                let cell = if on_char { x.floor() } else { x.round() }.max(0.0) as usize;
                cell.min(text_len)
            }
            // Other rows are taken whole.
            None if y >= d.top + d.h / 2.0 => text_len,
            None => 0,
        };
        Some(TextPoint { row: d.row, col })
    }

    /// What a selection's columns count in on `row`: the code alone on a
    /// line of code, else the row's text.
    pub(super) fn row_text(&self, row: usize) -> String {
        let Some(loc) = self.core.cur() else {
            return String::new();
        };
        let v = &loc.view;
        if let Some(text) = v.line_text(row) {
            return text.to_string();
        }
        match v.row(row) {
            Some(Row::Desc(i)) => v.desc()[i]
                .spans
                .iter()
                .map(|s| s.content.as_ref())
                .collect(),
            Some(Row::Mark(i)) => v.marks()[i].note.clone().unwrap_or_default(),
            Some(Row::Reply(i, r)) => v.marks()[i].replies[r].text.clone(),
            Some(Row::File(fi)) => v.files[fi].display_path(),
            Some(Row::Note(text)) => text.to_string(),
            _ => String::new(),
        }
    }

    // ---- text ---------------------------------------------------------------------

    /// A terminal style as a run of text: `fill` is what the row behind it
    /// is filled with, which a reversed run takes as its foreground.
    pub(super) fn highlight(&self, style: Style, fill: Style) -> HighlightStyle {
        let mut fg = style.fg.and_then(|c| self.color(c));
        let mut bg = style.bg.and_then(|c| self.color(c));
        let m = style.add_modifier;
        if m.contains(Modifier::REVERSED) {
            let behind = bg
                .or(fill.bg.and_then(|c| self.color(c)))
                .unwrap_or(hsla(self.theme.bg));
            bg = Some(fg.unwrap_or(hsla(self.theme.fg)));
            fg = Some(behind);
        }
        HighlightStyle {
            color: fg,
            background_color: bg,
            font_weight: m.contains(Modifier::BOLD).then_some(FontWeight::BOLD),
            font_style: m.contains(Modifier::ITALIC).then_some(FontStyle::Italic),
            underline: m.contains(Modifier::UNDERLINED).then(|| UnderlineStyle {
                thickness: px(1.0),
                ..Default::default()
            }),
            strikethrough: m
                .contains(Modifier::CROSSED_OUT)
                .then(|| StrikethroughStyle {
                    thickness: px(1.0),
                    ..Default::default()
                }),
            fade_out: m.contains(Modifier::DIM).then_some(0.4),
        }
    }

    /// Spans as styled text.
    pub(super) fn styled(
        &self,
        spans: &[Span<'static>],
        fill: Style,
        selected: Option<(usize, usize)>,
    ) -> StyledText {
        let pieces: Vec<(String, Style, bool)> = spans
            .iter()
            .map(|s| (s.content.to_string(), s.style, false))
            .collect();
        code_text(
            &pieces,
            |style, _| self.highlight(style, fill),
            selected,
            hsla(self.theme.selection),
        )
    }
}

/// The offset of the view's top in all its rows and their total height,
/// in pixels, and where the marks are, in their colors.
pub(super) struct Extent {
    pub offset: f32,
    pub total: f32,
    marks: Vec<(f32, Hsla)>,
}

struct CardHead {
    label: String,
    author: Author,
    stale: bool,
    re: Option<String>,
    /// The comment's place in the order marks were made, on a mark's own
    /// row; `None` on a reply.
    seq: Option<usize>,
}

enum CardText {
    Text(Option<String>),
    Editor(gpui::Entity<super::field::TextField>),
}

fn file_head(v: &crate::ui::diffview::DiffView, fi: usize) -> FileHead {
    let f = &v.files[fi];
    FileHead {
        fi,
        of: v.files.len(),
        path: f.path.clone(),
        old_path: f
            .old_path
            .clone()
            .filter(|o| f.status == Status::Renamed && *o != f.path),
        status: f.status,
        added: f.added,
        removed: f.removed,
        binary: f.binary,
        plain: f.plain,
        lines: f.new_lines.len(),
    }
}

/// Pieces of text as one run of styled text, `selected` characters behind
/// the selection's color.
fn code_text(
    pieces: &[(String, Style, bool)],
    style: impl Fn(Style, bool) -> HighlightStyle,
    selected: Option<(usize, usize)>,
    selection: Hsla,
) -> StyledText {
    let text: String = pieces.iter().map(|p| p.0.as_str()).collect();
    let byte = |ch: usize| text.char_indices().nth(ch).map_or(text.len(), |(i, _)| i);
    let selected = selected.map(|(s, e)| (byte(s), byte(e)));
    let mut runs = Vec::new();
    let mut at = 0;
    for (piece, st, emph) in pieces {
        let end = at + piece.len();
        let base = style(*st, *emph);
        // Cut at the selection's ends, so each piece is in it or out.
        let mut cuts = vec![at, end];
        if let Some((s, e)) = selected {
            cuts.extend([s, e].into_iter().filter(|&c| c > at && c < end));
        }
        cuts.sort_unstable();
        for w in cuts.windows(2) {
            let (s, e) = (w[0], w[1]);
            if s == e {
                continue;
            }
            let mut h = base;
            if selected.is_some_and(|(ss, se)| ss <= s && e <= se) {
                h.background_color = Some(selection);
            }
            runs.push((s..e, h));
        }
        at = end;
    }
    StyledText::new(text).with_highlights(runs)
}

/// The coloured edge down a card's left side, inside its border.
fn edge(ink: Hsla) -> Div {
    div()
        .absolute()
        .left_0()
        .top_0()
        .bottom_0()
        .w(px(CARD_EDGE))
        .bg(ink)
}

/// The characters of a line's code a selection covers.
fn selected_cols(sel: &Selection, row: usize, part: &Part) -> Option<(usize, usize)> {
    let Part::Line(l) = part else { return None };
    let len: usize = l.pieces.iter().map(|p| p.0.chars().count()).sum();
    sel.cols(row, len)
}
