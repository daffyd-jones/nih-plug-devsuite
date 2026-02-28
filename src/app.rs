use crate::build_system::BuildSystem;
use crate::project::Project;
use crate::scaffolding::{scaffold_project, ScaffoldOptions};
use crate::ui;
use crate::ui::new_project_dialog::{NewProjectDialog, NewProjectResult};
use eframe::egui;

#[derive(Debug)]
pub enum AppAction {
    NewProject,
    OpenProject,
    SaveActiveFile,
    SaveAllFiles,
    Build,
}

pub struct PlaygroundApp {
    project: Option<Project>,
    build_system: BuildSystem,
    file_browser: ui::file_browser::FileBrowser,
    new_project_dialog: NewProjectDialog,
    left_panel_width: f32,
    build_panel_height: f32,
    error_log: Vec<String>,
}

impl PlaygroundApp {
    pub fn new() -> Self {
        Self {
            project: None,
            build_system: BuildSystem::new(),
            file_browser: ui::file_browser::FileBrowser::new(),
            new_project_dialog: NewProjectDialog::default(),
            left_panel_width: 300.0,
            build_panel_height: 200.0,
            error_log: Vec::new(),
        }
    }

    fn handle_action(&mut self, action: AppAction) {
        match action {
            AppAction::NewProject => {
                self.new_project_dialog.open();
            }
            AppAction::OpenProject => {
                if let Some(path) = rfd::FileDialog::new().pick_folder() {
                    match Project::open(path) {
                        Ok(project) => {
                            self.project = Some(project);
                            self.error_log.clear();
                        }
                        Err(e) => self.error_log.push(e),
                    }
                }
            }
            AppAction::SaveActiveFile => {
                if let Some(ref mut project) = self.project {
                    if let Err(e) = project.save_active_file() {
                        self.error_log.push(e);
                    }
                }
            }
            AppAction::SaveAllFiles => {
                if let Some(ref mut project) = self.project {
                    if let Err(e) = project.save_all_files() {
                        self.error_log.push(e);
                    }
                }
            }
            AppAction::Build => {
                if let Some(ref mut project) = self.project {
                    if let Err(e) = project.save_all_files() {
                        self.error_log.push(e);
                        return;
                    }
                    self.build_system.start_build(&project.config.path);
                }
            }
        }
    }

    fn handle_new_project_result(&mut self, result: NewProjectResult) {
        match result {
            NewProjectResult::None | NewProjectResult::Cancelled => {}
            NewProjectResult::Create {
                project_name,
                parent_dir,
            } => {
                let opts = ScaffoldOptions {
                    parent_dir,
                    project_name,
                };
                match scaffold_project(&opts) {
                    Ok(project_dir) => {
                        // Auto-open the newly created project
                        match Project::open(project_dir.clone()) {
                            Ok(mut project) => {
                                // Auto-open lib.rs as the first file
                                let lib_path = project_dir.join("src").join("lib.rs");
                                let _ = project.open_file(&lib_path);
                                self.project = Some(project);
                                self.error_log.clear();
                            }
                            Err(e) => self.error_log.push(e),
                        }
                    }
                    Err(e) => self.error_log.push(e),
                }
            }
        }
    }
}

impl eframe::App for PlaygroundApp {
    fn update(&mut self, ctx: &eframe::egui::Context, _frame: &mut eframe::Frame) {
        self.build_system.poll();

        if self.build_system.status == crate::build_system::BuildStatus::Building {
            ctx.request_repaint();
        }

        // Handle the new project dialog — must be called every frame
        let dialog_result = self.new_project_dialog.show(ctx);
        self.handle_new_project_result(dialog_result);

        let mut action: Option<AppAction> = None;

        if ctx.input(|i| i.modifiers.command && i.key_pressed(egui::Key::S)) {
            if ctx.input(|i| i.modifiers.shift) {
                action = Some(AppAction::SaveAllFiles);
            } else {
                action = Some(AppAction::SaveActiveFile);
            }
        }

        if ctx.input(|i| i.modifiers.command && i.key_pressed(egui::Key::B)) {
            action = Some(AppAction::Build);
        }

        egui::TopBottomPanel::top("top_bar").show(ctx, |ui| {
            let bar_action =
                ui::top_bar::show(ui, &self.build_system.status, self.project.is_some());
            if action.is_none() {
                action = bar_action;
            }
        });

        if let Some(act) = action {
            self.handle_action(act);
        }

        egui::TopBottomPanel::bottom("build_panel")
            .resizable(true)
            .default_height(self.build_panel_height)
            .min_height(60.0)
            .max_height(400.0)
            .show(ctx, |ui| {
                ui::build_panel::show(ui, &self.build_system);
            });

        egui::SidePanel::left("file_browser_panel")
            .resizable(true)
            .default_width(self.left_panel_width)
            .min_width(150.0)
            .max_width(500.0)
            .show(ctx, |ui| {
                if let Some(ref mut project) = self.project {
                    let project_path = project.config.path.clone();
                    if let Some(clicked_path) = self.file_browser.show(ui, &project_path) {
                        if let Err(e) = project.open_file(&clicked_path) {
                            self.error_log.push(e);
                        }
                    }
                } else {
                    ui.vertical_centered(|ui| {
                        ui.add_space(100.0);
                        if ui
                            .button(egui::RichText::new("New Project").size(16.0))
                            .clicked()
                        {
                            self.handle_action(AppAction::NewProject);
                        }
                        ui.add_space(8.0);
                        if ui
                            .button(egui::RichText::new("Open Project Folder").size(16.0))
                            .clicked()
                        {
                            self.handle_action(AppAction::OpenProject);
                        }
                    });
                }
            });

        egui::SidePanel::right("plugin_panel")
            .resizable(true)
            .default_width(400.0)
            .min_width(200.0)
            .show(ctx, |ui| {
                ui.centered_and_justified(|ui| {
                    ui.label(
                        egui::RichText::new("🔌 Plugin View\n(Phase 5+)")
                            .color(egui::Color32::from_rgb(120, 120, 120))
                            .size(20.0),
                    );
                });
            });

        egui::CentralPanel::default().show(ctx, |ui| {
            if let Some(ref mut project) = self.project {
                ui::code_editor::show(ui, project);
            } else {
                ui.centered_and_justified(|ui| {
                    ui.vertical_centered(|ui| {
                        ui.add_space(60.0);
                        ui.label(
                            egui::RichText::new("NIH-plug Playground")
                                .size(32.0)
                                .strong(),
                        );
                        ui.add_space(12.0);
                        ui.label(
                            egui::RichText::new(
                                "Create a new project or open an existing folder\n\
                                 Cmd/Ctrl+B to build",
                            )
                            .size(16.0)
                            .color(egui::Color32::GRAY),
                        );
                    });
                });
            }
        });

        if !self.error_log.is_empty() {
            egui::Window::new("Errors")
                .collapsible(true)
                .resizable(true)
                .show(ctx, |ui| {
                    for err in &self.error_log {
                        ui.label(egui::RichText::new(err).color(egui::Color32::RED));
                    }
                    if ui.button("Clear").clicked() {
                        self.error_log.clear();
                    }
                });
        }
    }
}
