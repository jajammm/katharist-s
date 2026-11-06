//! The confirmation and report flow every delete in the app goes through. Nothing
//! is deleted until the user presses the confirm button of a dialog here, and what
//! the report says comes from `DeleteReport` alone.

use std::collections::HashSet;
use std::io;
use std::path::{Path, PathBuf};

use katharistis_core::delete::{self, DeleteMode, DeleteReport, FailReason, Target};
use katharistis_core::format::format_size;
use katharistis_core::protect::ProtectContext;
use katharistis_core::recycle;

use crate::i18n::l;
use crate::jobs::{Job, JobPanicked};

pub struct ConfirmRequest {
    pub items: Vec<Target>,
    pub mode: DeleteMode,
    pub warn_app_owned: usize,
    pub context: ConfirmContext,
}

/// `Cleanup`, `Leftovers` and `Duplicates` are raised by the screens of tasks 14 to 16.
#[allow(dead_code)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConfirmContext {
    User,
    Cleanup,
    Leftovers,
    Duplicates,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ConfirmSummary {
    pub count: usize,
    pub total: u64,
    pub permanent: bool,
    pub says_not_in_bin: bool,
    pub app_owned: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ReportSummary {
    pub freed: u64,
    pub in_bin: u64,
    pub failed: usize,
    pub offer_empty_bin: bool,
}

pub fn confirm_summary(req: &ConfirmRequest) -> ConfirmSummary {
    let permanent = req.mode == DeleteMode::Permanent;
    ConfirmSummary {
        count: req.items.len(),
        total: req.items.iter().map(|t| t.size).sum(),
        permanent,
        says_not_in_bin: permanent,
        app_owned: req.warn_app_owned,
    }
}

pub fn report_summary(r: &DeleteReport) -> ReportSummary {
    ReportSummary {
        freed: r.bytes_freed,
        in_bin: r.bytes_in_bin,
        failed: r.failed.len(),
        offer_empty_bin: r.bytes_in_bin > 0 || !r.recycled.is_empty(),
    }
}

/// What the shell refused to recycle, offered for a separate permanent delete.
/// Only `NotRecyclable`: `GuardKept` in particular is never offered, since its data
/// may exist nowhere else.
pub fn not_recyclable(r: &DeleteReport) -> Vec<Target> {
    r.failed
        .iter()
        .filter(|f| f.reason == FailReason::NotRecyclable)
        .map(|f| Target {
            path: f.path.clone(),
            size: f.size,
        })
        .collect()
}

/// The items of the permanent-delete step and which of them the user ticked.
pub struct PermanentOffer {
    pub items: Vec<Target>,
    pub checked: Vec<bool>,
}

impl PermanentOffer {
    /// Nothing starts ticked.
    pub fn new(items: Vec<Target>) -> Self {
        let checked = vec![false; items.len()];
        PermanentOffer { items, checked }
    }

    pub fn selected(&self) -> Vec<Target> {
        self.items
            .iter()
            .zip(&self.checked)
            .filter(|(_, &on)| on)
            .map(|(t, _)| t.clone())
            .collect()
    }
}

/// Takes out the `NotRecyclable` failure of each item sent to the permanent step: it
/// said the item was left where it was, which stopped being known at that point.
fn drop_sent_failures(report: &mut DeleteReport, sent: &[Target]) {
    let sent: HashSet<&PathBuf> = sent.iter().map(|t| &t.path).collect();
    report
        .failed
        .retain(|f| !(f.reason == FailReason::NotRecyclable && sent.contains(&f.path)));
}

/// `first` with the outcome of the permanent step in place of the `NotRecyclable`
/// failure of each item that was sent to it. Items the user left out keep theirs.
fn merge_permanent_step(
    mut first: DeleteReport,
    sent: &[Target],
    second: DeleteReport,
) -> DeleteReport {
    drop_sent_failures(&mut first, sent);
    first.removed.extend(second.removed);
    first.recycled.extend(second.recycled);
    first.failed.extend(second.failed);
    first.bytes_freed += second.bytes_freed;
    first.bytes_in_bin += second.bytes_in_bin;
    first
}

/// `first` for a permanent step that panicked: the failures of the items sent to it
/// no longer hold, so they come out, and the items are returned to be named as unknown.
fn split_unknown(mut first: DeleteReport, sent: Vec<Target>) -> (DeleteReport, Vec<Target>) {
    drop_sent_failures(&mut first, &sent);
    (first, sent)
}

/// What is known after asking Windows to empty the bin. The system asks the user
/// too, and a "no" there does not reliably come back as an error, so whether the
/// bin was emptied is read from the bin afterwards.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BinEmptied {
    pub before: Option<(u64, u64)>,
    pub after: Option<(u64, u64)>,
    pub error: Option<String>,
}

impl BinEmptied {
    fn is_empty_now(&self) -> bool {
        self.after == Some((0, 0))
    }
}

/// The bin's contents as shown in its confirmation.
pub enum BinLoad {
    Loading(Job<io::Result<(u64, u64)>>),
    Known(Option<(u64, u64)>),
}

pub enum DeleteFlow {
    Idle,
    Confirm(ConfirmRequest),
    Running(Job<DeleteReport>),
    ConfirmPermanent(PermanentOffer, DeleteReport),
    /// The permanent step: its job, what was sent to it, the report so far.
    RunningPermanent(Job<DeleteReport>, Vec<Target>, DeleteReport),
    Report(DeleteReport),
    ConfirmEmptyBin(DeleteReport, BinLoad),
    EmptyingBin(Job<BinEmptied>, DeleteReport),
    Emptied(DeleteReport, BinEmptied),
    /// A delete job panicked. The report holds only what an earlier step proved; the
    /// targets were sent to the job that panicked, so nothing can be said about them.
    OutcomeUnknown(DeleteReport, Vec<Target>),
}

impl DeleteFlow {
    pub fn is_idle(&self) -> bool {
        matches!(self, DeleteFlow::Idle)
    }

    /// A job that can change the disk is running.
    pub fn is_working(&self) -> bool {
        matches!(
            self,
            DeleteFlow::Running(_) | DeleteFlow::RunningPermanent(..) | DeleteFlow::EmptyingBin(..)
        )
    }

    pub fn outcome_unknown(&self) -> bool {
        matches!(self, DeleteFlow::OutcomeUnknown(..))
    }
}

/// Whether a request to close the window has to wait. Killing a delete or an emptying
/// halfway can leave a guard file or an outcome nobody can tell.
pub fn refuse_close(close_requested: bool, flow: &DeleteFlow) -> bool {
    close_requested && flow.is_working()
}

