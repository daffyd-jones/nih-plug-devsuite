use crate::audio_engine::AudioStatus;
use crate::midi_engine::MidiEngine;
use crate::plugin_host::{HostStatus, PluginHost, PluginMode};
use crate::ui::midi_panel::PianoWidget;
use eframe::egui;
// use raw_window_handle::HasWindowHandle;

/// The right-side panel showing plugin controls, piano, and MIDI monitor.
pub struct PluginPanel {
    pub piano: PianoWidget,
    pub piano_visible: bool,
}

impl Default for PluginPanel {
    fn default() -> Self {
        Self {
            piano: PianoWidget::default(),
            piano_visible: true,
        }
    }
}

impl PluginPanel {
    pub fn show(
        &mut self,
        ui: &mut egui::Ui,
        // frame: &mut eframe::Frame,
        plugin_host: &mut PluginHost,
        midi_engine: &MidiEngine,
        audio_running: bool,
    ) {
        ui.add_space(6.0);

        // ── Plugin controls ──────────────────────────────────────────────
        ui.horizontal(|ui| {
            ui.label(egui::RichText::new("🔌 Plugin Host").strong().size(14.0));
        });
        ui.separator();

        // Status
        let (status_text, status_color) = match &plugin_host.status {
            HostStatus::Unloaded => ("No plugin loaded", egui::Color32::GRAY),
            HostStatus::Loaded => ("Loaded (inactive)", egui::Color32::YELLOW),
            HostStatus::Active => ("Active", egui::Color32::from_rgb(100, 220, 100)),
            HostStatus::Processing => ("Processing", egui::Color32::from_rgb(100, 255, 100)),
            HostStatus::Error(e) => {
                ui.label(
                    egui::RichText::new(format!("Error: {}", e))
                        .color(egui::Color32::RED)
                        .size(11.0),
                );
                ("Error", egui::Color32::RED)
            }
        };

        ui.horizontal(|ui| {
            ui.label(
                egui::RichText::new(format!("● {}", status_text))
                    .color(status_color)
                    .size(12.0),
            );
        });

        if let Some(name) = &plugin_host.plugin_name {
            ui.label(
                egui::RichText::new(format!("Plugin: {}", name))
                    .size(12.0)
                    .color(egui::Color32::LIGHT_GRAY),
            );
        }

        ui.add_space(4.0);

        // Mode selector
        ui.horizontal(|ui| {
            ui.label("Mode:");
            ui.selectable_value(
                &mut plugin_host.mode,
                PluginMode::Instrument,
                "🎹 Instrument",
            );
            ui.selectable_value(&mut plugin_host.mode, PluginMode::Effect, "🎛 Effect");
        });

        ui.add_space(4.0);

        // GUI button
        // if plugin_host.is_gui_open() {
        //     // Draw a titled frame around the plugin GUI area
        //     egui::Frame::none()
        //         .stroke(egui::Stroke::new(1.5, egui::Color32::from_rgb(80, 80, 100)))
        //         .rounding(4.0)
        //         .inner_margin(8.0)
        //         .show(ui, |ui| {
        //             // Header bar inside the frame
        //             ui.horizontal(|ui| {
        //                 ui.label(
        //                     egui::RichText::new(
        //                         plugin_host.plugin_name.as_deref().unwrap_or("Plugin GUI"),
        //                     )
        //                     .small()
        //                     .color(egui::Color32::from_rgb(160, 160, 180)),
        //                 );
        //                 ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
        //                     if ui.small_button("✕").clicked() {
        //                         plugin_host.close_gui();
        //                     }
        //                 });
        //             });

        //             ui.separator();

        //             // Reserve the space the plugin is drawing into
        //             let desired_size = egui::vec2(640.0, 480.0);
        //             let (rect, _) = ui.allocate_exact_size(desired_size, egui::Sense::hover());

        //             // Subtle background so the region is visually distinct
        //             ui.painter()
        //                 .rect_filled(rect, 0.0, egui::Color32::from_rgb(20, 20, 25));
        //         });
        // } else

        if plugin_host.is_loaded() {
            ui.horizontal(|ui| {
                if plugin_host.is_gui_open() {
                    if ui.button("Close GUI").clicked() {
                        plugin_host.close_gui();
                    }
                } else if ui.button("Open GUI").clicked() {
                    // No window handle plumbing — PluginHost creates its
                    // own native window.
                    if let Err(e) = plugin_host.open_gui() {
                        eprintln!("[plugin_panel] Failed to open GUI: {e}");
                    }
                }

                if ui
                    .button("⏏ Unload")
                    .on_hover_text("Unload the plugin")
                    .clicked()
                {
                    plugin_host.unload();
                }
            });
        }

        ui.separator();

        // ── MIDI routing info ────────────────────────────────────────────
        if plugin_host.is_loaded() {
            ui.label(
                egui::RichText::new("MIDI → Plugin")
                    .size(11.0)
                    .color(egui::Color32::from_rgb(180, 180, 255)),
            );
        } else {
            ui.label(
                egui::RichText::new("MIDI → Direct Output")
                    .size(11.0)
                    .color(egui::Color32::GRAY),
            );
        }

        ui.add_space(4.0);

        // ── Collapsible piano ────────────────────────────────────────────
        ui.horizontal(|ui| {
            if ui
                .selectable_label(self.piano_visible, "🎹 Virtual Piano")
                .clicked()
            {
                self.piano_visible = !self.piano_visible;
            }
        });

        if self.piano_visible {
            self.piano.show(ui, midi_engine, plugin_host);
        }
    }
}
