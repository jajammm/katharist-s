use std::path::{Path, PathBuf};

use eframe::CreationContext;
use katharistis_core::format::format_size;
use katharistis_core::protect::ProtectContext;
use katharistis_core::scan::ScanTree;
use katharistis_core::settings::{self, Settings, Theme};
use katharistis_core::win;

use crate::confirm::{self, DeleteFlow};
use crate::i18n::{self, l};
use crate::jobs::Job;
use crate::views::home::{self, HomeAction, HomeState};
use crate::{theme, views};

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
    pub tree: Option<ScanTree>,
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
    /// A close was refused and the job it waited for is still running.
    close_refused: bool,
}

impl App {
    pub fn new(cc: &CreationContext<'_>) -> Self {
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
                self.home.start_scan(ctx, root, excluded, &mut self.tree);
            }
            HomeAction::Delete(request) => {
                if self.flow.is_idle() {
                    self.flow = DeleteFlow::Confirm(request);
                }
            }
        }
    }

    /// Runs the delete flow on top of whatever screen is open and brings the app in
    /// line with what it did.
    fn run_flow(&mut self, ctx: &egui::Context) {
        let was_working = self.flow.is_working();
        if let Some(report) = confirm::show_flow(ctx, &mut self.flow, &self.protect) {
            if let Some(tree) = &mut self.tree {
                self.home.after_report(tree, &report);
            }
        }
        if self.flow.outcome_unknown() {
            self.home.mark_stale();
        }
        if was_working && !self.flow.is_working() {
            self.home.refresh_space(ctx);
            self.drive_job = Some(Job::spawn(ctx.clone(), system_drive));
        }
        self.guard_close(ctx);
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

    fn placeholder(ui: &mut egui::Ui, title: &str) {
        ui.heading(title);
        ui.add_space(8.0);
        ui.weak(l("Coming soon.", "Segera hadir."));
    }
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
        self.poll_drive();
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
                            self.tree.as_ref(),
                            self.settings.stale_days,
                            &self.protect,
                        );
                        if let Some(action) = action {
                            self.handle_home(&ctx, action);
                        }
                    }
                    Tool::Clean => Self::placeholder(ui, l("Cleanup", "Pembersihan")),
                    Tool::Apps => Self::placeholder(ui, l("Apps", "Aplikasi")),
                    Tool::Large => Self::placeholder(ui, l("Largest", "Terbesar")),
                    Tool::Stale => {
                        Self::placeholder(ui, l("Not changed in a while", "Lama Tak Diubah"))
                    }
                    Tool::Dupes => Self::placeholder(ui, l("Duplicates", "Duplikat")),
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
