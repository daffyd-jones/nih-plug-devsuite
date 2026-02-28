use midir::{Ignore, MidiInput, MidiInputConnection, MidiOutput, MidiOutputConnection};
use std::sync::{Arc, Mutex};

// ── MIDI message types ────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq)]
pub enum MidiMessageKind {
    NoteOn { channel: u8, note: u8, velocity: u8 },
    NoteOff { channel: u8, note: u8 },
    ControlChange { channel: u8, cc: u8, value: u8 },
    PitchBend { channel: u8, value: i16 },
    Other(Vec<u8>),
}

#[derive(Debug, Clone)]
pub struct MidiEvent {
    pub timestamp_us: u64,
    pub kind: MidiMessageKind,
}

impl MidiEvent {
    fn parse(stamp: u64, data: &[u8]) -> Self {
        let kind = if data.is_empty() {
            MidiMessageKind::Other(data.to_vec())
        } else {
            let status = data[0] & 0xF0;
            let channel = data[0] & 0x0F;
            match (status, data.len()) {
                (0x90, 3) if data[2] > 0 => MidiMessageKind::NoteOn {
                    channel,
                    note: data[1],
                    velocity: data[2],
                },
                (0x80, 3) | (0x90, 3) => MidiMessageKind::NoteOff {
                    channel,
                    note: data[1],
                },
                (0xB0, 3) => MidiMessageKind::ControlChange {
                    channel,
                    cc: data[1],
                    value: data[2],
                },
                (0xE0, 3) => {
                    let raw = ((data[2] as i16) << 7) | (data[1] as i16);
                    MidiMessageKind::PitchBend {
                        channel,
                        value: raw - 8192,
                    }
                }
                _ => MidiMessageKind::Other(data.to_vec()),
            }
        };
        Self {
            timestamp_us: stamp,
            kind,
        }
    }
}

// ── Status ────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq)]
pub enum MidiStatus {
    Disconnected,
    InputOnly,
    OutputOnly,
    Connected,
    Error(String),
}

// ── Shared event log ──────────────────────────────────────────────────────────

pub type MidiEventLog = Arc<Mutex<Vec<MidiEvent>>>;
const MAX_LOG: usize = 256;

// ── Engine ────────────────────────────────────────────────────────────────────

pub struct MidiEngine {
    pub status: MidiStatus,

    pub input_port_names: Vec<String>,
    pub output_port_names: Vec<String>,

    pub selected_input_idx: Option<usize>,  // None = no input
    pub selected_output_idx: Option<usize>, // None = no output

    pub event_log: MidiEventLog,

    // Active connections — dropped to disconnect
    _input_conn: Option<MidiInputConnection<()>>,
    _output_conn: Option<Arc<Mutex<MidiOutputConnection>>>,

    // Expose output connection so piano widget can send
    pub output: Option<Arc<Mutex<MidiOutputConnection>>>,
}

impl MidiEngine {
    pub fn new() -> Self {
        let (inputs, outputs) = enumerate_ports();
        let selected_input = if inputs.is_empty() { None } else { Some(0) };
        let selected_output = if outputs.is_empty() { None } else { Some(0) };
        Self {
            status: MidiStatus::Disconnected,
            input_port_names: inputs,
            output_port_names: outputs,
            selected_input_idx: selected_input,
            selected_output_idx: selected_output,
            event_log: Arc::new(Mutex::new(Vec::new())),
            _input_conn: None,
            _output_conn: None,
            output: None,
        }
    }

    pub fn refresh_ports(&mut self) {
        let was_connected = self.status != MidiStatus::Disconnected;
        if was_connected {
            self.disconnect();
        }
        let (inputs, outputs) = enumerate_ports();
        self.input_port_names = inputs;
        self.output_port_names = outputs;
        // Clamp selections
        self.selected_input_idx = self
            .selected_input_idx
            .filter(|&i| i < self.input_port_names.len());
        self.selected_output_idx = self
            .selected_output_idx
            .filter(|&i| i < self.output_port_names.len());
    }

    pub fn connect(&mut self) {
        self.disconnect();

        let input_conn = self
            .selected_input_idx
            .and_then(|idx| open_input(idx, Arc::clone(&self.event_log)).ok());

        let output_conn = self
            .selected_output_idx
            .and_then(|idx| open_output(idx).ok().map(|c| Arc::new(Mutex::new(c))));

        self.status = match (&input_conn, &output_conn) {
            (Some(_), Some(_)) => MidiStatus::Connected,
            (Some(_), None) => MidiStatus::InputOnly,
            (None, Some(_)) => MidiStatus::OutputOnly,
            (None, None) => MidiStatus::Error("No ports could be opened".into()),
        };

        self.output = output_conn.clone();
        self._output_conn = output_conn;
        self._input_conn = input_conn;
    }

    pub fn disconnect(&mut self) {
        self._input_conn = None;
        self._output_conn = None;
        self.output = None;
        self.status = MidiStatus::Disconnected;
    }

    /// Send a raw MIDI message to the output if connected.
    pub fn send(&self, bytes: &[u8]) {
        if let Some(ref out) = self.output {
            if let Ok(mut conn) = out.lock() {
                let _ = conn.send(bytes);
            }
        }
    }

    pub fn send_note_on(&self, channel: u8, note: u8, velocity: u8) {
        self.send(&[0x90 | (channel & 0x0F), note & 0x7F, velocity & 0x7F]);
    }

    pub fn send_note_off(&self, channel: u8, note: u8) {
        self.send(&[0x80 | (channel & 0x0F), note & 0x7F, 0]);
    }

    /// Drain and return recent events for display.
    pub fn drain_events(&self) -> Vec<MidiEvent> {
        self.event_log
            .lock()
            .map(|mut log| log.drain(..).collect())
            .unwrap_or_default()
    }
}

// ── Private helpers ───────────────────────────────────────────────────────────

fn enumerate_ports() -> (Vec<String>, Vec<String>) {
    let inputs = MidiInput::new("nih-plug-playground-enum")
        .ok()
        .map(|mi| {
            mi.ports()
                .iter()
                .filter_map(|p| mi.port_name(p).ok())
                .collect()
        })
        .unwrap_or_default();

    let outputs = MidiOutput::new("nih-plug-playground-enum")
        .ok()
        .map(|mo| {
            mo.ports()
                .iter()
                .filter_map(|p| mo.port_name(p).ok())
                .collect()
        })
        .unwrap_or_default();

    (inputs, outputs)
}

fn open_input(
    idx: usize,
    log: MidiEventLog,
) -> Result<MidiInputConnection<()>, Box<dyn std::error::Error>> {
    let mut mi = MidiInput::new("nih-plug-playground-in")?;
    mi.ignore(Ignore::None);
    let ports = mi.ports();
    let port = ports.get(idx).ok_or("Port index out of range")?;
    let conn = mi.connect(
        port,
        "nih-plug-in",
        move |stamp, data, _| {
            let event = MidiEvent::parse(stamp, data);
            if let Ok(mut log) = log.lock() {
                if log.len() >= MAX_LOG {
                    log.remove(0);
                }
                log.push(event);
            }
        },
        (),
    )?;
    Ok(conn)
}

fn open_output(idx: usize) -> Result<MidiOutputConnection, Box<dyn std::error::Error>> {
    let mo = MidiOutput::new("nih-plug-playground-out")?;
    let ports = mo.ports();
    let port = ports.get(idx).ok_or("Port index out of range")?;
    let conn = mo.connect(port, "nih-plug-out")?;
    Ok(conn)
}

