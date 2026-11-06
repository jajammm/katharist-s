//! What the Largest and Not-changed screens share: a flat, cached list of files from
//! the one tree in `App`, with ticks, and the way into the confirmation. The tree and
//! the stale/lock state belong to Home; these screens only read them and ask for a
//! delete through `HomeAction`.

use std::collections::{BTreeSet, HashMap};
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use egui::text::{LayoutJob, TextFormat};
use egui::{RichText, Sense};
use katharistis_core::format::format_size;
use katharistis_core::protect::ProtectContext;
use katharistis_core::scan::{NodeId, ScanTree};

use crate::i18n::l;
use crate::jobs::Job;
use crate::views::home::{
    action_bar, can_select, cell, compute_flags, format_date, location_or_root, name_width,
    row_marks, HomeAction, HomeState, RowFlags, CHECK_WIDTH, DATE_WIDTH, FLAG_ROWS_PER_FRAME,
    MARK_WIDTH, SIZE_WIDTH,
};

/// A list made from the tree. The first build walks and sorts every node, so it runs
/// in a job and the screen says so meanwhile. It is made again only for a new tree or
/// a new choice of the screen (`key`). After a delete the rows that left the tree are
/// taken out of the list instead; nothing is walked again.
#[derive(Default)]
pub struct FileList {
    /// `(epoch, key)` the rows were built for.
    stamp: Option<(u64, u64)>,
    /// The tree generation the rows, flags and ticks are in line with.
    seen_generation: u64,
    build: Option<Job<Vec<NodeId>>>,
    failed: bool,
    pub ids: Vec<NodeId>,
    pub flags: HashMap<NodeId, RowFlags>,
    pub selection: BTreeSet<NodeId>,
}

/// Takes out every id that is no longer reachable from the root.
pub fn prune_gone(tree: &ScanTree, ids: &mut Vec<NodeId>) {
    ids.retain(|&id| location_or_root(tree, id) == id);
}

impl FileList {
    /// Brings the list in line with the tree. `build` runs in a job, and only when
    /// the epoch or `key` is new. Any change of the tree drops the ticks and the
    /// flags, and what left the tree leaves the rows.
    pub fn sync(
        &mut self,
        ctx: &egui::Context,
        tree: &Arc<ScanTree>,
        (epoch, generation): (u64, u64),
        key: u64,
        build: impl FnOnce(&ScanTree) -> Vec<NodeId> + Send + 'static,
    ) {
        if self.stamp != Some((epoch, key)) {
            self.stamp = Some((epoch, key));
            self.seen_generation = generation;
            self.ids = Vec::new();
            self.flags.clear();
            self.selection.clear();
            self.failed = false;
            let shared = Arc::clone(tree);
            self.build = Some(Job::spawn(ctx.clone(), move || build(&shared)));
        }
        if let Some(job) = &mut self.build {
            match job.poll() {
                None => return,
                Some(Ok(ids)) => self.ids = ids,
                Some(Err(_)) => self.failed = true,
            }
            self.build = None;
        }
        if self.seen_generation != generation {
            self.seen_generation = generation;
            prune_gone(tree, &mut self.ids);
            self.flags.clear();
            self.selection.clear();
        }
    }

