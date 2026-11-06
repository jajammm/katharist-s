//! Duplicate files. One copy of every group, `keep`, always stays: it has no checkbox,
//! `DupeSelection` refuses it, and the files are compared again, off the UI thread,
//! right before the confirmation opens. A group that fails that check is taken out
//! of the list and the user is told; the others go on.

use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use egui::RichText;
use katharistis_core::delete::{DeleteMode, DeleteReport, Target};
use katharistis_core::dupes::{find_duplicates, targets_still_safe, DuplicateGroup};
use katharistis_core::format::format_size;
use katharistis_core::protect::ProtectContext;
use katharistis_core::scan::{NodeId, ScanProgress, ScanTree};

use crate::confirm::{ConfirmContext, ConfirmRequest};
use crate::i18n::l;
use crate::jobs::{Job, JobPanicked};
use crate::theme;
use crate::views::filelist::{stale_notice, without_tree};
use crate::views::home::{
    can_select, cell, compute_flags, find_node, row_marks, HomeAction, HomeState, RowFlags,
    CHECK_WIDTH, FLAG_ROWS_PER_FRAME, MARK_WIDTH,
};

/// Files smaller than this are not looked at.
pub const MIN_BYTES: u64 = 1024 * 1024;

fn lower(p: &Path) -> String {
    p.to_string_lossy().to_lowercase()
}

/// `path` is `base` or lies under it, ignoring case as Windows does.
fn is_at_or_under(path: &Path, base: &Path) -> bool {
    PathBuf::from(lower(path)).starts_with(PathBuf::from(lower(base)))
}

/// The duplicate groups and what is ticked in them. Only files in `others` can be
/// ticked, so no route leads to `keep`.
#[derive(Debug, Default)]
pub struct DupeSelection {
    groups: Vec<DuplicateGroup>,
    picked: Vec<BTreeSet<PathBuf>>,
    rev: u64,
}

impl DupeSelection {
    pub fn new(groups: Vec<DuplicateGroup>) -> Self {
        DupeSelection {
            picked: vec![BTreeSet::new(); groups.len()],
            groups,
            rev: 0,
        }
    }

    pub fn groups(&self) -> &[DuplicateGroup] {
        &self.groups
    }

    /// Changes whenever the groups do, so a list made from them knows to be made again.
    pub fn rev(&self) -> u64 {
        self.rev
    }

    /// Ticks or unticks `path` in `group`. Refuses `keep`, anything not in the
    /// group's `others`, and a group that is not there.
    pub fn toggle(&mut self, group: usize, path: &Path) {
        let Some(g) = self.groups.get(group) else {
            return;
        };
        if lower(path) == lower(&g.keep) {
            return;
        }
        let Some(other) = g.others.iter().find(|o| lower(o) == lower(path)) else {
            return;
        };
        let set = &mut self.picked[group];
        if !set.remove(other) {
            set.insert(other.clone());
        }
    }

    pub fn is_picked(&self, group: usize, path: &Path) -> bool {
        self.picked.get(group).is_some_and(|s| s.contains(path))
    }

    pub fn picked_in(&self, group: usize) -> Vec<PathBuf> {
        self.picked
            .get(group)
            .map(|s| s.iter().cloned().collect())
            .unwrap_or_default()
    }

    pub fn picked_count(&self) -> usize {
        self.picked.iter().map(BTreeSet::len).sum()
    }

    pub fn picked_bytes(&self) -> u64 {
        self.groups
            .iter()
            .zip(&self.picked)
            .map(|(g, p)| g.size * p.len() as u64)
            .sum()
    }

    pub fn clear_picks(&mut self) {
        for set in &mut self.picked {
            set.clear();
        }
    }

    /// Takes a group out with its ticks. Later groups keep their own ticks.
    pub fn remove_group(&mut self, group: usize) {
        if group < self.groups.len() {
            self.groups.remove(group);
            self.picked.remove(group);
            self.rev += 1;
        }
    }

