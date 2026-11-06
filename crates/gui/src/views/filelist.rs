//! What the Largest and Not-changed screens share: a flat, cached list of files from
//! the one tree in `App`, with ticks, and the way into the confirmation. The tree and
//! the stale/lock state belong to Home; these screens only read them and ask for a
//! delete through `HomeAction`.

use std::collections::{BTreeSet, HashMap};
use std::path::Path;
use std::time::Duration;

use egui::text::{LayoutJob, TextFormat};
use egui::{RichText, Sense};
use katharistis_core::format::format_size;
use katharistis_core::protect::ProtectContext;
use katharistis_core::scan::{NodeId, ScanTree};

use crate::i18n::l;
use crate::views::home::{
    action_bar, can_select, cell, compute_flags, format_date, name_width, row_marks, HomeAction,
    HomeState, RowFlags, CHECK_WIDTH, DATE_WIDTH, FLAG_ROWS_PER_FRAME, MARK_WIDTH, SIZE_WIDTH,
};

/// A list made from one state of the tree. `stamp` is the tree's generation and the
/// screen's own choice (such as an age) when it was built; when either moves, the
/// rows, the flags and the ticks all go.
#[derive(Default)]
pub struct FileList {
    stamp: Option<(u64, u64)>,
    pub ids: Vec<NodeId>,
    pub flags: HashMap<NodeId, RowFlags>,
    pub selection: BTreeSet<NodeId>,
}

impl FileList {
    /// Builds the rows again, and drops every tick, unless the stamp is the same.
    /// Returns whether it built them.
    pub fn sync(&mut self, generation: u64, key: u64, build: impl FnOnce() -> Vec<NodeId>) -> bool {
        if self.stamp == Some((generation, key)) {
            return false;
        }
        self.stamp = Some((generation, key));
        self.ids = build();
        self.flags.clear();
        self.selection.clear();
        true
    }

    /// A stale map allows no tick at all.
    pub fn drop_ticks_if_stale(&mut self, map_stale: bool) {
        if map_stale {
            self.selection.clear();
        }
    }
}

/// Offers a scan of Home's chosen location. Home keeps the scan and its progress, so
/// the other screens show the same running scan instead of starting a second one.
pub fn without_tree(ui: &mut egui::Ui, home: &HomeState) -> Option<HomeAction> {
    if let Some(progress) = home.scan_progress() {
        ui.horizontal(|ui| {
            ui.spinner();
            ui.label(format!(
                "{} {} {}, {}",
                l("Scanning...", "Memindai..."),
                progress.files(),
                l("files", "file"),
                format_size(progress.bytes())
            ));
            if ui.button(l("Cancel", "Batalkan")).clicked() {
                progress.cancel();
            }
        });
        ui.ctx().request_repaint_after(Duration::from_millis(100));
        return None;
    }
    let location = home.selected_location();
    ui.label(l(
        "There is no scan yet. This list is made from a scan of one location.",
        "Belum ada pemindaian. Daftar ini dibuat dari pemindaian satu lokasi.",
    ));
    ui.weak(l(
        "It uses the location picked on Home. Pick another there first if you want a different one.",
        "Lokasinya yang dipilih di Beranda. Pilih lokasi lain di sana dulu kalau ingin yang berbeda.",
    ));
    let mut action = None;
    if ui
        .button(format!("{} {}", l("Scan", "Pindai"), location.display()))
        .clicked()
    {
        action = Some(HomeAction::Scan(location));
    }
    if let Some(err) = home.scan_error() {
        ui.label(format!("{} {err}", l("Could not scan:", "Gagal memindai:")));
    }
    action
}

/// The notice of a map that may no longer match the disk, with the way out.
pub fn stale_notice(ui: &mut egui::Ui, tree: &ScanTree) -> Option<HomeAction> {
    let mut action = None;
    ui.horizontal(|ui| {
        ui.label(l(
            "The map may no longer match the disk. Scan again before deleting more.",
            "Peta mungkin tidak lagi sesuai dengan disk. Pindai ulang sebelum menghapus lagi.",
        ));
        if ui.button(l("Scan again", "Pindai ulang")).clicked() {
            action = Some(HomeAction::Scan(tree.root_path.clone()));
        }
    });
    action
}