pub fn close_refused_text() -> &'static str {
    l(
        "Katharistís stays open until this finishes. Closing it now could leave a file only at a temporary path, or an outcome nobody can tell.",
        "Katharistís tetap terbuka sampai ini selesai. Menutupnya sekarang bisa meninggalkan file hanya di path sementara, atau hasil yang tidak bisa diketahui.",
    )
}

/// Failures after which nothing on the map can be trusted: a guard is about a file
/// that may be missing from where the map says it is. A panic gets here too, through
/// `DeleteFlow::outcome_unknown`.
pub fn map_may_be_stale(r: &DeleteReport) -> bool {
    r.failed
        .iter()
        .any(|f| matches!(f.reason, FailReason::GuardKept(_)))
}

/// Paths whose size on the map may be wrong after this report, since the delete may
/// have taken part of them. The rest of the map stays true. A protected or
/// not-recyclable item was not touched at all.
pub fn failed_to_lock(r: &DeleteReport) -> Vec<&Path> {
    r.failed
        .iter()
        .filter(|f| {
            matches!(
                f.reason,
                FailReason::InUse
                    | FailReason::AccessDenied
                    | FailReason::NotFound
                    | FailReason::Other(_)
            )
        })
        .map(|f| f.path.as_path())
        .collect()
}

fn mode_text(mode: DeleteMode) -> &'static str {
    match mode {
        DeleteMode::RecycleBin => l("Move to Recycle Bin", "Pindahkan ke Recycle Bin"),
        DeleteMode::Permanent => l("Delete permanently", "Hapus permanen"),
    }
}

fn items_text(count: usize, total: u64) -> String {
    format!("{count} {}, {}", l("items", "item"), format_size(total))
}

/// The lines of the confirmation dialog: count, size and mode, then anything else
/// the user should know before deciding.
pub fn confirm_lines(s: &ConfirmSummary, context: ConfirmContext) -> Vec<String> {
    let mode = if s.permanent {
        DeleteMode::Permanent
    } else {
        DeleteMode::RecycleBin
    };
    let mut lines = vec![
        items_text(s.count, s.total),
        format!("{} {}", l("Mode:", "Mode:"), mode_text(mode)),
    ];
    if s.says_not_in_bin {
        lines.push(
            l(
                "These items do not go to the Recycle Bin, so they cannot be restored from it.",
                "Item ini tidak masuk Recycle Bin, jadi tidak bisa dipulihkan dari sana.",
            )
            .to_string(),
        );
    }
    if context == ConfirmContext::Cleanup {
        lines.push(
            l(
                "Apps create this data again when they need it.",
                "Aplikasi membuat data ini lagi saat memerlukannya.",
            )
            .to_string(),
        );
    }
    if s.app_owned > 0 {
        lines.push(format!(
            "{} {}",
            s.app_owned,
            l(
                "of these items are used by an app: they are inside, or hold, an installed app's or a browser's folder.",
                "item di antaranya dipakai aplikasi: berada di dalam, atau berisi, folder aplikasi terpasang atau browser.",
            )
        ));
    }
    lines
}

/// Why an item did not go, in words. `GuardKept` names the guard path and promises
/// nothing beyond it.
pub fn reason_text(reason: &FailReason) -> String {
    match reason {
        FailReason::Protected => {
            l("Protected, not touched.", "Dilindungi, tidak disentuh.").to_string()
        }
        FailReason::NotFound => l(
            "Not found. It may have been moved or deleted already.",
            "Tidak ditemukan. Mungkin sudah dipindahkan atau dihapus.",
        )
        .to_string(),
        FailReason::InUse => {
            l("In use by another program.", "Sedang dipakai program lain.").to_string()
        }
        FailReason::AccessDenied => l("Access denied.", "Akses ditolak.").to_string(),
        FailReason::NotRecyclable => l(
            "Cannot go to the Recycle Bin. Left where it was.",
            "Tidak bisa masuk Recycle Bin. Dibiarkan di tempatnya.",
        )
        .to_string(),
        FailReason::GuardKept(guard) => format!(
            "{} {}",
            l(
                "Kept. A file's data may be only at:",
                "Dibiarkan. Data sebuah file mungkin hanya ada di:",
            ),
            guard.display()
        ),
        FailReason::Other(text) => text.clone(),
    }
}

/// The result of emptying the bin, as read back from the bin. Says "emptied" only
/// when the bin reads as empty.
pub fn bin_result_text(b: &BinEmptied) -> String {
    let mut text = match (b.is_empty_now(), b.before, b.after) {
        (true, Some((bytes, count)), _) if count > 0 || bytes > 0 => format!(
            "{} {}",
            l(
                "Recycle Bin emptied. It held:",
                "Recycle Bin dikosongkan. Tadinya berisi:"
            ),
            items_text(count as usize, bytes)
        ),
        (true, ..) => l("The Recycle Bin is empty.", "Recycle Bin kosong.").to_string(),
        (false, before, Some((bytes, count))) if before == Some((bytes, count)) => format!(
            "{} {}",
            l(
                "The Recycle Bin was not emptied. It still holds:",
                "Recycle Bin tidak dikosongkan. Masih berisi:",
            ),
            items_text(count as usize, bytes)
        ),
        (false, _, Some((bytes, count))) => format!(
            "{} {}",
            l("The Recycle Bin now holds:", "Recycle Bin sekarang berisi:"),
            items_text(count as usize, bytes)
        ),
        (false, _, None) => l(
            "Could not read the Recycle Bin, so whether it was emptied is unknown.",
            "Recycle Bin tidak terbaca, jadi tidak diketahui apakah sudah kosong.",
        )
        .to_string(),
    };
    if let Some(err) = &b.error {
        text.push(' ');
        text.push_str(err);
    }
    text
}

/// What the dialog says after a delete job panicked. Never suggests it worked.
pub fn unknown_outcome_text() -> &'static str {
    l(
        "The delete stopped unexpectedly, so its outcome is unknown. Some items may have been deleted or moved and others not. Scan again to see what is there now.",
        "Penghapusan berhenti tiba-tiba, jadi hasilnya tidak diketahui. Sebagian item mungkin sudah terhapus atau pindah, sebagian lagi belum. Pindai ulang untuk melihat keadaan sekarang.",
    )
}

fn offer_text() -> &'static str {
    l(
        "These items cannot go to the Recycle Bin. A link or a guard file inside a folder, a size above the bin's limit, or this drive's Recycle Bin settings can each be the reason. Delete permanently? A guard file stays either way. Tick the ones you want gone.",
        "File ini tidak bisa masuk Recycle Bin. Penyebabnya bisa tautan atau file pengaman di dalam folder, ukuran di atas batas Recycle Bin, atau pengaturan Recycle Bin drive ini. Hapus permanen? File pengaman tetap dibiarkan. Centang yang ingin dihapus.",
    )
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum FlowEvent {
    /// The confirm button of the dialog on screen.
    Confirm,
    /// Cancel, skip or Esc.
    Dismiss,
    AskEmptyBin,
}

