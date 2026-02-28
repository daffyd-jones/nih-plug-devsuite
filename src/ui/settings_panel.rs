use crate::audio_engine::{AudioEngine, AudioStatus, BUFFER_SIZE_OPTIONS, COMMON_SAMPLE_RATES};
use eframe::egui;

pub struct SettingsPanel {
    pub is_open: bool,
}

impl Default for SettingsPanel {
    fn default() -> Self {
        Self { is_open: false }
    }
}

impl SettingsPanel {
    pub fn open(&mut self, engine: &mut AudioEngine) {
        engine.refresh_devices();
        self.is_open = true;
    }

    /// Call every frame. Returns `true` if the user pressed Start/Stop so the
    /// caller can react if needed.
    pub fn show(&mut self, ctx: &egui::Context, engine: &mut AudioEngine) {
        if !self.is_open {
            return;
        }

        egui::Window::new("⚙  Audio Settings")
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .min_width(460.0)
            .open(&mut self.is_open)
            .show(ctx, |ui| {
                let running = engine.status == AudioStatus::Running;

                // ── Device selection ──────────────────────────────────────
                egui::Grid::new("audio_settings_grid")
                    .num_columns(2)
                    .spacing([12.0, 8.0])
                    .show(ui, |ui| {
                        // Input device
                        ui.label("Input device:");
                        egui::ComboBox::from_id_salt("in_dev")
                            .width(280.0)
                            .selected_text(&engine.input_device_names[engine.selected_input_idx])
                            .show_ui(ui, |ui| {
                                for (i, name) in engine.input_device_names.iter().enumerate() {
                                    ui.selectable_value(&mut engine.selected_input_idx, i, name);
                                }
                            });
                        ui.end_row();

                        // Output device
                        ui.label("Output device:");
                        egui::ComboBox::from_id_salt("out_dev")
                            .width(280.0)
                            .selected_text(&engine.output_device_names[engine.selected_output_idx])
                            .show_ui(ui, |ui| {
                                for (i, name) in engine.output_device_names.iter().enumerate() {
                                    ui.selectable_value(&mut engine.selected_output_idx, i, name);
                                }
                            });
                        ui.end_row();

                        // Sample rate
                        ui.label("Sample rate:");
                        egui::ComboBox::from_id_salt("sr")
                            .width(280.0)
                            .selected_text(format!(
                                "{} Hz",
                                COMMON_SAMPLE_RATES[engine.selected_sample_rate_idx]
                            ))
                            .show_ui(ui, |ui| {
                                for (i, &sr) in COMMON_SAMPLE_RATES.iter().enumerate() {
                                    ui.selectable_value(
                                        &mut engine.selected_sample_rate_idx,
                                        i,
                                        format!("{sr} Hz"),
                                    );
                                }
                            });
                        ui.end_row();

                        // Buffer size
                        ui.label("Buffer size:");
                        egui::ComboBox::from_id_salt("buf")
                            .width(280.0)
                            .selected_text(BUFFER_SIZE_OPTIONS[engine.selected_buffer_size_idx].0)
                            .show_ui(ui, |ui| {
                                for (i, (label, _)) in BUFFER_SIZE_OPTIONS.iter().enumerate() {
                                    ui.selectable_value(
                                        &mut engine.selected_buffer_size_idx,
                                        i,
                                        *label,
                                    );
                                }
                            });
                        ui.end_row();
                    });

                ui.separator();

                // ── Status banner ─────────────────────────────────────────
                match &engine.status {
                    AudioStatus::Stopped => {
                        ui.label(
                            egui::RichText::new("● Stopped")
                                .color(egui::Color32::GRAY)
                                .size(13.0),
                        );
                    }
                    AudioStatus::Running => {
                        if let Some(info) = &engine.running_info {
                            ui.label(
                                egui::RichText::new(format!(
                                    "● Running  │  {} Hz  │  {} samples  │  {} ch",
                                    info.sample_rate, info.buffer_size, info.channels
                                ))
                                .color(egui::Color32::from_rgb(100, 220, 100))
                                .size(13.0),
                            );
                        }
                    }
                    AudioStatus::Error(e) => {
                        ui.label(
                            egui::RichText::new(format!("✗  {e}"))
                                .color(egui::Color32::from_rgb(255, 90, 90))
                                .size(13.0),
                        );
                    }
                }

                ui.add_space(6.0);

                // ── Start / Stop ──────────────────────────────────────────
                ui.horizontal(|ui| {
                    if running {
                        if ui
                            .add(egui::Button::new(
                                egui::RichText::new("⏹  Stop Audio")
                                    .color(egui::Color32::from_rgb(255, 100, 100)),
                            ))
                            .clicked()
                        {
                            engine.stop();
                        }
                    } else {
                        if ui
                            .add(egui::Button::new(
                                egui::RichText::new("▶  Start Audio")
                                    .color(egui::Color32::from_rgb(100, 220, 100)),
                            ))
                            .clicked()
                        {
                            engine.start();
                        }
                    }

                    if ui.button("↺  Refresh Devices").clicked() {
                        engine.refresh_devices();
                    }
                });
            });
    }
}