    pub fn building(&self) -> bool {
        self.build.is_some()
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
    tree: Option<&Arc<ScanTree>>,
    home: &HomeState,
    protect: &ProtectContext,
    list: &mut FileList,
    key: u64,
    empty_text: &str,
    build: impl FnOnce(&ScanTree) -> Vec<NodeId> + Send + 'static,
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
    list.sync(
        ui.ctx(),
        tree,
        (home.epoch(), home.generation()),
        key,
        build,
    );
    list.drop_ticks_if_stale(home.map_stale());
    if list.building() {
        ui.add_space(6.0);
        ui.horizontal(|ui| {
            ui.spinner();
            ui.label(l("Reading the scan...", "Membaca hasil pindai..."));
        });
        return action;
    }
    if list.failed {
        ui.label(l(
            "The list stopped unexpectedly. Scan again.",
            "Daftar berhenti tiba-tiba. Pindai ulang.",
        ));
        return action;
    }
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
    use crate::views::test_support::{file, tree_of};
    use std::time::Instant;

    fn ok_flags() -> RowFlags {
        RowFlags {
            protected: false,
            app_owned: false,
            guard: false,
            locked: false,
        }
    }

    /// Syncs until the build job has landed.
    fn settle(
        list: &mut FileList,
        ctx: &egui::Context,
        tree: &Arc<ScanTree>,
        at: (u64, u64),
        key: u64,
    ) {
        let start = Instant::now();
        loop {
            list.sync(ctx, tree, at, key, |_| panic!("no rebuild expected"));
            if !list.building() {
                return;
            }
            assert!(start.elapsed() < Duration::from_secs(5));
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    fn start(
        list: &mut FileList,
        ctx: &egui::Context,
        tree: &Arc<ScanTree>,
        at: (u64, u64),
        key: u64,
    ) {
        list.sync(ctx, tree, at, key, |t| (1..t.nodes.len()).collect());
        let start = Instant::now();
        while list.building() {
            list.sync(ctx, tree, at, key, |_| panic!("no rebuild expected"));
            assert!(start.elapsed() < Duration::from_secs(5));
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    #[test]
    fn the_first_build_runs_in_a_job_and_the_list_says_so() {
        let ctx = egui::Context::default();
        let tree = Arc::new(tree_of(vec![file("a", 1), file("b", 2)]));
        let (tx, rx) = std::sync::mpsc::channel::<()>();
        let mut list = FileList::default();
        list.sync(&ctx, &tree, (1, 1), 0, move |_| {
            rx.recv().unwrap();
            vec![1, 2]
        });
        assert!(list.building());
        assert!(list.ids.is_empty());
        tx.send(()).unwrap();
        settle(&mut list, &ctx, &tree, (1, 1), 0);
        assert_eq!(list.ids, vec![1, 2]);
    }

    #[test]
    fn a_delete_takes_rows_out_without_building_again() {
        let ctx = egui::Context::default();
        let mut tree = Arc::new(tree_of(vec![file("a", 1), file("b", 2), file("c", 3)]));
        let mut list = FileList::default();
        start(&mut list, &ctx, &tree, (1, 1), 0);
        assert_eq!(list.ids, vec![1, 2, 3]);
        list.selection.insert(2);
        list.flags.insert(2, ok_flags());
        // The job has let go, so the tree can change in place.
        Arc::get_mut(&mut tree).unwrap().remove_subtree(2);
        settle(&mut list, &ctx, &tree, (1, 2), 0);
        assert_eq!(list.ids, vec![1, 3]);
        assert!(list.selection.is_empty());
        assert!(list.flags.is_empty());
    }

    #[test]
    fn a_new_tree_or_a_new_choice_builds_again_and_clears_ticks() {
        let ctx = egui::Context::default();
        let tree = Arc::new(tree_of(vec![file("a", 1), file("b", 2)]));
        let mut list = FileList::default();
        start(&mut list, &ctx, &tree, (1, 1), 0);
        list.selection.insert(1);
        start(&mut list, &ctx, &tree, (1, 1), 1);
        assert!(list.selection.is_empty());
        list.selection.insert(1);
        start(&mut list, &ctx, &tree, (2, 5), 1);
        assert!(list.selection.is_empty());
        assert_eq!(list.ids, vec![1, 2]);
    }

    #[test]
    fn a_stale_mark_clears_ticks_and_keeps_the_rows() {
        let ctx = egui::Context::default();
        let tree = Arc::new(tree_of(vec![file("a", 1)]));
        let mut list = FileList::default();
        start(&mut list, &ctx, &tree, (1, 1), 0);
        list.selection.insert(1);
        settle(&mut list, &ctx, &tree, (1, 2), 0);
        assert!(list.selection.is_empty());
        assert_eq!(list.ids, vec![1]);
        list.selection.insert(1);
        list.drop_ticks_if_stale(false);
        assert_eq!(list.selection.len(), 1);
        list.drop_ticks_if_stale(true);
        assert!(list.selection.is_empty());
    }
}