/// The side effects of the flow, swappable so tests never touch the disk or the bin.
#[derive(Clone, Copy)]
struct FlowOps {
    delete: fn(&[Target], DeleteMode, &ProtectContext) -> DeleteReport,
    bin_info: fn() -> io::Result<(u64, u64)>,
    empty_bin: fn() -> io::Result<()>,
}

const REAL_OPS: FlowOps = FlowOps {
    delete: delete::delete,
    bin_info: recycle::bin_info,
    empty_bin: recycle::empty_bin,
};

fn panicked_bin_text() -> String {
    l(
        "Emptying stopped unexpectedly.",
        "Pengosongan berhenti tiba-tiba.",
    )
    .to_string()
}

/// Takes finished jobs into the next state. Returns the report the first time the
/// whole delete has an outcome, and never again for that flow.
fn poll_flow(flow: &mut DeleteFlow) -> Option<DeleteReport> {
    match flow {
        DeleteFlow::Running(job) => match job.poll()? {
            Err(JobPanicked) => {
                *flow = DeleteFlow::OutcomeUnknown(DeleteReport::default(), Vec::new());
                None
            }
            Ok(report) => {
                let offered = not_recyclable(&report);
                if offered.is_empty() {
                    *flow = DeleteFlow::Report(report.clone());
                    Some(report)
                } else {
                    *flow = DeleteFlow::ConfirmPermanent(PermanentOffer::new(offered), report);
                    None
                }
            }
        },
        DeleteFlow::RunningPermanent(job, sent, first) => {
            let outcome = job.poll()?;
            let first = std::mem::take(first);
            match outcome {
                Err(JobPanicked) => {
                    let (proven, unknown) = split_unknown(first, std::mem::take(sent));
                    *flow = DeleteFlow::OutcomeUnknown(proven.clone(), unknown);
                    Some(proven)
                }
                Ok(second) => {
                    let merged = merge_permanent_step(first, sent, second);
                    *flow = DeleteFlow::Report(merged.clone());
                    Some(merged)
                }
            }
        }
        DeleteFlow::ConfirmEmptyBin(_, load) => {
            if let BinLoad::Loading(job) = load {
                if let Some(outcome) = job.poll() {
                    *load = BinLoad::Known(outcome.ok().and_then(Result::ok));
                }
            }
            None
        }
        DeleteFlow::EmptyingBin(job, report) => {
            let outcome = job.poll()?;
            let report = std::mem::take(report);
            let result = outcome.unwrap_or_else(|JobPanicked| BinEmptied {
                before: None,
                after: None,
                error: Some(panicked_bin_text()),
            });
            *flow = DeleteFlow::Emptied(report, result);
            None
        }
        _ => None,
    }
}

fn start_bin_query(ctx: &egui::Context, ops: FlowOps) -> BinLoad {
    BinLoad::Loading(Job::spawn(ctx.clone(), ops.bin_info))
}

/// The only place a delete or an emptying starts: from `FlowEvent::Confirm` on the
/// matching dialog. Anything else leaves the state as it was.
fn apply_event(
    ctx: &egui::Context,
    flow: &mut DeleteFlow,
    event: FlowEvent,
    protect: &ProtectContext,
    ops: FlowOps,
) -> Option<DeleteReport> {
    let state = std::mem::replace(flow, DeleteFlow::Idle);
    match (state, event) {
        (DeleteFlow::Confirm(req), FlowEvent::Confirm) if !req.items.is_empty() => {
            let protect = protect.clone();
            *flow = DeleteFlow::Running(Job::spawn(ctx.clone(), move || {
                (ops.delete)(&req.items, req.mode, &protect)
            }));
            None
        }
        (DeleteFlow::Confirm(_), FlowEvent::Dismiss) => None,
        (DeleteFlow::ConfirmPermanent(offer, report), FlowEvent::Confirm) => {
            let sent = offer.selected();
            if sent.is_empty() {
                *flow = DeleteFlow::ConfirmPermanent(offer, report);
                return None;
            }
            let protect = protect.clone();
            let items = sent.clone();
            let job = Job::spawn(ctx.clone(), move || {
                (ops.delete)(&items, DeleteMode::Permanent, &protect)
            });
            *flow = DeleteFlow::RunningPermanent(job, sent, report);
            None
        }
        (DeleteFlow::ConfirmPermanent(_, report), FlowEvent::Dismiss) => {
            *flow = DeleteFlow::Report(report.clone());
            Some(report)
        }
        (DeleteFlow::Report(report), FlowEvent::AskEmptyBin)
            if report_summary(&report).offer_empty_bin =>
        {
            *flow = DeleteFlow::ConfirmEmptyBin(report, start_bin_query(ctx, ops));
            None
        }
        (DeleteFlow::Emptied(report, result), FlowEvent::AskEmptyBin) if !result.is_empty_now() => {
            *flow = DeleteFlow::ConfirmEmptyBin(report, start_bin_query(ctx, ops));
            None
        }
        (DeleteFlow::ConfirmEmptyBin(report, BinLoad::Known(before)), FlowEvent::Confirm) => {
            let job = Job::spawn(ctx.clone(), move || {
                let error = (ops.empty_bin)().err().map(|e| e.to_string());
                BinEmptied {
                    before,
                    after: (ops.bin_info)().ok(),
                    error,
                }
            });
            *flow = DeleteFlow::EmptyingBin(job, report);
            None
        }
        (DeleteFlow::ConfirmEmptyBin(report, _), FlowEvent::Dismiss) => {
            *flow = DeleteFlow::Report(report);
            None
        }
        (
            DeleteFlow::Report(_) | DeleteFlow::Emptied(..) | DeleteFlow::OutcomeUnknown(..),
            FlowEvent::Dismiss,
        ) => None,
        (state, _) => {
            *flow = state;
            None
        }
    }
}

fn path_list(ui: &mut egui::Ui, salt: &str, rows: usize, text: impl Fn(usize) -> String) {
    let height = ui.text_style_height(&egui::TextStyle::Body);
    egui::ScrollArea::vertical()
        .id_salt(salt)
        .max_height(140.0)
        .auto_shrink([false, true])
        .show_rows(ui, height, rows, |ui, range| {
            for i in range {
                ui.add(egui::Label::new(text(i)).truncate());
            }
        });
}

fn confirm_button(ui: &mut egui::Ui, enabled: bool, text: &str) -> bool {
    ui.add_enabled(enabled, egui::Button::new(text)).clicked()
}

