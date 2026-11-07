//! egui's bundled fonts stop at Latin and a few symbols, so a file name in Chinese,
//! Thai or Arabic would draw as boxes and the user could not tell what is being deleted.
//! Windows' own fonts fill that in. Nothing is bundled and nothing is downloaded.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use egui::{FontData, FontDefinitions, FontFamily};

/// Tried in this order, after egui's own fonts, per character. `.ttc` files use their
/// first face, which is the regular one of the family.
const FALLBACKS: [&str; 10] = [
    "segoeui.ttf",  // Latin extended, Greek, Cyrillic, Arabic, Hebrew, Vietnamese
    "seguisym.ttf", // Symbols, and a number of smaller scripts
    "msyh.ttc",     // Chinese
    "YuGothR.ttc",  // Japanese
    "malgun.ttf",   // Korean
    "LeelawUI.ttf", // Thai, Lao, Khmer
    "Nirmala.ttc",  // Indian scripts
    "ebrima.ttf",   // African scripts
    "gadugi.ttf",   // Cherokee, Canadian Aboriginal
    "mmrtext.ttf",  // Myanmar
];

/// egui stops the program over a font it cannot parse. These files are the system's,
/// so a glance at the signature is enough to keep out an empty or wrecked one.
fn looks_like_a_font(bytes: &[u8]) -> bool {
    const SIGNATURES: [&[u8]; 4] = [b"\x00\x01\x00\x00", b"OTTO", b"true", b"ttcf"];
    bytes.len() > 12 && SIGNATURES.iter().any(|s| bytes.starts_with(s))
}

/// Adds what `read` can give from `dir` behind the fonts already in `defs`, so the
/// Latin text of the interface, `í` of the name included, keeps egui's font. A file
/// that is missing or unusable is left out, and the rest still count.
fn add_fallbacks(
    defs: &mut FontDefinitions,
    dir: &Path,
    read: impl Fn(&Path) -> Option<Vec<u8>>,
) -> usize {
    let mut added = 0;
    for file in FALLBACKS {
        let Some(bytes) = read(&dir.join(file)).filter(|b| looks_like_a_font(b)) else {
            continue;
        };
        defs.font_data
            .insert(file.to_string(), Arc::new(FontData::from_owned(bytes)));
        for family in [FontFamily::Proportional, FontFamily::Monospace] {
            defs.families
                .entry(family)
                .or_default()
                .push(file.to_string());
        }
        added += 1;
    }
    added
}

fn fonts_dir() -> PathBuf {
    let windows =
        std::env::var_os("SystemRoot").map_or_else(|| r"C:\Windows".into(), PathBuf::from);
    windows.join("Fonts")
}

/// The fonts of the app: egui's own, then what this Windows has.
pub fn with_system_fallbacks() -> FontDefinitions {
    let mut defs = FontDefinitions::default();
    add_fallbacks(&mut defs, &fonts_dir(), |p| std::fs::read(p).ok());
    defs
}

#[cfg(test)]
mod tests {
    use super::*;
    use egui::FontId;
    use std::time::Instant;

    fn has_glyph(ctx: &egui::Context, c: char) -> bool {
        ctx.fonts_mut(|f| f.has_glyph(&FontId::proportional(13.0), c))
    }

    #[test]
    fn missing_files_are_skipped_without_a_fuss() {
        let mut defs = FontDefinitions::default();
        let before = defs.families[&FontFamily::Proportional].clone();
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            add_fallbacks(&mut defs, dir.path(), |p| std::fs::read(p).ok()),
            0
        );
        assert_eq!(defs.families[&FontFamily::Proportional], before);
    }

    #[test]
    fn a_file_that_is_no_font_is_left_out_and_the_others_still_count() {
        let mut defs = FontDefinitions::default();
        let added = add_fallbacks(&mut defs, Path::new("x"), |p| {
            if p.ends_with("malgun.ttf") {
                Some(b"\x00\x01\x00\x00 not really, but signed like one".to_vec())
            } else if p.ends_with("msyh.ttc") {
                Some(b"<html>not a font at all</html>".to_vec())
            } else if p.ends_with("segoeui.ttf") {
                Some(Vec::new())
            } else {
                None
            }
        });
        assert_eq!(added, 1);
        assert!(defs.font_data.contains_key("malgun.ttf"));
        assert!(!defs.font_data.contains_key("msyh.ttc"));
        assert!(!defs.font_data.contains_key("segoeui.ttf"));
    }

    #[test]
    fn the_fonts_of_egui_stay_first() {
        let default = FontDefinitions::default();
        let defs = with_system_fallbacks();
        for family in [FontFamily::Proportional, FontFamily::Monospace] {
            let own = &default.families[&family];
            assert_eq!(&defs.families[&family][..own.len()], &own[..]);
        }
    }

    #[test]
    fn the_scripts_of_file_names_can_be_drawn_on_this_machine() {
        let ctx = egui::Context::default();
        let start = Instant::now();
        let defs = with_system_fallbacks();
        let read = start.elapsed();
        let bytes: usize = defs.font_data.values().map(|d| d.bytes().len()).sum();
        ctx.set_fonts(defs);
        let mut out = ctx.run_ui(egui::RawInput::default(), |_| {});
        out.textures_delta.clear();
        eprintln!(
            "fallback fonts: read in {read:?}, {} MiB, first frame after {:?}",
            bytes / (1024 * 1024),
            start.elapsed()
        );
        if !fonts_dir().join("segoeui.ttf").exists() {
            return;
        }
        for c in ['í', '中', 'あ', '한', 'ไ', 'ع', 'א', 'Ж', 'क'] {
            assert!(has_glyph(&ctx, c), "no glyph for {c}");
        }
        // Without them egui has no glyph for these, which is the whole point.
        let bare = egui::Context::default();
        let mut out = bare.run_ui(egui::RawInput::default(), |_| {});
        out.textures_delta.clear();
        assert!(!has_glyph(&bare, '中'));
    }
}
