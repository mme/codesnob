//! The viewer in a window of its own, drawn with GPUI: `codesnob --gui`.
//! The state, the keys and everything agents ask for are the terminal
//! viewer's own; this draws what it shows with native views and turns the
//! window's input into the events the viewer already answers to.

mod body;
mod chrome;
mod field;
mod fling;
pub mod fonts;
mod keys;
mod measure;
mod select;
pub mod theme;
mod wheel;

use std::collections::VecDeque;
use std::rc::Rc;
use std::sync::mpsc::Receiver;
use std::time::{Duration, Instant};

use anyhow::Result;
use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
use futures::StreamExt;
use gpui::prelude::*;
use gpui::{
    Application, Bounds, ClipboardItem, Context, DispatchPhase, Entity, FocusHandle, Hsla,
    KeyDownEvent, MouseButton, MouseMoveEvent, MouseUpEvent, Pixels, ScrollDelta, ScrollWheelEvent,
    SharedString, Subscription, Timer, TitlebarOptions, TouchPhase, Window, WindowBounds,
    WindowOptions, div, px, size,
};
use ratatui::style::Color;

use super::App as Core;
use super::panel::Kind;
use super::{Compose, Palette};
use crate::watch::Msg;
use field::{Edited, TextField};
use keys::Clipboard;
use measure::{Editor, Heights, Metrics};
use select::Selection;
use theme::Theme;

pub struct Options {
    /// The code's font and size; the terminal's when not given.
    pub font: Option<String>,
    pub font_size: Option<f32>,
    /// The proportional font around the code; the desktop's when not given.
    pub ui_font: Option<String>,
    /// `--light`: the palette for a light background, whatever the theme.
    pub light: bool,
}

/// How long after the last scroll the scroll bar stays in sight.
const SCROLLING: Duration = Duration::from_millis(600);
/// How long the scroll bar takes to fade after that.
const FADE: Duration = Duration::from_millis(400);
/// A swipe keeps to the direction it started in until its events pause
/// this long.
const RAIL: Duration = Duration::from_millis(150);
/// Where the compositor does not tell when the fingers lift, its events
/// stopping this long counts as lifting.
const LIFT: Duration = Duration::from_millis(45);
/// Chromium scrolls by a touchpad at twelve times the compositor's values,
/// GPUI at three: its motion counts four times here.
const TOUCHPAD_GAIN: f32 = 4.0;
/// Chromium scrolls 120 pixels a wheel notch; GPUI counts three lines.
const WHEEL_LINE: f32 = 40.0;
/// A display frame, as Chromium times its animations.
const FRAME: Duration = Duration::from_micros(16_667);

/// Open the window and run until it closes or the viewer quits.
pub fn run(core: Core, rx: Receiver<Msg>, theme: Theme, opts: Options) -> Result<()> {
    Application::new().run(move |cx: &mut gpui::App| {
        let bounds = Bounds::centered(None, size(px(1280.0), px(860.0)), cx);
        let options = WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(bounds)),
            titlebar: Some(TitlebarOptions {
                title: Some("codesnob".into()),
                ..Default::default()
            }),
            app_id: Some("codesnob".into()),
            ..Default::default()
        };
        let opened = cx.open_window(options, |window, cx| {
            cx.new(|cx| Viewer::new(core, rx, theme, opts, window, cx))
        });
        if let Err(e) = opened {
            eprintln!("codesnob: could not open a window: {e}");
            cx.quit();
            return;
        }
        cx.on_window_closed(|cx| cx.quit()).detach();
        cx.activate(true);
    });
    Ok(())
}

