//! What frames the body: the status bar, the agent's waiting request, the
//! lists that float above the status bar, and the screen with nothing on
//! it.

use std::sync::Arc;

use crossterm::event::KeyCode;
use gpui::prelude::*;
use gpui::{Context, Div, FontWeight, Hsla, Image, ImageFormat, MouseButton, Pixels, div, img, px};

use super::measure::{Metrics, PAD_X};
use super::{Viewer, hsla};
use crate::ui::panel::{Kind, Panel};
use crate::ui::{Files, Notice, Parts, STICKER_MAX_WIDTH, Status};

/// The sticker, as an image.
#[derive(Clone)]
pub struct Sticker {
    image: Arc<Image>,
    width: f32,
    height: f32,
}

impl Sticker {
    pub fn new(png: &'static [u8]) -> Self {
        // The size from the PNG's header: width and height follow "IHDR".
        let dim = |at: usize| {
            png.get(at..at + 4)
                .map(|b| u32::from_be_bytes([b[0], b[1], b[2], b[3]]) as f32)
                .unwrap_or(1.0)
        };
        Self {
            image: Arc::new(Image::from_bytes(ImageFormat::Png, png.to_vec())),
            width: dim(16),
            height: dim(20),
        }
    }
}

/// A row of a list.
fn item_h(m: &Metrics) -> f32 {
    m.ui_line + 6.0
}

/// How tall an open list's card is with `rows` rows.
pub fn panel_h(m: &Metrics, rows: usize) -> f32 {
    m.ui_line + 16.0 + rows as f32 * item_h(m) + 2.0
}

impl Viewer {
    /// The rows an open list shows: as many as the terminal's would.
    pub(super) fn list_rows(&self) -> usize {
        let lines = (f32::from(self.body.size.height) / self.m.line) as u16;
        Panel::height_for(lines).saturating_sub(1).max(1) as usize
    }

    /// A key, drawn as one.
    fn keycap(&self, key: impl Into<gpui::SharedString>, color: Hsla) -> Div {
        div()
            .px(px(5.0))
            .rounded_sm()
            .border_1()
            .border_color(color.opacity(0.45))
            .text_color(color)
            .text_size(self.m.ui_size * 0.85)
            .font_weight(FontWeight::SEMIBOLD)
            .child(key.into())
    }

    /// `text` in the proportional font, with the terminal font's icons
    /// (the private use area, where Nerd Fonts put them) in that font.
    fn with_icons(&self, text: &str) -> Vec<Div> {
        let icon = |c: char| matches!(c, '\u{e000}'..='\u{f8ff}' | '\u{f0000}'..='\u{ffffd}');
        let mut out = Vec::new();
        let mut rest = text;
        while !rest.is_empty() {
            let cut = rest
                .char_indices()
                .find(|&(i, c)| icon(c) != icon(rest[..].chars().next().unwrap_or(' ')) && i > 0)
                .map_or(rest.len(), |(i, _)| i);
            let (piece, next) = rest.split_at(cut);
            let div = div().flex_none().child(piece.to_string());
            out.push(if piece.chars().next().is_some_and(icon) {
                div.font_family(self.m.mono.clone()).mr(px(4.0))
            } else if next.is_empty() {
                div.flex_shrink()
                    .min_w_0()
                    .overflow_hidden()
                    .text_ellipsis()
            } else {
                div
            });
            rest = next;
        }
        out
    }