    /// Takes out what a delete removed, a folder taking everything inside it, without
    /// reading any file. A group whose `keep` went is dropped whole: its other files
    /// are no longer a copy of something that stays. A group left with nothing in
    /// `others` is dropped too.
    pub fn remove_paths(&mut self, gone: &[PathBuf]) {
        if gone.is_empty() {
            return;
        }
        let hit = |p: &Path| gone.iter().any(|g| is_at_or_under(p, g));
        let mut i = 0;
        while i < self.groups.len() {
            if hit(&self.groups[i].keep) {
                self.remove_group(i);
                continue;
            }
            let before = self.groups[i].others.len();
            self.groups[i].others.retain(|o| !hit(o));
            self.picked[i].retain(|p| !hit(p));
            if self.groups[i].others.len() != before {
                self.rev += 1;
            }
            if self.groups[i].others.is_empty() {
                self.remove_group(i);
                continue;
            }
            i += 1;
        }
    }
}

/// One line of the list.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DupeRow {
    Header(usize),
    Keep(usize),
    Other(usize, usize),
}

pub fn rows_for(sel: &DupeSelection) -> Vec<DupeRow> {
    let mut rows = Vec::new();
    for (g, group) in sel.groups().iter().enumerate() {
        rows.push(DupeRow::Header(g));
        rows.push(DupeRow::Keep(g));
        rows.extend((0..group.others.len()).map(|k| DupeRow::Other(g, k)));
    }
    rows
}

/// What the check before the confirmation found. `ok` pairs each group with the files
/// ticked in it; `failed` are the groups that were not safe.
#[derive(Debug, Default)]
pub struct Checked {
    pub ok: Vec<(DuplicateGroup, Vec<PathBuf>)>,
    pub failed: Vec<DuplicateGroup>,
}

pub fn check_groups(
    snapshot: Vec<(DuplicateGroup, Vec<PathBuf>)>,
    safe: impl Fn(&DuplicateGroup, &[PathBuf]) -> bool,
) -> Checked {
    let mut out = Checked::default();
    for (group, picked) in snapshot {
        if safe(&group, &picked) {
            out.ok.push((group, picked));
        } else {
            out.failed.push(group);
        }
    }
    out
}

/// The groups with something ticked, with copies of what is ticked.
pub fn snapshot(sel: &DupeSelection) -> Vec<(DuplicateGroup, Vec<PathBuf>)> {
    (0..sel.groups().len())
        .filter_map(|g| {
            let picked = sel.picked_in(g);
            (!picked.is_empty()).then(|| (sel.groups()[g].clone(), picked))
        })
        .collect()
}

/// Takes the failed groups out of the selection, and builds the confirmation from
/// the rest. Whatever cannot have a checkbox stays out, whatever was ticked.
pub fn apply_check(
    sel: &mut DupeSelection,
    checked: &Checked,
    flags: &HashMap<PathBuf, RowFlags>,
    mode: DeleteMode,
) -> Option<ConfirmRequest> {
    for failed in &checked.failed {
        if let Some(i) = sel.groups().iter().position(|g| g.keep == failed.keep) {
            sel.remove_group(i);
        }
    }
    let mut items = Vec::new();
    let mut warn_app_owned = 0;
    for (group, picked) in &checked.ok {
        for path in picked {
            let f = flags.get(path);
            if !can_select(f) || lower(path) == lower(&group.keep) {
                continue;
            }
            warn_app_owned += usize::from(f.is_some_and(|f| f.app_owned));
            items.push(Target {
                path: path.clone(),
                size: group.size,
            });
        }
    }
    (!items.is_empty()).then_some(ConfirmRequest {
        items,
        mode,
        warn_app_owned,
        context: ConfirmContext::Duplicates,
        empty_bin: None,
        guards: checked.ok.clone(),
    })
}

pub fn removed_notice(n: usize) -> String {
    format!(
        "{n} {}",
        l(
            "group(s) changed on disk since the search and were taken off the list. Nothing in them was deleted.",
            "kelompok berubah di disk sejak pencarian dan dikeluarkan dari daftar. Tidak ada isinya yang dihapus.",
        )
    )
}

enum Phase {
    Finding {
        job: Job<Vec<DuplicateGroup>>,
        progress: ScanProgress,
        generation: u64,
    },
    Checking {
        job: Job<Checked>,
        mode: DeleteMode,
        generation: u64,
    },
}

type RowInfo = (RowFlags, Option<NodeId>);

#[derive(Default)]
pub struct DupesState {
    sel: Option<DupeSelection>,
    rows: Vec<DupeRow>,
    rows_rev: Option<u64>,
    flags: HashMap<PathBuf, RowInfo>,
    phase: Option<Phase>,
    notice: Option<String>,
    /// The search was cancelled, so the empty list means nothing.
    cancelled: bool,
    epoch: u64,
    generation: u64,
}