/// The window's one view: the viewer, and what drawing it natively needs.
struct Viewer {
    core: Core,
    theme: Theme,
    /// `--light` was given.
    light: bool,
    focus: FocusHandle,
    /// What the prompts and the comment editor type into.
    field: Entity<TextField>,
    heights: Rc<Heights>,
    m: Metrics,
    /// Where the body was drawn last, in the window.
    body: Bounds<Pixels>,
    /// The rows drawn last, top to bottom, for the mouse.
    drawn: Vec<body::Drawn>,
    selection: Option<Selection>,
    /// The left button is down and dragging out `selection`.
    selecting: bool,
    /// The scroll bar's thumb is being dragged, held this far below its top.
    thumb_grab: Option<f32>,
    /// A redraw is due on a timer, for what changes with time.
    ticking: bool,
    /// A wheel's notches being animated.
    wheeling: Option<Wheeling>,
    /// The display's scale, which a wheel animation's length counts.
    scale: f32,
    /// The swipe's last scroll frames, newest first: each one's time since
    /// the frame before it, and how far it moved, in Chrome's pixels.
    swipes: VecDeque<(Duration, f32)>,
    /// When the touchpad last sent a scroll frame.
    last_frame: Option<Instant>,
    /// A swipe is under way.
    swiping: bool,
    /// Coasting after a swipe.
    coast: Option<fling::Fling>,
    /// Whether the next fling adds the last one's speed.
    booster: fling::Booster,
    /// The compositor says when the fingers lift (Wayland does); elsewhere
    /// a pause stands in for it.
    lift_said: bool,
    /// The compositor says when fingers come to rest on the touchpad (the
    /// hold gesture), so they can stop a fling.
    hold_said: bool,
    /// When the view was last scrolled by the wheel, the touchpad or the
    /// scroll bar.
    scrolled: Option<Instant>,
    /// How far the code is scrolled sideways, in pixels: the viewer's
    /// `hscroll` counts whole characters of it.
    hpx: f32,
    /// The viewer's `hscroll` as last seen, to tell when keys moved it.
    seen_h: usize,
    /// How far the code can scroll sideways, and how wide its room is.
    hmax: f32,
    hroom: f32,
    /// The direction the current swipe keeps to (sideways or not), and
    /// when it last moved.
    rail: Option<(bool, Instant)>,
    /// When the code was last scrolled sideways.
    hscrolled: Option<Instant>,
    /// The bottom scroll bar's thumb is being dragged, held this far right
    /// of its left edge.
    hgrab: Option<f32>,
    /// Scrolling short of a whole pixel.
    fraction: f32,
    sticker_dark: chrome::Sticker,
    sticker_light: chrome::Sticker,
    title: String,
    _theme_watch: Option<notify::RecommendedWatcher>,
    _field_edits: Subscription,
}

/// A wheel's notches being animated: the curve, when its first frame
/// showed, how far it has moved the view, and how far it may go each way.
struct Wheeling {
    scroll: wheel::WheelScroll,
    start: Option<Instant>,
    applied: f32,
    lo: f32,
    hi: f32,
}

fn hsla(c: theme::Rgb) -> Hsla {
    gpui::rgb(c).into()
}

impl Viewer {
    fn new(
        mut core: Core,
        rx: Receiver<Msg>,
        theme: Theme,
        opts: Options,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let focus = cx.focus_handle();
        window.focus(&focus);

        // What the viewer hears from loaders, watchers and the daemon,
        // brought onto the window's thread.
        let (tx, mut msgs) = futures::channel::mpsc::unbounded();
        std::thread::spawn(move || {
            while let Ok(m) = rx.recv() {
                if tx.unbounded_send(m).is_err() {
                    break;
                }
            }
        });
        cx.spawn(async move |this, cx| {
            while let Some(m) = msgs.next().await {
                let mut batch = vec![m];
                while let Ok(m) = msgs.try_recv() {
                    batch.push(m);
                }
                let applied = this.update(cx, |v, cx| {
                    for m in batch {
                        v.dispatch(m);
                    }
                    v.after(cx);
                });
                if applied.is_err() {
                    break;
                }
            }
        })
        .detach();

        let code = fonts::code(opts.font, opts.font_size);
        let ui = SharedString::from(fonts::ui(opts.ui_font));
        let mono = SharedString::from(code.family);
        let text = window.text_system();
        let id = text.resolve_font(&gpui::font(mono.clone()));
        let cell = text
            .advance(id, px(code.px), 'm')
            .map(|s| f32::from(s.width))
            .unwrap_or(code.px * 0.6);
        let m = Metrics::new(mono, code.px, ui, cell);
        let heights = Rc::new(Heights::new(m.clone(), cx.text_system().clone()));
        let measure: Rc<dyn super::diffview::Measure> = heights.clone();
        core.measure = Some(measure.clone());
        for loc in &mut core.locations {
            loc.view.measure = Some(measure.clone());
        }

        let field = cx.new(TextField::new);
        let field_edits = cx.subscribe(&field, |this, field, _: &Edited, cx| {
            let text = field.read(cx).text().to_string();
            this.core.set_panel_input(text);
            cx.notify();
        });

        Self {
            core,
            theme,
            light: opts.light,
            focus,
            field,
            heights,
            m,
            body: Bounds::default(),
            drawn: Vec::new(),
            selection: None,
            selecting: false,
            thumb_grab: None,
            ticking: false,
            wheeling: None,
            scale: window.scale_factor(),
            swipes: VecDeque::new(),
            last_frame: None,
            swiping: false,
            coast: None,
            booster: fling::Booster::default(),
            lift_said: std::env::var_os("WAYLAND_DISPLAY").is_some(),
            hold_said: false,
            scrolled: None,
            hpx: 0.0,
            seen_h: 0,
            hmax: 0.0,
            hroom: 0.0,
            rail: None,
            hscrolled: None,
            hgrab: None,
            fraction: 0.0,
            sticker_dark: chrome::Sticker::new(super::logo::ON_DARK),
            sticker_light: chrome::Sticker::new(super::logo::ON_LIGHT),
            title: String::new(),
            _theme_watch: follow_theme(cx),
            _field_edits: field_edits,
        }
    }

