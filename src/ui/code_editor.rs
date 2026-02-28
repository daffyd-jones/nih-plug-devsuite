use crate::project::Project;
use eframe::egui;
use egui_code_editor::{CodeEditor, ColorTheme, Syntax};

pub fn show(ui: &mut egui::Ui, project: &mut Project) {
    if !project.open_files.is_empty() {
        ui.horizontal(|ui| {
            let mut close_idx: Option<usize> = None;
            let mut switch_idx: Option<usize> = None;

            for (idx, file) in project.open_files.iter().enumerate() {
                let is_active = project.active_file_index == Some(idx);

                let filename = file
                    .path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or("untitled");

                let label = if file.modified {
                    format!("● {}", filename)
                } else {
                    filename.to_string()
                };

                ui.horizontal(|ui| {
                    if ui.selectable_label(is_active, &label).clicked() {
                        switch_idx = Some(idx);
                    }
                    if ui.small_button("×").clicked() {
                        close_idx = Some(idx);
                    }
                });

                ui.separator();
            }

            if let Some(idx) = switch_idx {
                project.active_file_index = Some(idx);
            }
            if let Some(idx) = close_idx {
                project.close_file(idx);
            }
        });

        ui.separator();
    }

    if let Some(active_idx) = project.active_file_index {
        let file = &mut project.open_files[active_idx];
        let syntax = detect_syntax(&file.path);

        egui::ScrollArea::both()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                let previous_content = file.content.clone();

                CodeEditor::default()
                    .id_source("code_editor")
                    .with_rows(50)
                    .with_fontsize(14.0)
                    .with_theme(ColorTheme::GRUVBOX)
                    .with_syntax(syntax)
                    .with_numlines(true)
                    .show(ui, &mut file.content);

                if file.content != previous_content {
                    file.modified = true;
                }
            });
    } else {
        ui.centered_and_justified(|ui| {
            ui.label(
                egui::RichText::new("Open a file from the browser to start editing")
                    .color(egui::Color32::GRAY)
                    .size(16.0),
            );
        });
    }
}

fn detect_syntax(path: &std::path::Path) -> Syntax {
    match path.extension().and_then(|e| e.to_str()) {
        Some("rs") => Syntax::rust(),
        Some("toml") => Syntax::default(),
        _ => Syntax::default(),
    }
}
