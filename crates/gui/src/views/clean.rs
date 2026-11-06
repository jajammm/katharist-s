//! The cleanup screen: categories of caches and temp files, ticked per item. It only
//! builds a request. Nothing is deleted or emptied here: that happens in the shared
//! confirmation flow, after the user confirms what the request lists.

use std::collections::HashSet;
use std::path::PathBuf;
use std::time::SystemTime;

use katharistis_core::delete::{DeleteMode, Target};
use katharistis_core::format::format_size;
use katharistis_core::junk::{scan_junk, JunkCategory, JunkItem, JunkKind, JunkRoots};
use katharistis_core::scan::ScanProgress;

use crate::confirm::{ConfirmContext, ConfirmRequest};
use crate::i18n::l;
use crate::jobs::{Job, JobPanicked};

/// How a category reads from its items' ticks.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tri {
    None,
    Some,
    All,
}

/// The Recycle Bin has no items of its own. Its tick lives on one marker item of the
/// screen's copy of the category: empty path, the bin's size, and `checked` as the
/// tick. It is never a target; only `selected_targets` reads it, as the second value.
fn is_bin(cat: &JunkCategory) -> bool {
    cat.kind == JunkKind::RecycleBin
}

/// What the screen starts from a scan: the same categories, with the bin's marker
/// item added, unticked.
pub fn with_bin_marker(mut cats: Vec<JunkCategory>) -> Vec<JunkCategory> {
    for cat in cats.iter_mut().filter(|c| is_bin(c)) {
        cat.items = vec![JunkItem {
            path: Default::default(),
            name: String::new(),
            size: cat.bin_size,
            checked: false,
        }];
    }
    cats
}

fn bin_ticked(cat: &JunkCategory) -> bool {
    cat.bin_size > 0 && cat.items.first().is_some_and(|i| i.checked)
}

/// The items to delete and whether to empty the Recycle Bin. Only ticks count.
pub fn selected_targets(cats: &[JunkCategory]) -> (Vec<Target>, bool) {
    let mut targets = Vec::new();
    let mut empty_bin = false;
    for cat in cats {
        if is_bin(cat) {
            empty_bin |= bin_ticked(cat);
            continue;
        }
        targets.extend(cat.items.iter().filter(|i| i.checked).map(|i| Target {
            path: i.path.clone(),
            size: i.size,
        }));
    }
    (targets, empty_bin)
}

pub fn selected_total(cats: &[JunkCategory]) -> u64 {
    let (targets, empty_bin) = selected_targets(cats);
    let bin: u64 = cats
        .iter()
        .filter(|c| is_bin(c) && empty_bin)
        .map(|c| c.bin_size)
        .sum();
    targets.iter().map(|t| t.size).sum::<u64>() + bin
}

pub fn tri_state(cat: &JunkCategory) -> Tri {
    if is_bin(cat) {
        return if bin_ticked(cat) { Tri::All } else { Tri::None };
    }
    let on = cat.items.iter().filter(|i| i.checked).count();
    if on == 0 {
        Tri::None
    } else if on == cat.items.len() {
        Tri::All
    } else {
        Tri::Some
    }
}

pub fn set_category(cat: &mut JunkCategory, on: bool) {
    if is_bin(cat) && cat.bin_size == 0 {
        return;
    }
    for item in &mut cat.items {
        item.checked = on;
    }
}

/// The confirmation for what is ticked, always permanent. `None` when nothing is.
pub fn request_from(cats: &[JunkCategory]) -> Option<ConfirmRequest> {
    let (items, empty_bin) = selected_targets(cats);
    if items.is_empty() && !empty_bin {
        return None;
    }
    let bin_size = cats.iter().find(|c| is_bin(c)).map_or(0, |c| c.bin_size);
    Some(ConfirmRequest {
        items,
        mode: DeleteMode::Permanent,
        warn_app_owned: 0,
        context: ConfirmContext::Cleanup,
        empty_bin: empty_bin.then_some(bin_size),
        guards: Vec::new(),
    })
}

