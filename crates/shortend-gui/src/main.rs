#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]
//! `shortend-gui`: the desktop app.

fn main() -> eframe::Result {
    let options = eframe::NativeOptions {
        viewport: eframe::egui::ViewportBuilder::default()
            .with_title("ShortEnd")
            .with_inner_size([980.0, 760.0])
            .with_min_inner_size([760.0, 520.0])
            .with_drag_and_drop(true),
        ..Default::default()
    };
    eframe::run_native(
        "ShortEnd",
        options,
        Box::new(|_cc| Ok(Box::new(shortend_gui::ShortEndApp::new()))),
    )
}
