// Release builds on Windows open only the app window, without a console.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod system_fonts;
mod track_drag;
mod ui_style;

fn main() -> eframe::Result {
    if let Err(message) = system_fonts::load() {
        rfd::MessageDialog::new()
            .set_title("LoudTrace could not load system fonts")
            .set_description(&message)
            .set_level(rfd::MessageLevel::Error)
            .show();
        return Err(eframe::Error::AppCreation(message.into()));
    }
    let options = eframe::NativeOptions {
        viewport: eframe::egui::ViewportBuilder::default()
            .with_title("LoudTrace")
            .with_inner_size([1120.0, 760.0])
            .with_min_inner_size([900.0, 600.0])
            .with_icon(ui_style::app_icon()),
        ..Default::default()
    };
    eframe::run_native(
        "LoudTrace",
        options,
        Box::new(|cc| Ok(Box::new(app::LoudTraceApp::new(cc)?))),
    )
}
