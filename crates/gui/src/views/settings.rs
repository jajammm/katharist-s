use std::path::PathBuf;

use katharistis_core::settings::{Language, Settings, Theme};

use crate::i18n::l;

/// Draws the settings form. Returns true when any value changed.
pub fn show(ui: &mut egui::Ui, s: &mut Settings, new_excluded: &mut String) -> bool {
    let mut changed = false;
    ui.heading(l("Settings", "Pengaturan"));
    ui.add_space(12.0);

    ui.horizontal(|ui| {
        ui.label(l("Language", "Bahasa"));
        let text = match s.language {
            Language::Auto => l("Follow Windows", "Ikuti Windows"),
            Language::English => "English",
            Language::Indonesian => "Bahasa Indonesia",
        };
        egui::ComboBox::from_id_salt("language")
            .selected_text(text)
            .show_ui(ui, |ui| {
                changed |= ui
                    .selectable_value(
                        &mut s.language,
                        Language::Auto,
                        l("Follow Windows", "Ikuti Windows"),
                    )
                    .changed();
                changed |= ui
                    .selectable_value(&mut s.language, Language::English, "English")
                    .changed();
                changed |= ui
                    .selectable_value(&mut s.language, Language::Indonesian, "Bahasa Indonesia")
                    .changed();
            });
    });

    ui.horizontal(|ui| {
        ui.label(l("Theme", "Tema"));
        changed |= ui
            .selectable_value(&mut s.theme, Theme::System, l("System", "Sistem"))
            .changed();
        changed |= ui
            .selectable_value(&mut s.theme, Theme::Light, l("Light", "Terang"))
            .changed();
        changed |= ui
            .selectable_value(&mut s.theme, Theme::Dark, l("Dark", "Gelap"))
            .changed();
    });

    ui.horizontal(|ui| {
        ui.label(l("Count a file as old after", "Anggap file lama setelah"));
        changed |= ui
            .add(
                egui::DragValue::new(&mut s.stale_days)
                    .range(1..=3650)
                    .suffix(l(" days", " hari")),
            )
            .changed();
    });

    ui.add_space(16.0);
    ui.strong(l(
        "Folders excluded from scans",
        "Folder yang dikecualikan dari pemindaian",
    ));
    let mut remove = None;
    for (i, p) in s.excluded.iter().enumerate() {
        ui.horizontal(|ui| {
            if ui.small_button(l("Remove", "Hapus")).clicked() {
                remove = Some(i);
            }
            ui.label(p.display().to_string());
        });
    }
    if let Some(i) = remove {
        s.excluded.remove(i);
        changed = true;
    }
    ui.horizontal(|ui| {
        ui.add(
            egui::TextEdit::singleline(new_excluded)
                .hint_text(r"C:\Folder")
                .desired_width(320.0),
        );
        if ui.button(l("Add", "Tambah")).clicked() {
            let p = PathBuf::from(new_excluded.trim());
            if !p.as_os_str().is_empty() && !s.excluded.contains(&p) {
                s.excluded.push(p);
                new_excluded.clear();
                changed = true;
            }
        }
    });
    changed
}