fn kind_name(kind: JunkKind) -> &'static str {
    match kind {
        JunkKind::Temp => l("Temporary files", "File sementara"),
        JunkKind::BrowserCache => l("Browser caches", "Cache browser"),
        JunkKind::CrashLogs => l("Crash reports and logs", "Laporan crash dan log"),
        JunkKind::DevData => l("Developer caches", "Cache pengembangan"),
        JunkKind::GeneralCache => l(".cache folder", "Folder .cache"),
        JunkKind::PartialDownloads => l("Unfinished downloads", "Unduhan belum selesai"),
        JunkKind::RecycleBin => l("Recycle Bin", "Recycle Bin"),
    }
}

struct RunningScan {
    job: Job<Vec<JunkCategory>>,
    progress: ScanProgress,
}

#[derive(Default)]
pub struct CleanState {
    cats: Vec<JunkCategory>,
    expanded: HashSet<usize>,
    scan: Option<RunningScan>,
    /// A scan was started at least once, so opening the screen again does not rescan.
    started: bool,
    cancelled: bool,
    failed: bool,
    /// The excluded folders the list was made with.
    excluded: Vec<PathBuf>,
}

impl CleanState {
    /// Scans in a job. The old list goes first: nothing can be ticked from it while a
    /// newer one is on its way.
    pub fn start_scan(&mut self, ctx: &egui::Context, excluded: &[PathBuf]) {
        if self.scan.is_some() {
            return;
        }
        self.started = true;
        self.cancelled = false;
        self.failed = false;
        self.cats.clear();
        self.expanded.clear();
        self.excluded = excluded.to_vec();
        let excluded = excluded.to_vec();
        let progress = ScanProgress::default();
        let seen = progress.clone();
        let job = Job::spawn(ctx.clone(), move || {
            scan_junk(&JunkRoots::from_env(), SystemTime::now(), &excluded, &seen)
        });
        self.scan = Some(RunningScan { job, progress });
    }

    /// Takes in a finished scan. Runs every frame, so a scan finishes also while
    /// another tool is open.
    pub fn poll(&mut self) {
        let Some(outcome) = self.scan.as_mut().and_then(|s| s.job.poll()) else {
            return;
        };
        self.scan = None;
        match outcome {
            Ok(cats) => self.cats = with_bin_marker(cats),
            Err(JobPanicked) => self.failed = true,
        }
    }
}

fn category_row(ui: &mut egui::Ui, cat: &mut JunkCategory, idx: usize, open: &mut bool) {
    let bin = is_bin(cat);
    let total: u64 = if bin {
        cat.bin_size
    } else {
        cat.items.iter().map(|i| i.size).sum()
    };
    let empty = total == 0 && (bin || cat.items.is_empty());
    ui.horizontal(|ui| {
        if !bin && !cat.items.is_empty() {
            let arrow = if *open { "v" } else { ">" };
            if ui.small_button(arrow).clicked() {
                *open = !*open;
            }
        } else {
            ui.add_space(ui.spacing().interact_size.x * 0.5);
        }
        let tri = tri_state(cat);
        let mut on = tri == Tri::All;
        let name = kind_name(cat.kind);
        let label = if empty {
            egui::RichText::new(name).weak()
        } else {
            egui::RichText::new(name)
        };
        let changed = ui
            .add_enabled(
                !empty,
                egui::Checkbox::new(&mut on, label).indeterminate(tri == Tri::Some),
            )
            .changed();
        if changed {
            set_category(cat, on);
        }
        if !bin {
            ui.weak(format!("{} {}", cat.items.len(), l("items", "item")));
        }
        ui.label(format_size(total));
        if cat.unreadable {
            ui.weak(l(
                "Some locations could not be read.",
                "Sebagian lokasi tidak terbaca.",
            ));
        }
    });
    if bin {
        ui.indent(("clean-bin-note", idx), |ui| {
            ui.weak(l(
                "Empties everything in it, on every drive, through Windows.",
                "Mengosongkan seluruh isinya, di semua drive, lewat Windows.",
            ));
        });
    }
}

fn item_rows(ui: &mut egui::Ui, cat: &mut JunkCategory, idx: usize) {
    let height = ui.spacing().interact_size.y;
    let rows = cat.items.len();
    ui.indent(("clean-items-indent", idx), |ui| {
        egui::ScrollArea::vertical()
            .id_salt(("clean-items", idx))
            .max_height(220.0)
            .auto_shrink([false, true])
            .show_rows(ui, height, rows, |ui, range| {
                for i in range {
                    let item = &mut cat.items[i];
                    ui.horizontal(|ui| {
                        ui.checkbox(&mut item.checked, "");
                        ui.add(egui::Label::new(item.name.as_str()).truncate());
                        ui.weak(format_size(item.size));
                    });
                }
            });
    });
}