fn render_confirm(ui: &mut egui::Ui, req: &ConfirmRequest) -> Option<FlowEvent> {
    let s = confirm_summary(req);
    ui.heading(l("Confirm", "Konfirmasi"));
    ui.add_space(6.0);
    for line in confirm_lines(&s, req.context) {
        ui.label(line);
    }
    ui.add_space(6.0);
    path_list(ui, "confirm-items", req.items.len(), |i| {
        req.items[i].path.display().to_string()
    });
    ui.add_space(8.0);
    let mut event = None;
    ui.horizontal(|ui| {
        if confirm_button(ui, s.count > 0, mode_text(req.mode)) {
            event = Some(FlowEvent::Confirm);
        }
        if ui.button(l("Cancel", "Batal")).clicked() {
            event = Some(FlowEvent::Dismiss);
        }
    });
    event
}

fn render_offer(ui: &mut egui::Ui, offer: &mut PermanentOffer) -> Option<FlowEvent> {
    ui.heading(l("Delete permanently?", "Hapus permanen?"));
    ui.add_space(6.0);
    ui.label(offer_text());
    ui.add_space(6.0);
    let height = ui.spacing().interact_size.y;
    let PermanentOffer { items, checked } = offer;
    egui::ScrollArea::vertical()
        .id_salt("offer-items")
        .max_height(160.0)
        .auto_shrink([false, true])
        .show_rows(ui, height, items.len(), |ui, range| {
            for i in range {
                ui.horizontal(|ui| {
                    ui.checkbox(&mut checked[i], "");
                    ui.add(
                        egui::Label::new(format!(
                            "{}  ({})",
                            items[i].path.display(),
                            format_size(items[i].size)
                        ))
                        .truncate(),
                    );
                });
            }
        });
    let picked = offer.selected();
    let total: u64 = picked.iter().map(|t| t.size).sum();
    ui.add_space(6.0);
    ui.label(items_text(picked.len(), total));
    ui.label(format!(
        "{} {}",
        l("Mode:", "Mode:"),
        mode_text(DeleteMode::Permanent)
    ));
    ui.add_space(8.0);
    let mut event = None;
    ui.horizontal(|ui| {
        if confirm_button(ui, !picked.is_empty(), mode_text(DeleteMode::Permanent)) {
            event = Some(FlowEvent::Confirm);
        }
        if ui.button(l("Keep them", "Biarkan")).clicked() {
            event = Some(FlowEvent::Dismiss);
        }
    });
    event
}

/// The numbers and lists of a report. `in_bin_line` is off once the bin reads as
/// empty, since those items are no longer there.
fn report_body(ui: &mut egui::Ui, r: &DeleteReport, in_bin_line: bool) {
    let s = report_summary(r);
    if in_bin_line {
        ui.label(format!(
            "{} {}",
            l(
                "Moved to the Recycle Bin, disk space not freed yet:",
                "Dipindahkan ke Recycle Bin, belum mengosongkan disk:",
            ),
            format_size(s.in_bin)
        ));
    }
    ui.strong(format!(
        "{} {}",
        l("Space gained:", "Ruang yang didapat:"),
        format_size(s.freed)
    ));
    if !r.removed.is_empty() {
        ui.add_space(4.0);
        ui.weak(l("Deleted for good:", "Terhapus permanen:"));
        path_list(ui, "report-removed", r.removed.len(), |i| {
            r.removed[i].display().to_string()
        });
    }
    if s.failed > 0 {
        ui.add_space(6.0);
        ui.strong(format!(
            "{} {}",
            l("Not done:", "Tidak terlaksana:"),
            s.failed
        ));
        for f in r
            .failed
            .iter()
            .filter(|f| matches!(f.reason, FailReason::GuardKept(_)))
        {
            ui.label(format!("{}: {}", f.path.display(), reason_text(&f.reason)));
        }
        let rest: Vec<_> = r
            .failed
            .iter()
            .filter(|f| !matches!(f.reason, FailReason::GuardKept(_)))
            .collect();
        path_list(ui, "report-failed", rest.len(), |i| {
            format!(
                "{}: {}",
                rest[i].path.display(),
                reason_text(&rest[i].reason)
            )
        });
    }
}

fn close_button(ui: &mut egui::Ui, event: &mut Option<FlowEvent>) {
    if ui.button(l("Close", "Tutup")).clicked() {
        *event = Some(FlowEvent::Dismiss);
    }
}

fn empty_bin_button(ui: &mut egui::Ui, event: &mut Option<FlowEvent>) {
    if ui
        .button(l("Empty Recycle Bin", "Kosongkan Recycle Bin"))
        .clicked()
    {
        *event = Some(FlowEvent::AskEmptyBin);
    }
}

fn render_report(ui: &mut egui::Ui, r: &DeleteReport) -> Option<FlowEvent> {
    ui.heading(l("Report", "Laporan"));
    ui.add_space(6.0);
    report_body(ui, r, true);
    ui.add_space(8.0);
    let mut event = None;
    ui.horizontal(|ui| {
        if report_summary(r).offer_empty_bin {
            empty_bin_button(ui, &mut event);
        }
        close_button(ui, &mut event);
    });
    event
}

fn render_empty_bin(ui: &mut egui::Ui, load: &BinLoad) -> Option<FlowEvent> {
    ui.heading(l("Empty the Recycle Bin?", "Kosongkan Recycle Bin?"));
    ui.add_space(6.0);
    match load {
        BinLoad::Loading(_) => {
            ui.spinner();
        }
        BinLoad::Known(Some((bytes, count))) => {
            ui.label(format!(
                "{} {}",
                l("It holds:", "Isinya:"),
                items_text(*count as usize, *bytes)
            ));
        }
        BinLoad::Known(None) => {
            ui.label(l("Its size could not be read.", "Ukurannya tidak terbaca."));
        }
    }
    ui.label(l(
        "This covers every drive and everything in the bin, including what was there before. Items cannot be restored afterwards. Windows may ask once more.",
        "Ini mencakup semua drive dan seluruh isi Recycle Bin, termasuk yang sudah ada sebelumnya. Isinya tidak bisa dipulihkan setelah itu. Windows mungkin bertanya sekali lagi.",
    ));
    ui.add_space(8.0);
    let mut event = None;
    ui.horizontal(|ui| {
        let ready = matches!(load, BinLoad::Known(_));
        if confirm_button(ui, ready, l("Empty Recycle Bin", "Kosongkan Recycle Bin")) {
            event = Some(FlowEvent::Confirm);
        }
        if ui.button(l("Cancel", "Batal")).clicked() {
            event = Some(FlowEvent::Dismiss);
        }
    });
    event
}

fn render_emptied(ui: &mut egui::Ui, r: &DeleteReport, b: &BinEmptied) -> Option<FlowEvent> {
    ui.heading(l("Report", "Laporan"));
    ui.add_space(6.0);
    ui.label(bin_result_text(b));
    report_body(ui, r, !b.is_empty_now());
    ui.add_space(8.0);
    let mut event = None;
    ui.horizontal(|ui| {
        if !b.is_empty_now() {
            empty_bin_button(ui, &mut event);
        }
        close_button(ui, &mut event);
    });
    event
}