impl DupesState {
    /// The user left the screen. A finished check is dropped unseen, so no
    /// confirmation can appear later on another screen; what is ticked stays.
    pub fn leave(&mut self) {
        if matches!(self.phase, Some(Phase::Checking { .. })) {
            self.phase = None;
        }
    }

    /// Stops a search that holds the tree, so a delete can change the tree.
    pub fn cancel_search(&mut self) {
        if let Some(Phase::Finding { progress, .. }) = &self.phase {
            progress.cancel();
        }
    }

    fn idle(&self) -> bool {
        self.phase.is_none()
    }

    /// A delete finished, from any screen: its paths leave the results. Nothing is
    /// read again.
    pub fn after_report(&mut self, report: &DeleteReport) {
        if let Some(sel) = &mut self.sel {
            let gone: Vec<PathBuf> = report
                .removed
                .iter()
                .chain(&report.recycled)
                .cloned()
                .collect();
            sel.remove_paths(&gone);
        }
    }

    /// Brings the state in line with the tree. A new tree makes the results
    /// meaningless; any other change drops the ticks and what was worked out for rows.
    fn sync(&mut self, home: &HomeState) {
        if self.epoch != home.epoch() {
            if let Some(Phase::Finding { progress, .. }) = &self.phase {
                progress.cancel();
            }
            *self = DupesState {
                epoch: home.epoch(),
                generation: home.generation(),
                ..DupesState::default()
            };
        } else if self.generation != home.generation() {
            self.generation = home.generation();
            self.flags.clear();
            if let Some(sel) = &mut self.sel {
                sel.clear_picks();
            }
        }
        if home.map_stale() {
            if let Some(sel) = &mut self.sel {
                sel.clear_picks();
            }
        }
    }

    /// Takes in a finished job. `generation` is the tree's now. May return the
    /// confirmation to open.
    fn poll(&mut self, generation: u64) -> Option<ConfirmRequest> {
        match self.phase.take() {
            Some(Phase::Finding {
                mut job,
                progress,
                generation: started,
            }) => {
                let Some(outcome) = job.poll() else {
                    self.phase = Some(Phase::Finding {
                        job,
                        progress,
                        generation: started,
                    });
                    return None;
                };
                self.cancelled = progress.is_cancelled();
                self.notice = None;
                self.sel = None;
                self.rows_rev = None;
                if self.cancelled {
                    return None;
                }
                match outcome {
                    Ok(_) if started != generation => {
                        self.notice = Some(
                            l(
                                "The disk changed during the search. Search again.",
                                "Disk berubah selama pencarian. Cari lagi.",
                            )
                            .into(),
                        );
                    }
                    Ok(groups) => self.sel = Some(DupeSelection::new(groups)),
                    Err(JobPanicked) => {
                        self.notice = Some(
                            l(
                                "The search stopped unexpectedly. Try again.",
                                "Pencarian berhenti tiba-tiba. Coba lagi.",
                            )
                            .into(),
                        );
                    }
                }
                None
            }
            Some(Phase::Checking {
                mut job,
                mode,
                generation: started,
            }) => {
                let Some(outcome) = job.poll() else {
                    self.phase = Some(Phase::Checking {
                        job,
                        mode,
                        generation: started,
                    });
                    return None;
                };
                match outcome {
                    Ok(_) if started != generation => {
                        self.notice = Some(
                            l(
                                "The list changed while it was being checked. Nothing was deleted. Pick again.",
                                "Daftar berubah saat diperiksa. Tidak ada yang dihapus. Pilih lagi.",
                            )
                            .into(),
                        );
                        None
                    }
                    Ok(checked) => {
                        let flags: HashMap<PathBuf, RowFlags> = self
                            .flags
                            .iter()
                            .map(|(p, (f, _))| (p.clone(), *f))
                            .collect();
                        self.notice = (!checked.failed.is_empty())
                            .then(|| removed_notice(checked.failed.len()));
                        self.rows_rev = None;
                        let sel = self.sel.as_mut()?;
                        apply_check(sel, &checked, &flags, mode)
                    }
                    Err(JobPanicked) => {
                        self.notice = Some(
                            l(
                                "The check stopped unexpectedly. Nothing was deleted.",
                                "Pemeriksaan berhenti tiba-tiba. Tidak ada yang dihapus.",
                            )
                            .into(),
                        );
                        None
                    }
                }
            }
            other => {
                self.phase = other;
                None
            }
        }
    }
}

