//! Two-language UI text. Screens call `l(english, indonesian)`.

use std::sync::atomic::{AtomicU8, Ordering};

use katharistis_core::settings::Language;
use windows::Win32::Globalization::GetUserDefaultUILanguage;

const EN: u8 = 0;
const ID: u8 = 1;

static ACTIVE: AtomicU8 = AtomicU8::new(EN);

/// Indonesian when the primary language id (low 10 bits) is 0x21, else English.
pub fn pick(lang_id: u16) -> Language {
    if lang_id & 0x3FF == 0x21 {
        Language::Indonesian
    } else {
        Language::English
    }
}

fn system_language() -> Language {
    pick(unsafe { GetUserDefaultUILanguage() })
}

/// Makes `lang` the active UI language; `Auto` follows Windows.
pub fn set_language(lang: Language) {
    let resolved = match lang {
        Language::Auto => system_language(),
        other => other,
    };
    let v = if resolved == Language::Indonesian {
        ID
    } else {
        EN
    };
    ACTIVE.store(v, Ordering::Relaxed);
}

pub fn l(en: &'static str, id: &'static str) -> &'static str {
    if ACTIVE.load(Ordering::Relaxed) == ID {
        id
    } else {
        en
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use katharistis_core::settings::Language;

    #[test]
    fn language_auto_picks_indonesian_for_primary_0x21() {
        assert_eq!(pick(0x0421), Language::Indonesian);
        assert_eq!(pick(0x0409), Language::English);
    }
}
