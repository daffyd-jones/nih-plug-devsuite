use crate::app::AppAction;
use crate::build_system::BuildStatus;
use eframe::egui;

pub fn show(ui: &mut egui::Ui, build_status: &BuildStatus, has_project: bool) -> Option<AppAction> {
    let mut action = None;

    egui::MenuBar::new().ui(ui, |ui| {
        ui.menu_button("File", |ui| {
            if ui.button("New Project...").clicked() {
                action = Some(AppAction::NewProject);
                ui.close();
            }
            if ui.button("Open Project Folder...").clicked() {
                action = Some(AppAction::OpenProject);
                ui.close();
            }
            ui.separator();
            if ui
                .add_enabled(has_project, egui::Button::new("Save File"))
                .clicked()
            {
                action = Some(AppAction::SaveActiveFile);
                ui.close();
            }
            if ui
                .add_enabled(has_project, egui::Button::new("Save All"))
                .clicked()
            {
                action = Some(AppAction::SaveAllFiles);
                ui.close();
            }
        });

        ui.separator();

        let build_enabled = has_project && *build_status != BuildStatus::Building;
        let build_label = match build_status {
            BuildStatus::Building => "⏳ Building...",
            _ => "▶ Run",
        };

        let build_button = egui::Button::new(
            egui::RichText::new(build_label)
                .color(if build_enabled {
                    egui::Color32::from_rgb(100, 255, 100)
                } else {
                    egui::Color32::GRAY
                })
                .size(16.0),
        );

        if ui.add_enabled(build_enabled, build_button).clicked() {
            action = Some(AppAction::Build);
        }

        ui.separator();

        let (status_text, status_color) = match build_status {
            BuildStatus::Idle => ("Ready", egui::Color32::GRAY),
            BuildStatus::Building => ("Building...", egui::Color32::YELLOW),
            BuildStatus::Success => ("Build OK", egui::Color32::GREEN),
            BuildStatus::Failed => ("Build Failed", egui::Color32::RED),
        };
        ui.label(egui::RichText::new(status_text).color(status_color));
    });

    action
}

