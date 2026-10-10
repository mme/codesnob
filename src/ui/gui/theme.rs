//! The window's colors. A terminal lends the viewer its palette: the sixteen
//! ANSI colors that `base16` syntax and the status colors name, and its
//! default foreground and background. A window has none, so they come from
//! the desktop's theme where it has one (Omarchy's, followed as it changes)
//! and from a built-in palette elsewhere.

use std::path::PathBuf;
use std::process::Command;

use ratatui::style::Color;

/// A color as 0xRRGGBB.
pub type Rgb = u32;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Theme {
    /// The ANSI colors 0–15: black, red, green, yellow, blue, magenta, cyan,
    /// white, then their bright forms.
    pub ansi: [Rgb; 16],
    pub fg: Rgb,
    pub bg: Rgb,
    /// Behind selected text.
    pub selection: Rgb,
    /// The status bar and the panel's input line: a step off the background.
    pub chrome: Rgb,
    pub accent: Rgb,
    /// The theme is for a light background.
    pub light: bool,
}

impl Theme {
    /// Tomorrow Night.
    pub fn dark() -> Self {
        Self {
            ansi: [
                0x1d1f21, 0xcc6666, 0xb5bd68, 0xf0c674, 0x81a2be, 0xb294bb, 0x8abeb7, 0xc5c8c6,
                0x707880, 0xd54e53, 0xb9ca4a, 0xe7c547, 0x7aa6da, 0xc397d8, 0x70c0b1, 0xeaeaea,
            ],
            fg: 0xc5c8c6,
            bg: 0x1d1f21,
            selection: 0x373b41,
            chrome: 0x282a2e,
            accent: 0x81a2be,
            light: false,
        }
    }

    /// Tomorrow.
    pub fn light() -> Self {
        Self {
            ansi: [
                0x1d1f21, 0xc82829, 0x718c00, 0xc8a600, 0x4271ae, 0x8959a8, 0x3e999f, 0x8e908c,
                0x969896, 0xc82829, 0x718c00, 0xeab700, 0x4271ae, 0x8959a8, 0x3e999f, 0x1d1f21,
            ],
            fg: 0x4d4d4c,
            bg: 0xffffff,
            selection: 0xd6d6d6,
            chrome: 0xefefef,
            accent: 0x4271ae,
            light: true,
        }
    }

    /// The desktop's theme, if it has one we know; otherwise the built-in
    /// palette, light when `light`.
    pub fn detect(light: bool) -> Self {
        omarchy().unwrap_or_else(|| if light { Self::light() } else { Self::dark() })
    }

    /// A terminal color as the window shows it; `None` for the default,
    /// which the caller decides between foreground and background.
    pub fn color(&self, c: Color) -> Option<Rgb> {
        Some(match c {
            Color::Reset => return None,
            Color::Black => self.ansi[0],
            Color::Red => self.ansi[1],
            Color::Green => self.ansi[2],
            Color::Yellow => self.ansi[3],
            Color::Blue => self.ansi[4],
            Color::Magenta => self.ansi[5],
            Color::Cyan => self.ansi[6],
            Color::Gray => self.ansi[7],
            Color::DarkGray => self.ansi[8],
            Color::LightRed => self.ansi[9],
            Color::LightGreen => self.ansi[10],
            Color::LightYellow => self.ansi[11],
            Color::LightBlue => self.ansi[12],
            Color::LightMagenta => self.ansi[13],
            Color::LightCyan => self.ansi[14],
            Color::White => self.ansi[15],
            Color::Indexed(i) => self.indexed(i),
            Color::Rgb(r, g, b) => rgb(r, g, b),
        })
    }

    /// The xterm 256-color table over this theme's sixteen.
    fn indexed(&self, i: u8) -> Rgb {
        match i {
            0..=15 => self.ansi[i as usize],
            16..=231 => {
                let level = |v: u8| if v == 0 { 0 } else { 55 + v * 40 };
                let i = i - 16;
                rgb(level(i / 36), level(i / 6 % 6), level(i % 6))
            }
            _ => {
                let v = 8 + (i - 232) * 10;
                rgb(v, v, v)
            }
        }
    }
}

