// mod app;
// mod audio_engine;
// mod build_system;
// mod midi_engine;
// mod project;
// mod scaffolding;
// mod templates;
// mod ui;

// use app::PlaygroundApp;

// fn main() -> eframe::Result<()> {
//     let options = eframe::NativeOptions {
//         viewport: eframe::egui::ViewportBuilder::default()
//             .with_inner_size([1600.0, 900.0])
//             .with_title("NIH-plug Playground"),
//         ..Default::default()
//     };

//     eframe::run_native(
//         "NIH-plug Playground",
//         options,
//         Box::new(|cc| {
//             cc.egui_ctx.set_visuals(eframe::egui::Visuals::dark());
//             Ok(Box::new(PlaygroundApp::new()))
//         }),
//     )
// }

mod app;
mod audio_engine;
mod build_system;
mod midi_engine;
mod plugin_host;
mod project;
mod scaffolding;
mod templates;
mod ui;

use app::PlaygroundApp;

fn main() -> eframe::Result<()> {
    let options = eframe::NativeOptions {
        viewport: eframe::egui::ViewportBuilder::default()
            .with_inner_size([1600.0, 900.0])
            .with_title("NIH-plug Playground"),
        ..Default::default()
    };

    eframe::run_native(
        "NIH-plug Playground",
        options,
        Box::new(|cc| {
            cc.egui_ctx.set_visuals(eframe::egui::Visuals::dark());
            Ok(Box::new(PlaygroundApp::new()))
        }),
    )
}
