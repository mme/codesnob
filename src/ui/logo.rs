//! The sticker on the empty screen, drawn as a terminal image: Sixel, kitty
//! or iTerm2, whichever the terminal answers to. Terminals that show no
//! images get the words alone.

use std::time::Duration;

use image::{DynamicImage, Rgba, RgbaImage, imageops::FilterType};
use ratatui::buffer::Buffer;
use ratatui::layout::{Rect, Size};
use ratatui::widgets::Widget;
use ratatui_image::picker::cap_parser::QueryStdioOptions;
use ratatui_image::picker::{Capability, Picker, ProtocolType};
use ratatui_image::protocol::Protocol;
use ratatui_image::{Image, Resize};

/// Light lettering on a paper-white sticker, for dark terminals.
pub(super) const ON_DARK: &[u8] = include_bytes!("../../assets/codesnob-dark.png");
/// Black lettering, for light terminals.
pub(super) const ON_LIGHT: &[u8] = include_bytes!("../../assets/codesnob-transparent.png");

/// How long a terminal gets to answer what it can show.
const QUERY_TIMEOUT: Duration = Duration::from_millis(500);

/// The sticker is never drawn smaller than this, in cells.
const MIN_WIDTH: u16 = 16;
const MIN_HEIGHT: u16 = 3;

pub struct Logo {
    picker: Picker,
    light: bool,
    /// The terminal's background, when it said: what the sticker's soft
    /// edges are blended onto.
    background: Option<Rgba<u8>>,
    /// The sticker, decoded the first time it is drawn.
    image: Option<DynamicImage>,
    /// The sticker encoded for the size it was last drawn at.
    encoded: Option<(Size, Protocol)>,
}

impl Logo {
    /// Ask the terminal whether it shows images, and at what cell size.
    /// Call it with the terminal in raw mode and before anything reads
    /// input, which would take the answers. `None` when it shows none.
    pub fn detect(light: bool) -> Option<Self> {
        let options = QueryStdioOptions {
            timeout: QUERY_TIMEOUT,
            // Sixel transparency is all or nothing: the sticker's soft edges
            // are blended onto the terminal's own background.
            terminal_background_color_osc: true,
            ..Default::default()
        };
        let picker = Picker::from_query_stdio_with_options(options).ok()?;
        if picker.protocol_type() == ProtocolType::Halfblocks {
            return None;
        }
        let background = picker.capabilities().iter().find_map(|c| match c {
            Capability::Background(r, g, b) => Some(Rgba([*r, *g, *b, 255])),
            _ => None,
        });
        let mut logo = Self::new(picker, light);
        logo.background = background;
        Some(logo)
    }

    pub fn new(picker: Picker, light: bool) -> Self {
        Self {
            picker,
            light,
            background: None,
            image: None,
            encoded: None,
        }
    }

    fn image(&mut self) -> Option<&DynamicImage> {
        if self.image.is_none() {
            let bytes = if self.light { ON_LIGHT } else { ON_DARK };
            self.image = image::load_from_memory(bytes).ok();
        }
        self.image.as_ref()
    }

    fn resize() -> Resize {
        Resize::Fit(Some(FilterType::Triangle))
    }

    /// The size the sticker takes, in cells, at its largest within `room`;
    /// `None` when it would be too small to be worth drawing.
    pub fn size_in(&mut self, room: Size) -> Option<Size> {
        let font = self.picker.font_size();
        let size = Self::resize().size_for(self.image()?, font, room);
        (size.width >= MIN_WIDTH && size.height >= MIN_HEIGHT).then_some(size)
    }

    /// The sticker for `size` cells: scaled to the pixels they hold and
    /// centered in them. What is around it stays transparent. Every pixel
    /// of the sticker itself is made opaque, its soft edges blended onto
    /// the terminal's background, as Sixel draws a pixel or leaves it.
    fn prepared(&mut self, size: Size) -> Option<DynamicImage> {
        let font = self.picker.font_size();
        let w = u32::from(size.width) * u32::from(font.width);
        let h = u32::from(size.height) * u32::from(font.height);
        let background = self.background;
        let scaled = self.image()?.resize(w, h, FilterType::Triangle).to_rgba8();
        let mut canvas = RgbaImage::new(w, h);
        let (left, top) = (
            (w - scaled.width().min(w)) / 2,
            (h - scaled.height().min(h)) / 2,
        );
        for (x, y, p) in scaled.enumerate_pixels() {
            let Rgba([r, g, b, a]) = *p;
            let pixel = match (a, background) {
                (0, _) => continue,
                (255, _) => *p,
                (a, Some(Rgba([br, bg, bb, _]))) => {
                    let mix = |c: u8, under: u8| {
                        let (c, under, a) = (u32::from(c), u32::from(under), u32::from(a));
                        ((c * a + under * (255 - a) + 127) / 255) as u8
                    };
                    Rgba([mix(r, br), mix(g, bg), mix(b, bb), 255])
                }
                (a, None) if a >= 128 => Rgba([r, g, b, 255]),
                _ => continue,
            };
            if x + left < w && y + top < h {
                canvas.put_pixel(x + left, y + top, pixel);
            }
        }
        Some(canvas.into())
    }

    /// Draw the sticker in `area`, a size `size_in` gave.
    pub fn render(&mut self, area: Rect, buf: &mut Buffer) {
        let size = area.as_size();
        if self.encoded.as_ref().is_none_or(|(s, _)| *s != size) {
            let Some(image) = self.prepared(size) else {
                return;
            };
            // Already the size the cells hold: nothing left to resize.
            self.encoded = self
                .picker
                .new_protocol(image, size, Resize::Fit(None))
                .ok()
                .map(|p| (size, p));
        }
        if let Some((_, protocol)) = &self.encoded {
            Image::new(protocol).render(area, buf);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn around_the_sticker_is_transparent_and_its_edges_are_solid() {
        let mut logo = Logo::new(Picker::halfblocks(), false);
        logo.background = Some(Rgba([0x1a, 0x1b, 0x26, 255]));
        let size = logo.size_in(Size::new(40, 20)).unwrap();
        let font = logo.picker.font_size();
        let image = logo.prepared(size).unwrap().to_rgba8();
        assert_eq!(
            image.dimensions(),
            (
                u32::from(size.width) * u32::from(font.width),
                u32::from(size.height) * u32::from(font.height)
            ),
            "exactly the pixels of its cells"
        );
        assert!(
            image.pixels().all(|p| p[3] == 0 || p[3] == 255),
            "every pixel drawn or left, nothing in between"
        );
        assert_eq!(image.get_pixel(0, 0)[3], 0, "the corner is left");
        let (w, h) = image.dimensions();
        assert_eq!(image.get_pixel(w / 2, h / 2)[3], 255, "the middle is drawn");
        let edge_blended = image
            .pixels()
            .any(|p| p[3] == 255 && p.0[..3] != [255, 255, 255] && p.0[..3] != [0, 0, 0]);
        assert!(edge_blended, "soft edges take on the background");

        // Without a background to blend onto, edges are cut at half.
        logo.background = None;
        let image = logo.prepared(size).unwrap().to_rgba8();
        assert!(image.pixels().all(|p| p[3] == 0 || p[3] == 255));
    }
}
