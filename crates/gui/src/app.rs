use std::path::{Path, PathBuf};
use std::sync::Arc;

use eframe::CreationContext;
use katharistis_core::format::format_size;
use katharistis_core::protect::ProtectContext;
use katharistis_core::scan::ScanTree;
use katharistis_core::settings::{self, Settings, Theme};
use katharistis_core::win;

use crate::confirm::{self, ConfirmContext, ConfirmRequest, DeleteFlow, Guards};
use crate::i18n::{self, l};
use crate::jobs::Job;
use crate::views::apps::{self, AppsState};
use crate::views::clean::{self, CleanState};
use crate::views::dupes::{self, DupesState};
use crate::views::home::{self, HomeAction, HomeState};
use crate::views::large::{self, LargeState};
use crate::views::stale::{self, StaleState};
use crate::{fonts, theme, views};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tool {
    Home,
    Clean,
    Apps,
    Large,
    Stale,
    Dupes,
    Settings,
}

pub struct App {
    pub tool: Tool,
    pub settings: Settings,
    /// Shared with the jobs that read it (the duplicate search, the list builds), so
    /// none of them has to copy it. A delete changes it only when no job holds it.
    pub tree: Option<Arc<ScanTree>>,
    settings_path: PathBuf,
    save_error: Option<String>,
    new_excluded: String,
    /// `(drive label, total bytes, free bytes)` of the system drive.
    drive: Option<(String, u64, u64)>,
    drive_job: Option<Job<Option<(String, u64, u64)>>>,
    /// Read once: every delete and every row of the list goes by this one.
    protect: ProtectContext,
    home: HomeState,
    flow: DeleteFlow,
    /// The context of the request that started `flow`, which the report depends on.
    flow_context: ConfirmContext,
    /// The duplicate groups of the request that started `flow`, checked again by its
    /// worker right before it deletes.
    flow_guards: Guards,
    /// Reports whose tree update waits for a job to let go of the tree: `(epoch, report)`.
    pending_reports: Vec<(u64, katharistis_core::delete::DeleteReport)>,
    clean: CleanState,
    apps: AppsState,
    large: LargeState,
    stale: StaleState,
    dupes: DupesState,
    /// A close was refused and the job it waited for is still running.
    close_refused: bool,
}

impl App {
    pub fn new(cc: &CreationContext<'_>) -> Self {
        cc.egui_ctx.set_fonts(fonts::with_system_fallbacks());
        let settings_path = settings::default_path();
        let settings = settings::load_from(&settings_path);
        let protect = ProtectContext::from_env();
        let home = HomeState::new(&cc.egui_ctx, &protect);
        let app = App {
            tool: Tool::Home,
            settings,
            tree: None,
            settings_path,
            save_error: None,
            new_excluded: String::new(),
            drive: system_drive(),
            drive_job: None,
            protect,
            home,
            flow: DeleteFlow::Idle,
            flow_context: ConfirmContext::User,
            flow_guards: Vec::new(),
            pending_reports: Vec::new(),
            clean: CleanState::default(),
            apps: AppsState::default(),
            large: LargeState::default(),
            stale: StaleState::default(),
            dupes: DupesState::default(),
            close_refused: false,
        };
        app.apply_language();
        app.apply_theme(&cc.egui_ctx);
        app
    }

    /// Folders every GUI scan must skip.
    pub fn excluded(&self) -> Vec<PathBuf> {
        self.settings.excluded.clone()
    }

    fn handle_home(&mut self, ctx: &egui::Context, action: HomeAction) {
        match action {
            HomeAction::Scan(root) => {
                let excluded = self.excluded();
                begin_scan(
                    ctx,
                    &mut self.home,
                    &mut self.dupes,
                    root,
                    excluded,
                    &mut self.tree,
                );
            }
            HomeAction::Delete(request) => self.start_flow(ctx, request),
        }
    }

    /// Opens the confirmation. Only one flow runs at a time.
    fn start_flow(&mut self, ctx: &egui::Context, mut request: ConfirmRequest) {
        if self.flow.is_idle() {
            self.flow_context = request.context;
            self.flow_guards = std::mem::take(&mut request.guards);
            self.flow = confirm::open_request(ctx, request);
        }
    }

    /// Runs the delete flow on top of whatever screen is open and brings the app in
    /// line with what it did.
    fn run_flow(&mut self, ctx: &egui::Context) {
        let was_working = self.flow.is_working();
        if let Some(report) = confirm::show_flow(
            ctx,
            &mut self.flow,
            &self.protect,
            self.flow_context,
            &self.flow_guards,
        ) {
            self.dupes.after_report(&report);
            if self.flow_context == ConfirmContext::Leftovers {
                self.apps.after_report(&report);
            }
            self.pending_reports.push((self.home.epoch(), report));
        }
        self.apply_pending(ctx);
        if self.flow.outcome_unknown() {
            self.home.mark_stale();
        }
        if was_working && !self.flow.is_working() {
            self.home.refresh_space(ctx);
            self.drive_job = Some(Job::spawn(ctx.clone(), system_drive));
            if self.flow_context == ConfirmContext::Cleanup {
                let excluded = self.excluded();
                self.clean.start_scan(ctx, &excluded);
            }
            self.clean.refresh_bin(ctx);
        }
        self.guard_close(ctx);
    }