fn render_unknown(
    ui: &mut egui::Ui,
    proven: &DeleteReport,
    unknown: &[Target],
) -> Option<FlowEvent> {
    ui.heading(l("Outcome unknown", "Hasil tidak diketahui"));
    ui.add_space(6.0);
    ui.label(unknown_outcome_text());
    if !unknown.is_empty() {
        ui.add_space(6.0);
        ui.strong(l(
            "Sent to delete permanently, outcome unknown:",
            "Dikirim untuk dihapus permanen, hasil tidak diketahui:",
        ));
        path_list(ui, "unknown-items", unknown.len(), |i| {
            unknown[i].path.display().to_string()
        });
    }
    let s = report_summary(proven);
    if s.freed > 0 || s.offer_empty_bin || s.failed > 0 {
        ui.add_space(6.0);
        ui.weak(l(
            "Known from the step before it:",
            "Yang diketahui dari langkah sebelumnya:",
        ));
        report_body(ui, proven, true);
    }
    ui.add_space(8.0);
    let mut event = None;
    close_button(ui, &mut event);
    event
}

fn render_busy(ui: &mut egui::Ui, text: &str) {
    ui.horizontal(|ui| {
        ui.spinner();
        ui.label(text);
    });
}

fn render_flow(ctx: &egui::Context, flow: &mut DeleteFlow) -> Option<FlowEvent> {
    if flow.is_idle() {
        return None;
    }
    let modal = egui::Modal::new(egui::Id::unique("delete-flow")).show(ctx, |ui| {
        ui.set_min_width(420.0);
        ui.set_max_width(540.0);
        match flow {
            DeleteFlow::Idle => None,
            DeleteFlow::Confirm(req) => render_confirm(ui, req),
            DeleteFlow::ConfirmPermanent(offer, _) => render_offer(ui, offer),
            DeleteFlow::Report(r) => render_report(ui, r),
            DeleteFlow::ConfirmEmptyBin(_, load) => render_empty_bin(ui, load),
            DeleteFlow::Emptied(r, b) => render_emptied(ui, r, b),
            DeleteFlow::OutcomeUnknown(proven, unknown) => render_unknown(ui, proven, unknown),
            DeleteFlow::Running(_) | DeleteFlow::RunningPermanent(..) => {
                render_busy(ui, l("Working...", "Sedang dikerjakan..."));
                None
            }
            DeleteFlow::EmptyingBin(..) => {
                render_busy(
                    ui,
                    l("Emptying the Recycle Bin...", "Mengosongkan Recycle Bin..."),
                );
                None
            }
        }
    });
    let escape = !flow.is_working() && ctx.input(|i| i.key_pressed(egui::Key::Escape));
    modal.inner.or(escape.then_some(FlowEvent::Dismiss))
}

fn drive_flow(
    ctx: &egui::Context,
    flow: &mut DeleteFlow,
    protect: &ProtectContext,
    ops: FlowOps,
) -> Option<DeleteReport> {
    let polled = poll_flow(flow);
    let applied = render_flow(ctx, flow).and_then(|e| apply_event(ctx, flow, e, protect, ops));
    polled.or(applied)
}

/// Draws the current step. Returns the report once, when the delete has finished,
/// so the caller can update its tree. Emptying the bin later returns nothing more.
pub fn show_flow(
    ctx: &egui::Context,
    flow: &mut DeleteFlow,
    protect: &ProtectContext,
) -> Option<DeleteReport> {
    drive_flow(ctx, flow, protect, REAL_OPS)
}

#[cfg(test)]
mod tests {
    use super::*;
    use katharistis_core::delete::Failed;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::{Duration, Instant};

    fn t(path: &str, size: u64) -> Target {
        Target {
            path: PathBuf::from(path),
            size,
        }
    }

    fn fail(path: &str, size: u64, reason: FailReason) -> Failed {
        Failed {
            path: PathBuf::from(path),
            size,
            reason,
        }
    }

    fn req(mode: DeleteMode, context: ConfirmContext) -> ConfirmRequest {
        ConfirmRequest {
            items: vec![t(r"C:\t\a", 10), t(r"C:\t\b", 5)],
            mode,
            warn_app_owned: 0,
            context,
        }
    }

    fn protect() -> ProtectContext {
        ProtectContext {
            windows_dir: r"C:\Windows".into(),
            program_files: vec![r"C:\Program Files".into()],
            program_data: r"C:\ProgramData".into(),
            users_dir: r"C:\Users".into(),
            profile: r"C:\Users\katharistis-test-profile".into(),
            own_dirs: vec![r"C:\Users\katharistis-test-profile\own".into()],
        }
    }

    fn delete_refused(_: &[Target], _: DeleteMode, _: &ProtectContext) -> DeleteReport {
        panic!("no delete may start here");
    }
    fn delete_all_gone(items: &[Target], mode: DeleteMode, _: &ProtectContext) -> DeleteReport {
        assert_eq!(mode, DeleteMode::Permanent);
        DeleteReport {
            removed: items.iter().map(|t| t.path.clone()).collect(),
            bytes_freed: items.iter().map(|t| t.size).sum(),
            ..Default::default()
        }
    }
    fn delete_panics(_: &[Target], _: DeleteMode, _: &ProtectContext) -> DeleteReport {
        panic!("boom");
    }
    fn info_refused() -> io::Result<(u64, u64)> {
        panic!("the bin may not be read here");
    }
    fn empty_refused() -> io::Result<()> {
        panic!("the bin may not be emptied here");
    }

    const NO_OPS: FlowOps = FlowOps {
        delete: delete_refused,
        bin_info: info_refused,
        empty_bin: empty_refused,
    };

    /// Runs the job poller until the flow leaves its working state.
    fn settle(flow: &mut DeleteFlow) -> Vec<DeleteReport> {
        let start = Instant::now();
        let mut reports = Vec::new();
        while flow.is_working()
            || matches!(flow, DeleteFlow::ConfirmEmptyBin(_, BinLoad::Loading(_)))
        {
            reports.extend(poll_flow(flow));
            assert!(start.elapsed() < Duration::from_secs(5));
            std::thread::sleep(Duration::from_millis(2));
        }
        reports.extend(poll_flow(flow));
        reports
    }

    fn running(report: DeleteReport) -> DeleteFlow {
        DeleteFlow::Running(Job::spawn(egui::Context::default(), move || report))
    }

    #[test]
    fn report_separates_bin_from_freed() {
        let s = report_summary(&DeleteReport {
            bytes_freed: 10,
            bytes_in_bin: 100,
            ..Default::default()
        });
        assert_eq!(s.freed, 10);
        assert_eq!(s.in_bin, 100);
        assert!(s.offer_empty_bin);
    }

