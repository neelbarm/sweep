//! Colour palette with a graceful 256-colour fallback.
//!
//! Every colour is defined twice: a truecolour RGB value for modern terminals
//! and the closest xterm-256 index for everything else. `sweep` picks once, at
//! startup, by looking at `COLORTERM`.

use ratatui::style::Color;

/// A palette entry: 24-bit value plus its 256-colour stand-in.
#[derive(Debug, Clone, Copy)]
pub struct Swatch {
    pub rgb: (u8, u8, u8),
    pub indexed: u8,
}

const fn sw(r: u8, g: u8, b: u8, indexed: u8) -> Swatch {
    Swatch {
        rgb: (r, g, b),
        indexed,
    }
}

pub const BRAND: Swatch = sw(0x7C, 0x9C, 0xF5, 111);
pub const TEXT: Swatch = sw(0xD5, 0xDB, 0xE5, 252);
pub const DIM: Swatch = sw(0x6B, 0x72, 0x80, 244);
pub const FAINT: Swatch = sw(0x3B, 0x41, 0x4C, 238);
pub const OK: Swatch = sw(0x7B, 0xD8, 0x8F, 114);
pub const WARN: Swatch = sw(0xE6, 0xB4, 0x55, 179);
pub const DANGER: Swatch = sw(0xF2, 0x77, 0x7A, 210);
pub const ACCENT: Swatch = sw(0x9D, 0x7C, 0xF5, 141);
/// Zebra stripe and selection backgrounds.
pub const ZEBRA: Swatch = sw(0x1B, 0x1E, 0x24, 235);
pub const ROW_SEL: Swatch = sw(0x27, 0x2C, 0x38, 237);
pub const FLASH: Swatch = sw(0x2E, 0x3A, 0x2E, 22);

/// Per-kind colour, muted enough to read as a badge rather than an alarm.
pub fn kind_swatch(kind: &str) -> Swatch {
    match kind {
        "node_modules" => sw(0x86, 0xC0, 0x6C, 107),
        "js_cache" => sw(0xE0, 0xAF, 0x68, 179),
        "js_build" => sw(0xD7, 0xA3, 0xE0, 176),
        "venv" => sw(0x7A, 0xA2, 0xF7, 111),
        "pycache" => sw(0x6C, 0x9B, 0xD1, 74),
        "py_tooling" => sw(0x5F, 0xB3, 0xB3, 73),
        "cargo_target" => sw(0xE0, 0x7A, 0x5F, 173),
        "gradle" => sw(0x5E, 0xC4, 0xB6, 79),
        "flutter" => sw(0x53, 0xB4, 0xF0, 75),
        "cocoapods" => sw(0xF5, 0x8A, 0x8A, 210),
        "derived_data" => sw(0xC3, 0x9B, 0xD3, 140),
        "go_vendor" => sw(0x6F, 0xD6, 0xE2, 81),
        "zig" => sw(0xF5, 0xA9, 0x7F, 216),
        "terraform" => sw(0xA7, 0x8B, 0xFA, 141),
        _ => TEXT,
    }
}

/// Resolves swatches to concrete terminal colours.
#[derive(Debug, Clone, Copy)]
pub struct Theme {
    pub truecolor: bool,
}

impl Theme {
    /// Detect once from the environment.
    pub fn detect() -> Theme {
        let truecolor = std::env::var("COLORTERM")
            .map(|v| {
                let v = v.to_ascii_lowercase();
                v.contains("truecolor") || v.contains("24bit")
            })
            .unwrap_or(false)
            || std::env::var("TERM_PROGRAM")
                .map(|v| {
                    matches!(
                        v.as_str(),
                        "iTerm.app" | "WezTerm" | "vscode" | "ghostty" | "Apple_Terminal"
                    )
                })
                .unwrap_or(false);
        Theme { truecolor }
    }

    pub fn c(&self, s: Swatch) -> Color {
        if self.truecolor {
            Color::Rgb(s.rgb.0, s.rgb.1, s.rgb.2)
        } else {
            Color::Indexed(s.indexed)
        }
    }

    pub fn kind(&self, kind: &str) -> Color {
        self.c(kind_swatch(kind))
    }

    /// Blend two swatches, `t` in `0.0..=1.0`. Falls back to the nearer of the
    /// two indexed colours when the terminal cannot do 24-bit.
    pub fn blend(&self, a: Swatch, b: Swatch, t: f32) -> Color {
        let t = t.clamp(0.0, 1.0);
        if self.truecolor {
            let mix = |x: u8, y: u8| (x as f32 + (y as f32 - x as f32) * t).round() as u8;
            Color::Rgb(
                mix(a.rgb.0, b.rgb.0),
                mix(a.rgb.1, b.rgb.1),
                mix(a.rgb.2, b.rgb.2),
            )
        } else if t < 0.5 {
            Color::Indexed(a.indexed)
        } else {
            Color::Indexed(b.indexed)
        }
    }
}

/// Spinner frames for the scanning indicator.
pub const SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

/// Eighth-width block characters, used for sub-cell precision in bar charts.
pub const BLOCKS: [&str; 9] = ["", "▏", "▎", "▍", "▌", "▋", "▊", "▉", "█"];

/// Render a fractional-width bar with sub-character precision.
pub fn bar(fraction: f64, width: usize) -> String {
    let fraction = fraction.clamp(0.0, 1.0);
    let exact = fraction * width as f64;
    let full = exact.floor() as usize;
    let rem = ((exact - full as f64) * 8.0).round() as usize;
    let mut s = String::with_capacity(width * 3);
    for _ in 0..full.min(width) {
        s.push('█');
    }
    if full < width && rem > 0 {
        s.push_str(BLOCKS[rem.min(8)]);
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bars_respect_their_width() {
        assert_eq!(bar(0.0, 10).chars().count(), 0);
        assert_eq!(bar(1.0, 10).chars().count(), 10);
        assert!(bar(0.5, 10).chars().count() <= 10);
        assert!(bar(2.0, 6).chars().count() <= 6);
        assert!(
            bar(0.06, 10).chars().count() >= 1,
            "small values still show"
        );
    }

    #[test]
    fn fallback_uses_indexed_colors() {
        let t = Theme { truecolor: false };
        assert!(matches!(t.c(BRAND), Color::Indexed(_)));
        let t = Theme { truecolor: true };
        assert!(matches!(t.c(BRAND), Color::Rgb(..)));
    }

    #[test]
    fn every_kind_has_a_distinct_color() {
        let kinds = crate::rules::all_kinds();
        let mut seen: Vec<(u8, u8, u8)> = kinds.iter().map(|k| kind_swatch(k).rgb).collect();
        let n = seen.len();
        seen.sort_unstable();
        seen.dedup();
        assert_eq!(seen.len(), n, "kind colours must be distinguishable");
    }

    #[test]
    fn blending_moves_between_endpoints() {
        let t = Theme { truecolor: true };
        assert_eq!(t.blend(OK, DANGER, 0.0), t.c(OK));
        assert_eq!(t.blend(OK, DANGER, 1.0), t.c(DANGER));
    }
}