    /// Puts finished deletes into the tree. A job reading the tree (a duplicate
    /// search, a list build) holds a share of it, and changing it then would copy it
    /// on the UI thread. So the search is cancelled and the update waits for the job to
    /// let go, a few frames at most.
    fn apply_pending(&mut self, ctx: &egui::Context) {
        let epoch = self.home.epoch();
        self.pending_reports.retain(|(e, _)| *e == epoch);
        if self.pending_reports.is_empty() {
            return;
        }
        let Some(tree) = self.tree.as_mut() else {
            self.pending_reports.clear();
            return;
        };
        let Some(tree) = Arc::get_mut(tree) else {
            self.dupes.cancel_search();
            ctx.request_repaint();
            return;
        };
        for (_, report) in self.pending_reports.drain(..) {
            self.home.after_report(tree, &report);
        }
    }

    /// Keeps the window open while a job that can change the disk runs.
    fn guard_close(&mut self, ctx: &egui::Context) {
        let requested = ctx.input(|i| i.viewport().close_requested());
        if confirm::refuse_close(requested, &self.flow) {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            self.close_refused = true;
        }
        self.close_refused &= self.flow.is_working();
        if self.close_refused {
            egui::Area::new(egui::Id::unique("close-refused"))
                .order(egui::Order::Tooltip)
                .anchor(egui::Align2::CENTER_TOP, egui::vec2(0.0, 16.0))
                .interactable(false)
                .show(ctx, |ui| {
                    egui::Frame::popup(ui.style()).show(ui, |ui| {
                        ui.set_max_width(460.0);
                        ui.label(confirm::close_refused_text());
                    });
                });
        }
    }

    fn poll_drive(&mut self) {
        if let Some(outcome) = self.drive_job.as_mut().and_then(Job::poll) {
            self.drive_job = None;
            if let Ok(drive) = outcome {
                self.drive = drive;
            }
        }
    }

    fn apply_language(&self) {
        i18n::set_language(self.settings.language);
    }

    fn apply_theme(&self, ctx: &egui::Context) {
        ctx.set_visuals_of(egui::Theme::Dark, theme::visuals(&theme::DARK, true));
        ctx.set_visuals_of(egui::Theme::Light, theme::visuals(&theme::LIGHT, false));
        ctx.set_theme(match self.settings.theme {
            Theme::System => egui::ThemePreference::System,
            Theme::Light => egui::ThemePreference::Light,
            Theme::Dark => egui::ThemePreference::Dark,
        });
    }

    fn save(&mut self) {
        self.save_error = settings::save_to(&self.settings_path, &self.settings)
            .err()
            .map(|e| e.to_string());
    }

    fn nav(&mut self, ui: &mut egui::Ui, tool: Tool, label: &str) {
        let selected = self.tool == tool;
        let mut text = egui::RichText::new(label);
        if selected {
            text = text.color(ui.visuals().selection.stroke.color);
        }
        if ui.selectable_label(selected, text).clicked() {
            self.tool = tool;
        }
    }

    fn sidebar(&mut self, ui: &mut egui::Ui) {
        ui.add_space(8.0);
        ui.heading("Katharistís");
        ui.add_space(12.0);
        ui.weak(l("Tools", "Alat"));
        self.nav(ui, Tool::Home, l("Home", "Beranda"));
        self.nav(ui, Tool::Clean, l("Cleanup", "Pembersihan"));
        self.nav(ui, Tool::Apps, l("Apps", "Aplikasi"));
        ui.add_space(12.0);
        ui.weak(l("Analysis", "Analisis"));
        self.nav(ui, Tool::Large, l("Largest", "Terbesar"));
        self.nav(
            ui,
            Tool::Stale,
            l("Not changed in a while", "Lama Tak Diubah"),
        );
        self.nav(ui, Tool::Dupes, l("Duplicates", "Duplikat"));
        ui.with_layout(egui::Layout::bottom_up(egui::Align::Min), |ui| {
            ui.add_space(8.0);
            self.nav(ui, Tool::Settings, l("Settings", "Pengaturan"));
            if let Some((name, total, free)) = &self.drive {
                ui.add_space(8.0);
                let used = total.saturating_sub(*free);
                let frac = if *total > 0 {
                    used as f32 / *total as f32
                } else {
                    0.0
                };
                ui.add(egui::ProgressBar::new(frac).desired_height(6.0));
                ui.weak(format!("{} {}", format_size(*free), l("free", "tersisa")));
                ui.label(name.as_str());
            }
        });
    }
}

