//! A text field: one line for the prompts, or wrapping lines for comments.
//! It edits the way desktop fields do (caret, selection, words, the
//! clipboard, input methods); what the field holds is the viewer's input,
//! which the window copies over on every edit.

use std::ops::Range;

use gpui::prelude::*;
use gpui::{
    App, Bounds, ClipboardItem, Context, CursorStyle, Element, ElementId, ElementInputHandler,
    Entity, EntityInputHandler, EventEmitter, FocusHandle, GlobalElementId, Hsla, IntoElement,
    KeyDownEvent, LayoutId, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, Pixels,
    Point, SharedString, Style, TextAlign, TextRun, UTF16Selection, UnderlineStyle, Window,
    WrappedLine, div, fill, point, px, relative, size,
};

/// The field's text changed.
pub struct Edited;

pub struct TextField {
    pub focus: FocusHandle,
    text: String,
    selected: Range<usize>,
    /// The caret is at the start of `selected`.
    reversed: bool,
    /// Text an input method is composing.
    marked: Option<Range<usize>>,
    pub placeholder: SharedString,
    /// Wraps, and Shift-Enter starts a new line.
    pub multiline: bool,
    /// How tall the window draws it.
    pub height: Pixels,
    pub caret: Hsla,
    pub selection: Hsla,
    pub muted: Hsla,
    /// The last layout, for the mouse and the input method.
    layout: Option<Shaped>,
    selecting: bool,
}

#[derive(Clone)]
struct Shaped {
    /// One per paragraph, wrapped.
    lines: Vec<WrappedLine>,
    bounds: Bounds<Pixels>,
    line_h: Pixels,
    /// How far a long single line is scrolled to keep the caret in sight.
    scroll_x: Pixels,
}

impl EventEmitter<Edited> for TextField {}

impl TextField {
    pub fn new(cx: &mut Context<Self>) -> Self {
        Self {
            focus: cx.focus_handle(),
            text: String::new(),
            selected: 0..0,
            reversed: false,
            marked: None,
            placeholder: SharedString::default(),
            multiline: false,
            height: px(16.0),
            caret: gpui::white(),
            selection: gpui::blue(),
            muted: gpui::white(),
            layout: None,
            selecting: false,
        }
    }

    pub fn text(&self) -> &str {
        &self.text
    }

    /// Replace the text, the caret after it.
    pub fn set_text(&mut self, text: &str, cx: &mut Context<Self>) {
        if self.text != text {
            self.text = text.to_string();
            self.selected = self.text.len()..self.text.len();
            self.reversed = false;
            self.marked = None;
            cx.notify();
        }
    }

    fn caret_at(&self) -> usize {
        if self.reversed {
            self.selected.start
        } else {
            self.selected.end
        }
    }

    fn move_to(&mut self, at: usize, cx: &mut Context<Self>) {
        self.selected = at..at;
        self.reversed = false;
        cx.notify();
    }

    fn select_to(&mut self, at: usize, cx: &mut Context<Self>) {
        if self.reversed {
            self.selected.start = at;
        } else {
            self.selected.end = at;
        }
        if self.selected.end < self.selected.start {
            self.reversed = !self.reversed;
            self.selected = self.selected.end..self.selected.start;
        }
        cx.notify();
    }

    fn go(&mut self, at: usize, extend: bool, cx: &mut Context<Self>) {
        if extend {
            self.select_to(at, cx);
        } else {
            self.move_to(at, cx);
        }
    }

    fn replace(&mut self, range: Range<usize>, new: &str, cx: &mut Context<Self>) {
        let new = if self.multiline {
            new.replace('\r', "")
        } else {
            new.replace(['\n', '\r'], " ")
        };
        self.text.replace_range(range.clone(), &new);
        let at = range.start + new.len();
        self.selected = at..at;
        self.reversed = false;
        self.marked = None;
        cx.emit(Edited);
        cx.notify();
    }

