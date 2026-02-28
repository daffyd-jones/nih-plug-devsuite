use crate::midi_engine::{MidiEngine, MidiMessageKind, MidiStatus};
use eframe::egui;

// ── Settings window ───────────────────────────────────────────────────────────

pub struct MidiSettingsPanel {
    pub is_open: bool,
}

impl Default for MidiSettingsPanel {
    fn default() -> Self {
        Self { is_open: false }
    }
}

impl MidiSettingsPanel {
    pub fn open(&mut self, engine: &mut MidiEngine) {
        engine.refresh_ports();
        self.is_open = true;
    }

    pub fn show(&mut self, ctx: &egui::Context, engine: &mut MidiEngine) {
        if !self.is_open {
            return;
        }

        egui::Window::new("🎹  MIDI Settings")
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .min_width(480.0)
            .open(&mut self.is_open)
            .show(ctx, |ui| {
                let connected = !matches!(
                    engine.status,
                    MidiStatus::Disconnected | MidiStatus::Error(_)
                );

                egui::Grid::new("midi_settings_grid")
                    .num_columns(2)
                    .spacing([12.0, 8.0])
                    .show(ui, |ui| {
                        // Input port
                        ui.label("Input port:");
                        let in_label = engine
                            .selected_input_idx
                            .and_then(|i| engine.input_port_names.get(i))
                            .map(|s| s.as_str())
                            .unwrap_or("None");

                        egui::ComboBox::from_id_salt("midi_in")
                            .width(300.0)
                            .selected_text(in_label)
                            .show_ui(ui, |ui| {
                                ui.selectable_value(&mut engine.selected_input_idx, None, "None");
                                for (i, name) in engine.input_port_names.iter().enumerate() {
                                    ui.selectable_value(
                                        &mut engine.selected_input_idx,
                                        Some(i),
                                        name,
                                    );
                                }
                            });
                        ui.end_row();

                        // Output port
                        ui.label("Output port:");
                        let out_label = engine
                            .selected_output_idx
                            .and_then(|i| engine.output_port_names.get(i))
                            .map(|s| s.as_str())
                            .unwrap_or("None");

                        egui::ComboBox::from_id_salt("midi_out")
                            .width(300.0)
                            .selected_text(out_label)
                            .show_ui(ui, |ui| {
                                ui.selectable_value(&mut engine.selected_output_idx, None, "None");
                                for (i, name) in engine.output_port_names.iter().enumerate() {
                                    ui.selectable_value(
                                        &mut engine.selected_output_idx,
                                        Some(i),
                                        name,
                                    );
                                }
                            });
                        ui.end_row();
                    });

                ui.separator();

                // Status banner
                let (status_text, status_color) = match &engine.status {
                    MidiStatus::Disconnected => ("● Disconnected", egui::Color32::DARK_GRAY),
                    MidiStatus::InputOnly => ("● Input only", egui::Color32::YELLOW),
                    MidiStatus::OutputOnly => ("● Output only", egui::Color32::YELLOW),
                    MidiStatus::Connected => {
                        ("● Connected", egui::Color32::from_rgb(100, 220, 100))
                    }
                    MidiStatus::Error(e) => {
                        let _ = e;
                        ("✗  Error", egui::Color32::from_rgb(255, 90, 90))
                    }
                };
                ui.label(
                    egui::RichText::new(status_text)
                        .color(status_color)
                        .size(13.0),
                );
                if let MidiStatus::Error(e) = &engine.status {
                    ui.label(
                        egui::RichText::new(e)
                            .color(egui::Color32::from_rgb(255, 90, 90))
                            .size(11.0),
                    );
                }

                ui.add_space(6.0);

                ui.horizontal(|ui| {
                    if connected {
                        if ui.button("⏹  Disconnect").clicked() {
                            engine.disconnect();
                        }
                    } else {
                        if ui.button("▶  Connect").clicked() {
                            engine.connect();
                        }
                    }
                    if ui.button("↺  Refresh Ports").clicked() {
                        engine.refresh_ports();
                    }
                });
            });
    }
}

// ── Piano keyboard widget ─────────────────────────────────────────────────────

/// Stateful piano widget. Tracks held keys so note-off is sent correctly.
pub struct PianoWidget {
    /// Notes currently held via mouse press (note number).
    held_notes: std::collections::HashSet<u8>,
    /// Which key the mouse is currently over (for hover highlight).
    hovered_note: Option<u8>,
    /// Starting MIDI note (C2 = 36 is a reasonable default).
    pub start_note: u8,
    /// Number of white keys to draw.
    pub white_key_count: usize,
    /// MIDI channel to use (0-indexed).
    pub channel: u8,
    /// Velocity for note-on messages.
    pub velocity: u8,
    // Recent events to display
    pub event_log: Vec<String>,
}

impl Default for PianoWidget {
    fn default() -> Self {
        Self {
            held_notes: std::collections::HashSet::new(),
            hovered_note: None,
            start_note: 36, // C2
            white_key_count: 28,
            channel: 0,
            velocity: 100,
            event_log: Vec::new(),
        }
    }
}