/// A new scan makes the old tree worthless, so a search still hashing on it stops now.
/// The Duplicates screen would only notice when it is opened again.
fn begin_scan(
    ctx: &egui::Context,
    home: &mut HomeState,
    dupes: &mut DupesState,
    root: PathBuf,
    excluded: Vec<PathBuf>,
    tree: &mut Option<Arc<ScanTree>>,
) {
    dupes.cancel_search();
    home.start_scan(ctx, root, excluded, tree);
}

fn system_drive() -> Option<(String, u64, u64)> {
    let name = std::env::var("SystemDrive").unwrap_or_else(|_| "C:".into());
    let root = format!("{name}\\");
    let (total, free) = win::volume_space(Path::new(&root)).ok()?;
    Some((name, total, free))
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let before = (self.settings.language, self.settings.theme);
        let ctx = ui.ctx().clone();
        self.home.poll(&ctx, &mut self.tree);
        self.apply_pending(&ctx);
        if self.tool != Tool::Dupes {
            self.dupes.leave();
        }
        self.poll_drive();
        self.clean.poll();
        self.apps.poll(&ctx);
        if self.apps.take_marks_dirty() {
            self.home.refresh_marks(&ctx, &self.protect);
        }
        egui::Panel::left("nav")
            .exact_size(210.0)
            .resizable(false)
            .show(ui, |ui| self.sidebar(ui));
        egui::CentralPanel::default().show(ui, |ui| {
            egui::Frame::new().inner_margin(16.0).show(ui, |ui| {
                if let Some(err) = &self.save_error {
                    ui.colored_label(
                        ui.visuals().error_fg_color,
                        format!(
                            "{} {err}",
                            l("Could not save settings:", "Gagal menyimpan pengaturan:")
                        ),
                    );
                    ui.add_space(8.0);
                }
                match self.tool {
                    Tool::Home => {
                        let action = home::show(
                            ui,
                            &mut self.home,
                            self.tree.as_deref(),
                            self.settings.stale_days,
                            &self.protect,
                        );
                        if let Some(action) = action {
                            self.handle_home(&ctx, action);
                        }
                    }
                    Tool::Clean => {
                        let busy = !self.flow.is_idle();
                        let excluded = self.excluded();
                        if let Some(request) = clean::show(ui, &mut self.clean, busy, &excluded) {
                            self.start_flow(&ctx, request);
                        }
                    }
                    Tool::Apps => {
                        let busy = !self.flow.is_idle();
                        if let Some(request) = apps::show(ui, &mut self.apps, busy) {
                            self.start_flow(&ctx, request);
                        }
                    }
                    Tool::Large => {
                        let action = large::show(
                            ui,
                            &mut self.large,
                            self.tree.as_ref(),
                            &self.home,
                            &self.protect,
                        );
                        if let Some(action) = action {
                            self.handle_home(&ctx, action);
                        }
                    }
                    Tool::Stale => {
                        let action = stale::show(
                            ui,
                            &mut self.stale,
                            self.tree.as_ref(),
                            &self.home,
                            &self.protect,
                        );
                        if let Some(action) = action {
                            self.handle_home(&ctx, action);
                        }
                    }
                    Tool::Dupes => {
                        let busy = !self.flow.is_idle();
                        let action = dupes::show(
                            ui,
                            &mut self.dupes,
                            self.tree.as_ref(),
                            &self.home,
                            &self.protect,
                            busy,
                        );
                        if let Some(action) = action {
                            self.handle_home(&ctx, action);
                        }
                    }
                    Tool::Settings => {
                        if views::settings::show(ui, &mut self.settings, &mut self.new_excluded) {
                            self.save();
                        }
                    }
                }
            });
        });
        self.run_flow(&ctx);
        if before != (self.settings.language, self.settings.theme) {
            self.apply_language();
            self.apply_theme(ui.ctx());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use katharistis_core::scan::ScanProgress;

    #[test]
    fn a_new_scan_stops_a_search_on_the_old_tree() {
        let ctx = egui::Context::default();
        let dir = tempfile::tempdir().unwrap();
        let protect = ProtectContext::from_env();
        let mut home = HomeState::new(&ctx, &protect);
        let progress = ScanProgress::default();
        let mut dupes = DupesState::searching(&ctx, progress.clone());
        let mut tree = None;
        begin_scan(
            &ctx,
            &mut home,
            &mut dupes,
            dir.path().to_path_buf(),
            Vec::new(),
            &mut tree,
        );
        assert!(progress.is_cancelled());
    }
}