    #[test]
    fn empty_bin_is_offered_only_when_this_delete_used_the_bin() {
        let permanent = DeleteReport {
            removed: vec![PathBuf::from(r"C:\t\a")],
            bytes_freed: 10,
            ..Default::default()
        };
        assert!(!report_summary(&permanent).offer_empty_bin);
        let empty_file = DeleteReport {
            recycled: vec![PathBuf::from(r"C:\t\a")],
            ..Default::default()
        };
        assert!(report_summary(&empty_file).offer_empty_bin);
    }

    #[test]
    fn not_recyclable_is_offered_separately_and_nothing_else_is() {
        let r = DeleteReport {
            failed: vec![
                fail(r"C:\t\big", 7, FailReason::NotRecyclable),
                fail(r"C:\t\open", 8, FailReason::InUse),
                fail(r"C:\t\guard", 9, FailReason::GuardKept(r"C:\t\.g".into())),
                fail(r"C:\t\sys", 1, FailReason::Protected),
            ],
            ..Default::default()
        };
        let offered = not_recyclable(&r);
        assert_eq!(offered.len(), 1);
        assert_eq!(offered[0].path, Path::new(r"C:\t\big"));
        assert_eq!(offered[0].size, 7);
        assert_eq!(report_summary(&r).failed, 4);
    }

    #[test]
    fn cleanup_confirmation_states_permanent() {
        let s = confirm_summary(&req(DeleteMode::Permanent, ConfirmContext::Cleanup));
        assert!(s.permanent);
        assert!(s.says_not_in_bin);
        let lines = confirm_lines(&s, ConfirmContext::Cleanup).join("\n");
        assert!(lines.contains(mode_text(DeleteMode::Permanent)));
        assert!(lines.contains("do not go to the Recycle Bin"));
    }

    #[test]
    fn recycle_confirmation_states_count_size_and_mode() {
        let s = confirm_summary(&req(DeleteMode::RecycleBin, ConfirmContext::User));
        assert_eq!((s.count, s.total), (2, 15));
        assert!(!s.permanent);
        assert!(!s.says_not_in_bin);
        let lines = confirm_lines(&s, ConfirmContext::User).join("\n");
        assert!(lines.contains("2 items"));
        assert!(lines.contains("15 B"));
        assert!(lines.contains(mode_text(DeleteMode::RecycleBin)));
        assert!(!lines.contains("do not go to the Recycle Bin"));
    }

    #[test]
    fn app_owned_warning_counts_items() {
        let mut r = req(DeleteMode::RecycleBin, ConfirmContext::User);
        let quiet = confirm_lines(&confirm_summary(&r), r.context).join("\n");
        assert!(!quiet.contains("used by an app"));
        r.warn_app_owned = 2;
        let s = confirm_summary(&r);
        assert_eq!(s.app_owned, 2);
        let lines = confirm_lines(&s, r.context).join("\n");
        assert!(lines.contains("2 of these items are used by an app"));
    }

    #[test]
    fn offer_starts_with_nothing_ticked() {
        let mut offer = PermanentOffer::new(vec![t(r"C:\a", 1), t(r"C:\b", 2)]);
        assert!(offer.selected().is_empty());
        offer.checked[1] = true;
        let picked = offer.selected();
        assert_eq!(picked.len(), 1);
        assert_eq!(picked[0].path, Path::new(r"C:\b"));
    }

    #[test]
    fn offer_wording_is_not_only_about_size() {
        assert!(offer_text().contains("link"));
        assert!(offer_text().contains("settings"));
        assert!(offer_text().contains("size"));
    }

    #[test]
    fn guard_kept_names_the_path_and_promises_nothing() {
        let text = reason_text(&FailReason::GuardKept(r"C:\t\.katharistis-guard-1".into()));
        assert!(text.contains(r"C:\t\.katharistis-guard-1"));
        assert!(text.contains("may be only at"));
    }

    #[test]
    fn only_a_guard_makes_the_whole_map_stale() {
        let report = |reason| DeleteReport {
            failed: vec![fail(r"C:\a", 1, reason)],
            ..Default::default()
        };
        assert!(map_may_be_stale(&report(FailReason::GuardKept(
            r"C:\g".into()
        ))));
        for reason in [
            FailReason::Protected,
            FailReason::NotRecyclable,
            FailReason::InUse,
            FailReason::AccessDenied,
            FailReason::NotFound,
            FailReason::Other("x".into()),
        ] {
            assert!(!map_may_be_stale(&report(reason)));
        }
    }

    #[test]
    fn ordinary_failures_are_locked_and_untouched_items_are_not() {
        let r = DeleteReport {
            failed: vec![
                fail(r"C:\use", 1, FailReason::InUse),
                fail(r"C:\deny", 1, FailReason::AccessDenied),
                fail(r"C:\gone", 1, FailReason::NotFound),
                fail(r"C:\other", 1, FailReason::Other("x".into())),
                fail(r"C:\prot", 1, FailReason::Protected),
                fail(r"C:\nobin", 1, FailReason::NotRecyclable),
                fail(r"C:\guard", 1, FailReason::GuardKept(r"C:\g".into())),
            ],
            ..Default::default()
        };
        assert_eq!(
            failed_to_lock(&r),
            [
                Path::new(r"C:\use"),
                Path::new(r"C:\deny"),
                Path::new(r"C:\gone"),
                Path::new(r"C:\other"),
            ]
        );
    }

    #[test]
    fn flow_returns_the_report_exactly_once() {
        let mut flow = running(DeleteReport {
            recycled: vec![PathBuf::from(r"C:\t\a")],
            bytes_in_bin: 10,
            ..Default::default()
        });
        let reports = settle(&mut flow);
        assert_eq!(reports.len(), 1);
        assert_eq!(reports[0].bytes_in_bin, 10);
        assert!(matches!(flow, DeleteFlow::Report(_)));
        assert!(poll_flow(&mut flow).is_none());
    }

    #[test]
    fn a_panicked_delete_never_reads_as_success() {
        let job: Job<DeleteReport> = Job::spawn(egui::Context::default(), || panic!("boom"));
        let mut flow = DeleteFlow::Running(job);
        let reports = settle(&mut flow);
        assert!(reports.is_empty());
        assert!(flow.outcome_unknown());
        assert!(unknown_outcome_text().contains("unknown"));
        assert!(unknown_outcome_text().contains("Scan again"));
    }