    fn dispatch(&mut self, msg: Msg) {
        // What is shown may change under a selection: it goes.
        if matches!(
            msg,
            Msg::Loaded { .. } | Msg::Show(..) | Msg::View(..) | Msg::Pr { .. }
        ) {
            self.selection = None;
        }
        self.core.dispatch(msg);
    }

    /// After anything the viewer handled: quit when it says so, and draw.
    fn after(&mut self, cx: &mut Context<Self>) {
        if self.core.quit {
            cx.quit();
        }
        cx.notify();
    }

    fn press(&mut self, code: KeyCode, cx: &mut Context<Self>) {
        self.selection = None;
        self.core
            .handle(Event::Key(KeyEvent::new(code, KeyModifiers::NONE)));
        self.after(cx);
    }

    /// Put the cursor on `row` and press `code` there.
    fn press_on(&mut self, row: usize, code: KeyCode, cx: &mut Context<Self>) {
        if let Some(loc) = self.core.cur_mut() {
            loc.view.click_row(row);
        }
        self.press(code, cx);
    }

    fn set_theme(&mut self, theme: Option<Theme>, cx: &mut Context<Self>) {
        let Some(theme) = theme.filter(|t| *t != self.theme) else {
            return;
        };
        let light = self.light || theme.light;
        if light != self.core.palette.light {
            self.core.palette = if light {
                Palette::light()
            } else {
                Palette::dark()
            };
            // Descriptions carry the palette's colors: drawn again.
            for loc in &mut self.core.locations {
                loc.desc_dirty = true;
            }
        }
        self.theme = theme;
        cx.notify();
    }

    fn color(&self, c: Color) -> Option<Hsla> {
        self.theme.color(c).map(hsla)
    }

    fn fg(&self, c: Color) -> Hsla {
        self.color(c).unwrap_or(hsla(self.theme.fg))
    }

    fn typing(&self, window: &Window, cx: &gpui::App) -> bool {
        self.field.read(cx).focus.is_focused(window)
    }

    // ---- keys and the clipboard -------------------------------------------

    fn on_key(&mut self, ev: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        self.coast = None;
        self.wheeling = None;
        let k = &ev.keystroke;
        if self.typing(window, cx) {
            // The field edits; what it leaves alone that the viewer's
            // prompt answers to (Enter, Esc, Tab, moving through the list)
            // goes on to the viewer.
            let multiline = self.field.read(cx).multiline;
            let list = !multiline
                && (matches!(k.key.as_str(), "up" | "down" | "pageup" | "pagedown")
                    || k.modifiers.control && matches!(k.key.as_str(), "n" | "p"));
            let submit = matches!(k.key.as_str(), "enter" | "escape") && !k.modifiers.shift;
            if (submit || k.key == "tab" || list)
                && let Some(key) = keys::translate(k)
            {
                self.core.handle(Event::Key(key));
                cx.stop_propagation();
                self.after(cx);
            }
            return;
        }
        match keys::clipboard(k) {
            Some(Clipboard::Copy) => {
                self.core.message = Some(match self.selected_text() {
                    Some(text) => {
                        cx.write_to_clipboard(ClipboardItem::new_string(text));
                        "copied".to_string()
                    }
                    None => "select text to copy · q quits".to_string(),
                });
                cx.notify();
                return;
            }
            Some(Clipboard::Paste) => {
                if let Some(text) = cx.read_from_clipboard().and_then(|c| c.text()) {
                    self.core.handle(Event::Paste(text));
                }
                self.after(cx);
                return;
            }
            None => {}
        }
        let Some(key) = keys::translate(k) else {
            return;
        };
        // Esc lets go of a selection before it means anything else.
        if key.code == KeyCode::Esc && self.selection.take().is_some() {
            cx.notify();
            return;
        }
        self.selection = None;
        self.core.handle(Event::Key(key));
        self.after(cx);
    }