// Maps a white key index (from start note's octave) to semitone offset
const WHITE_OFFSETS: [u8; 7] = [0, 2, 4, 5, 7, 9, 11];
const BLACK_OFFSETS: [u8; 5] = [1, 3, 6, 8, 10];
const BLACK_POSITIONS: [usize; 5] = [0, 1, 3, 4, 5]; // white-key indices with a black key to their right

/// Returns the MIDI note number for the nth white key starting from `start_note`.
fn white_key_note(start: u8, white_idx: usize) -> u8 {
    // Snap start to the C of its octave
    let octave_offset = start % 12;
    let c_of_octave = start - octave_offset;
    let semitones = WHITE_OFFSETS[white_idx % 7] + (white_idx / 7) as u8 * 12;
    c_of_octave.saturating_add(semitones)
}

fn black_key_note(start: u8, white_idx: usize) -> Option<u8> {
    let pos_in_octave = white_idx % 7;
    if [0, 1, 3, 4, 5].contains(&pos_in_octave) {
        let octave_offset = start % 12;
        let c_of_octave = start - octave_offset;
        let semitones = WHITE_OFFSETS[pos_in_octave] + 1 + (white_idx / 7) as u8 * 12;
        Some(c_of_octave.saturating_add(semitones))
    } else {
        None
    }
}

fn note_name(note: u8) -> String {
    const NAMES: [&str; 12] = [
        "C", "C#", "D", "D#", "E", "F", "F#", "G", "G#", "A", "A#", "B",
    ];
    let octave = (note / 12) as i32 - 1;
    format!("{}{}", NAMES[(note % 12) as usize], octave)
}

impl PianoWidget {
    /// Draw the piano and return any MIDI messages that should be sent this frame.
    /// Call `engine.send_note_on/off` with the returned messages.
    pub fn show(&mut self, ui: &mut egui::Ui, midi: &MidiEngine) {
        // Drain incoming MIDI events into the log
        for event in midi.drain_events() {
            let desc = match &event.kind {
                MidiMessageKind::NoteOn {
                    channel,
                    note,
                    velocity,
                } => format!(
                    "NoteOn  ch:{} note:{} ({}) vel:{}",
                    channel + 1,
                    note,
                    note_name(*note),
                    velocity
                ),
                MidiMessageKind::NoteOff { channel, note } => format!(
                    "NoteOff ch:{} note:{} ({})",
                    channel + 1,
                    note,
                    note_name(*note)
                ),
                MidiMessageKind::ControlChange { channel, cc, value } => {
                    format!("CC      ch:{} cc:{} val:{}", channel + 1, cc, value)
                }
                MidiMessageKind::PitchBend { channel, value } => {
                    format!("PBend   ch:{} val:{}", channel + 1, value)
                }
                MidiMessageKind::Other(bytes) => format!("Other   {:?}", bytes),
            };
            self.event_log.push(desc);
            if self.event_log.len() > 64 {
                self.event_log.remove(0);
            }
        }

        ui.vertical(|ui| {
            self.draw_controls(ui);
            ui.add_space(6.0);
            self.draw_keyboard(ui, midi);
            ui.add_space(8.0);
            self.draw_event_log(ui);
        });
    }