/// Draws the screen. Returns the request when the user asks to delete what is
/// ticked; the caller hands it to the confirmation flow. `busy` is a flow in progress.
/// A list made before the excluded folders changed may hold one of them ticked, so it
/// is made again.
pub fn show(
    ui: &mut egui::Ui,
    state: &mut CleanState,
    busy: bool,
    excluded: &[PathBuf],
) -> Option<ConfirmRequest> {
    if !state.started || (!busy && state.scan.is_none() && state.excluded != excluded) {
        state.start_scan(ui.ctx(), excluded);
    }
    let scanning = state.scan.is_some();
    let mut request = None;
    ui.horizontal(|ui| {
        ui.heading(l("Cleanup", "Pembersihan"));
        ui.add_space(12.0);
        ui.label(format!(
            "{} {}",
            l("Selected:", "Terpilih:"),
            format_size(selected_total(&state.cats))
        ));
    });
    ui.add_space(4.0);
    ui.weak(l(
        "Everything here is deleted permanently, not moved to the Recycle Bin. Nothing is deleted until you confirm.",
        "Semua di sini dihapus permanen, tidak dipindahkan ke Recycle Bin. Tidak ada yang dihapus sebelum Anda konfirmasi.",
    ));
    ui.add_space(8.0);
    ui.horizontal(|ui| {
        if scanning {
            ui.spinner();
            ui.label(l("Scanning...", "Memindai..."));
            if ui.button(l("Cancel", "Batal")).clicked() {
                if let Some(scan) = &state.scan {
                    scan.progress.cancel();
                }
                state.cancelled = true;
            }
        } else {
            if ui
                .add_enabled(!busy, egui::Button::new(l("Scan again", "Pindai ulang")))
                .clicked()
            {
                state.start_scan(ui.ctx(), excluded);
            }
            let pending = request_from(&state.cats);
            let go = ui.add_enabled(
                !busy && pending.is_some(),
                egui::Button::new(l("Delete selected...", "Hapus terpilih...")),
            );
            if go.clicked() {
                request = pending;
            }
        }
    });
    if state.cancelled {
        ui.weak(l(
            "Scan cancelled. The list may be incomplete.",
            "Pemindaian dibatalkan. Daftar mungkin belum lengkap.",
        ));
    }
    if state.failed {
        ui.label(l(
            "The scan stopped unexpectedly. Scan again.",
            "Pemindaian berhenti tiba-tiba. Pindai ulang.",
        ));
    }
    ui.add_space(8.0);
    egui::ScrollArea::vertical()
        .id_salt("clean-list")
        .auto_shrink([false, false])
        .show(ui, |ui| {
            ui.add_enabled_ui(!busy && !scanning, |ui| {
                for idx in 0..state.cats.len() {
                    let mut open = state.expanded.contains(&idx);
                    category_row(ui, &mut state.cats[idx], idx, &mut open);
                    if open {
                        item_rows(ui, &mut state.cats[idx], idx);
                        state.expanded.insert(idx);
                    } else {
                        state.expanded.remove(&idx);
                    }
                    ui.add_space(4.0);
                }
            });
        });
    request
}

#[cfg(test)]
mod tests {
    use super::*;
    use katharistis_core::junk::{JunkItem, JunkKind};

    fn item(name: &str, size: u64, checked: bool) -> JunkItem {
        JunkItem {
            path: PathBuf::from(format!(r"C:\junk\{name}")),
            name: name.to_string(),
            size,
            checked,
        }
    }

    fn cat(kind: JunkKind, items: Vec<JunkItem>) -> JunkCategory {
        JunkCategory {
            kind,
            items,
            unreadable: false,
            bin_size: 0,
        }
    }

    fn bin(size: u64, checked: bool) -> JunkCategory {
        JunkCategory {
            kind: JunkKind::RecycleBin,
            items: vec![JunkItem {
                path: PathBuf::new(),
                name: String::new(),
                size,
                checked,
            }],
            unreadable: false,
            bin_size: size,
        }
    }