fn marks_text(tree: &ScanTree, info: &RowInfo) -> String {
    match info.1 {
        Some(id) => row_marks(&tree.nodes[id], Some(&info.0)),
        None => row_marks(&placeholder_node(), Some(&info.0)),
    }
}

/// A row whose node is no longer in the tree still shows its flag marks.
fn placeholder_node() -> katharistis_core::scan::Node {
    katharistis_core::scan::Node {
        name: String::new(),
        size: 0,
        modified: None,
        class: katharistis_core::scan::EntryClass::File,
        cloud: false,
        parent: None,
        children: Vec::new(),
    }
}

/// Draws the screen. Returns the confirmation to open or a scan to start.
pub fn show(
    ui: &mut egui::Ui,
    st: &mut DupesState,
    tree: Option<&Arc<ScanTree>>,
    home: &HomeState,
    protect: &ProtectContext,
    flow_busy: bool,
) -> Option<HomeAction> {
    ui.heading(l("Duplicates", "Duplikat"));
    ui.weak(l(
        "Files with the same content, 1 MB and up. One copy in each group always stays.",
        "File dengan isi yang sama, 1 MB ke atas. Satu salinan di tiap kelompok selalu tersisa.",
    ));
    ui.add_space(8.0);
    let Some(tree) = tree else {
        return without_tree(ui, home);
    };
    st.sync(home);
    let mut action = st.poll(home.generation()).map(HomeAction::Delete);
    if home.map_stale() {
        action = action.or(stale_notice(ui, tree));
    }
    if tree.cancelled {
        ui.label(l(
            "The scan was cancelled, so this search covers only part of the location.",
            "Pemindaian dibatalkan, jadi pencarian ini hanya mencakup sebagian lokasi.",
        ));
    }
    controls(ui, st, tree, home);
    if let Some(text) = &st.notice {
        ui.label(text.as_str());
    }
    match st.phase.as_ref() {
        Some(Phase::Finding { progress, .. }) => {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label(if progress.is_cancelled() {
                    l("Cancelling...", "Membatalkan...")
                } else {
                    l("Comparing files...", "Membandingkan file...")
                });
                if ui.button(l("Cancel", "Batalkan")).clicked() {
                    progress.cancel();
                }
            });
            ui.ctx().request_repaint_after(Duration::from_millis(100));
            return action;
        }
        Some(Phase::Checking { .. }) => {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label(l(
                    "Checking the copies once more before deleting...",
                    "Memeriksa salinan sekali lagi sebelum menghapus...",
                ));
            });
            ui.ctx().request_repaint_after(Duration::from_millis(100));
        }
        None => {}
    }
    match &st.sel {
        None => {
            if st.cancelled {
                ui.weak(l("The search was cancelled.", "Pencarian dibatalkan."));
            }
            action
        }
        Some(sel) if sel.groups().is_empty() => {
            ui.add_space(6.0);
            ui.weak(l(
                "No duplicates in the list.",
                "Tidak ada duplikat di daftar.",
            ));
            action
        }
        Some(_) => action.or(list(ui, st, tree, home, protect, flow_busy)),
    }
}

fn controls(ui: &mut egui::Ui, st: &mut DupesState, tree: &Arc<ScanTree>, home: &HomeState) {
    let idle = st.idle();
    ui.horizontal(|ui| {
        let label = if st.sel.is_some() {
            l("Search again", "Cari lagi")
        } else {
            l("Find duplicates", "Cari duplikat")
        };
        if ui.add_enabled(idle, egui::Button::new(label)).clicked() {
            let shared = Arc::clone(tree);
            let progress = ScanProgress::default();
            let seen = progress.clone();
            let job = Job::spawn(ui.ctx().clone(), move || {
                find_duplicates(&shared, MIN_BYTES, &seen)
            });
            st.sel = None;
            st.flags.clear();
            st.rows_rev = None;
            st.notice = None;
            st.cancelled = false;
            st.phase = Some(Phase::Finding {
                job,
                progress,
                generation: home.generation(),
            });
        }
    });
    ui.add_space(4.0);
}