    /// Insert `text` over the selection, as typing or pasting does.
    pub fn insert(&mut self, text: &str, cx: &mut Context<Self>) {
        self.replace(self.selected.clone(), text, cx);
    }

    fn prev_char(&self, at: usize) -> usize {
        self.text[..at]
            .char_indices()
            .next_back()
            .map_or(0, |(i, _)| i)
    }

    fn next_char(&self, at: usize) -> usize {
        self.text[at..]
            .chars()
            .next()
            .map_or(self.text.len(), |c| at + c.len_utf8())
    }

    fn prev_word(&self, at: usize) -> usize {
        let before = &self.text[..at];
        let trimmed = before.trim_end_matches(|c: char| !c.is_alphanumeric() && c != '_');
        trimmed
            .char_indices()
            .rev()
            .find(|(_, c)| !c.is_alphanumeric() && *c != '_')
            .map_or(0, |(i, c)| i + c.len_utf8())
    }

    fn next_word(&self, at: usize) -> usize {
        let after = &self.text[at..];
        let skip = after.len()
            - after
                .trim_start_matches(|c: char| !c.is_alphanumeric() && c != '_')
                .len();
        let rest = &after[skip..];
        at + skip
            + rest
                .char_indices()
                .find(|(_, c)| !c.is_alphanumeric() && *c != '_')
                .map_or(rest.len(), |(i, _)| i)
    }

    /// The word around `at`, or the one character there.
    fn word_at(&self, at: usize) -> Range<usize> {
        let wordy = |c: char| c.is_alphanumeric() || c == '_';
        if !self.text[at..].chars().next().is_some_and(wordy) {
            return at..self.next_char(at);
        }
        let start = self.text[..at]
            .char_indices()
            .rev()
            .find(|(_, c)| !wordy(*c))
            .map_or(0, |(i, c)| i + c.len_utf8());
        let end = self.text[at..]
            .char_indices()
            .find(|(_, c)| !wordy(*c))
            .map_or(self.text.len(), |(i, _)| at + i);
        start..end
    }

    fn line_start(&self, at: usize) -> usize {
        self.text[..at].rfind('\n').map_or(0, |i| i + 1)
    }

    fn line_end(&self, at: usize) -> usize {
        self.text[at..]
            .find('\n')
            .map_or(self.text.len(), |i| at + i)
    }

    /// Where byte `at` is drawn, relative to the field's top left.
    fn position(&self, at: usize) -> Option<Point<Pixels>> {
        position(self.layout.as_ref()?, at)
    }

    /// The byte nearest `pos`, in window coordinates.
    fn index_at(&self, pos: Point<Pixels>) -> usize {
        let Some(shaped) = self.layout.as_ref() else {
            return 0;
        };
        let local = pos - shaped.bounds.origin;
        let mut start = 0;
        let mut y = px(0.0);
        for (n, line) in shaped.lines.iter().enumerate() {
            let h = line.size(shaped.line_h).height;
            let last = n + 1 == shaped.lines.len();
            if local.y < y + h || last {
                let at = point(
                    local.x + shaped.scroll_x,
                    (local.y - y).max(px(0.0)).min(h - px(1.0)),
                );
                let i = line
                    .closest_index_for_position(at, shaped.line_h)
                    .unwrap_or_else(|i| i);
                return start + i.min(line.len());
            }
            y += h;
            start += line.len() + 1;
        }
        self.text.len()
    }

    /// Up or down a visual line from the caret.
    fn vertical(&self, down: bool) -> Option<usize> {
        let shaped = self.layout.as_ref()?;
        let p = self.position(self.caret_at())?;
        let y = if down {
            p.y + shaped.line_h * 1.5
        } else {
            p.y - shaped.line_h * 0.5
        };
        let total: Pixels = shaped
            .lines
            .iter()
            .map(|l| l.size(shaped.line_h).height)
            .fold(px(0.0), |a, b| a + b);
        if y < px(0.0) || y >= total {
            return None;
        }
        Some(self.index_at(shaped.bounds.origin + point(p.x, y)))
    }

