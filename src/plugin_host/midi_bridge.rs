use crate::plugin_host::handlers::DevHost;
use clack_extensions::note_ports::{NoteDialects, NotePortInfoBuffer, PluginNotePorts};
use clack_host::events::event_types::{MidiEvent, NoteOffEvent, NoteOnEvent};
use clack_host::events::io::EventBuffer;
use clack_host::events::{EventFlags, Match};
use clack_host::prelude::*;
use rtrb::Consumer;

/// A raw 3-byte MIDI message to push into the ring buffer.
#[derive(Debug, Clone, Copy)]
pub struct RawMidiEvent {
    pub data: [u8; 3],
    pub len: u8,
}

/// Sits on the audio thread, drains MIDI from the ring buffer and converts to CLAP events.
pub struct MidiBridge {
    consumer: Consumer<RawMidiEvent>,
    event_buffer: EventBuffer,
    note_port_index: u16,
    prefers_midi: bool,
}

impl MidiBridge {
    pub fn new(consumer: Consumer<RawMidiEvent>, instance: &mut PluginInstance<DevHost>) -> Self {
        let (port_index, prefers_midi) = find_main_note_port_index(instance).unwrap_or((0, true));

        Self {
            consumer,
            event_buffer: EventBuffer::with_capacity(256),
            note_port_index: port_index,
            prefers_midi,
        }
    }

    /// Drain all pending MIDI events and return them as CLAP InputEvents.
    /// Call this once per audio process block.
    pub fn drain_to_input_events(&mut self, _frame_count: u32) -> InputEvents<'_> {
        self.event_buffer.clear();

        let mut events_processed = 0;
        while let Ok(raw) = self.consumer.pop() {
            events_processed += 1;
            let data = &raw.data[..raw.len as usize];
            if data.is_empty() {
                continue;
            }

            let status = data[0] & 0xF0;
            let channel = data[0] & 0x0F;

            // Sample time 0 for all events in this block (simplified)
            let sample_time = 0u32;

            if !self.prefers_midi && data.len() >= 3 {
                match status {
                    0x90 if data[2] > 0 => {
                        let velocity = data[2] as f64 / 127.0;
                        self.event_buffer.push(
                            &NoteOnEvent::new(
                                sample_time,
                                Pckn::new(
                                    self.note_port_index,
                                    channel,
                                    data[1] as u16,
                                    Match::All,
                                ),
                                velocity,
                            )
                            .with_flags(EventFlags::IS_LIVE),
                        );
                        continue;
                    }
                    0x80 | 0x90 => {
                        self.event_buffer.push(
                            &NoteOffEvent::new(
                                sample_time,
                                Pckn::new(
                                    self.note_port_index,
                                    channel,
                                    data[1] as u16,
                                    Match::All,
                                ),
                                0.0,
                            )
                            .with_flags(EventFlags::IS_LIVE),
                        );
                        continue;
                    }
                    _ => {}
                }
            }

            // Fallback: send as raw MIDI event
            if data.len() >= 3 {
                let midi_data = [data[0], data[1], data[2]];
                self.event_buffer.push(
                    &MidiEvent::new(sample_time, self.note_port_index, midi_data)
                        .with_flags(EventFlags::IS_LIVE),
                );
            }
        }

        if events_processed > 0 {
            eprintln!("[midi_bridge] Processed {} MIDI events", events_processed);
        }

        self.event_buffer.as_input()
    }
}

/// Find the main note port index and whether it prefers MIDI dialect.
fn find_main_note_port_index(instance: &mut PluginInstance<DevHost>) -> Option<(u16, bool)> {
    let mut handle = instance.plugin_handle();
    let note_ports = handle.get_extension::<PluginNotePorts>()?;

    let mut buffer = NotePortInfoBuffer::new();
    let count = note_ports.count(&mut handle, true).min(u16::MAX as u32);

    for i in 0..count {
        let Some(info) = note_ports.get(&mut handle, i, true, &mut buffer) else {
            continue;
        };

        if !info
            .supported_dialects
            .intersects(NoteDialects::CLAP | NoteDialects::MIDI)
        {
            continue;
        }

        let prefers_midi = !info.supported_dialects.intersects(NoteDialects::CLAP);
        return Some((i as u16, prefers_midi));
    }

    None
}
