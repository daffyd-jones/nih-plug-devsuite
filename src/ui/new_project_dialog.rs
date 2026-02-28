use eframe::egui;

#[derive(Default)]
pub struct NewProjectDialog {
    pub is_open: bool,
    project_name: String,
    parent_dir: Option<std::path::PathBuf>,
    error: Option<String>,
}

pub enum NewProjectResult {
    None,
    Cancelled,
    Create {
        project_name: String,
        parent_dir: std::path::PathBuf,
    },
}

impl NewProjectDialog {
    pub fn open(&mut self) {
        self.is_open = true;
        self.project_name.clear();
        self.error = None;
        // Default to user's home documents folder
        self.parent_dir = dirs::document_dir()
            .or_else(|| dirs::home_dir());
    }

    pub fn show(&mut self, ctx: &egui::Context) -> NewProjectResult {
        if !self.is_open {
            return NewProjectResult::None;
        }

        let mut result = NewProjectResult::None;
        let mut still_open = true;

        egui::Window::new("New Project")
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .fixed_size([400.0, 220.0])
            .open(&mut still_open)
            .show(ctx, |ui| {
                ui.add_space(8.0);

                ui.label("Project Name:");
                ui.add_space(4.0);

                let name_edit = ui.add(
                    egui::TextEdit::singleline(&mut self.project_name)
                        .hint_text("my_plugin")
                        .desired_width(f32::INFINITY),
                );

                // Auto-focus the name field when the dialog opens
                if !name_edit.has_focus() && self.project_name.is_empty() {
                    name_edit.request_focus();
                }

                ui.add_space(12.0);
                ui.label("Location:");
                ui.add_space(4.0);

                ui.horizontal(|ui| {
                    let dir_text = self
                        .parent_dir
                        .as_ref()
                        .and_then(|p| p.to_str())
                        .unwrap_or("Not selected");

                    ui.add(
                        egui::TextEdit::singleline(&mut dir_text.to_string())
                            .desired_width(300.0)
                            .interactive(false),
                    );

                    if ui.button("Browse...").clicked() {
                        if let Some(path) = rfd::FileDialog::new().pick_folder() {
                            self.parent_dir = Some(path);
                        }
                    }
                });

                // Preview full path
                if let Some(ref parent) = self.parent_dir {
                    if !self.project_name.is_empty() {
                        let preview = parent
                            .join(&self.project_name)
                            .display()
                            .to_string();
                        ui.add_space(4.0);
                        ui.label(
                            egui::RichText::new(format!("→ {}", preview))
                                .size(11.0)
                                .color(egui::Color32::GRAY),
                        );
                    }
                }

                // Error message
                if let Some(ref err) = self.error {
                    ui.add_space(4.0);
                    ui.label(
                        egui::RichText::new(err)
                            .color(egui::Color32::from_rgb(255, 100, 100)),
                    );
                }

                ui.add_space(12.0);
                ui.separator();
                ui.add_space(8.0);

                ui.horizontal(|ui| {
                    let can_create = !self.project_name.is_empty()
                        && self.parent_dir.is_some();

                    if ui
                        .add_enabled(can_create, egui::Button::new("Create Project"))
                        .clicked()
                        || (ctx.input(|i| i.key_pressed(egui::Key::Enter)) && can_create)
                    {
                        result = NewProjectResult::Create {
                            project_name: self.project_name.clone(),
                            parent_dir: self.parent_dir.clone().unwrap(),
                        };
                        self.is_open = false;
                    }

                    if ui.button("Cancel").clicked() {
                        result = NewProjectResult::Cancelled;
                        self.is_open = false;
                    }
                });
            });

        if !still_open {
            self.is_open = false;
            return NewProjectResult::Cancelled;
        }

        result
    }
}
