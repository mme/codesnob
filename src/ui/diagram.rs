//! Diagrams in a mark's text: a ```mermaid fence is drawn in place of its
//! source, as a picture where the terminal shows pictures, else as
//! box-drawing text. Rendering runs off the UI thread and is cached by
//! source and target, so a diagram is drawn once and a renderer that
//! misbehaves can only ever stall its own diagram.

use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use image::RgbaImage;

use crate::watch::Msg;

/// How long a diagram may take before it is given up on.
const PATIENCE: Duration = Duration::from_secs(3);

/// Pictures are drawn at this many times the size they ask for, when the
/// room allows: the SVG's own size reads small beside terminal text.
const MAX_SCALE: f32 = 2.0;

/// A piece of a mark's text.
#[derive(Debug, PartialEq, Eq)]
pub enum Segment {
    /// Lines shown as they are.
    Text(String),
    /// The source inside a ```mermaid fence.
    Mermaid(String),
}

/// Split text at its ```mermaid fences. Any other fence is text like the
/// rest; a fence left open runs to the end.
pub fn segments(text: &str) -> Vec<Segment> {
    let mut out: Vec<Segment> = Vec::new();
    let mut plain: Vec<&str> = Vec::new();
    let mut lines = text.lines();
    while let Some(line) = lines.next() {
        let trimmed = line.trim_start();
        let is_mermaid = trimmed
            .strip_prefix("```")
            .is_some_and(|info| info.trim().eq_ignore_ascii_case("mermaid"));
        if !is_mermaid {
            plain.push(line);
            continue;
        }
        let mut source: Vec<&str> = Vec::new();
        for inner in lines.by_ref() {
            if inner.trim_start().starts_with("```") {
                break;
            }
            source.push(inner);
        }
        if !plain.is_empty() {
            out.push(Segment::Text(plain.join("\n")));
            plain.clear();
        }
        out.push(Segment::Mermaid(source.join("\n")));
    }
    if !plain.is_empty() {
        out.push(Segment::Text(plain.join("\n")));
    }
    out
}

/// What a diagram is drawn as, and the room it has.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Target {
    /// Box-drawing text, no wider than this many columns.
    Text { width: usize },
    /// A picture no wider than `cols` cells of `cell` pixels each, in the
    /// light or the dark theme. Around the drawing it is transparent, so
    /// the mark's tint shows as the terminal draws it.
    Picture {
        cols: usize,
        cell: (u16, u16),
        light: bool,
    },
}

impl Target {
    /// The same target with `lead` fewer columns: what is drawn sits that
    /// far in from the left.
    pub fn after(&self, lead: usize) -> Target {
        match self {
            Target::Text { width } => Target::Text {
                width: width.saturating_sub(lead).max(20),
            },
            Target::Picture { cols, cell, light } => Target::Picture {
                cols: cols.saturating_sub(lead).max(10),
                cell: *cell,
                light: *light,
            },
        }
    }
}

/// A diagram as a picture: its pixels, and the cells they fill.
#[derive(Debug)]
pub struct Picture {
    pub image: RgbaImage,
    pub cols: u16,
    pub rows: u16,
}

/// A diagram drawn.
#[derive(Clone, Debug)]
pub enum Drawn {
    Text(String),
    Picture(Arc<Picture>),
}

#[derive(Clone, Debug)]
enum State {
    Pending,
    Done(Drawn),
    Failed(String),
}

fn cache() -> &'static Mutex<HashMap<(u64, Target), State>> {
    static CACHE: OnceLock<Mutex<HashMap<(u64, Target), State>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

fn key(source: &str, target: &Target) -> (u64, Target) {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    source.hash(&mut h);
    (h.finish(), target.clone())
}

/// Draw `source` as text no wider than `width`.
fn render_text(source: &str, width: usize) -> Result<String, String> {
    let opts = mermansi::MermansiOptions {
        output_mode: mermansi::OutputMode::Concise,
        max_width: width.max(20),
        ..Default::default()
    };
    let drawn = std::panic::catch_unwind(|| mermansi::render_source(source, &opts))
        .map_err(|_| "the renderer gave up".to_string())?
        .map_err(|e| e.to_string())?;
    Ok(drawn.trim_end().to_string())
}

/// The face every picture's words are set in, carried in the binary so a
/// diagram looks the same on every machine: Inter, under the SIL Open
/// Font License (see `assets/fonts/Inter-LICENSE.txt`).
const FONT: &[u8] = include_bytes!("../../assets/fonts/Inter-Regular.ttf");
/// Its family name, for the SVG and for what `sans-serif` resolves to.
const FONT_FAMILY: &str = "Inter";