    pub(super) fn status(&self, height: Pixels) -> Div {
        let fg = hsla(self.theme.fg);
        let dim = fg.opacity(0.7);
        let bar = div()
            .h(height)
            .w_full()
            .flex_none()
            .flex()
            .items_center()
            .px(px(PAD_X))
            .bg(hsla(self.theme.chrome))
            .border_t_1()
            .border_color(hsla(self.theme.selection))
            .text_color(fg)
            .overflow_hidden()
            .whitespace_nowrap();
        match self.core.status() {
            Status::Strip(names) => {
                let mut bar = bar;
                for (name, here) in names {
                    let Some(name) = name else { continue };
                    let chip = div().mr(px(6.0)).px(px(8.0)).rounded_sm().child(name);
                    bar = bar.child(if here {
                        chip.bg(hsla(self.theme.accent))
                            .text_color(hsla(self.theme.bg))
                            .font_weight(FontWeight::SEMIBOLD)
                    } else {
                        chip.text_color(dim)
                    });
                }
                bar
            }
            Status::Whole(text) => bar.child(text.trim().to_string()),
            Status::Parts(parts) => bar
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .flex()
                        .overflow_hidden()
                        .children(self.with_icons(&parts.scope)),
                )
                .child(self.status_parts(&parts, dim)),
        }
    }

    /// The right of the status bar: each part that applies, apart.
    fn status_parts(&self, parts: &Parts, dim: Hsla) -> Div {
        let p = &self.core.palette;
        let gap = px(16.0);
        let mut right = div().ml(gap).flex().items_center().text_color(dim);
        if parts.labels.len() > 1 {
            let mut labels = div().flex().mr(gap);
            for l in &parts.labels {
                labels = labels.child(
                    div()
                        .mr(px(4.0))
                        .child(self.badge(l.clone(), self.fg(p.mark_fg))),
                );
            }
            right = right.child(labels);
        }
        if let Some((n, unread)) = parts.comments {
            let mut c = div()
                .mr(gap)
                .flex()
                .items_center()
                .child(format!("{n} comment{}", if n == 1 { "" } else { "s" }));
            if unread > 0 {
                c = c.child(
                    div()
                        .ml(px(6.0))
                        .px(px(6.0))
                        .rounded_full()
                        .bg(self.fg(p.comment_fg))
                        .text_color(hsla(self.theme.bg))
                        .text_size(self.m.ui_size * 0.85)
                        .font_weight(FontWeight::SEMIBOLD)
                        .child(format!("{unread} unread")),
                );
            }
            right = right.child(c);
        }
        match &parts.marks {
            Some((Some((label, i)), n)) => {
                // In the color of who made it.
                let author = self
                    .core
                    .cur()
                    .and_then(|l| l.view.marks().iter().find(|m| m.label == *label))
                    .map_or(crate::ui::diffview::Author::Agent, |m| m.author);
                right = right.child(
                    div()
                        .mr(gap)
                        .flex()
                        .items_center()
                        .child(self.badge(label.clone(), self.fg(p.mark_colors(author).1)))
                        .child(div().ml(px(6.0)).child(format!("{i} of {n}"))),
                )
            }
            Some((None, n)) => {
                right = right.child(
                    div()
                        .mr(gap)
                        .child(format!("{n} mark{}", if *n == 1 { "" } else { "s" })),
                )
            }
            None => {}
        }
        let changes = |added: usize, removed: usize| {
            div()
                .ml(px(8.0))
                .flex()
                .font_family(self.m.mono.clone())
                .text_size(self.m.mono_size)
                .child(
                    div()
                        .text_color(self.fg(ratatui::style::Color::Green))
                        .child(format!("+{added}")),
                )
                .child(
                    div()
                        .ml(px(4.0))
                        .text_color(self.fg(ratatui::style::Color::Red))
                        .child(format!("−{removed}")),
                )
        };
        right = match parts.files {
            Files::None => right,
            Files::Plain { at, of } => {
                right.child(div().mr(gap).child(format!("File {at} of {of}")))
            }
            Files::One {
                at,
                of,
                added,
                removed,
            } => right.child(
                div()
                    .mr(gap)
                    .flex()
                    .items_center()
                    .child(format!("File {at} of {of}"))
                    .child(changes(added, removed)),
            ),
            Files::Diff {
                shown,
                of,
                added,
                removed,
                at,
            } => {
                let noun = if of == 1 { "file" } else { "files" };
                let count = if shown < of {
                    format!("{shown} of {of} {noun}")
                } else {
                    format!("{of} {noun}")
                };
                let mut d = div()
                    .mr(gap)
                    .flex()
                    .items_center()
                    .child(count)
                    .child(changes(added, removed));
                if let Some(i) = at {
                    d = d.child(div().ml(px(10.0)).child(format!("{}/{shown}", i + 1)));
                }
                right.child(d)
            }
        };
        if let Some(label) = &parts.location {
            right = right.child(
                div()
                    .mr(gap)
                    .px(px(6.0))
                    .rounded_sm()
                    .bg(hsla(self.theme.fg).opacity(0.08))
                    .child(label.clone()),
            );
        }
        right
            .child(self.keycap(":", dim))
            .child(div().ml(px(6.0)).child("Commands"))
    }

    /// The agent's request, held while the user has unread comments.
    pub(super) fn banner(&self, height: Pixels, words: String, cx: &mut Context<Self>) -> Div {
        let p = &self.core.palette;
        let (bg, fg) = (self.fg(p.comment_bg), self.fg(p.comment_fg));
        let button = |id: &'static str, key: &'static str, label: &'static str| {
            div()
                .id(id)
                .ml(px(10.0))
                .flex()
                .items_center()
                .px(px(8.0))
                .rounded_sm()
                .border_1()
                .border_color(fg.opacity(0.6))
                .cursor_pointer()
                .hover(|s| s.bg(fg.opacity(0.15)))
                .child(label)
                .child(div().ml(px(6.0)).child(self.keycap(key, fg)))
        };
        div()
            .h(height)
            .w_full()
            .flex_none()
            .flex()
            .items_center()
            .px(px(PAD_X))
            .bg(bg)
            .text_color(fg)
            .overflow_hidden()
            .whitespace_nowrap()
            .child(
                div()
                    .font_weight(FontWeight::SEMIBOLD)
                    .child(format!("The agent wants to show {words}")),
            )
            .child(div().flex_1())
            .child(
                button("banner-switch", "y", "Switch")
                    .on_click(cx.listener(|this, _, _, cx| this.press(KeyCode::Char('y'), cx))),
            )
            .child(
                button("banner-drop", "Esc", "Dismiss")
                    .on_click(cx.listener(|this, _, _, cx| this.press(KeyCode::Esc, cx))),
            )
    }

    /// The open list, floating above the status bar. The comment editor
    /// opens in the body instead.
    pub(super) fn panel(&mut self, above: Pixels, cx: &mut Context<Self>) -> Option<Div> {
        let rows = self.list_rows();
        let info = self.core.mode_label();
        let p = &self.core.palette;
        let (note, accent, key) = (
            self.fg(p.note_fg),
            hsla(self.theme.accent),
            self.fg(p.key_fg),
        );
        let fg = hsla(self.theme.fg);
        let theme = self.theme.clone();
        let m = self.m.clone();
        // A comment is written in the body, unless its place is not shown.
        let inline = self.heights.editor.borrow().is_some();
        let panel = self
            .core
            .panel
            .as_mut()
            .filter(|p| p.kind != Kind::Comment || !inline)?;
        let kind = panel.kind;
        let title = kind.title();
        let info = panel.note.clone().unwrap_or(info);
        let empty = panel.empty_hint().trim().to_string();
        let items: Vec<_> = panel
            .visible(rows)
            .into_iter()
            .map(|(item, selected)| {
                let tag = item
                    .tag
                    .as_ref()
                    .map(|(t, c)| (t.clone(), theme.color(*c).map_or(hsla(theme.fg), hsla)));
                (tag, item.text.clone(), item.hint.clone(), selected)
            })
            .collect();
        let item_h = item_h(&m);

        let input = div()
            .h(px(m.ui_line + 16.0))
            .px(px(12.0))
            .flex()
            .items_center()
            .border_b_1()
            .border_color(hsla(theme.selection))
            .whitespace_nowrap()
            .overflow_hidden()
            .child(
                div()
                    .mr(px(10.0))
                    .px(px(7.0))
                    .rounded_sm()
                    .bg(accent.opacity(0.18))
                    .text_color(accent)
                    .font_weight(FontWeight::SEMIBOLD)
                    .child(title),
            )
            .child(div().flex_1().min_w_0().child(self.field.clone()))
            .child(div().ml(px(12.0)).flex_none().text_color(note).child(info));

        let mut list = div()
            .h(px(item_h * rows as f32))
            .py(px(1.0))
            .flex()
            .flex_col()
            .overflow_hidden();
        if items.is_empty() {
            list = list.child(
                div()
                    .h(px(item_h))
                    .px(px(14.0))
                    .flex()
                    .items_center()
                    .text_color(note)
                    .child(empty),
            );
        }
        for (n, (tag, text, hint, selected)) in items.into_iter().enumerate() {
            let mut row = div()
                .id(("panel-row", n))
                .h(px(item_h))
                .flex_none()
                .mx(px(4.0))
                .px(px(10.0))
                .rounded_sm()
                .flex()
                .items_center()
                .whitespace_nowrap()
                .overflow_hidden()
                .cursor_pointer()
                .hover(|s| s.bg(fg.opacity(0.06)))
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |this, _, _, cx| {
                        cx.stop_propagation();
                        this.choose(n, cx);
                    }),
                );
            if selected {
                row = row.bg(accent.opacity(0.18));
            }
            if let Some((tag, color)) = tag {
                row = row.child(
                    div()
                        .mr(px(10.0))
                        .w(px(m.cell * 2.0))
                        .font_family(m.mono.clone())
                        .text_size(m.mono_size)
                        .font_weight(FontWeight::BOLD)
                        .text_color(color)
                        .child(tag),
                );
            }
            row = row.child(
                div()
                    .flex_1()
                    .min_w_0()
                    .overflow_hidden()
                    .text_ellipsis()
                    .when(selected, |d| d.font_weight(FontWeight::SEMIBOLD))
                    .child(text),
            );
            if !hint.is_empty() {
                // Commands show their key as a key; other lists counts and
                // ages.
                row = row.child(if kind == Kind::Commands {
                    div().ml(px(12.0)).child(self.keycap(hint, key))
                } else {
                    div().ml(px(12.0)).text_color(note).child(hint)
                });
            }
            list = list.child(row);
        }

        let card = div()
            .w_full()
            .max_w(px(920.0))
            .flex()
            .flex_col()
            .bg(hsla(theme.bg))
            .border_1()
            .border_color(hsla(theme.selection))
            .rounded_lg()
            .shadow_lg()
            .overflow_hidden()
            .child(input)
            .child(list);
        Some(
            div()
                .absolute()
                .left_0()
                .right_0()
                .bottom(above + px(8.0))
                .px(px(16.0))
                .flex()
                .justify_center()
                .child(card),
        )
    }

    /// No diff to show: the sticker, and the words under it.
    pub(super) fn empty(&self, notice: &Notice, height: f32) -> Div {
        let p = &self.core.palette;
        let sticker = if p.light {
            &self.sticker_light
        } else {
            &self.sticker_dark
        };
        let mut out = div()
            .size_full()
            .flex()
            .flex_col()
            .items_center()
            .justify_center();
        if notice.sticker {
            // Like the terminal's: at most so wide, and the height less room
            // for the words.
            let room_h = height - self.m.ui_line * 4.0;
            let w = (self.m.cell * STICKER_MAX_WIDTH as f32)
                .min(room_h * sticker.width / sticker.height);
            if w >= self.m.cell * 16.0 {
                out = out.child(
                    img(sticker.image.clone())
                        .w(px(w))
                        .h(px(w * sticker.height / sticker.width)),
                );
            }
        }
        out.child(
            div()
                .mt(px(self.m.ui_line))
                .text_color(self.fg(p.note_fg))
                .child(notice.text.clone()),
        )
    }
}
