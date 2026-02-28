use crate::audio_engine::{AudioEngine, AudioStatus};
use crate::build_system::BuildSystem;
use crate::midi_engine::MidiEngine;
use crate::project::Project;
use crate::scaffolding::{scaffold_project, ScaffoldOptions};
use crate::ui;
use crate::ui::midi_panel::{MidiSettingsPanel, PianoWidget};
use crate::ui::new_project_dialog::{NewProjectDialog, NewProjectResult};
use crate::ui::settings_panel::SettingsPanel;
use eframe::egui;

#[derive(Debug)]
pub enum AppAction {
    NewProject,
    OpenProject,
    SaveActiveFile,
    SaveAllFiles,
    Build,
    OpenAudioSettings,
    ToggleAudio,
    OpenMidiSettings,
    ToggleMidi,
}

pub struct PlaygroundApp {
    project: Option<Project>,
    build_system: BuildSystem,
    audio_engine: AudioEngine,
    midi_engine: MidiEngine,
    file_browser: ui::file_browser::FileBrowser,
    new_project_dialog: NewProjectDialog,
    settings_panel: SettingsPanel,
    midi_settings_panel: MidiSettingsPanel,
    piano: PianoWidget,
    left_panel_width: f32,
    build_panel_height: f32,
    error_log: Vec<String>,
}

impl PlaygroundApp {
    pub fn new() -> Self {
        Self {
            project: None,
            build_system: BuildSystem::new(),
            audio_engine: AudioEngine::new(),
            midi_engine: MidiEngine::new(),
            file_browser: ui::file_browser::FileBrowser::new(),
            new_project_dialog: NewProjectDialog::default(),
            settings_panel: SettingsPanel::default(),
            midi_settings_panel: MidiSettingsPanel::default(),
            piano: PianoWidget::default(),
            left_panel_width: 300.0,
            build_panel_height: 200.0,
            error_log: Vec::new(),
        }
    }