    fn selected_text(&mut self) -> Option<String> {
        let sel = self.selection.filter(|s| !s.is_empty())?;
        Some(sel.text(|row| self.row_text(row)))
    }

    // ---- the mouse ----------------------------------------------------------

    fn on_drag(&mut self, ev: &MouseMoveEvent, cx: &mut Context<Self>) {
        if let Some(grab) = self.hgrab {
            if ev.pressed_button == Some(MouseButton::Left) {
                self.drag_hthumb(f32::from(ev.position.x) - grab);
                cx.notify();
            } else {
                self.hgrab = None;
            }
            return;
        }
        if let Some(grab) = self.thumb_grab {
            if ev.pressed_button == Some(MouseButton::Left) {
                self.drag_thumb(f32::from(ev.position.y) - grab);
                cx.notify();
            } else {
                self.thumb_grab = None;
            }
            return;
        }
        if !self.selecting {
            return;
        }
        if ev.pressed_button != Some(MouseButton::Left) {
            // Released where the window never heard of it.
            self.release(cx);
            return;
        }
        // Dragged to the top or bottom edge, or past it: the view scrolls
        // along.
        let edge = px(self.m.line * 0.5);
        if ev.position.y < self.body.top() + edge {
            self.glide(-self.m.line);
        } else if ev.position.y >= self.body.bottom() - edge {
            self.glide(self.m.line);
        }
        let head = self.point_at(ev.position, true, false);
        if let (Some(sel), Some(head)) = (self.selection.as_mut(), head) {
            sel.head = head;
        }
        cx.notify();
    }

    fn release(&mut self, cx: &mut Context<Self>) {
        self.thumb_grab = None;
        self.hgrab = None;
        if std::mem::take(&mut self.selecting) {
            self.offer_primary(cx);
        }
    }

    /// Follow a drag over the whole window and past its edges, which the
    /// elements' own listeners, heard only while hovered, would miss.
    fn drag_tracker(&self, cx: &mut Context<Self>) -> gpui::Canvas<()> {
        let this = cx.entity();
        gpui::canvas(
            |_, _, _| {},
            move |_, _, window, _| {
                let moved = this.clone();
                window.on_mouse_event(move |ev: &MouseMoveEvent, phase, _, cx| {
                    if phase == DispatchPhase::Bubble {
                        moved.update(cx, |v, cx| v.on_drag(ev, cx));
                    }
                });
                window.on_mouse_event(move |ev: &MouseUpEvent, phase, _, cx| {
                    if phase == DispatchPhase::Bubble && ev.button == MouseButton::Left {
                        this.update(cx, |v, cx| v.release(cx));
                    }
                });
            },
        )
        .absolute()
        .size_full()
    }

    /// What is selected becomes the primary selection, as Linux has it:
    /// middle-click pastes it anywhere.
    #[cfg(any(target_os = "linux", target_os = "freebsd"))]
    fn offer_primary(&mut self, cx: &mut Context<Self>) {
        if let Some(text) = self.selected_text() {
            cx.write_to_primary(ClipboardItem::new_string(text));
        }
    }

    /// Elsewhere there is no primary selection.
    #[cfg(not(any(target_os = "linux", target_os = "freebsd")))]
    fn offer_primary(&mut self, _: &mut Context<Self>) {}

    /// Scroll the body by `delta` pixels, smoothly.
    fn glide(&mut self, delta: f32) {
        self.fraction += delta;
        let whole = self.fraction.trunc();
        self.fraction -= whole;
        if let Some(loc) = self.core.cur_mut() {
            loc.view.glide(whole as isize);
        }
    }