    fn selected_text(&self) -> Option<String> {
        (!self.selected.is_empty()).then(|| self.text[self.selected.clone()].to_string())
    }

    /// Editing keys. Keys it leaves alone (Enter, Esc, Tab, and in one
    /// line Up and Down) go on to the window.
    pub fn on_key(&mut self, ev: &KeyDownEvent, _: &mut Window, cx: &mut Context<Self>) {
        let k = &ev.keystroke;
        let m = &k.modifiers;
        if m.platform {
            return;
        }
        let at = self.caret_at();
        let shift = m.shift;
        let handled = match (k.key.as_str(), m.control, m.alt) {
            ("backspace", ctrl, _) => {
                if self.selected.is_empty() {
                    let from = if ctrl {
                        self.prev_word(at)
                    } else {
                        self.prev_char(at)
                    };
                    self.selected = from..at;
                }
                self.insert("", cx);
                true
            }
            ("delete", ctrl, _) => {
                if self.selected.is_empty() {
                    let to = if ctrl {
                        self.next_word(at)
                    } else {
                        self.next_char(at)
                    };
                    self.selected = at..to;
                }
                self.insert("", cx);
                true
            }
            ("left", ctrl, _) => {
                let to = match (ctrl, self.selected.is_empty() || shift) {
                    (true, _) => self.prev_word(at),
                    (false, true) => self.prev_char(at),
                    (false, false) => self.selected.start,
                };
                self.go(to, shift, cx);
                true
            }
            ("right", ctrl, _) => {
                let to = match (ctrl, self.selected.is_empty() || shift) {
                    (true, _) => self.next_word(at),
                    (false, true) => self.next_char(at),
                    (false, false) => self.selected.end,
                };
                self.go(to, shift, cx);
                true
            }
            ("home", ..) => {
                self.go(self.line_start(at), shift, cx);
                true
            }
            ("end", ..) => {
                self.go(self.line_end(at), shift, cx);
                true
            }
            ("up", false, false) | ("down", false, false) if self.multiline => {
                match self.vertical(k.key == "down") {
                    Some(to) => self.go(to, shift, cx),
                    None if k.key == "down" => self.go(self.text.len(), shift, cx),
                    None => self.go(0, shift, cx),
                }
                true
            }
            ("enter", false, false) if shift && self.multiline => {
                self.insert("\n", cx);
                true
            }
            ("a", true, false) => {
                self.selected = 0..self.text.len();
                self.reversed = false;
                cx.notify();
                true
            }
            // As the terminal's prompt has them: Ctrl-U clears, Ctrl-W
            // takes the word before the caret.
            ("u", true, false) => {
                self.selected = 0..self.text.len();
                self.insert("", cx);
                true
            }
            ("w", true, false) => {
                self.selected = self.prev_word(at)..at;
                self.insert("", cx);
                true
            }
            ("c", true, false) | ("insert", true, false) if !self.selected.is_empty() => {
                if let Some(text) = self.selected_text() {
                    cx.write_to_clipboard(ClipboardItem::new_string(text));
                }
                true
            }
            ("x", true, false) if !self.selected.is_empty() => {
                if let Some(text) = self.selected_text() {
                    cx.write_to_clipboard(ClipboardItem::new_string(text));
                }
                self.insert("", cx);
                true
            }
            ("v", true, false) | ("insert", false, false) if k.key == "v" || shift => {
                if let Some(text) = cx.read_from_clipboard().and_then(|c| c.text()) {
                    self.insert(&text, cx);
                }
                true
            }
            _ => false,
        };
        if handled {
            cx.stop_propagation();
        }
    }