/// The fonts pictures are drawn with, loaded once: the bundled face, which
/// `sans-serif` resolves to, with the system's fonts behind it for the
/// glyphs it lacks and for a family a diagram asks for by name.
fn fonts() -> Arc<resvg::usvg::fontdb::Database> {
    static FONTS: OnceLock<Arc<resvg::usvg::fontdb::Database>> = OnceLock::new();
    FONTS
        .get_or_init(|| {
            let mut db = resvg::usvg::fontdb::Database::new();
            db.load_system_fonts();
            db.load_font_data(FONT.to_vec());
            db.set_sans_serif_family(FONT_FAMILY);
            Arc::new(db)
        })
        .clone()
}

/// Draw `source` as a picture for `target`.
fn render_picture(
    source: &str,
    cols: usize,
    cell: (u16, u16),
    light: bool,
) -> Result<Picture, String> {
    let mut theme = if light {
        mermaid_svg::Theme::default()
    } else {
        mermaid_svg::Theme::dark()
    };
    theme.font_family = FONT_FAMILY.into();
    let svg = std::panic::catch_unwind(|| mermaid_svg::render_with(source, &theme))
        .map_err(|_| "the renderer gave up".to_string())?
        .map_err(|e| e.to_string())?;
    let opt = resvg::usvg::Options {
        fontdb: fonts(),
        ..Default::default()
    };
    let tree = resvg::usvg::Tree::from_str(&svg, &opt).map_err(|e| e.to_string())?;
    let size = tree.size();
    if size.width() < 1.0 || size.height() < 1.0 || cols == 0 {
        return Err("nothing to draw".to_string());
    }
    // As big as it asks, up to a point, and never wider than the room.
    let room = (cols as f32 * f32::from(cell.0)).max(1.0);
    let scale = MAX_SCALE.min(room / size.width());
    let (w, h) = (size.width() * scale, size.height() * scale);
    let (c, r) = (
        (w / f32::from(cell.0)).ceil().max(1.0) as u16,
        (h / f32::from(cell.1)).ceil().max(1.0) as u16,
    );
    let (pw, ph) = (
        u32::from(c) * u32::from(cell.0),
        u32::from(r) * u32::from(cell.1),
    );
    let mut pixmap = resvg::tiny_skia::Pixmap::new(pw, ph).ok_or("no room for the picture")?;
    resvg::render(
        &tree,
        resvg::tiny_skia::Transform::from_scale(scale, scale),
        &mut pixmap.as_mut(),
    );
    let mut image = RgbaImage::new(pw, ph);
    for (px, out) in pixmap.pixels().iter().zip(image.pixels_mut()) {
        let p = px.demultiply();
        *out = image::Rgba([p.red(), p.green(), p.blue(), p.alpha()]);
    }
    Ok(Picture {
        image,
        cols: c,
        rows: r,
    })
}

fn render(source: &str, target: &Target) -> Result<Drawn, String> {
    match target {
        Target::Text { width } => render_text(source, *width).map(Drawn::Text),
        Target::Picture { cols, cell, light } => {
            render_picture(source, *cols, *cell, *light).map(|p| Drawn::Picture(Arc::new(p)))
        }
    }
}

/// Start drawing `source` for `target` unless it is drawn or being drawn;
/// `tx` hears when it is done, or given up on.
fn ask(source: &str, target: &Target, tx: &Sender<Msg>) {
    let k = key(source, target);
    {
        let mut c = cache().lock().unwrap();
        if c.contains_key(&k) {
            return;
        }
        c.insert(k.clone(), State::Pending);
    }
    let (source, target) = (source.to_string(), target.clone());
    let (done_tx, done_rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = done_tx.send(render(&source, &target));
    });
    let tx = tx.clone();
    std::thread::spawn(move || {
        let state = match done_rx.recv_timeout(PATIENCE) {
            Ok(Ok(drawn)) => State::Done(drawn),
            Ok(Err(e)) => State::Failed(e),
            Err(_) => State::Failed("took too long to draw".to_string()),
        };
        cache().lock().unwrap().insert(k, state);
        let _ = tx.send(Msg::Diagram);
    });
}

/// Text as shown: the lines, and the pictures among them, each at the
/// line it starts on, taking as many lines as it has rows.
#[derive(Clone, Debug, Default)]
pub struct Shown {
    pub text: String,
    pub pictures: Vec<(usize, Arc<Picture>)>,
}