    fn draw_controls(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.label("Octave start:");
            let mut octave = (self.start_note / 12) as i32 - 1;
            if ui.small_button("◀").clicked() && octave > -1 {
                octave -= 1;
                self.start_note = ((octave + 1) * 12) as u8;
            }
            ui.label(format!("C{}", octave));
            if ui.small_button("▶").clicked() && octave < 8 {
                octave += 1;
                self.start_note = ((octave + 1) * 12) as u8;
            }

            ui.separator();
            ui.label("Velocity:");
            // ui.add(egui::Slider::new(&mut self.velocity, 1..=127).clamp_to_range(true));
            ui.add(
                egui::Slider::new(&mut self.velocity, 1..=127)
                    .clamping(egui::SliderClamping::Always),
            );

            ui.separator();
            ui.label("Channel:");
            let mut ch_display = self.channel + 1;
            // ui.add(egui::Slider::new(&mut ch_display, 1..=16).clamp_to_range(true));
            ui.add(
                egui::Slider::new(&mut ch_display, 1..=16).clamping(egui::SliderClamping::Always),
            );
            self.channel = ch_display - 1;
        });
    }

    fn draw_keyboard(&mut self, ui: &mut egui::Ui, midi: &MidiEngine) {
        let white_w = 24.0_f32;
        let white_h = 80.0_f32;
        let black_w = 16.0_f32;
        let black_h = 50.0_f32;

        let total_width = white_w * self.white_key_count as f32;
        let (response, painter) = ui.allocate_painter(
            egui::vec2(total_width, white_h + 4.0),
            egui::Sense::click_and_drag(),
        );

        let origin = response.rect.min;
        let mouse_pos = response.hover_pos();

        // ── Determine hovered/pressed notes from pointer ──────────────────
        // We process black keys first (they sit on top).

        let mut newly_pressed: Option<u8> = None;
        let mut newly_released: Vec<u8> = Vec::new();

        // Release all held notes when mouse button is lifted
        if response.drag_stopped()
            || (!response.is_pointer_button_down_on() && !self.held_notes.is_empty())
        {
            for note in self.held_notes.drain() {
                midi.send_note_off(self.channel, note);
                newly_released.push(note);
            }
        }

        let mut hovered: Option<u8> = None;

        if let Some(pos) = mouse_pos {
            let local = pos - origin;
            // Check black keys first
            'outer: for wi in 0..self.white_key_count {
                if let Some(black_note) = black_key_note(self.start_note, wi) {
                    let bx = wi as f32 * white_w + white_w - black_w / 2.0;
                    let black_rect = egui::Rect::from_min_size(
                        egui::pos2(bx, 0.0),
                        egui::vec2(black_w, black_h),
                    );
                    if black_rect.contains(egui::pos2(local.x, local.y)) {
                        hovered = Some(black_note);
                        break 'outer;
                    }
                }
            }
            // Then white keys
            if hovered.is_none() {
                let wi = (local.x / white_w) as usize;
                if wi < self.white_key_count {
                    hovered = Some(white_key_note(self.start_note, wi));
                }
            }
        }

        self.hovered_note = hovered;

        // Press on click
        if response.is_pointer_button_down_on() {
            if let Some(note) = hovered {
                if !self.held_notes.contains(&note) {
                    self.held_notes.insert(note);
                    midi.send_note_on(self.channel, note, self.velocity);
                    newly_pressed = Some(note);
                }
                // Release notes no longer under mouse during drag
                let to_release: Vec<u8> = self
                    .held_notes
                    .iter()
                    .copied()
                    .filter(|&n| n != note)
                    .collect();
                for n in to_release {
                    self.held_notes.remove(&n);
                    midi.send_note_off(self.channel, n);
                }
            }
        }

        // Log newly pressed/released
        if let Some(n) = newly_pressed {
            let msg = format!(
                "► NoteOn  ch:{} {} vel:{}",
                self.channel + 1,
                note_name(n),
                self.velocity
            );
            self.event_log.push(msg);
            if self.event_log.len() > 64 {
                self.event_log.remove(0);
            }
        }
        for n in newly_released {
            let msg = format!("◀ NoteOff ch:{} {}", self.channel + 1, note_name(n));
            self.event_log.push(msg);
            if self.event_log.len() > 64 {
                self.event_log.remove(0);
            }
        }

        // ── Draw white keys ───────────────────────────────────────────────
        for wi in 0..self.white_key_count {
            let note = white_key_note(self.start_note, wi);
            let x = wi as f32 * white_w;
            let rect = egui::Rect::from_min_size(
                origin + egui::vec2(x, 0.0),
                egui::vec2(white_w - 1.0, white_h),
            );

            let color = if self.held_notes.contains(&note) {
                egui::Color32::from_rgb(100, 180, 255)
            } else if self.hovered_note == Some(note) {
                egui::Color32::from_rgb(220, 230, 255)
            } else {
                egui::Color32::WHITE
            };

            painter.rect(
                rect,
                2.0,
                color,
                egui::Stroke::new(1.0, egui::Color32::DARK_GRAY),
                egui::StrokeKind::Outside,
            );

            // Label C notes
            if note % 12 == 0 {
                painter.text(
                    rect.center_bottom() - egui::vec2(0.0, 6.0),
                    egui::Align2::CENTER_CENTER,
                    note_name(note),
                    egui::FontId::proportional(9.0),
                    egui::Color32::GRAY,
                );
            }
        }

        // ── Draw black keys ───────────────────────────────────────────────
        for wi in 0..self.white_key_count {
            if let Some(note) = black_key_note(self.start_note, wi) {
                let x = wi as f32 * white_w + white_w - black_w / 2.0;
                let rect = egui::Rect::from_min_size(
                    origin + egui::vec2(x, 0.0),
                    egui::vec2(black_w, black_h),
                );

                let color = if self.held_notes.contains(&note) {
                    egui::Color32::from_rgb(60, 120, 220)
                } else if self.hovered_note == Some(note) {
                    egui::Color32::from_rgb(80, 80, 120)
                } else {
                    egui::Color32::from_rgb(30, 30, 30)
                };

                painter.rect(
                    rect,
                    2.0,
                    color,
                    egui::Stroke::new(1.0, egui::Color32::BLACK),
                    egui::StrokeKind::Outside,
                );
            }
        }
    }

    fn draw_event_log(&self, ui: &mut egui::Ui) {
        ui.label(egui::RichText::new("MIDI Monitor").strong().size(12.0));
        egui::ScrollArea::vertical()
            .id_salt("midi_log")
            .max_height(120.0)
            .auto_shrink([false, false])
            .stick_to_bottom(true)
            .show(ui, |ui| {
                for line in &self.event_log {
                    ui.label(
                        egui::RichText::new(line)
                            .font(egui::FontId::monospace(11.0))
                            .color(egui::Color32::from_rgb(180, 220, 180)),
                    );
                }
            });
    }
}