    fn on_mouse_down(&mut self, ev: &MouseDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        window.focus(&self.focus);
        let at = self.index_at(ev.position);
        match ev.click_count {
            1 => {
                self.selecting = true;
                self.go(at, ev.modifiers.shift, cx);
            }
            2 => {
                self.selected = self.word_at(at);
                self.reversed = false;
                cx.notify();
            }
            _ => {
                self.selected = self.line_start(at)..self.line_end(at);
                self.reversed = false;
                cx.notify();
            }
        }
        cx.stop_propagation();
    }

    fn on_mouse_move(&mut self, ev: &MouseMoveEvent, _: &mut Window, cx: &mut Context<Self>) {
        if self.selecting && ev.pressed_button == Some(MouseButton::Left) {
            self.select_to(self.index_at(ev.position), cx);
        }
    }

    fn on_mouse_up(&mut self, _: &MouseUpEvent, _: &mut Window, cx: &mut Context<Self>) {
        self.selecting = false;
        #[cfg(any(target_os = "linux", target_os = "freebsd"))]
        if let Some(text) = self.selected_text() {
            cx.write_to_primary(ClipboardItem::new_string(text));
        }
        #[cfg(not(any(target_os = "linux", target_os = "freebsd")))]
        let _ = cx;
    }

    #[cfg_attr(
        not(any(target_os = "linux", target_os = "freebsd")),
        allow(unused_variables)
    )]
    fn on_middle_click(
        &mut self,
        ev: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        window.focus(&self.focus);
        #[cfg(any(target_os = "linux", target_os = "freebsd"))]
        if let Some(text) = cx.read_from_primary().and_then(|c| c.text()) {
            let at = self.index_at(ev.position);
            self.move_to(at, cx);
            self.insert(&text, cx);
        }
        cx.stop_propagation();
    }

    // UTF-16 offsets, as input methods count.

    fn utf8_at(&self, offset: usize) -> usize {
        let mut utf8 = 0;
        let mut utf16 = 0;
        for c in self.text.chars() {
            if utf16 >= offset {
                break;
            }
            utf16 += c.len_utf16();
            utf8 += c.len_utf8();
        }
        utf8
    }

    fn utf16_at(&self, offset: usize) -> usize {
        self.text[..offset.min(self.text.len())]
            .chars()
            .map(char::len_utf16)
            .sum()
    }

    fn utf8_range(&self, r: &Range<usize>) -> Range<usize> {
        self.utf8_at(r.start)..self.utf8_at(r.end)
    }

    fn utf16_range(&self, r: &Range<usize>) -> Range<usize> {
        self.utf16_at(r.start)..self.utf16_at(r.end)
    }
}