/// The parent folder, shown after the name so equal names can be told apart.
fn parent_of(path: &Path) -> String {
    path.parent()
        .map(|p| p.display().to_string())
        .unwrap_or_default()
}

fn name_job(ui: &egui::Ui, name: &str, parent: &str) -> LayoutJob {
    let font = egui::TextStyle::Body.resolve(ui.style());
    let mut job = LayoutJob::default();
    job.append(
        name,
        0.0,
        TextFormat::simple(font.clone(), ui.visuals().text_color()),
    );
    job.append(
        parent,
        8.0,
        TextFormat::simple(font, ui.visuals().weak_text_color()),
    );
    job.wrap.max_rows = 1;
    job.wrap.break_anywhere = true;
    job
}

fn header(ui: &mut egui::Ui, row_height: f32) {
    ui.horizontal(|ui| {
        let width = name_width(ui);
        cell(ui, CHECK_WIDTH, row_height, false, |_| {});
        cell(ui, width, row_height, false, |ui| {
            ui.weak(l("Name", "Nama"));
        });
        cell(ui, MARK_WIDTH, row_height, false, |_| {});
        cell(ui, SIZE_WIDTH, row_height, true, |ui| {
            ui.weak(l("Size", "Ukuran"));
        });
        cell(ui, DATE_WIDTH, row_height, true, |ui| {
            ui.weak(l("Changed", "Diubah"));
        });
    });
}

/// The virtualized rows. Flags come a few rows per frame, and a row has no checkbox
/// until it has them. Ticks only when the map is current.
pub fn draw_rows(
    ui: &mut egui::Ui,
    salt: &str,
    tree: &ScanTree,
    home: &HomeState,
    protect: &ProtectContext,
    list: &mut FileList,
    height: f32,
) {
    let row_height = ui.spacing().interact_size.y;
    header(ui, row_height);
    let marks = home.marks();
    let ticks_allowed = !home.map_stale();
    let mut budget = FLAG_ROWS_PER_FRAME;
    let rows = list.ids.len();
    egui::ScrollArea::vertical()
        .id_salt(salt)
        .max_height(height.max(row_height))
        .auto_shrink([false, false])
        .show_rows(ui, row_height, rows, |ui, range| {
            for i in range {
                let id = list.ids[i];
                let node = &tree.nodes[id];
                if let Some(marks) = marks {
                    if !list.flags.contains_key(&id) && budget > 0 {
                        budget -= 1;
                        let path = tree.path_of(id);
                        list.flags.insert(
                            id,
                            RowFlags {
                                locked: home.locks().is_locked(tree, id),
                                ..compute_flags(&path, protect, marks)
                            },
                        );
                    }
                }
                let flags = list.flags.get(&id).copied();
                if flags.is_none() && marks.is_some() {
                    ui.ctx().request_repaint();
                }
                ui.horizontal(|ui| {
                    let width = name_width(ui);
                    cell(ui, CHECK_WIDTH, row_height, false, |ui| {
                        if can_select(flags.as_ref()) {
                            let mut on = list.selection.contains(&id);
                            let tick = egui::Checkbox::new(&mut on, "");
                            if ui.add_enabled(ticks_allowed, tick).changed() {
                                if on {
                                    list.selection.insert(id);
                                } else {
                                    list.selection.remove(&id);
                                }
                            }
                        }
                    });
                    cell(ui, width, row_height, false, |ui| {
                        let path = tree.path_of(id);
                        let job = name_job(ui, &node.name, &parent_of(&path));
                        ui.add(egui::Label::new(job).truncate().sense(Sense::hover()))
                            .on_hover_text(path.display().to_string());
                    });
                    cell(ui, MARK_WIDTH, row_height, false, |ui| {
                        let text = RichText::new(row_marks(node, flags.as_ref())).weak();
                        ui.add(egui::Label::new(text).truncate());
                    });
                    cell(ui, SIZE_WIDTH, row_height, true, |ui| {
                        ui.label(format_size(node.size));
                    });
                    cell(ui, DATE_WIDTH, row_height, true, |ui| {
                        ui.label(format_date(node.modified));
                    });
                });
            }
        });
}