    fn handle_action(&mut self, action: AppAction) {
        match action {
            AppAction::NewProject => self.new_project_dialog.open(),
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
                if let Some(ref mut p) = self.project {
                    if let Err(e) = p.save_active_file() {
                        self.error_log.push(e);
                    }
                }
            }
            AppAction::SaveAllFiles => {
                if let Some(ref mut p) = self.project {
                    if let Err(e) = p.save_all_files() {
                        self.error_log.push(e);
                    }
                }
            }
            AppAction::Build => {
                if let Some(ref mut p) = self.project {
                    if let Err(e) = p.save_all_files() {
                        self.error_log.push(e);
                        return;
                    }
                    self.build_system.start_build(&p.config.path);
                }
            }
            AppAction::OpenAudioSettings => {
                self.settings_panel.open(&mut self.audio_engine);
            }
            AppAction::ToggleAudio => {
                if self.audio_engine.status == AudioStatus::Running {
                    self.audio_engine.stop();
                } else {
                    self.audio_engine.start();
                }
            }
            AppAction::OpenMidiSettings => {
                self.midi_settings_panel.open(&mut self.midi_engine);
            }
            AppAction::ToggleMidi => {
                use crate::midi_engine::MidiStatus;
                if self.midi_engine.status == MidiStatus::Disconnected {
                    self.midi_engine.connect();
                } else {
                    self.midi_engine.disconnect();
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
                    Ok(project_dir) => match Project::open(project_dir.clone()) {
                        Ok(mut project) => {
                            let lib_path = project_dir.join("src").join("lib.rs");
                            let _ = project.open_file(&lib_path);
                            self.project = Some(project);
                            self.error_log.clear();
                        }
                        Err(e) => self.error_log.push(e),
                    },
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

        // ── Modal dialogs ─────────────────────────────────────────────────
        let dialog_result = self.new_project_dialog.show(ctx);
        self.handle_new_project_result(dialog_result);
        self.settings_panel.show(ctx, &mut self.audio_engine);
        self.midi_settings_panel.show(ctx, &mut self.midi_engine);

        // ── Keyboard shortcuts ────────────────────────────────────────────
        let mut action: Option<AppAction> = None;

        if ctx.input(|i| i.modifiers.command && i.key_pressed(egui::Key::S)) {
            action = Some(if ctx.input(|i| i.modifiers.shift) {
                AppAction::SaveAllFiles
            } else {
                AppAction::SaveActiveFile
            });
        }
        if ctx.input(|i| i.modifiers.command && i.key_pressed(egui::Key::B)) {
            action = Some(AppAction::Build);
        }

        // ── Top bar ───────────────────────────────────────────────────────
        egui::TopBottomPanel::top("top_bar").show(ctx, |ui| {
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
                        .add_enabled(self.project.is_some(), egui::Button::new("Save File"))
                        .clicked()
                    {
                        action = Some(AppAction::SaveActiveFile);
                        ui.close();
                    }
                    if ui
                        .add_enabled(self.project.is_some(), egui::Button::new("Save All"))
                        .clicked()
                    {
                        action = Some(AppAction::SaveAllFiles);
                        ui.close();
                    }
                });

                ui.menu_button("Audio", |ui| {
                    let running = self.audio_engine.status == AudioStatus::Running;
                    if ui
                        .button(if running {
                            "⏹  Stop Pass-through"
                        } else {
                            "▶  Start Pass-through"
                        })
                        .clicked()
                    {
                        action = Some(AppAction::ToggleAudio);
                        ui.close();
                    }
                    ui.separator();
                    if ui.button("⚙  Audio Settings...").clicked() {
                        action = Some(AppAction::OpenAudioSettings);
                        ui.close();
                    }
                });

                ui.menu_button("MIDI", |ui| {
                    use crate::midi_engine::MidiStatus;
                    let connected = self.midi_engine.status != MidiStatus::Disconnected;
                    if ui
                        .button(if connected {
                            "⏹  Disconnect"
                        } else {
                            "▶  Connect"
                        })
                        .clicked()
                    {
                        action = Some(AppAction::ToggleMidi);
                        ui.close();
                    }
                    ui.separator();
                    if ui.button("⚙  MIDI Settings...").clicked() {
                        action = Some(AppAction::OpenMidiSettings);
                        ui.close();
                    }
                });

                ui.separator();

                // Build button
                let build_enabled = self.project.is_some()
                    && self.build_system.status != crate::build_system::BuildStatus::Building;
                let build_label = if build_enabled {
                    "▶ Build"
                } else {
                    "⏳ Building..."
                };
                if ui
                    .add_enabled(
                        build_enabled,
                        egui::Button::new(
                            egui::RichText::new(build_label)
                                .color(if build_enabled {
                                    egui::Color32::from_rgb(100, 255, 100)
                                } else {
                                    egui::Color32::GRAY
                                })
                                .size(16.0),
                        ),
                    )
                    .clicked()
                {
                    action = Some(AppAction::Build);
                }

                ui.separator();

                let (build_text, build_color) = match self.build_system.status {
                    crate::build_system::BuildStatus::Idle => ("Ready", egui::Color32::GRAY),
                    crate::build_system::BuildStatus::Building => {
                        ("Building...", egui::Color32::YELLOW)
                    }
                    crate::build_system::BuildStatus::Success => ("Build OK", egui::Color32::GREEN),
                    crate::build_system::BuildStatus::Failed => {
                        ("Build Failed", egui::Color32::RED)
                    }
                };
                ui.label(egui::RichText::new(build_text).color(build_color));

                // Status pills (right side)
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    // MIDI pill
                    use crate::midi_engine::MidiStatus;
                    let (midi_text, midi_color) = match &self.midi_engine.status {
                        MidiStatus::Disconnected => ("● MIDI Off", egui::Color32::DARK_GRAY),
                        MidiStatus::InputOnly => ("● MIDI In", egui::Color32::YELLOW),
                        MidiStatus::OutputOnly => ("● MIDI Out", egui::Color32::YELLOW),
                        MidiStatus::Connected => {
                            ("● MIDI On", egui::Color32::from_rgb(100, 220, 100))
                        }
                        MidiStatus::Error(_) => {
                            ("● MIDI Err", egui::Color32::from_rgb(255, 90, 90))
                        }
                    };
                    if ui
                        .add(
                            egui::Label::new(
                                egui::RichText::new(midi_text).color(midi_color).size(12.0),
                            )
                            .sense(egui::Sense::click()),
                        )
                        .on_hover_text("Click to open MIDI Settings")
                        .clicked()
                    {
                        action = Some(AppAction::OpenMidiSettings);
                    }

                    ui.separator();

                    // Audio pill
                    let (audio_text, audio_color) = match &self.audio_engine.status {
                        AudioStatus::Stopped => ("● Audio Off", egui::Color32::DARK_GRAY),
                        AudioStatus::Running => {
                            ("● Audio On", egui::Color32::from_rgb(100, 220, 100))
                        }
                        AudioStatus::Error(_) => {
                            ("● Audio Err", egui::Color32::from_rgb(255, 90, 90))
                        }
                    };
                    if ui
                        .add(
                            egui::Label::new(
                                egui::RichText::new(audio_text)
                                    .color(audio_color)
                                    .size(12.0),
                            )
                            .sense(egui::Sense::click()),
                        )
                        .on_hover_text("Click to open Audio Settings")
                        .clicked()
                    {
                        action = Some(AppAction::OpenAudioSettings);
                    }
                });
            });
        });

        if let Some(act) = action {
            self.handle_action(act);
        }

        // ── Panels ────────────────────────────────────────────────────────
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

        // Right panel — piano keyboard + MIDI monitor
        egui::SidePanel::right("piano_panel")
            .resizable(true)
            .default_width(700.0)
            .min_width(400.0)
            .show(ctx, |ui| {
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    ui.label(egui::RichText::new("🎹 Virtual Piano").strong().size(14.0));
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        use crate::midi_engine::MidiStatus;
                        if self.midi_engine.status == MidiStatus::Disconnected {
                            if ui.button("Connect MIDI").clicked() {
                                self.midi_engine.connect();
                            }
                        }
                    });
                });
                ui.separator();
                self.piano.show(ui, &self.midi_engine);
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
                                "Create or open a project to start editing\n\
                                 Use Audio/MIDI menus to configure engines",
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
