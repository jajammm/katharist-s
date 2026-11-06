//! Teal-on-charcoal palette with a light counterpart.

use egui::{Color32, Visuals};

/// Map tile fills by kind of area, and the text drawn on them. None of them means danger.
pub struct TilePalette {
    pub profile: Color32,
    pub program_files: Color32,
    pub windows: Color32,
    pub other: Color32,
    pub rest: Color32,
    pub text: Color32,
}

pub struct Palette {
    pub bg: Color32,
    pub panel: Color32,
    pub text: Color32,
    pub weak: Color32,
    pub accent: Color32,
    pub on_accent: Color32,
    /// Dates older than the "not changed in a while" setting.
    pub stale: Color32,
    pub tiles: TilePalette,
}

pub const DARK: Palette = Palette {
    bg: Color32::from_rgb(0x14, 0x18, 0x1c),
    panel: Color32::from_rgb(0x1b, 0x21, 0x27),
    text: Color32::from_rgb(0xe6, 0xed, 0xf3),
    weak: Color32::from_rgb(0x9a, 0xa7, 0xb2),
    accent: Color32::from_rgb(0x2d, 0xd4, 0xbf),
    on_accent: Color32::from_rgb(0x08, 0x14, 0x12),
    stale: Color32::from_rgb(0xfa, 0xcc, 0x15),
    tiles: TilePalette {
        profile: Color32::from_rgb(0x1f, 0x6f, 0x68),
        program_files: Color32::from_rgb(0x2f, 0x5f, 0x8f),
        windows: Color32::from_rgb(0x5b, 0x4f, 0x8a),
        other: Color32::from_rgb(0x4a, 0x55, 0x60),
        rest: Color32::from_rgb(0x33, 0x3b, 0x44),
        text: Color32::from_rgb(0xff, 0xff, 0xff),
    },
};

pub const LIGHT: Palette = Palette {
    bg: Color32::from_rgb(0xf6, 0xf8, 0xfa),
    panel: Color32::from_rgb(0xe9, 0xee, 0xf2),
    text: Color32::from_rgb(0x1f, 0x23, 0x28),
    weak: Color32::from_rgb(0x57, 0x60, 0x6a),
    accent: Color32::from_rgb(0x0f, 0x76, 0x6e),
    on_accent: Color32::from_rgb(0xff, 0xff, 0xff),
    stale: Color32::from_rgb(0x85, 0x5d, 0x00),
    tiles: TilePalette {
        profile: Color32::from_rgb(0xa7, 0xe3, 0xda),
        program_files: Color32::from_rgb(0xb5, 0xd3, 0xf0),
        windows: Color32::from_rgb(0xd5, 0xcd, 0xf2),
        other: Color32::from_rgb(0xd3, 0xd9, 0xdf),
        rest: Color32::from_rgb(0xc2, 0xc8, 0xcf),
        text: Color32::from_rgb(0x1f, 0x23, 0x28),
    },
};

pub fn palette(dark: bool) -> &'static Palette {
    if dark {
        &DARK
    } else {
        &LIGHT
    }
}

pub fn visuals(p: &Palette, dark: bool) -> Visuals {
    let mut v = if dark {
        Visuals::dark()
    } else {
        Visuals::light()
    };
    v.panel_fill = p.bg;
    v.window_fill = p.panel;
    v.extreme_bg_color = p.panel;
    v.faint_bg_color = p.panel;
    v.override_text_color = Some(p.text);
    v.weak_text_color = Some(p.weak);
    v.hyperlink_color = p.accent;
    v.selection.bg_fill = p.accent;
    v.selection.stroke.color = p.on_accent;
    v.widgets.noninteractive.fg_stroke.color = p.text;
    v.widgets.inactive.weak_bg_fill = p.panel;
    v.widgets.inactive.bg_fill = p.panel;
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lin(c: u8) -> f64 {
        let s = f64::from(c) / 255.0;
        if s <= 0.03928 {
            s / 12.92
        } else {
            ((s + 0.055) / 1.055).powf(2.4)
        }
    }

    fn luminance(c: Color32) -> f64 {
        0.2126 * lin(c.r()) + 0.7152 * lin(c.g()) + 0.0722 * lin(c.b())
    }

    pub fn contrast(a: Color32, b: Color32) -> f64 {
        let (x, y) = (luminance(a), luminance(b));
        let (hi, lo) = if x > y { (x, y) } else { (y, x) };
        (hi + 0.05) / (lo + 0.05)
    }

    #[test]
    fn text_is_readable_in_both_themes() {
        for (name, p) in [("dark", &DARK), ("light", &LIGHT)] {
            let pairs = [
                ("text/bg", p.text, p.bg),
                ("text/panel", p.text, p.panel),
                ("weak/bg", p.weak, p.bg),
                ("weak/panel", p.weak, p.panel),
                ("accent/bg", p.accent, p.bg),
                ("accent/panel", p.accent, p.panel),
                ("on_accent/accent", p.on_accent, p.accent),
                ("stale/bg", p.stale, p.bg),
                ("stale/panel", p.stale, p.panel),
                ("tile text/profile", p.tiles.text, p.tiles.profile),
                (
                    "tile text/program files",
                    p.tiles.text,
                    p.tiles.program_files,
                ),
                ("tile text/windows", p.tiles.text, p.tiles.windows),
                ("tile text/other", p.tiles.text, p.tiles.other),
                ("tile text/rest", p.tiles.text, p.tiles.rest),
            ];
            for (label, fg, bg) in pairs {
                let c = contrast(fg, bg);
                println!("{name} {label}: {c:.2}:1");
                assert!(c >= 4.5, "{name} {label} is {c:.2}:1");
            }
        }
    }
}