/// Everything under a screen's own heading and controls: the tree's notices, the
/// cached rows and the delete buttons. `key` is whatever the screen's rows depend on
/// besides the tree; `build` makes them and runs only when the tree or `key` moved.
#[allow(clippy::too_many_arguments)]
pub fn show_list(
    ui: &mut egui::Ui,
    salt: &str,
    tree: Option<&ScanTree>,
    home: &HomeState,
    protect: &ProtectContext,
    list: &mut FileList,
    key: u64,
    empty_text: &str,
    build: impl FnOnce(&ScanTree) -> Vec<NodeId>,
) -> Option<HomeAction> {
    let Some(tree) = tree else {
        return without_tree(ui, home);
    };
    let mut action = None;
    if tree.cancelled {
        ui.label(l(
            "The scan was cancelled, so this list is partial.",
            "Pemindaian dibatalkan, jadi daftar ini sebagian.",
        ));
    }
    if home.map_stale() {
        action = stale_notice(ui, tree);
    }
    if home.marks_failed() {
        ui.label(l(
            "Installed apps could not be read, so \"used by an app\" marks are missing.",
            "Daftar aplikasi tidak terbaca, jadi tanda \"dipakai aplikasi\" tidak ada.",
        ));
    }
    list.sync(home.generation(), key, || build(tree));
    list.drop_ticks_if_stale(home.map_stale());
    if list.ids.is_empty() {
        ui.add_space(6.0);
        ui.weak(empty_text);
        return action;
    }
    ui.add_space(4.0);
    let row = ui.spacing().interact_size.y;
    let height = ui.available_height() - 2.0 * row - 24.0;
    draw_rows(ui, salt, tree, home, protect, list, height);
    ui.add_space(4.0);
    let bar = action_bar(
        ui,
        tree,
        &list.flags,
        &list.selection,
        home.marks().is_some(),
        home.map_stale(),
    );
    action.or(bar)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    #[test]
    fn the_list_is_built_once_per_tree_state_and_key() {
        let mut list = FileList::default();
        let builds = Cell::new(0);
        let build = || {
            builds.set(builds.get() + 1);
            vec![1, 2]
        };
        assert!(list.sync(3, 0, build));
        assert!(!list.sync(3, 0, build));
        assert_eq!(builds.get(), 1);
        assert!(list.sync(3, 1, build));
        assert!(list.sync(4, 1, build));
        assert_eq!(builds.get(), 3);
    }

    #[test]
    fn ticks_and_flags_clear_when_the_tree_changes() {
        let mut list = FileList::default();
        list.sync(1, 0, || vec![1, 2, 3]);
        list.selection.insert(2);
        list.flags.insert(
            2,
            RowFlags {
                protected: false,
                app_owned: false,
                guard: false,
                locked: false,
            },
        );
        assert!(!list.sync(1, 0, Vec::new));
        assert_eq!(list.selection.len(), 1);
        list.sync(2, 0, || vec![1, 3]);
        assert!(list.selection.is_empty());
        assert!(list.flags.is_empty());
        assert_eq!(list.ids, vec![1, 3]);
    }

    #[test]
    fn a_choice_change_clears_ticks_too() {
        let mut list = FileList::default();
        list.sync(1, 0, || vec![1, 2]);
        list.selection.insert(1);
        list.sync(1, 1, || vec![1]);
        assert!(list.selection.is_empty());
    }

    #[test]
    fn a_stale_map_drops_every_tick() {
        let mut list = FileList::default();
        list.sync(1, 0, || vec![1, 2]);
        list.selection.insert(1);
        list.drop_ticks_if_stale(false);
        assert_eq!(list.selection.len(), 1);
        list.drop_ticks_if_stale(true);
        assert!(list.selection.is_empty());
    }
}