fn rgb(r: u8, g: u8, b: u8) -> Rgb {
    (u32::from(r) << 16) | (u32::from(g) << 8) | u32::from(b)
}

/// Where Omarchy keeps the current theme; it rewrites what is under it on
/// every theme change.
pub fn omarchy_dir() -> Option<PathBuf> {
    let dir = PathBuf::from(std::env::var_os("HOME")?).join(".local/state/omarchy/current");
    dir.join("theme/colors.toml").is_file().then_some(dir)
}

/// The current Omarchy theme, as its own resolver reads it, or straight
/// from its colors.toml where the resolver is missing.
pub fn omarchy() -> Option<Theme> {
    let dir = omarchy_dir()?;
    let resolved = resolver_output().unwrap_or_default();
    let raw = std::fs::read_to_string(dir.join("theme/colors.toml")).unwrap_or_default();
    from_colors(&resolved, &raw)
}

/// `omarchy-theme-color --all`: every name the theme answers to, aliases
/// and derived shades resolved, one `key<TAB>value` per line.
fn resolver_output() -> Option<String> {
    let home = PathBuf::from(std::env::var_os("HOME")?);
    let bundled = home.join(".local/share/omarchy/bin/omarchy-theme-color");
    let program = if bundled.is_file() {
        bundled
    } else {
        PathBuf::from("omarchy-theme-color")
    };
    let out = Command::new(program).arg("--all").output().ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).into_owned())
}

/// A theme from the resolver's `key<TAB>value` lines, with colors.toml's
/// own `key = "value"` lines filling in what the resolver did not say.
fn from_colors(resolved: &str, raw: &str) -> Option<Theme> {
    let mut colors: Vec<(String, String)> = resolved
        .lines()
        .filter_map(|l| l.split_once('\t'))
        .map(|(k, v)| (k.trim().to_string(), v.trim().to_string()))
        .collect();
    colors.extend(raw.lines().filter_map(|l| {
        let l = l.trim();
        if l.starts_with('#') {
            return None;
        }
        let (k, v) = l.split_once('=')?;
        let v = v.trim();
        // Quoted, ignoring what follows the closing quote.
        let v = match v.strip_prefix(['"', '\'']) {
            Some(rest) => rest.split(['"', '\'']).next()?,
            None => v,
        };
        Some((
            k.trim().trim_matches(['"', '\'']).to_string(),
            v.to_string(),
        ))
    }));
    let get = |key: &str| {
        colors
            .iter()
            .find(|(k, _)| k == key)
            .and_then(|(_, v)| parse_hex(v))
    };
    let any = |keys: &[&str]| keys.iter().find_map(|k| get(k));
    let bg = any(&["background", "bg", "color0"])?;
    let fg = any(&["foreground", "fg", "color7"])?;
    // The ANSI colors by their semantic names first, as Omarchy defines them.
    let names: [&[&str]; 16] = [
        &["color0", "background"],
        &["color1", "red"],
        &["color2", "green"],
        &["color3", "yellow"],
        &["color4", "blue"],
        &["color5", "magenta", "purple"],
        &["color6", "cyan"],
        &["color7", "foreground"],
        &["color8", "muted", "dark_foreground"],
        &["color9", "bright_red", "red"],
        &["color10", "bright_green", "green"],
        &["color11", "bright_yellow", "yellow"],
        &["color12", "bright_blue", "blue"],
        &["color13", "bright_magenta", "magenta"],
        &["color14", "bright_cyan", "cyan"],
        &["color15", "bright_foreground", "foreground"],
    ];
    let mut ansi = [0; 16];
    for (slot, keys) in ansi.iter_mut().zip(names) {
        *slot = any(keys).unwrap_or(fg);
    }
    let mode = colors
        .iter()
        .find(|(k, _)| k == "mode" || k == "theme_type")
        .map(|(_, v)| v.as_str());
    let light = match mode {
        Some("light") => true,
        Some("dark") => false,
        _ => luminance(bg) > 382,
    };
    Some(Theme {
        ansi,
        fg,
        bg,
        selection: any(&["selection", "selection_background", "color8"]).unwrap_or(fg),
        chrome: any(&["lighter_background", "lighter_bg"]).unwrap_or(bg),
        accent: any(&["accent", "blue", "color4"]).unwrap_or(fg),
        light,
    })
}