    #[test]
    fn selection_uses_item_flags_and_bin_only_when_checked() {
        let cats = vec![
            cat(
                JunkKind::Temp,
                vec![
                    item("a", 10, true),
                    item("b", 20, false),
                    item("c", 5, true),
                ],
            ),
            cat(JunkKind::GeneralCache, vec![item("d", 7, false)]),
            bin(900, false),
        ];
        let (targets, empty_bin) = selected_targets(&cats);
        let paths: Vec<_> = targets.iter().map(|t| t.path.clone()).collect();
        assert_eq!(
            paths,
            [PathBuf::from(r"C:\junk\a"), PathBuf::from(r"C:\junk\c")]
        );
        assert_eq!(targets.iter().map(|t| t.size).sum::<u64>(), 15);
        assert!(!empty_bin);

        let mut cats = cats;
        cats[2].items[0].checked = true;
        let (targets, empty_bin) = selected_targets(&cats);
        assert!(empty_bin);
        // The bin's tick never turns into a path to delete.
        assert_eq!(targets.len(), 2);
        assert!(targets.iter().all(|t| !t.path.as_os_str().is_empty()));
    }

    #[test]
    fn unchecked_category_contributes_nothing() {
        let cats = vec![
            cat(
                JunkKind::Temp,
                vec![item("a", 10, false), item("b", 3, false)],
            ),
            cat(JunkKind::BrowserCache, Vec::new()),
            bin(0, false),
        ];
        let (targets, empty_bin) = selected_targets(&cats);
        assert!(targets.is_empty());
        assert!(!empty_bin);
    }

    #[test]
    fn category_tick_is_tri_state_over_its_items() {
        let mut c = cat(
            JunkKind::Temp,
            vec![item("a", 1, true), item("b", 1, false)],
        );
        assert_eq!(tri_state(&c), Tri::Some);
        set_category(&mut c, true);
        assert_eq!(tri_state(&c), Tri::All);
        set_category(&mut c, false);
        assert_eq!(tri_state(&c), Tri::None);
    }

    #[test]
    fn scan_results_start_with_the_bin_unticked_and_empty_bin_cannot_be_ticked() {
        let scanned = vec![JunkCategory {
            kind: JunkKind::RecycleBin,
            items: Vec::new(),
            unreadable: false,
            bin_size: 0,
        }];
        let mut cats = with_bin_marker(scanned);
        let (targets, empty_bin) = selected_targets(&cats);
        assert!(targets.is_empty() && !empty_bin);
        set_category(&mut cats[0], true);
        assert!(!selected_targets(&cats).1);
        assert!(request_from(&cats).is_none());
    }

    #[test]
    fn request_is_permanent_and_names_the_bin_only_when_ticked() {
        let mut cats = vec![
            cat(JunkKind::Temp, vec![item("a", 10, true)]),
            bin(900, false),
        ];
        let r = request_from(&cats).unwrap();
        assert_eq!(r.mode, DeleteMode::Permanent);
        assert_eq!(r.context, ConfirmContext::Cleanup);
        assert_eq!(r.items.len(), 1);
        assert_eq!(r.empty_bin, None);
        assert_eq!(selected_total(&cats), 10);

        cats[1].items[0].checked = true;
        assert_eq!(request_from(&cats).unwrap().empty_bin, Some(900));
        assert_eq!(selected_total(&cats), 910);

        // Only the bin: nothing to delete, but a request that empties it.
        cats[0].items[0].checked = false;
        let r = request_from(&cats).unwrap();
        assert!(r.items.is_empty());
        assert_eq!(r.empty_bin, Some(900));
    }

    #[test]
    fn the_screen_draws_without_starting_anything() {
        let mut state = CleanState {
            cats: vec![
                cat(JunkKind::Temp, vec![item("a", 10, true)]),
                cat(JunkKind::CrashLogs, Vec::new()),
                bin(900, false),
            ],
            started: true,
            ..Default::default()
        };
        state.expanded.insert(0);
        let ctx = egui::Context::default();
        for _ in 0..2 {
            let mut out = ctx.run_ui(egui::RawInput::default(), |ui| {
                assert!(show(ui, &mut state, false, &[]).is_none());
            });
            out.textures_delta.clear();
        }
        assert!(state.cats[0].items[0].checked);
    }
}
