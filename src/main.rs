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
use eframe::egui;
use egui::{FontData, FontDefinitions, FontFamily};
// use winit::platform::x11::EventLoopBuilderExtX11;

pub fn setup_fonts(ctx: &egui::Context) {
    let mut fonts = FontDefinitions::default();

    // Load your NerdFont (e.g., JetBrainsMono Nerd Font)
    // The font file must be included at compile time or loaded at runtime
    fonts.font_data.insert(
        "JetBrainsMono Nerd Font".to_owned(),
        std::sync::Arc::new(FontData::from_static(include_bytes!(
            "../assets/fonts/JetBrainsMonoNerdFont-Regular.ttf"
        ))),
    );

    // Add as highest priority for Proportional (UI text)
    fonts
        .families
        .entry(FontFamily::Proportional)
        .or_default()
        .insert(0, "JetBrainsMono Nerd Font".to_owned());

    // Add as highest priority for Monospace (code blocks, terminals)
    fonts
        .families
        .entry(FontFamily::Monospace)
        .or_default()
        .insert(0, "JetBrainsMono Nerd Font".to_owned());

    ctx.set_fonts(fonts);
}

fn main() -> eframe::Result<()> {
    // let options = eframe::NativeOptions {
    //     #[cfg(target_os = "linux")]
    //     event_loop_builder: Some(Box::new(|builder| {
    //         use winit::platform::x11::EventLoopBuilderExtX11;
    //         builder.with_x11();
    //     })),
    //     viewport: eframe::egui::ViewportBuilder::default()
    //         .with_inner_size([1600.0, 900.0])
    //         .with_title("NIH-plug Playground"),
    //     ..Default::default()
    // };
    let options = eframe::NativeOptions {
        // Force X11 on Linux — Wayland has no window reparenting, which
        // CLAP's embedded GUI model requires. On Windows/macOS this
        // field is simply omitted.
        #[cfg(target_os = "linux")]
        event_loop_builder: Some(Box::new(|builder| {
            use winit::platform::x11::EventLoopBuilderExtX11;
            builder.with_x11();
        })),
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
            setup_fonts(&cc.egui_ctx);
            Ok(Box::new(PlaygroundApp::new()))
        }),
    )
}
