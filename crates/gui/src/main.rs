#![windows_subsystem = "windows"]

mod app;
mod confirm;
mod i18n;
mod jobs;
mod theme;
mod views;

fn main() -> eframe::Result {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("Katharistís")
            .with_inner_size([1000.0, 680.0])
            .with_min_inner_size([760.0, 520.0]),
        ..Default::default()
    };
    eframe::run_native(
        "Katharistís",
        options,
        Box::new(|cc| Ok(Box::new(app::App::new(cc)))),
    )
}