fn list(
    ui: &mut egui::Ui,
    st: &mut DupesState,
    tree: &ScanTree,
    home: &HomeState,
    protect: &ProtectContext,
    flow_busy: bool,
) -> Option<HomeAction> {
    let ticks_allowed = !home.map_stale() && st.idle();
    let sel = st.sel.as_mut()?;
    if st.rows_rev != Some(sel.rev()) {
        st.rows = rows_for(sel);
        st.rows_rev = Some(sel.rev());
    }
    let marks = home.marks();
    let row_height = ui.spacing().interact_size.y;
    let weak = ui.visuals().weak_text_color();
    let height = (ui.available_height() - 2.0 * row_height - 24.0).max(row_height);
    let mut budget = FLAG_ROWS_PER_FRAME;
    let rows = st.rows.len();
    let flags = &mut st.flags;
    let list_rows = &st.rows;
    egui::ScrollArea::vertical()
        .id_salt("dupes-list")
        .max_height(height)
        .auto_shrink([false, false])
        .show_rows(ui, row_height, rows, |ui, range| {
            for i in range {
                let row = list_rows[i];
                let (g, path) = match row {
                    DupeRow::Header(g) => {
                        let group = &sel.groups()[g];
                        let copies = group.others.len() + 1;
                        ui.label(
                            RichText::new(format!(
                                "{copies} {} {} {}",
                                l("copies of", "salinan dari"),
                                format_size(group.size),
                                l("each", "masing-masing"),
                            ))
                            .strong(),
                        );
                        continue;
                    }
                    DupeRow::Keep(g) => (g, sel.groups()[g].keep.clone()),
                    DupeRow::Other(g, k) => (g, sel.groups()[g].others[k].clone()),
                };
                if let Some(marks) = marks {
                    if !flags.contains_key(&path) && budget > 0 {
                        budget -= 1;
                        let node = find_node(tree, &path);
                        let mut f = compute_flags(&path, protect, marks);
                        f.locked = node.is_some_and(|id| home.locks().is_locked(tree, id));
                        flags.insert(path.clone(), (f, node));
                    }
                }
                let info = flags.get(&path).copied();
                if info.is_none() && marks.is_some() {
                    ui.ctx().request_repaint();
                }
                ui.horizontal(|ui| {
                    cell(ui, CHECK_WIDTH, row_height, false, |ui| {
                        let selectable = can_select(info.as_ref().map(|i| &i.0));
                        if matches!(row, DupeRow::Other(..)) && selectable {
                            let mut on = sel.is_picked(g, &path);
                            let tick = egui::Checkbox::new(&mut on, "");
                            if ui.add_enabled(ticks_allowed, tick).changed() {
                                sel.toggle(g, &path);
                            }
                        }
                    });
                    cell(ui, MARK_WIDTH, row_height, false, |ui| {
                        let mut text = String::new();
                        if matches!(row, DupeRow::Keep(_)) {
                            text.push_str(l("stays", "tetap"));
                        }
                        if let Some(info) = &info {
                            let more = marks_text(tree, info);
                            if !more.is_empty() {
                                if !text.is_empty() {
                                    text.push_str(", ");
                                }
                                text.push_str(&more);
                            }
                        }
                        ui.add(egui::Label::new(RichText::new(text).color(weak)).truncate());
                    });
                    let shown = path.display().to_string();
                    ui.add(egui::Label::new(shown.as_str()).truncate())
                        .on_hover_text(shown.as_str());
                });
            }
        });
    ui.add_space(4.0);
    bar(ui, st, home, flow_busy)
}

