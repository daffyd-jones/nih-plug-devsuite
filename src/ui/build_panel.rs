use crate::build_system::{BuildOutputLine, BuildStatus, BuildSystem};
use eframe::egui;

pub fn show(ui: &mut egui::Ui, build_system: &BuildSystem) {
    ui.horizontal(|ui| {
        ui.label(egui::RichText::new("Build Output").strong().size(13.0));

        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            let (status_text, color) = match build_system.status {
                BuildStatus::Idle => ("Idle", egui::Color32::GRAY),
                BuildStatus::Building => ("Building...", egui::Color32::YELLOW),
                BuildStatus::Success => ("Success", egui::Color32::GREEN),
                BuildStatus::Failed => ("Failed", egui::Color32::RED),
            };
            ui.label(egui::RichText::new(status_text).color(color).size(12.0));
        });
    });

    ui.separator();

    egui::ScrollArea::vertical()
        .auto_shrink([false, false])
        .stick_to_bottom(true)
        .show(ui, |ui| {
            for line in &build_system.output_lines {
                show_output_line(ui, line);
            }
        });
}

fn show_output_line(ui: &mut egui::Ui, line: &BuildOutputLine) {
    let color = if line.is_error {
        egui::Color32::from_rgb(255, 100, 100)
    } else if line.text.starts_with("warning") || line.text.contains("warning") {
        egui::Color32::from_rgb(255, 200, 50)
    } else if line.text.starts_with('✓') {
        egui::Color32::from_rgb(100, 255, 100)
    } else {
        egui::Color32::from_rgb(200, 200, 200)
    };

    ui.label(
        egui::RichText::new(&line.text)
            .color(color)
            .font(egui::FontId::monospace(12.0)),
    );
}