fn parse_hex(v: &str) -> Option<Rgb> {
    let hex = v.strip_prefix('#')?;
    (hex.len() == 6)
        .then(|| u32::from_str_radix(hex, 16).ok())
        .flatten()
}

/// The sum of the channels, as Omarchy judges a background light.
fn luminance(c: Rgb) -> u32 {
    (c >> 16) + ((c >> 8) & 0xff) + (c & 0xff)
}

#[cfg(test)]
mod tests {
    use super::*;

    const TOKYO: &str = r##"mode = "dark"

accent = "#7aa2f7"
selection = "#292e42"
muted = "#414868"

background = "#1a1b26"
lighter_background = "#24283b"
foreground = "#a9b1d6"
bright_foreground = "#c0caf5"

red = "#f7768e"   # a comment
yellow = "#e0af68"
green = "#9ece6a"
cyan = "#449dab"
blue = "#7aa2f7"
magenta = "#ad8ee6"
bright_red = "#ff7a93"
"##;

    #[test]
    fn colors_toml_alone_makes_a_theme() {
        let t = from_colors("", TOKYO).unwrap();
        assert_eq!(t.bg, 0x1a1b26);
        assert_eq!(t.fg, 0xa9b1d6);
        assert_eq!(t.ansi[0], 0x1a1b26);
        assert_eq!(t.ansi[1], 0xf7768e);
        assert_eq!(t.ansi[8], 0x414868);
        assert_eq!(t.ansi[9], 0xff7a93);
        // No bright green: the plain one stands in.
        assert_eq!(t.ansi[10], 0x9ece6a);
        assert_eq!(t.ansi[15], 0xc0caf5);
        assert_eq!(t.selection, 0x292e42);
        assert_eq!(t.chrome, 0x24283b);
        assert!(!t.light);
    }

    #[test]
    fn the_resolver_wins_over_the_file() {
        let resolved = "color10\t#b9f27c\nbackground\t#000000\nmode\tlight\n";
        let t = from_colors(resolved, TOKYO).unwrap();
        assert_eq!(t.ansi[10], 0xb9f27c);
        assert_eq!(t.bg, 0x000000);
        assert!(t.light);
    }

    #[test]
    fn mode_falls_back_to_the_background() {
        let t = from_colors("", "background = \"#fafafa\"\nforeground = \"#111111\"").unwrap();
        assert!(t.light);
        assert!(from_colors("", "foreground = \"#111111\"").is_none());
    }

    #[test]
    fn terminal_colors_map_through_the_theme() {
        let t = Theme::dark();
        assert_eq!(t.color(Color::Reset), None);
        assert_eq!(t.color(Color::DarkGray), Some(t.ansi[8]));
        assert_eq!(t.color(Color::Indexed(3)), Some(t.ansi[3]));
        assert_eq!(t.color(Color::Indexed(16)), Some(0x000000));
        assert_eq!(t.color(Color::Indexed(231)), Some(0xffffff));
        assert_eq!(t.color(Color::Indexed(196)), Some(0xff0000));
        assert_eq!(t.color(Color::Indexed(232)), Some(0x080808));
        assert_eq!(t.color(Color::Rgb(1, 2, 3)), Some(0x010203));
    }
}