    fn on_wheel(&mut self, ev: &ScrollWheelEvent, _: &mut Window, cx: &mut Context<Self>) {
        let now = Instant::now();
        let still = matches!(ev.delta, ScrollDelta::Pixels(d) if d.x == px(0.0) && d.y == px(0.0));
        // Fingers came to rest on the touchpad: a fling stops.
        if matches!(ev.touch_phase, TouchPhase::Started) && still {
            self.hold_said = true;
            self.swiping = false;
            self.catch(now);
            cx.notify();
            return;
        }
        // The time since the touchpad's last frame, as Chromium times it.
        let since = self.last_frame.map_or(FRAME, |t| now - t);
        // Lifted fingers end with a frame that moves nothing; Hyprland sends
        // one even where GPUI does not say `Ended`.
        if still {
            self.lift_said = true;
            self.last_frame = Some(now);
            self.lifted(now, since, cx);
            return;
        }
        let line = self.m.line;
        let (dx, dy, wheel) = match ev.delta {
            ScrollDelta::Lines(d) => (d.x * WHEEL_LINE, d.y * WHEEL_LINE, true),
            ScrollDelta::Pixels(d) => (
                f32::from(d.x) * TOUCHPAD_GAIN,
                f32::from(d.y) * TOUCHPAD_GAIN,
                false,
            ),
        };
        if !wheel {
            self.last_frame = Some(now);
        }
        // A swipe keeps to the direction it started in, so one a little
        // off straight doesn't drift sideways.
        let sideways = ev.modifiers.shift
            || match self.rail {
                Some((sideways, at)) if now - at < RAIL => sideways,
                _ => dx.abs() > dy.abs(),
            };
        self.rail = Some((sideways, now));
        if self
            .core
            .panel
            .as_ref()
            .is_some_and(|p| p.kind != Kind::Comment)
        {
            // Over an open list the wheel moves its highlight, a row a
            // line of the compositor's own motion.
            let rows = (-dy
                / if wheel {
                    WHEEL_LINE
                } else {
                    TOUCHPAD_GAIN * line
                })
            .round() as isize;
            for _ in 0..rows.unsigned_abs() {
                self.core.wheel(rows.signum());
            }
        } else if sideways {
            // Only as far as there is code to see; no fling sideways.
            self.coast = None;
            self.swipes.clear();
            let d = if ev.modifiers.shift { dy } else { dx };
            if self.hmax > 0.0 {
                self.hpx = (self.hpx - d).clamp(0.0, self.hmax);
                self.hscrolled = Some(now);
            }
        } else if wheel {
            self.catch(now);
            self.notch(-dy);
            self.scrolled_now();
        } else {
            // Touchpad motion goes on screen as it comes, and catches a fling.
            self.wheeling = None;
            if self.coast.is_some() {
                self.catch(now);
            }
            if !self.swiping {
                self.swiping = true;
                self.swipes.clear();
                self.booster.began(now);
            }
            self.booster.moved(now, -dy);
            self.swipes.push_front((since, -dy));
            self.swipes.truncate(fling::FRAMES);
            self.glide(-dy);
            self.scrolled_now();
            if !self.lift_said {
                self.watch_for_lift(cx);
            }
        }
        cx.notify();
    }

    /// A fling caught by fingers or another swipe stops; it may lend the
    /// next its speed.
    fn catch(&mut self, now: Instant) {
        if self.coast.take().is_some() {
            self.booster.caught(now);
        }
    }

    /// The fingers lifted, `since` after the swipe's last frame: if they
    /// were still moving, the view flings on, as Chromium's does.
    fn lifted(&mut self, now: Instant, since: Duration, cx: &mut Context<Self>) {
        self.swiping = false;
        let frames: Vec<_> = self.swipes.drain(..).collect();
        // macOS flings by itself.
        if cfg!(target_os = "macos") || frames.is_empty() {
            return;
        }
        let measured = fling::velocity(&frames, since);
        let velocity = self.booster.fling(now, measured);
        // Where fingers coming back can stop it, a fling goes as far as
        // Chromium's; where nothing but a click or a key can, it is a short
        // glide, so a fast one can't run away.
        let height = f32::from(self.body.size.height);
        let (limit, longest) = if self.hold_said {
            (
                fling::Fling::limit(height, velocity, measured),
                fling::LONGEST,
            )
        } else {
            (0.5 * height, 0.5)
        };
        self.coast = fling::Fling::new(velocity, now, limit, longest);
        if self.coast.is_some() {
            cx.notify();
        }
    }