    #[test]
    fn panic_through_the_real_transition_is_also_unknown() {
        let ctx = egui::Context::default();
        let mut flow = DeleteFlow::Confirm(req(DeleteMode::RecycleBin, ConfirmContext::User));
        let ops = FlowOps {
            delete: delete_panics,
            ..NO_OPS
        };
        apply_event(&ctx, &mut flow, FlowEvent::Confirm, &protect(), ops);
        assert!(flow.is_working());
        assert!(settle(&mut flow).is_empty());
        assert!(flow.outcome_unknown());
    }

    #[test]
    fn nothing_is_deleted_without_the_confirm_event() {
        let ctx = egui::Context::default();
        let p = protect();
        let mut flow = DeleteFlow::Confirm(req(DeleteMode::RecycleBin, ConfirmContext::User));
        assert!(poll_flow(&mut flow).is_none());
        assert!(matches!(flow, DeleteFlow::Confirm(_)));
        apply_event(&ctx, &mut flow, FlowEvent::AskEmptyBin, &p, NO_OPS);
        assert!(matches!(flow, DeleteFlow::Confirm(_)));
        apply_event(&ctx, &mut flow, FlowEvent::Dismiss, &p, NO_OPS);
        assert!(flow.is_idle());
    }

    #[test]
    fn a_confirm_with_no_items_starts_nothing() {
        let mut flow = DeleteFlow::Confirm(ConfirmRequest {
            items: Vec::new(),
            mode: DeleteMode::Permanent,
            warn_app_owned: 0,
            context: ConfirmContext::User,
        });
        let ctx = egui::Context::default();
        apply_event(&ctx, &mut flow, FlowEvent::Confirm, &protect(), NO_OPS);
        assert!(matches!(flow, DeleteFlow::Confirm(_)));
    }

    fn with_not_recyclable() -> DeleteReport {
        DeleteReport {
            recycled: vec![PathBuf::from(r"C:\t\small")],
            bytes_in_bin: 3,
            failed: vec![
                fail(r"C:\t\big", 7, FailReason::NotRecyclable),
                fail(r"C:\t\huge", 9, FailReason::NotRecyclable),
                fail(r"C:\t\open", 8, FailReason::InUse),
            ],
            ..Default::default()
        }
    }

    #[test]
    fn not_recyclable_stops_at_a_step_with_nothing_ticked_and_no_report_yet() {
        let mut flow = running(with_not_recyclable());
        assert!(settle(&mut flow).is_empty());
        let DeleteFlow::ConfirmPermanent(offer, _) = &flow else {
            panic!("expected the permanent step");
        };
        assert_eq!(offer.items.len(), 2);
        assert!(offer.selected().is_empty());
    }

    #[test]
    fn keeping_them_reports_once_and_leaves_them_failed() {
        let ctx = egui::Context::default();
        let mut flow = running(with_not_recyclable());
        settle(&mut flow);
        let got = apply_event(&ctx, &mut flow, FlowEvent::Dismiss, &protect(), NO_OPS);
        let got = got.expect("the report");
        assert_eq!(got.failed.len(), 3);
        assert_eq!(got.bytes_freed, 0);
        assert!(matches!(flow, DeleteFlow::Report(_)));
        assert!(poll_flow(&mut flow).is_none());
    }

    #[test]
    fn permanent_step_with_nothing_ticked_starts_nothing() {
        let ctx = egui::Context::default();
        let mut flow = running(with_not_recyclable());
        settle(&mut flow);
        apply_event(&ctx, &mut flow, FlowEvent::Confirm, &protect(), NO_OPS);
        assert!(matches!(flow, DeleteFlow::ConfirmPermanent(..)));
    }

    #[test]
    fn permanent_step_deletes_only_what_was_ticked_and_merges() {
        let ctx = egui::Context::default();
        let mut flow = running(with_not_recyclable());
        settle(&mut flow);
        let DeleteFlow::ConfirmPermanent(offer, _) = &mut flow else {
            panic!("expected the permanent step");
        };
        offer.checked[0] = true;
        let ops = FlowOps {
            delete: delete_all_gone,
            ..NO_OPS
        };
        apply_event(&ctx, &mut flow, FlowEvent::Confirm, &protect(), ops);
        let reports = settle(&mut flow);
        assert_eq!(reports.len(), 1);
        let r = &reports[0];
        assert_eq!(r.removed, [PathBuf::from(r"C:\t\big")]);
        assert_eq!(r.bytes_freed, 7);
        assert_eq!(r.bytes_in_bin, 3);
        assert_eq!(r.recycled.len(), 1);
        let left: Vec<_> = r.failed.iter().map(|f| f.path.clone()).collect();
        assert_eq!(
            left,
            [PathBuf::from(r"C:\t\huge"), PathBuf::from(r"C:\t\open")]
        );
        // The permanent step does not offer itself again.
        assert!(matches!(flow, DeleteFlow::Report(_)));
    }

    #[test]
    fn panic_in_the_permanent_step_keeps_only_what_was_proven() {
        let ctx = egui::Context::default();
        let mut flow = running(with_not_recyclable());
        settle(&mut flow);
        let DeleteFlow::ConfirmPermanent(offer, _) = &mut flow else {
            panic!("expected the permanent step");
        };
        offer.checked[0] = true;
        let ops = FlowOps {
            delete: delete_panics,
            ..NO_OPS
        };
        apply_event(&ctx, &mut flow, FlowEvent::Confirm, &protect(), ops);
        let reports = settle(&mut flow);
        assert_eq!(reports.len(), 1);
        assert_eq!(reports[0].bytes_freed, 0);
        assert_eq!(reports[0].recycled.len(), 1);
        let big = PathBuf::from(r"C:\t\big");
        // The ticked item may be partly or fully gone: nothing may still say it stayed.
        assert!(reports[0].failed.iter().all(|f| f.path != big));
        let DeleteFlow::OutcomeUnknown(proven, unknown) = &flow else {
            panic!("expected the unknown outcome");
        };
        assert!(proven.failed.iter().all(|f| f.path != big));
        assert_eq!(unknown.len(), 1);
        assert_eq!(unknown[0].path, big);
        // What was not sent keeps its reason: left out, and in use.
        let left: Vec<_> = proven.failed.iter().map(|f| f.path.clone()).collect();
        assert_eq!(
            left,
            [PathBuf::from(r"C:\t\huge"), PathBuf::from(r"C:\t\open")]
        );
    }