fn bar(
    ui: &mut egui::Ui,
    st: &mut DupesState,
    home: &HomeState,
    flow_busy: bool,
) -> Option<HomeAction> {
    let idle = st.idle();
    let sel = st.sel.as_ref()?;
    let can =
        sel.picked_count() > 0 && home.marks().is_some() && !home.map_stale() && !flow_busy && idle;
    let pal = theme::palette(ui.visuals().dark_mode);
    let mut mode = None;
    ui.horizontal(|ui| {
        let primary = egui::Button::new(
            RichText::new(l("Move to Recycle Bin", "Pindahkan ke Recycle Bin"))
                .color(pal.on_accent),
        )
        .fill(pal.accent);
        if ui.add_enabled(can, primary).clicked() {
            mode = Some(DeleteMode::RecycleBin);
        }
        if ui
            .add_enabled(
                can,
                egui::Button::new(l("Delete permanently...", "Hapus permanen...")),
            )
            .clicked()
        {
            mode = Some(DeleteMode::Permanent);
        }
        ui.label(format!(
            "{} {}, {}",
            sel.picked_count(),
            l("selected", "dipilih"),
            format_size(sel.picked_bytes())
        ));
    });
    let mode = mode?;
    let snap = snapshot(sel);
    let job = Job::spawn(ui.ctx().clone(), move || {
        check_groups(snap, targets_still_safe)
    });
    st.phase = Some(Phase::Checking {
        job,
        mode,
        generation: home.generation(),
    });
    st.notice = None;
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use katharistis_core::scan::{scan, ScanOptions};
    use std::fs;

    fn group(keep: &str, others: &[&str]) -> DuplicateGroup {
        DuplicateGroup {
            size: 10,
            keep: PathBuf::from(keep),
            others: others.iter().map(PathBuf::from).collect(),
        }
    }

    fn two_groups() -> DupeSelection {
        DupeSelection::new(vec![
            group(r"C:\a\k1", &[r"C:\a\x1", r"C:\a\x2"]),
            group(r"C:\b\k2", &[r"C:\b\y1"]),
        ])
    }

    #[test]
    fn keep_cannot_be_selected() {
        let mut sel = two_groups();
        sel.toggle(0, Path::new(r"C:\a\k1"));
        sel.toggle(0, Path::new(r"C:\A\K1"));
        assert_eq!(sel.picked_count(), 0);
        sel.toggle(0, Path::new(r"C:\b\y1"));
        sel.toggle(0, Path::new(r"C:\nowhere"));
        sel.toggle(9, Path::new(r"C:\a\x1"));
        assert_eq!(sel.picked_count(), 0);
        sel.toggle(0, Path::new(r"C:\a\x1"));
        assert!(sel.is_picked(0, Path::new(r"C:\a\x1")));
        assert_eq!(sel.picked_count(), 1);
        sel.toggle(0, Path::new(r"C:\a\x1"));
        assert_eq!(sel.picked_count(), 0);
        assert!(!sel.is_picked(0, Path::new(r"C:\a\k1")));
    }

    #[test]
    fn selection_clears_when_group_is_removed() {
        let mut sel = two_groups();
        sel.toggle(0, Path::new(r"C:\a\x1"));
        sel.toggle(1, Path::new(r"C:\b\y1"));
        sel.remove_group(0);
        assert_eq!(sel.groups().len(), 1);
        assert_eq!(sel.picked_count(), 1);
        assert!(sel.is_picked(0, Path::new(r"C:\b\y1")));
        assert!(!sel.is_picked(0, Path::new(r"C:\a\x1")));
        sel.remove_group(0);
        assert_eq!(sel.picked_count(), 0);
        assert!(sel.groups().is_empty());
    }

    #[test]
    fn deleted_paths_leave_and_empty_groups_go_without_reading_anything() {
        let mut sel = two_groups();
        sel.toggle(0, Path::new(r"C:\a\x1"));
        sel.toggle(0, Path::new(r"C:\a\x2"));
        let rev = sel.rev();
        sel.remove_paths(&[PathBuf::from(r"c:\a\X1")]);
        assert!(sel.rev() > rev);
        assert_eq!(sel.groups()[0].others, vec![PathBuf::from(r"C:\a\x2")]);
        assert_eq!(sel.picked_count(), 1);
        sel.remove_paths(&[PathBuf::from(r"C:\a\x2")]);
        assert_eq!(sel.groups().len(), 1);
        assert_eq!(sel.groups()[0].keep, PathBuf::from(r"C:\b\k2"));
        assert_eq!(sel.picked_count(), 0);
    }

    #[test]
    fn a_removed_folder_takes_what_is_inside_and_a_lost_keep_drops_the_group() {
        let mut sel = two_groups();
        sel.remove_paths(&[PathBuf::from(r"C:\b")]);
        assert_eq!(sel.groups().len(), 1);
        let mut sel = DupeSelection::new(vec![group(r"C:\a\k1", &[r"C:\z\x1", r"C:\z\x2"])]);
        sel.toggle(0, Path::new(r"C:\z\x1"));
        sel.remove_paths(&[PathBuf::from(r"C:\a\k1")]);
        assert!(sel.groups().is_empty());
        assert_eq!(sel.picked_count(), 0);
    }

    #[test]
    fn rows_list_header_keep_then_others() {
        let sel = two_groups();
        assert_eq!(
            rows_for(&sel),
            vec![
                DupeRow::Header(0),
                DupeRow::Keep(0),
                DupeRow::Other(0, 0),
                DupeRow::Other(0, 1),
                DupeRow::Header(1),
                DupeRow::Keep(1),
                DupeRow::Other(1, 0),
            ]
        );
    }

    fn ok_flags() -> RowFlags {
        RowFlags {
            protected: false,
            app_owned: false,
            guard: false,
            locked: false,
        }
    }

    #[test]
    fn a_group_that_fails_the_check_is_removed_and_the_rest_go_on() {
        let mut sel = two_groups();
        sel.toggle(0, Path::new(r"C:\a\x1"));
        sel.toggle(1, Path::new(r"C:\b\y1"));
        let checked = check_groups(snapshot(&sel), |g, _| g.keep != Path::new(r"C:\a\k1"));
        assert_eq!(checked.failed.len(), 1);
        let flags: HashMap<PathBuf, RowFlags> = [
            (PathBuf::from(r"C:\a\x1"), ok_flags()),
            (PathBuf::from(r"C:\b\y1"), ok_flags()),
        ]
        .into();
        let request = apply_check(&mut sel, &checked, &flags, DeleteMode::RecycleBin).unwrap();
        assert_eq!(sel.groups().len(), 1);
        assert_eq!(sel.groups()[0].keep, PathBuf::from(r"C:\b\k2"));
        assert_eq!(request.items.len(), 1);
        assert_eq!(request.items[0].path, PathBuf::from(r"C:\b\y1"));
        assert_eq!(request.context, ConfirmContext::Duplicates);
        assert_eq!(request.mode, DeleteMode::RecycleBin);
        assert!(removed_notice(1).starts_with("1 "));
    }

    #[test]
    fn when_every_group_fails_nothing_is_asked_for() {
        let mut sel = two_groups();
        sel.toggle(0, Path::new(r"C:\a\x1"));
        let checked = check_groups(snapshot(&sel), |_, _| false);
        let request = apply_check(&mut sel, &checked, &HashMap::new(), DeleteMode::Permanent);
        assert!(request.is_none());
        assert_eq!(sel.groups().len(), 1);
    }

    #[test]
    fn rows_that_cannot_have_a_checkbox_stay_out_of_the_request() {
        let mut sel = two_groups();
        sel.toggle(0, Path::new(r"C:\a\x1"));
        sel.toggle(0, Path::new(r"C:\a\x2"));
        let checked = check_groups(snapshot(&sel), |_, _| true);
        let mut locked = ok_flags();
        locked.locked = true;
        let flags: HashMap<PathBuf, RowFlags> = [
            (PathBuf::from(r"C:\a\x1"), locked),
            (PathBuf::from(r"C:\a\x2"), ok_flags()),
        ]
        .into();
        let request = apply_check(&mut sel, &checked, &flags, DeleteMode::RecycleBin).unwrap();
        assert_eq!(request.items.len(), 1);
        assert_eq!(request.items[0].path, PathBuf::from(r"C:\a\x2"));
        let none = apply_check(&mut sel, &checked, &HashMap::new(), DeleteMode::RecycleBin);
        assert!(none.is_none());
    }

    #[test]
    fn nothing_ticked_is_not_checked() {
        let sel = two_groups();
        assert!(snapshot(&sel).is_empty());
    }

    #[test]
    fn twin_files_and_a_hard_link_in_a_real_folder() {
        let d = tempfile::tempdir().unwrap();
        let data = vec![7u8; 2 * 1024 * 1024];
        let a = d.path().join("a.bin");
        fs::write(&a, &data).unwrap();
        fs::hard_link(&a, d.path().join("a_link.bin")).unwrap();
        let twin = d.path().join("twin_of_a.bin");
        fs::write(&twin, &data).unwrap();
        fs::write(d.path().join("other.bin"), vec![8u8; 2 * 1024 * 1024]).unwrap();
        let opts = ScanOptions {
            threads: 2,
            excluded: Vec::new(),
        };
        let mut tree = scan(d.path(), &opts, &ScanProgress::default()).unwrap();
        let groups = find_duplicates(&tree, MIN_BYTES, &ScanProgress::default());
        assert_eq!(groups.len(), 1);
        assert_eq!(
            groups[0].others.len(),
            1,
            "the hard link is not a duplicate"
        );
        assert_eq!(groups[0].keep, a);
        assert_eq!(groups[0].others[0], twin);

        let mut sel = DupeSelection::new(groups);
        sel.toggle(0, &a);
        assert_eq!(sel.picked_count(), 0, "keep refuses the tick");
        sel.toggle(0, &twin);
        assert_eq!(sel.picked_count(), 1);
        let checked = check_groups(snapshot(&sel), targets_still_safe);
        assert!(checked.failed.is_empty());
        assert_eq!(checked.ok.len(), 1);

        // Only inside this test's own folder, and permanent, never the Recycle Bin.
        let target = Target {
            path: twin.clone(),
            size: 2 * 1024 * 1024,
        };
        let report = katharistis_core::delete::delete(
            std::slice::from_ref(&target),
            DeleteMode::Permanent,
            &ProtectContext::from_env(),
        );
        assert_eq!(report.removed, vec![twin.clone()], "{report:?}");
        assert!(!twin.exists());
        assert!(a.exists());
        crate::views::home::apply_report(&mut tree, &report);
        let mut st = DupesState {
            sel: Some(sel),
            ..DupesState::default()
        };
        st.after_report(&report);
        assert!(st.sel.as_ref().unwrap().groups().is_empty());
    }

    #[test]
    fn leaving_the_screen_drops_a_finished_check_and_keeps_the_ticks() {
        let mut sel = two_groups();
        sel.toggle(0, Path::new(r"C:\a\x1"));
        let mut st = DupesState {
            sel: Some(sel),
            ..DupesState::default()
        };
        st.phase = Some(Phase::Checking {
            job: Job::spawn(egui::Context::default(), || Checked {
                ok: vec![(group(r"C:\a\k1", &[r"C:\a\x1"]), vec![r"C:\a\x1".into()])],
                failed: Vec::new(),
            }),
            mode: DeleteMode::RecycleBin,
            generation: 0,
        });
        std::thread::sleep(Duration::from_millis(100));
        st.leave();
        assert!(st.phase.is_none());
        assert!(st.poll(0).is_none(), "nothing may start after leaving");
        assert_eq!(st.sel.as_ref().unwrap().picked_count(), 1);
    }

    #[test]
    fn a_running_search_is_not_dropped_by_leaving_but_can_be_cancelled() {
        let progress = ScanProgress::default();
        let mut st = DupesState {
            phase: Some(Phase::Finding {
                job: Job::spawn(egui::Context::default(), Vec::new),
                progress: progress.clone(),
                generation: 0,
            }),
            ..DupesState::default()
        };
        st.leave();
        assert!(matches!(st.phase, Some(Phase::Finding { .. })));
        st.cancel_search();
        assert!(progress.is_cancelled());
    }

    #[test]
    fn the_request_carries_the_checked_groups_for_the_check_at_delete_time() {
        let mut sel = two_groups();
        sel.toggle(0, Path::new(r"C:\a\x1"));
        let checked = check_groups(snapshot(&sel), |_, _| true);
        let flags: HashMap<PathBuf, RowFlags> = [(PathBuf::from(r"C:\a\x1"), ok_flags())].into();
        let request = apply_check(&mut sel, &checked, &flags, DeleteMode::Permanent).unwrap();
        assert_eq!(request.guards.len(), 1);
        assert_eq!(request.guards[0].0.keep, PathBuf::from(r"C:\a\k1"));
        assert_eq!(request.guards[0].1, vec![PathBuf::from(r"C:\a\x1")]);
    }

    #[test]
    fn a_row_without_flags_has_no_checkbox_and_stays_out_of_the_request() {
        assert!(!can_select(None));
        let mut sel = two_groups();
        sel.toggle(0, Path::new(r"C:\a\x1"));
        let checked = check_groups(snapshot(&sel), |_, _| true);
        assert!(apply_check(&mut sel, &checked, &HashMap::new(), DeleteMode::Permanent).is_none());
    }
}