    /// Where the compositor doesn't say when the fingers lift, events
    /// stopping stands in for it.
    fn watch_for_lift(&mut self, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
            Timer::after(LIFT + Duration::from_millis(5)).await;
            this.update(cx, |v, cx| {
                let paused = v.last_frame.is_some_and(|t| t.elapsed() >= LIFT);
                if paused && v.swiping && !v.lift_said {
                    let now = Instant::now();
                    let since = v.last_frame.map_or(FRAME, |t| now - t);
                    v.lifted(now, since, cx);
                }
            })
            .ok();
        })
        .detach();
    }

    /// Fling a frame's worth along the curve; more next frame.
    fn coast(&mut self, window: &mut Window) {
        let Some(f) = self.coast.as_mut() else {
            return;
        };
        let (by, over) = f.step(Instant::now());
        let before = self.position();
        if by != 0.0 {
            self.glide(by);
        }
        // Over, or at the top or the bottom.
        let stuck = by != 0.0 && self.position() == before;
        if over || stuck {
            self.coast = None;
            self.booster.reset();
        }
        self.scrolled_now();
        if self.coast.is_some() {
            window.request_animation_frame();
        }
    }

    /// A wheel's notch, `by` pixels: animated as Chromium animates it, a
    /// notch arriving mid-way carrying the scroll on to the further target.
    fn notch(&mut self, by: f32) {
        if by.abs() <= 0.1 {
            self.glide(by);
            return;
        }
        let now = Instant::now();
        if let Some(w) = self.wheeling.as_mut() {
            let t = w.start.map_or(0.0, |s| (now - s).as_secs_f32());
            let target = (w.scroll.target() + by).clamp(w.lo, w.hi);
            w.scroll.retarget(t, target);
            return;
        }
        // From here the view can scroll back to its top and on to its end.
        let height = f32::from(self.body.size.height);
        let (lo, hi) = self.extent().map_or((0.0, 0.0), |e| {
            (-e.offset, (e.total - height - e.offset).max(0.0))
        });
        let target = by.clamp(-height, height).clamp(lo, hi);
        if target == 0.0 {
            return;
        }
        self.wheeling = Some(Wheeling {
            scroll: wheel::WheelScroll::new(target, self.scale),
            start: None,
            applied: 0.0,
            lo,
            hi,
        });
    }

    /// Move the wheel's animation on to this frame.
    fn wheel_frame(&mut self, window: &mut Window) {
        let Some(w) = self.wheeling.as_mut() else {
            return;
        };
        let now = Instant::now();
        // Its first frame shows it a frame along, as Chromium's does.
        let start = *w.start.get_or_insert(now - FRAME);
        let t = (now - start).as_secs_f32();
        let to = w.scroll.value(t);
        let by = to - w.applied;
        w.applied = to;
        let done = w.scroll.done(t);
        self.glide(by);
        if done {
            self.wheeling = None;
        } else {
            window.request_animation_frame();
        }
        self.scrolled_now();
    }

    /// Where the view stands, to tell whether it moved.
    fn position(&self) -> Option<(usize, usize)> {
        self.core
            .cur()
            .map(|l| (l.view.scroll, l.view.top_hidden()))
    }

    /// The view was just scrolled.
    fn scrolled_now(&mut self) {
        self.scrolled = Some(Instant::now());
    }

    /// Keep the sideways scroll within the code: none when every line fits,
    /// at most as far as the widest line's end. Keys move the viewer's
    /// `hscroll` by characters; the window follows them from there.
    fn fit_sideways(&mut self, width: f32) {
        let cell = self.m.cell;
        let Some(loc) = self.core.cur_mut() else {
            return;
        };
        let code = self.m.columns(loc.view.number_width(), false).code;
        self.hroom = (width - code - measure::PAD_X).max(cell);
        self.hmax = (loc.view.widest() as f32 * cell - self.hroom).max(0.0);
        if loc.view.hscroll != self.seen_h {
            self.hpx = loc.view.hscroll as f32 * cell;
            self.hscrolled = Some(Instant::now());
        }
        self.hpx = self.hpx.clamp(0.0, self.hmax);
        loc.view.hscroll = (self.hpx / cell).floor() as usize;
        self.seen_h = loc.view.hscroll;
    }

    /// How much of a scroll bar shows: all of it while scrolling, fading
    /// out after.
    fn bar_shown(&self) -> f32 {
        Self::shown_since(self.scrolled)
    }

    fn hbar_shown(&self) -> f32 {
        Self::shown_since(self.hscrolled)
    }

    fn shown_since(at: Option<Instant>) -> f32 {
        let Some(t) = at else { return 0.0 };
        let since = t.elapsed();
        if since < SCROLLING {
            1.0
        } else {
            1.0 - ((since - SCROLLING).as_secs_f32() / FADE.as_secs_f32()).min(1.0)
        }
    }

    fn choose(&mut self, n: usize, cx: &mut Context<Self>) {
        if let Some(p) = self.core.panel.as_mut() {
            p.select_visible(n);
        }
        self.press(KeyCode::Enter, cx);
    }

    // ---- what the editor and the prompts show ------------------------------

    /// Keep the field in step with the viewer's prompt: its text when the
    /// viewer changed it, its kind, and who has the keyboard.
    fn sync_field(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let panel = self
            .core
            .panel
            .as_ref()
            .map(|p| (p.kind, p.input().to_string()));
        let (caret, selection, muted) = (
            hsla(self.theme.fg),
            hsla(self.theme.selection),
            self.fg(self.core.palette.note_fg),
        );
        let placeholder = match panel.as_ref().map(|p| p.0) {
            Some(Kind::Comment) => match &self.core.composing {
                Some(Compose::Reply { .. }) => "Reply…",
                _ => "Comment…",
            },
            Some(Kind::Open) => "A path, revision, range or pull request",
            Some(_) => "Type to filter",
            None => "",
        };
        let line = self.m.ui_line;
        let heights = self.heights.clone();
        let inline = heights.editor.borrow().is_some();
        self.field.update(cx, |f, cx| {
            f.caret = caret;
            f.selection = selection;
            f.muted = muted;
            f.placeholder = placeholder.into();
            if let Some((kind, input)) = &panel {
                // A comment wraps in the body's editor; one it cannot place
                // there is a line in the list's input.
                f.multiline = *kind == Kind::Comment && inline;
                if f.text() != input {
                    f.set_text(input, cx);
                }
                f.height = if f.multiline {
                    px(heights.field_h(input, heights.card_text_w()))
                } else {
                    px(line)
                };
            }
        });
        let typing = self.typing(window, cx);
        match (panel.is_some(), typing) {
            (true, false) => {
                let focus = self.field.read(cx).focus.clone();
                window.focus(&focus);
            }
            (false, true) => window.focus(&self.focus),
            _ => {}
        }
    }

    /// Where the comment editor opens, if it is open.
    fn editor(&self, cx: &gpui::App) -> Option<Editor> {
        let panel = self
            .core
            .panel
            .as_ref()
            .filter(|p| p.kind == Kind::Comment)?;
        let view = &self.core.cur()?.view;
        let field = self.field.read(cx).text();
        let text = if field.is_empty() {
            panel.input()
        } else {
            field
        }
        .to_string();
        let (row, replace) = match self.core.composing.as_ref()? {
            // Editing a comment: in its card, in place of its text.
            Compose::Comment(_, Some(seq)) => (view.mark_row(*seq)?, true),
            // A new one: below its last line.
            Compose::Comment(region, None) => (
                view.line_row(&region.path, region.end, region.old_side)?,
                false,
            ),
            Compose::Reply { seq, editing } => {
                let at = view.mark_row(*seq)?;
                let mark = view.marks().iter().find(|m| m.seq == *seq)?;
                match editing {
                    Some(n) => {
                        let label = format!("{}{n}", mark.label);
                        let i = mark.replies.iter().position(|r| r.label == label)?;
                        (at + 1 + i, true)
                    }
                    // A new reply: below the last of them.
                    None => (at + mark.replies.len(), false),
                }
            }
        };
        Some(Editor { row, replace, text })
    }

    fn update_title(&mut self, window: &mut Window) {
        let title = match self.core.cur() {
            Some(loc) => format!("codesnob · {} · {}", loc.name(), self.core.range_scope()),
            None => "codesnob".to_string(),
        };
        if title != self.title {
            window.set_window_title(&title);
            self.title = title;
        }
    }

    /// While something on screen changes with time, draw again shortly.
    fn tick(&mut self, cx: &mut Context<Self>) {
        if self.ticking || !self.core.needs_tick() {
            return;
        }
        self.ticking = true;
        cx.spawn(async move |this, cx| {
            Timer::after(Duration::from_millis(50)).await;
            this.update(cx, |v, cx| {
                v.ticking = false;
                cx.notify();
            })
            .ok();
        })
        .detach();
    }
}