    #[test]
    fn closing_is_refused_only_while_a_job_can_change_the_disk() {
        let ctx = egui::Context::default();
        let (tx, rx) = std::sync::mpsc::channel::<()>();
        let job = Job::spawn(ctx.clone(), move || {
            let _ = rx.recv();
            DeleteReport::default()
        });
        let mut flow = DeleteFlow::Running(job);
        assert!(refuse_close(true, &flow));
        assert!(!refuse_close(false, &flow));
        tx.send(()).unwrap();
        settle(&mut flow);
        assert!(!refuse_close(true, &flow));

        let not_working = [
            DeleteFlow::Idle,
            DeleteFlow::Confirm(req(DeleteMode::RecycleBin, ConfirmContext::User)),
            DeleteFlow::Report(DeleteReport::default()),
            DeleteFlow::OutcomeUnknown(DeleteReport::default(), Vec::new()),
        ];
        for flow in &not_working {
            assert!(!refuse_close(true, flow));
        }

        let empty = Job::spawn(ctx.clone(), || BinEmptied {
            before: None,
            after: None,
            error: None,
        });
        let emptying = DeleteFlow::EmptyingBin(empty, DeleteReport::default());
        assert!(refuse_close(true, &emptying));
        let permanent = Job::spawn(ctx, DeleteReport::default);
        let second = DeleteFlow::RunningPermanent(permanent, Vec::new(), DeleteReport::default());
        assert!(refuse_close(true, &second));
    }

    #[test]
    fn empty_bin_is_refused_when_not_offered() {
        let ctx = egui::Context::default();
        let mut flow = DeleteFlow::Report(DeleteReport::default());
        apply_event(&ctx, &mut flow, FlowEvent::AskEmptyBin, &protect(), NO_OPS);
        assert!(matches!(flow, DeleteFlow::Report(_)));
    }

    static FAKE_BIN_EMPTIED: AtomicBool = AtomicBool::new(false);
    fn fake_info() -> io::Result<(u64, u64)> {
        Ok(if FAKE_BIN_EMPTIED.load(Ordering::SeqCst) {
            (0, 0)
        } else {
            (100, 2)
        })
    }
    fn fake_empty() -> io::Result<()> {
        FAKE_BIN_EMPTIED.store(true, Ordering::SeqCst);
        Ok(())
    }

    #[test]
    fn emptying_needs_its_own_confirmation_and_reports_nothing_twice() {
        let ctx = egui::Context::default();
        let p = protect();
        let ops = FlowOps {
            bin_info: fake_info,
            empty_bin: fake_empty,
            ..NO_OPS
        };
        let mut flow = DeleteFlow::Report(DeleteReport {
            bytes_in_bin: 10,
            ..Default::default()
        });
        assert!(apply_event(&ctx, &mut flow, FlowEvent::AskEmptyBin, &p, ops).is_none());
        assert!(settle(&mut flow).is_empty());
        assert!(matches!(
            flow,
            DeleteFlow::ConfirmEmptyBin(_, BinLoad::Known(Some((100, 2))))
        ));
        assert!(!FAKE_BIN_EMPTIED.load(Ordering::SeqCst));

        // Backing out goes back to the report and empties nothing.
        apply_event(&ctx, &mut flow, FlowEvent::Dismiss, &p, ops);
        assert!(matches!(flow, DeleteFlow::Report(_)));
        assert!(!FAKE_BIN_EMPTIED.load(Ordering::SeqCst));

        apply_event(&ctx, &mut flow, FlowEvent::AskEmptyBin, &p, ops);
        settle(&mut flow);
        apply_event(&ctx, &mut flow, FlowEvent::Confirm, &p, ops);
        assert!(flow.is_working());
        assert!(settle(&mut flow).is_empty());
        let DeleteFlow::Emptied(report, result) = &flow else {
            panic!("expected the emptied state");
        };
        assert_eq!(report.bytes_in_bin, 10);
        assert_eq!(result.before, Some((100, 2)));
        assert_eq!(result.after, Some((0, 0)));
        apply_event(&ctx, &mut flow, FlowEvent::Dismiss, &p, ops);
        assert!(flow.is_idle());
    }

    #[test]
    fn confirming_before_the_bin_is_read_does_nothing() {
        let ctx = egui::Context::default();
        let job = Job::spawn(ctx.clone(), || Ok((1, 1)));
        let mut flow = DeleteFlow::ConfirmEmptyBin(DeleteReport::default(), BinLoad::Loading(job));
        apply_event(&ctx, &mut flow, FlowEvent::Confirm, &protect(), NO_OPS);
        assert!(matches!(flow, DeleteFlow::ConfirmEmptyBin(..)));
    }

    #[test]
    fn bin_result_says_emptied_only_when_the_bin_reads_empty() {
        let result = |before, after, error: Option<&str>| BinEmptied {
            before,
            after,
            error: error.map(str::to_string),
        };
        let emptied = bin_result_text(&result(Some((2048, 3)), Some((0, 0)), None));
        assert!(emptied.contains("emptied"));
        assert!(emptied.contains("3 items"));
        assert!(emptied.contains("2.0 KiB"));

        // The user said no in Windows' own dialog: same contents, no error.
        let declined = bin_result_text(&result(Some((2048, 3)), Some((2048, 3)), None));
        assert!(declined.contains("not emptied"));

        let partial = bin_result_text(&result(Some((2048, 3)), Some((1024, 1)), Some("E")));
        assert!(partial.contains("now holds"));
        assert!(partial.contains("1.0 KiB"));
        assert!(partial.ends_with('E'));

        let unread = bin_result_text(&result(None, None, None));
        assert!(unread.contains("unknown"));
    }

    #[test]
    fn every_step_draws_and_stays_put_without_a_click() {
        let ctx = egui::Context::default();
        let p = protect();
        let report = || DeleteReport {
            removed: vec![PathBuf::from(r"C:\t\gone")],
            recycled: vec![PathBuf::from(r"C:\t\bin")],
            failed: vec![
                fail(r"C:\t\open", 8, FailReason::InUse),
                fail(r"C:\t\g", 9, FailReason::GuardKept(r"C:\t\.g".into())),
            ],
            bytes_freed: 1,
            bytes_in_bin: 2,
        };
        let done = BinEmptied {
            before: Some((1, 1)),
            after: Some((1, 1)),
            error: None,
        };
        let mut states = vec![
            DeleteFlow::Idle,
            DeleteFlow::Confirm(req(DeleteMode::Permanent, ConfirmContext::Cleanup)),
            DeleteFlow::ConfirmPermanent(PermanentOffer::new(vec![t(r"C:\a", 1)]), report()),
            DeleteFlow::Report(report()),
            DeleteFlow::ConfirmEmptyBin(report(), BinLoad::Known(None)),
            DeleteFlow::Emptied(report(), done),
            DeleteFlow::OutcomeUnknown(report(), vec![t(r"C:\t\sent", 1)]),
        ];
        let name = |f: &DeleteFlow| std::mem::discriminant(f);
        for flow in &mut states {
            let before = name(flow);
            for _ in 0..2 {
                let mut out = ctx.run_ui(egui::RawInput::default(), |ui| {
                    assert!(drive_flow(ui.ctx(), flow, &p, NO_OPS).is_none());
                });
                out.textures_delta.clear();
            }
            assert_eq!(before, name(flow));
        }
    }
}
