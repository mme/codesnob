//! The window's fonts: code in the terminal's font at the terminal's size,
//! so the two read alike, and everything around it in the desktop's
//! proportional font.

use std::path::{Path, PathBuf};
use std::process::Command;

/// A font family at a size in pixels.
#[derive(Clone, Debug, PartialEq)]
pub struct Font {
    pub family: String,
    pub px: f32,
}

/// A font family and a size in pixels, each when a configuration says.
type Found = (Option<String>, Option<f32>);

/// When nothing says otherwise.
const FALLBACK: (&str, f32) = ("JetBrainsMono Nerd Font", 12.0);

/// Points to pixels at the 96 DPI terminals assume; the desktop's scale
/// applies on top, as it does to them.
const PX_PER_PT: f32 = 96.0 / 72.0;

/// The font for code: what the default terminal draws with, as far as its
/// configuration says, with `family` and `px` overriding either part.
pub fn code(family: Option<String>, px: Option<f32>) -> Font {
    let term = terminal_font();
    let family = family
        .or_else(|| term.as_ref().and_then(|t| t.0.clone()))
        .unwrap_or_else(|| FALLBACK.0.to_string());
    let px = px
        .or_else(|| term.as_ref().and_then(|t| t.1))
        .unwrap_or(FALLBACK.1);
    Font { family, px }
}

/// The desktop's proportional font, as fontconfig picks it.
pub fn ui(family: Option<String>) -> String {
    family
        .or_else(|| {
            let out = Command::new("fc-match")
                .args(["-f", "%{family[0]}", "sans-serif"])
                .output()
                .ok()?;
            let name = String::from_utf8_lossy(&out.stdout).trim().to_string();
            (out.status.success() && !name.is_empty()).then_some(name)
        })
        .unwrap_or_else(|| "sans-serif".to_string())
}

/// The terminals we read, by the desktop entry `xdg-terminal-exec` names.
const TERMINALS: [(&str, &str); 4] = [
    ("foot.desktop", "foot"),
    ("com.mitchellh.ghostty.desktop", "ghostty"),
    ("kitty.desktop", "kitty"),
    ("Alacritty.desktop", "alacritty"),
];

/// The default terminal's font family and size in pixels, each when its
/// configuration says; otherwise the first terminal configured.
fn terminal_font() -> Option<Found> {
    let config = PathBuf::from(std::env::var_os("HOME")?).join(".config");
    let default = Command::new("xdg-terminal-exec")
        .arg("--print-id")
        .output()
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .and_then(|id| {
            let id = id.split(':').next().unwrap_or_default().to_string();
            TERMINALS.iter().find(|(d, _)| *d == id).map(|(_, t)| *t)
        });
    let order = default.into_iter().chain(TERMINALS.iter().map(|(_, t)| *t));
    order
        .filter_map(|t| read(t, &config))
        .find(|(family, px)| family.is_some() || px.is_some())
}

fn read(terminal: &str, config: &Path) -> Option<Found> {
    let (path, parse): (_, fn(&str) -> Found) = match terminal {
        "foot" => (config.join("foot/foot.ini"), foot),
        "ghostty" => (config.join("ghostty/config"), ghostty),
        "kitty" => (config.join("kitty/kitty.conf"), kitty),
        "alacritty" => (config.join("alacritty/alacritty.toml"), alacritty),
        _ => return None,
    };
    Some(parse(&std::fs::read_to_string(path).ok()?))
}

/// `font=Family:size=9` (points) or `:pixelsize=12`, in `[main]`; the
/// first of a comma-separated list.
fn foot(text: &str) -> Found {
    let mut section = "main";
    for line in text.lines().map(str::trim) {
        if let Some(name) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
            section = name;
            continue;
        }
        let Some(("font", value)) = line.split_once('=').map(|(k, v)| (k.trim(), v.trim())) else {
            continue;
        };
        if section != "main" {
            continue;
        }
        let first = value.split(',').next().unwrap_or_default();
        let mut parts = first.split(':');
        let family = parts.next().map(str::trim).filter(|f| !f.is_empty());
        let mut px = None;
        for opt in parts {
            match opt.split_once('=') {
                Some(("size", v)) => px = v.trim().parse::<f32>().ok().map(|pt| pt * PX_PER_PT),
                Some(("pixelsize", v)) => px = v.trim().parse().ok(),
                _ => {}
            }
        }
        return (family.map(str::to_string), px);
    }
    (None, None)
}

/// `font-family = "Family"` and `font-size = 9` (points).
fn ghostty(text: &str) -> Found {
    let mut family = None;
    let mut px = None;
    for line in text.lines().map(str::trim) {
        let Some((k, v)) = line.split_once('=') else {
            continue;
        };
        let v = v.trim().trim_matches('"');
        match k.trim() {
            "font-family" if family.is_none() && !v.is_empty() => family = Some(v.to_string()),
            "font-size" => px = v.parse::<f32>().ok().map(|pt| pt * PX_PER_PT),
            _ => {}
        }
    }
    (family, px)
}

/// `font_family Family` and `font_size 9.0` (points).
fn kitty(text: &str) -> Found {
    let mut family = None;
    let mut px = None;
    for line in text.lines().map(str::trim) {
        let Some((k, v)) = line.split_once(char::is_whitespace) else {
            continue;
        };
        let v = v.trim();
        match k {
            "font_family" if v != "monospace" => family = Some(v.to_string()),
            "font_size" => px = v.parse::<f32>().ok().map(|pt| pt * PX_PER_PT),
            _ => {}
        }
    }
    (family, px)
}

/// `[font]` with `size = 10` (points) and `normal = { family = "…" }`.
fn alacritty(text: &str) -> Found {
    let mut section = "";
    let mut family = None;
    let mut px = None;
    for line in text.lines().map(str::trim) {
        if let Some(name) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
            section = name;
            continue;
        }
        let Some((k, v)) = line.split_once('=') else {
            continue;
        };
        match (section, k.trim()) {
            ("font", "size") => px = v.trim().parse::<f32>().ok().map(|pt| pt * PX_PER_PT),
            ("font", "normal") => {
                family = v
                    .split_once("family")
                    .and_then(|(_, rest)| rest.split('"').nth(1))
                    .map(str::to_string);
            }
            ("font.normal", "family") => family = Some(v.trim().trim_matches('"').to_string()),
            _ => {}
        }
    }
    (family, px)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn foot_size_is_points() {
        let ini = "[main]\ninclude=x\nfont=JetBrainsMono Nerd Font:size=9\n\n[colors]\nfont=nope\n";
        assert_eq!(
            foot(ini),
            (Some("JetBrainsMono Nerd Font".into()), Some(12.0))
        );
        let px = "font=Iosevka:weight=bold:pixelsize=15,Noto Emoji\n";
        assert_eq!(foot(px), (Some("Iosevka".into()), Some(15.0)));
    }

    #[test]
    fn ghostty_kitty_alacritty() {
        assert_eq!(
            ghostty("font-family = \"JetBrainsMono Nerd Font\"\nfont-size = 9\n"),
            (Some("JetBrainsMono Nerd Font".into()), Some(12.0))
        );
        assert_eq!(
            kitty("font_family      JetBrainsMono Nerd Font\nfont_size        9.0\n"),
            (Some("JetBrainsMono Nerd Font".into()), Some(12.0))
        );
        assert_eq!(
            alacritty("[font]\nnormal = { family = \"JetBrainsMono Nerd Font\" }\nsize = 9\n"),
            (Some("JetBrainsMono Nerd Font".into()), Some(12.0))
        );
    }
}