impl Render for Viewer {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let viewport = window.viewport_size();
        let bar_h = (self.m.ui_line + 10.0).round();
        let banner = self.core.pending_show.as_ref().map(|p| p.words.clone());
        let banner_h = if banner.is_some() { bar_h } else { 0.0 };
        let width = f32::from(viewport.width);
        let body_h = (f32::from(viewport.height) - bar_h - banner_h)
            .max(self.m.line)
            .floor();
        self.body = Bounds::new(
            gpui::point(px(0.0), px(banner_h)),
            size(viewport.width, px(body_h)),
        );

        self.scale = window.scale_factor();
        self.wheel_frame(window);
        self.coast(window);
        self.fit_sideways(width);
        // Draw again until the cursor and the scroll bars settle.
        if self
            .scrolled
            .into_iter()
            .chain(self.hscrolled)
            .any(|t| t.elapsed() < SCROLLING + FADE)
        {
            window.request_animation_frame();
        }
        self.heights.set_width(width);
        *self.heights.editor.borrow_mut() = self.editor(cx);
        self.sync_field(window, cx);
        self.core.list_rows = Some(self.list_rows());
        // An open list floats over the bottom of the body: the view keeps
        // its cursor clear of it, as the terminal's shortens the view.
        let covered = match self.core.panel.as_ref().map(|p| p.kind) {
            None => 0.0,
            Some(Kind::Comment) if self.heights.editor.borrow().is_some() => 0.0,
            Some(_) => chrome::panel_h(&self.m, self.list_rows()) + 8.0,
        };
        // Descriptions wrap here, not in the viewer: it gets room enough.
        self.core
            .prepare(4096, (body_h - covered).max(self.m.line) as usize);
        self.keep_editor_in_sight();
        self.update_title(window);
        self.tick(cx);