impl EntityInputHandler for TextField {
    fn text_for_range(
        &mut self,
        range: Range<usize>,
        actual: &mut Option<Range<usize>>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<String> {
        let range = self.utf8_range(&range);
        actual.replace(self.utf16_range(&range));
        Some(self.text[range].to_string())
    }

    fn selected_text_range(
        &mut self,
        _: bool,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<UTF16Selection> {
        Some(UTF16Selection {
            range: self.utf16_range(&self.selected),
            reversed: self.reversed,
        })
    }

    fn marked_text_range(&self, _: &mut Window, _: &mut Context<Self>) -> Option<Range<usize>> {
        self.marked.as_ref().map(|r| self.utf16_range(r))
    }

    fn unmark_text(&mut self, _: &mut Window, _: &mut Context<Self>) {
        self.marked = None;
    }

    fn replace_text_in_range(
        &mut self,
        range: Option<Range<usize>>,
        new: &str,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let range = range
            .map(|r| self.utf8_range(&r))
            .or(self.marked.clone())
            .unwrap_or(self.selected.clone());
        self.replace(range, new, cx);
    }

    fn replace_and_mark_text_in_range(
        &mut self,
        range: Option<Range<usize>>,
        new: &str,
        new_selected: Option<Range<usize>>,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let range = range
            .map(|r| self.utf8_range(&r))
            .or(self.marked.clone())
            .unwrap_or(self.selected.clone());
        self.text.replace_range(range.clone(), new);
        self.marked = (!new.is_empty()).then(|| range.start..range.start + new.len());
        // The new selection counts UTF-16 units within the new text.
        let byte = |units: usize| {
            let (mut bytes, mut seen) = (0, 0);
            for c in new.chars() {
                if seen >= units {
                    break;
                }
                seen += c.len_utf16();
                bytes += c.len_utf8();
            }
            bytes
        };
        self.selected = new_selected
            .map(|r| range.start + byte(r.start)..range.start + byte(r.end))
            .unwrap_or_else(|| range.start + new.len()..range.start + new.len());
        cx.emit(Edited);
        cx.notify();
    }

    fn bounds_for_range(
        &mut self,
        range: Range<usize>,
        bounds: Bounds<Pixels>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<Bounds<Pixels>> {
        let range = self.utf8_range(&range);
        let line_h = self.layout.as_ref()?.line_h;
        let a = self.position(range.start)?;
        let b = self.position(range.end)?;
        Some(Bounds::from_corners(
            bounds.origin + a,
            bounds.origin + point(b.x, b.y + line_h),
        ))
    }

    fn character_index_for_point(
        &mut self,
        pos: Point<Pixels>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<usize> {
        Some(self.utf16_at(self.index_at(pos)))
    }
}

/// The field drawn `height` tall, as wide as its parent.
pub struct FieldElement {
    pub field: Entity<TextField>,
    pub height: Pixels,
}

pub struct Prepainted {
    shaped: Shaped,
    caret: Option<Bounds<Pixels>>,
    selection: Vec<Bounds<Pixels>>,
    placeholder: bool,
}

impl IntoElement for FieldElement {
    type Element = Self;

    fn into_element(self) -> Self {
        self
    }
}

impl Element for FieldElement {
    type RequestLayoutState = ();
    type PrepaintState = Prepainted;

    fn id(&self) -> Option<ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static core::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&gpui::InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, ()) {
        let mut style = Style::default();
        style.size.width = relative(1.).into();
        style.size.height = self.height.into();
        (window.request_layout(style, [], cx), ())
    }

    fn prepaint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&gpui::InspectorElementId>,
        bounds: Bounds<Pixels>,
        _: &mut (),
        window: &mut Window,
        cx: &mut App,
    ) -> Prepainted {
        let field = self.field.read(cx);
        let style = window.text_style();
        let line_h = window.line_height();
        let placeholder = field.text.is_empty();
        let (text, color) = if placeholder {
            (field.placeholder.clone(), field.muted)
        } else {
            (SharedString::from(field.text.clone()), style.color)
        };
        let run = TextRun {
            len: text.len(),
            font: style.font(),
            color,
            background_color: None,
            underline: None,
            strikethrough: None,
        };
        let runs = match field.marked.as_ref().filter(|_| !placeholder) {
            Some(m) => vec![
                TextRun {
                    len: m.start,
                    ..run.clone()
                },
                TextRun {
                    len: m.end - m.start,
                    underline: Some(UnderlineStyle {
                        color: Some(color),
                        thickness: px(1.0),
                        wavy: false,
                    }),
                    ..run.clone()
                },
                TextRun {
                    len: text.len() - m.end,
                    ..run
                },
            ],
            None => vec![run],
        };
        let font_size = style.font_size.to_pixels(window.rem_size());
        let wrap = field.multiline.then_some(bounds.size.width);
        let lines: Vec<WrappedLine> = window
            .text_system()
            .shape_text(text, font_size, &runs, wrap, None)
            .map(|l| l.into_vec())
            .unwrap_or_default();
        let mut shaped = Shaped {
            lines,
            bounds,
            line_h,
            scroll_x: px(0.0),
        };
        let caret_at = field.caret_at();
        // A long line scrolls to keep the caret in sight.
        if !field.multiline
            && let Some(p) = position(&shaped, caret_at)
        {
            let room = bounds.size.width - px(4.0);
            if p.x > room {
                shaped.scroll_x = p.x - room;
            }
        }
        let caret = field
            .selected
            .is_empty()
            .then(|| position(&shaped, if placeholder { 0 } else { caret_at }))
            .flatten()
            .map(|p| Bounds::new(bounds.origin + p, size(px(1.5), line_h)));
        let mut selection = Vec::new();
        if !field.selected.is_empty()
            && let (Some(a), Some(b)) = (
                position(&shaped, field.selected.start),
                position(&shaped, field.selected.end),
            )
        {
            let o = bounds.origin;
            if a.y == b.y {
                selection.push(Bounds::from_corners(o + a, o + point(b.x, b.y + line_h)));
            } else {
                let right = bounds.size.width;
                selection.push(Bounds::from_corners(o + a, o + point(right, a.y + line_h)));
                if b.y > a.y + line_h {
                    selection.push(Bounds::from_corners(
                        o + point(px(0.0), a.y + line_h),
                        o + point(right, b.y),
                    ));
                }
                selection.push(Bounds::from_corners(
                    o + point(px(0.0), b.y),
                    o + point(b.x, b.y + line_h),
                ));
            }
        }
        Prepainted {
            shaped,
            caret,
            selection,
            placeholder,
        }
    }

    fn paint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&gpui::InspectorElementId>,
        bounds: Bounds<Pixels>,
        _: &mut (),
        pre: &mut Prepainted,
        window: &mut Window,
        cx: &mut App,
    ) {
        let (focus, selection_color, caret_color) = {
            let f = self.field.read(cx);
            (f.focus.clone(), f.selection, f.caret)
        };
        window.handle_input(
            &focus,
            ElementInputHandler::new(bounds, self.field.clone()),
            cx,
        );
        window.with_content_mask(Some(gpui::ContentMask { bounds }), |window| {
            for b in &pre.selection {
                window.paint_quad(fill(*b, selection_color));
            }
            let mut y = px(0.0);
            for line in &pre.shaped.lines {
                let origin = bounds.origin + point(-pre.shaped.scroll_x, y);
                line.paint(origin, pre.shaped.line_h, TextAlign::Left, None, window, cx)
                    .ok();
                y += line.size(pre.shaped.line_h).height;
            }
            if focus.is_focused(window)
                && let Some(caret) = pre.caret
            {
                window.paint_quad(fill(caret, caret_color));
            }
        });
        let shaped = pre.shaped.clone();
        let placeholder = pre.placeholder;
        self.field.update(cx, |f, _| {
            // The placeholder's layout says nothing about the text.
            f.layout = (!placeholder).then_some(shaped);
        });
    }
}

/// Where byte `at` of the text is drawn in `shaped`, relative to the
/// field's top left.
fn position(shaped: &Shaped, at: usize) -> Option<Point<Pixels>> {
    let mut start = 0;
    let mut y = px(0.0);
    for line in &shaped.lines {
        let end = start + line.len();
        if at <= end {
            let p = line.position_for_index(at - start, shaped.line_h)?;
            return Some(point(p.x - shaped.scroll_x, p.y + y));
        }
        y += line.size(shaped.line_h).height;
        start = end + 1;
    }
    None
}

impl Render for TextField {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .track_focus(&self.focus)
            .w_full()
            .h(self.height)
            .cursor(CursorStyle::IBeam)
            .on_key_down(cx.listener(Self::on_key))
            .on_mouse_down(MouseButton::Left, cx.listener(Self::on_mouse_down))
            .on_mouse_down(MouseButton::Middle, cx.listener(Self::on_middle_click))
            .on_mouse_move(cx.listener(Self::on_mouse_move))
            .on_mouse_up(MouseButton::Left, cx.listener(Self::on_mouse_up))
            .child(FieldElement {
                field: cx.entity(),
                height: self.height,
            })
    }
}