/// The text as shown: every ```mermaid fence replaced by its drawing, or
/// by a word on how that is going.
pub fn display(text: &str, target: &Target, tx: &Sender<Msg>) -> Shown {
    let segments = segments(text);
    if segments.len() == 1 && matches!(segments[0], Segment::Text(_)) {
        return Shown {
            text: text.to_string(),
            pictures: Vec::new(),
        };
    }
    let mut lines: Vec<String> = Vec::new();
    let mut pictures = Vec::new();
    for s in segments {
        match s {
            Segment::Text(t) => lines.extend(t.lines().map(str::to_owned)),
            Segment::Mermaid(source) => {
                ask(&source, target, tx);
                let state = cache().lock().unwrap().get(&key(&source, target)).cloned();
                match state {
                    Some(State::Done(Drawn::Text(drawn))) => {
                        lines.extend(drawn.lines().map(str::to_owned));
                    }
                    Some(State::Done(Drawn::Picture(p))) => {
                        pictures.push((lines.len(), p.clone()));
                        lines.extend(std::iter::repeat_n(String::new(), usize::from(p.rows)));
                    }
                    Some(State::Failed(e)) => {
                        lines.push("```mermaid".to_string());
                        lines.extend(source.lines().map(str::to_owned));
                        lines.push("```".to_string());
                        lines.push(format!("(diagram: {e})"));
                    }
                    Some(State::Pending) | None => lines.push("drawing the diagram…".to_string()),
                }
            }
        }
    }
    Shown {
        text: lines.join("\n"),
        pictures,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fences_are_split_out_and_anything_else_is_text() {
        assert_eq!(
            segments("plain\nwords"),
            [Segment::Text("plain\nwords".to_string())]
        );
        let text = "see:\n```mermaid\nflowchart LR\n  A --> B\n```\nand ```rust\nlet x;\n```";
        assert_eq!(
            segments(text),
            [
                Segment::Text("see:".to_string()),
                Segment::Mermaid("flowchart LR\n  A --> B".to_string()),
                Segment::Text("and ```rust\nlet x;\n```".to_string()),
            ]
        );
        assert_eq!(
            segments("``` Mermaid \ngraph TD\n  X"),
            [Segment::Mermaid("graph TD\n  X".to_string())],
            "an open fence runs to the end; the word is read loosely"
        );
    }

    #[test]
    fn the_bundled_face_is_loaded_and_answers_for_sans_serif() {
        use resvg::usvg::fontdb::{Family, Query};
        let db = fonts();
        let inter = db.query(&Query {
            families: &[Family::Name(FONT_FAMILY)],
            ..Default::default()
        });
        assert!(inter.is_some(), "the bundled face is in the database");
        let generic = db.query(&Query {
            families: &[Family::SansSerif],
            ..Default::default()
        });
        assert_eq!(generic, inter, "sans-serif resolves to the bundled face");
    }

    #[test]
    fn a_flowchart_is_drawn_with_boxes_and_a_bad_one_says_why() {
        let drawn = render_text("flowchart LR\n  A[Build] --> B[Test]", 80).unwrap();
        assert!(drawn.contains("Build") && drawn.contains('┌'), "{drawn}");
        assert!(drawn.lines().count() > 2, "{drawn}");
        let err = render_text("not a diagram at all", 80).unwrap_err();
        assert!(!err.is_empty());
    }

    #[test]
    fn a_picture_fills_whole_cells_and_never_more_than_the_room() {
        let p =
            render_picture("flowchart LR\n  A[Build] --> B[Test]", 40, (10, 20), false).unwrap();
        assert!(p.cols >= 1 && p.cols <= 40, "{} cols", p.cols);
        assert_eq!(p.image.width(), u32::from(p.cols) * 10);
        assert_eq!(p.image.height(), u32::from(p.rows) * 20);
        let corner = p.image.get_pixel(0, 0);
        assert_eq!(corner.0[3], 0, "around the drawing is transparent");
        assert!(
            p.image.pixels().any(|px| px.0[3] == 255),
            "the drawing itself is painted"
        );
        let tight =
            render_picture("flowchart LR\n  A[Build] --> B[Test]", 12, (10, 20), true).unwrap();
        assert!(tight.cols <= 12, "{} cols", tight.cols);
    }

    #[test]
    fn display_asks_once_and_shows_the_drawing_when_it_comes() {
        let (tx, rx) = std::sync::mpsc::channel();
        let text = "look:\n```mermaid\nflowchart LR\n  Q --> R\n```\ndone";
        let target = Target::Text { width: 60 };
        let first = display(text, &target, &tx);
        assert!(
            first.text.contains("drawing the diagram…"),
            "{}",
            first.text
        );
        assert!(matches!(rx.recv_timeout(PATIENCE * 2), Ok(Msg::Diagram)));
        let second = display(text, &target, &tx);
        assert!(
            second.text.starts_with("look:\n")
                && second.text.ends_with("\ndone")
                && second.text.contains('Q'),
            "{}",
            second.text
        );
        assert!(rx.try_recv().is_err(), "nothing drawn twice");
        // As a picture: blank lines hold its place, one per row.
        let target = Target::Picture {
            cols: 40,
            cell: (10, 20),
            light: false,
        };
        display(text, &target, &tx);
        assert!(matches!(rx.recv_timeout(PATIENCE * 2), Ok(Msg::Diagram)));
        let shown = display(text, &target, &tx);
        let (at, picture) = &shown.pictures[0];
        assert_eq!(*at, 1);
        let lines: Vec<&str> = shown.text.lines().collect();
        assert_eq!(lines.len(), 2 + usize::from(picture.rows));
        assert!(
            lines[1..=usize::from(picture.rows)]
                .iter()
                .all(|l| l.is_empty())
        );
        assert_eq!(lines.last(), Some(&"done"));
    }
}