        let banner = banner.map(|words| self.banner(px(bar_h), words, cx));
        let body = self.body_view(cx);
        let status = self.status(px(bar_h));
        let panel = self.panel(px(bar_h), cx);
        let tracker = self.drag_tracker(cx);
        div()
            .track_focus(&self.focus)
            .key_context("codesnob")
            .on_key_down(cx.listener(Self::on_key))
            .on_scroll_wheel(cx.listener(Self::on_wheel))
            .relative()
            .size_full()
            .flex()
            .flex_col()
            .bg(hsla(self.theme.bg))
            .text_color(hsla(self.theme.fg))
            // Glyphs the proportional font lacks, such as the terminal
            // font's icons, come from the code's font.
            .font(gpui::Font {
                fallbacks: Some(gpui::FontFallbacks::from_fonts(vec![
                    self.m.mono.to_string(),
                ])),
                ..gpui::font(self.m.ui.clone())
            })
            .text_size(self.m.ui_size)
            .line_height(px(self.m.ui_line))
            .child(tracker)
            .children(banner)
            .child(body)
            .child(div().flex_1())
            .child(status)
            .children(panel)
    }
}

/// Watch the desktop's theme and follow it; `None` where there is none.
fn follow_theme(cx: &mut Context<Viewer>) -> Option<notify::RecommendedWatcher> {
    use notify::Watcher;
    let dir = theme::omarchy_dir()?;
    let (tx, mut rx) = futures::channel::mpsc::unbounded();
    let mut watcher = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
        use notify::EventKind::{Create, Modify, Remove};
        use notify::event::ModifyKind;
        // Changes only: reading the theme, as we do, is an event too.
        if let Ok(ev) = res
            && matches!(ev.kind, Create(_) | Remove(_) | Modify(_))
            && !matches!(ev.kind, Modify(ModifyKind::Metadata(_)))
        {
            let _ = tx.unbounded_send(());
        }
    })
    .ok()?;
    watcher.watch(&dir, notify::RecursiveMode::Recursive).ok()?;
    cx.spawn(async move |this, cx| {
        while rx.next().await.is_some() {
            // A theme change rewrites many files: let it finish.
            Timer::after(Duration::from_millis(300)).await;
            while rx.try_recv().is_ok() {}
            let theme = cx
                .background_executor()
                .spawn(async { theme::omarchy() })
                .await;
            if this.update(cx, |v, cx| v.set_theme(theme, cx)).is_err() {
                break;
            }
        }
    })
    .detach();
    Some(watcher)
}
