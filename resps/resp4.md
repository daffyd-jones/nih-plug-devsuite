I'll provide all the files needed. This is a substantial implementation, so I'll go file by file.

Cargo.toml
toml
[
package
]
name = "nih-plug-playground"
version = "0.1.0"
edition = "2021"

[
dependencies
]
eframe = { version = "0.33", features = ["default"] }
egui_code_editor = "0.2.21"
serde = { version = "1", features = ["derive"] }
serde_json = "1"
rfd = "0.15"
dirs = "5"
cpal = "0.15"
midir = "0.10"
crossbeam-channel = "0.5"
rtrb = "0.3"

clack-host = { git = "https://github.com/prokopyl/clack", features = ["default"] }
clack-extensions = { git = "https://github.com/prokopyl/clack", features = [
    "clack-host", "audio-ports", "note-ports", "gui", "log", "params", "timer",
    "raw-window-handle_06"
] }
src/main.rs
rust
mod app;
mod audio_engine;
mod build_system;
mod midi_engine;
mod plugin_host;
mod project;
mod scaffolding;
mod templates;
mod ui;

use app::PlaygroundApp;

fn main() -> eframe::Result<()> {
    let options = eframe::NativeOptions {
        viewport: eframe::egui::ViewportBuilder::default()
            .with_inner_size([1600.0, 900.0])
            .with_title("NIH-plug Playground"),
        ..Default::default()
    };

    eframe::run_native(
        "NIH-plug Playground",
        options,
        Box::new(|cc| {
            cc.egui_ctx.set_visuals(eframe::egui::Visuals::dark());
            Ok(Box::new(PlaygroundApp::new()))
        }),
    )
}
src/plugin_host/mod.rs
rust
#![allow(unsafe_code)]

pub mod audio;
pub mod gui;
pub mod handlers;
pub mod loader;
pub mod midi_bridge;
pub mod timer;

use crate::plugin_host::audio::{PluginAudioProcessor, PluginAudioConfig};
use crate::plugin_host::gui::Gui;
use crate::plugin_host::handlers::{DevHost, DevHostShared, DevHostMainThread, MainThreadMessage};
use crate::plugin_host::loader::PluginBinary;
use crate::plugin_host::midi_bridge::RawMidiEvent;
use crate::plugin_host::timer::Timers;

use clack_host::prelude::*;
use crossbeam_channel::{Receiver, Sender, unbounded};
use std::ffi::CString;
use std::path::{Path, PathBuf};
use std::rc::Rc;

#[derive(Debug, Clone, PartialEq)]
pub enum HostStatus {
    Unloaded,
    Loaded,
    Active,
    Processing,
    Error(String),
}

/// Whether the loaded plugin is a synth (generates audio from MIDI)
/// or an effect (processes input audio).
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum PluginMode {
    Instrument,
    Effect,
}

/// Controls where MIDI events are routed.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum MidiRouting {
    /// MIDI goes only to the hosted plugin.
    PluginOnly,
    /// MIDI goes only to the external MIDI output.
    ExternalOnly,
    /// MIDI goes to both plugin and external output.
    Both,
}

pub struct PluginHost {
    pub status: HostStatus,
    pub plugin_name: Option<String>,
    pub plugin_id: Option<String>,
    pub plugin_mode: PluginMode,
    pub midi_routing: MidiRouting,

    // CLAP hosting internals
    entry: Option<PluginEntry>,
    instance: Option<PluginInstance<DevHost>>,
    main_thread_rx: Option<Receiver<MainThreadMessage>>,
    main_thread_tx: Option<Sender<MainThreadMessage>>,

    // MIDI ring buffer producer — UI/MIDI thread writes here
    midi_producer: Option<rtrb::Producer<RawMidiEvent>>,

    // GUI state
    gui: Option<Gui>,
    gui_is_open: bool,

    // Loaded binary path for display
    pub loaded_path: Option<PathBuf>,
}

impl PluginHost {
    pub fn new() -> Self {
        Self {
            status: HostStatus::Unloaded,
            plugin_name: None,
            plugin_id: None,
            plugin_mode: PluginMode::Instrument,
            midi_routing: MidiRouting::PluginOnly,
            entry: None,
            instance: None,
            main_thread_rx: None,
            main_thread_tx: None,
            midi_producer: None,
            gui: None,
            gui_is_open: false,
            loaded_path: None,
        }
    }

    /// Load a CLAP plugin from a .clap binary file.
    pub fn load(&mut self, path: &Path) -> Result<(), String> {
        self.unload();

        let binary = PluginBinary::load(path)?;

        let plugin_id_cstring = CString::new(binary.plugin_id.as_str())
            .map_err(|e| format!("Invalid plugin ID: {e}"))?;

        let host_info = HostInfo::new(
            "NIH-plug Playground",
            "NIH-plug Playground",
            "https://github.com/example",
            "0.1.0",
        )
        .map_err(|e| format!("Failed to create host info: {e}"))?;

        let (tx, rx) = unbounded();
        let sender = tx.clone();

        let instance = PluginInstance::<DevHost>::new(
            |_| DevHostShared::new(sender),
            |shared| DevHostMainThread::new(shared),
            &binary.entry,
            &plugin_id_cstring,
            &host_info,
        )
        .map_err(|e| format!("Failed to instantiate plugin: {e}"))?;

        // Check for GUI extension
        let gui_ext = instance.access_handler(|h| h.gui);

        self.plugin_name = Some(binary.plugin_name.clone());
        self.plugin_id = Some(binary.plugin_id.clone());
        self.loaded_path = Some(path.to_path_buf());
        self.entry = Some(binary.entry);
        self.instance = Some(instance);
        self.main_thread_tx = Some(tx);
        self.main_thread_rx = Some(rx);
        self.status = HostStatus::Loaded;

        // Initialize GUI handle if available
        if let Some(gui_ext) = gui_ext {
            if let Some(ref mut instance) = self.instance {
                let gui = Gui::new(gui_ext, &mut instance.plugin_handle());
                self.gui = Some(gui);
            }
        }

        Ok(())
    }

    /// Activate the plugin and return an audio processor ready for the audio thread.
    /// The returned `PluginAudioProcessor` should be moved to the cpal audio callback.
    pub fn activate(
        &mut self,
        sample_rate: u32,
        min_buffer_size: u32,
        max_buffer_size: u32,
    ) -> Result<PluginAudioProcessor, String> {
        let instance = self
            .instance
            .as_mut()
            .ok_or("No plugin loaded")?;

        // Create MIDI ring buffer
        let (producer, consumer) = rtrb::RingBuffer::new(2048);
        self.midi_producer = Some(producer);

        let config = PluginAudioConfig {
            sample_rate,
            min_buffer_size,
            max_buffer_size,
            plugin_mode: self.plugin_mode,
        };

        let processor = PluginAudioProcessor::create(instance, consumer, config)?;

        self.status = HostStatus::Active;
        Ok(processor)
    }

    /// Unload everything in the correct order.
    pub fn unload(&mut self) {
        // Close GUI first
        if self.gui_is_open {
            if let (Some(ref mut gui), Some(ref mut instance)) =
                (&mut self.gui, &mut self.instance)
            {
                gui.destroy(&mut instance.plugin_handle());
            }
            self.gui_is_open = false;
        }
        self.gui = None;

        // Drop MIDI producer
        self.midi_producer = None;

        // Drop instance (this deactivates the plugin)
        self.instance = None;

        // Drop entry (this unloads the .so/.dll/.dylib)
        self.entry = None;

        self.main_thread_rx = None;
        self.main_thread_tx = None;
        self.plugin_name = None;
        self.plugin_id = None;
        self.loaded_path = None;
        self.status = HostStatus::Unloaded;
    }

    /// Poll main-thread messages. Call every frame from the UI thread.
    pub fn poll_main_thread(&mut self) {
        let Some(ref rx) = self.main_thread_rx else {
            return;
        };

        while let Ok(msg) = rx.try_recv() {
            match msg {
                MainThreadMessage::RunOnMainThread => {
                    if let Some(ref mut instance) = self.instance {
                        instance.call_on_main_thread_callback();
                    }
                }
                MainThreadMessage::GuiClosed => {
                    self.gui_is_open = false;
                }
                MainThreadMessage::GuiRequestResized { .. } => {
                    // Floating windows handle their own sizing
                }
            }
        }

        // Tick timers if plugin supports them
        if let Some(ref mut instance) = self.instance {
            let timer_data = instance.access_handler(|h| {
                h.timer_support.map(|ext| (h.timers.clone(), ext))
            });
            if let Some((timers, timer_ext)) = timer_data {
                timers.tick_timers(&timer_ext, &mut instance.plugin_handle());
            }
        }
    }

    /// Open the plugin's floating GUI window.
    pub fn open_gui(&mut self) -> Result<(), String> {
        let gui = self.gui.as_mut().ok_or("Plugin has no GUI")?;
        let instance = self.instance.as_mut().ok_or("No plugin loaded")?;

        match gui.needs_floating() {
            Some(true) | None => {
                gui.open_floating(&mut instance.plugin_handle())
                    .map_err(|e| format!("Failed to open GUI: {e}"))?;
                self.gui_is_open = true;
                Ok(())
            }
            Some(false) => {
                // For now, we only support floating. Embedded requires platform-specific work.
                // Try floating anyway — some plugins support both
                gui.open_floating(&mut instance.plugin_handle())
                    .map_err(|e| format!("Plugin only supports embedded GUI (not yet supported): {e}"))
            }
        }
    }

    /// Close the plugin's GUI window.
    pub fn close_gui(&mut self) {
        if self.gui_is_open {
            if let (Some(ref mut gui), Some(ref mut instance)) =
                (&mut self.gui, &mut self.instance)
            {
                gui.destroy(&mut instance.plugin_handle());
            }
            self.gui_is_open = false;
        }
    }

    /// Send a MIDI note-on to the plugin via the ring buffer.
    pub fn send_note_on(&self, channel: u8, note: u8, velocity: u8) {
        if let Some(ref producer) = &self.midi_producer {
            let event = RawMidiEvent {
                data: [0x90 | (channel & 0x0F), note & 0x7F, velocity & 0x7F],
                len: 3,
            };
            // Producer is behind a shared ref — we need interior mutability
            // Actually rtrb::Producer requires &mut, so we handle this through
            // the audio engine which owns it. See send_midi_event().
        }
    }

    /// Push a raw MIDI event to the ring buffer. Returns false if buffer is full or no plugin loaded.
    pub fn push_midi_event(&mut self, event: RawMidiEvent) -> bool {
        if let Some(ref mut producer) = self.midi_producer {
            producer.push(event).is_ok()
        } else {
            false
        }
    }

    pub fn has_gui(&self) -> bool {
        self.gui.is_some()
    }

    pub fn is_gui_open(&self) -> bool {
        self.gui_is_open
    }

    pub fn is_loaded(&self) -> bool {
        self.status != HostStatus::Unloaded
    }

    pub fn is_active(&self) -> bool {
        matches!(self.status, HostStatus::Active | HostStatus::Processing)
    }
}
src/plugin_host/handlers.rs
rust
#![allow(unsafe_code)]

use crate::plugin_host::timer::Timers;

use clack_extensions::audio_ports::{HostAudioPortsImpl, RescanType};
use clack_extensions::gui::{GuiSize, HostGui, HostGuiImpl, PluginGui};
use clack_extensions::log::{HostLog, HostLogImpl, LogSeverity};
use clack_extensions::note_ports::{HostNotePortsImpl, NoteDialects, NotePortRescanFlags};
use clack_extensions::params::{
    HostParams, HostParamsImplMainThread, HostParamsImplShared, ParamClearFlags, ParamRescanFlags,
};
use clack_extensions::timer::{HostTimer, PluginTimer};
use clack_host::prelude::*;
use crossbeam_channel::Sender;
use std::rc::Rc;
use std::sync::OnceLock;

/// Messages sent to the main thread from plugin threads.
pub enum MainThreadMessage {
    RunOnMainThread,
    GuiClosed,
    GuiRequestResized { new_size: GuiSize },
}

/// Our host type marker.
pub struct DevHost;

impl HostHandlers for DevHost {
    type Shared<'a> = DevHostShared;
    type MainThread<'a> = DevHostMainThread<'a>;
    type AudioProcessor<'a> = ();

    fn declare_extensions(builder: &mut HostExtensions<Self>, _shared: &Self::Shared<'_>) {
        builder
            .register::<HostLog>()
            .register::<HostGui>()
            .register::<HostTimer>()
            .register::<HostParams>();
    }
}

/// Shared data accessible from all threads.
pub struct DevHostShared {
    pub sender: Sender<MainThreadMessage>,
    callbacks: OnceLock<()>,
}

impl DevHostShared {
    pub fn new(sender: Sender<MainThreadMessage>) -> Self {
        Self {
            sender,
            callbacks: OnceLock::new(),
        }
    }
}

impl<'a> SharedHandler<'a> for DevHostShared {
    fn initializing(&self, _instance: InitializingPluginHandle<'a>) {
        let _ = self.callbacks.set(());
    }

    fn request_restart(&self) {
        // Not supported in this dev host
    }

    fn request_process(&self) {
        // CPAL is always processing
    }

    fn request_callback(&self) {
        let _ = self.sender.send(MainThreadMessage::RunOnMainThread);
    }
}

/// Main-thread-only data.
pub struct DevHostMainThread<'a> {
    pub _shared: &'a DevHostShared,
    pub plugin: Option<InitializedPluginHandle<'a>>,
    pub timer_support: Option<PluginTimer>,
    pub timers: Rc<Timers>,
    pub gui: Option<PluginGui>,
}

impl<'a> DevHostMainThread<'a> {
    pub fn new(shared: &'a DevHostShared) -> Self {
        Self {
            _shared: shared,
            plugin: None,
            timer_support: None,
            timers: Rc::new(Timers::new()),
            gui: None,
        }
    }
}

impl<'a> MainThreadHandler<'a> for DevHostMainThread<'a> {
    fn initialized(&mut self, instance: InitializedPluginHandle<'a>) {
        self.gui = instance.get_extension();
        self.timer_support = instance.get_extension();
        self.plugin = Some(instance);
    }
}

// ── Extension implementations ────────────────────────────────────────────────

impl HostLogImpl for DevHostShared {
    fn log(&self, severity: LogSeverity, message: &str) {
        if severity <= LogSeverity::Debug {
            return;
        }
        eprintln!("[plugin {severity}] {message}");
    }
}

impl HostGuiImpl for DevHostShared {
    fn resize_hints_changed(&self) {}

    fn request_resize(&self, new_size: GuiSize) -> Result<(), HostError> {
        self.sender
            .send(MainThreadMessage::GuiRequestResized { new_size })
            .map_err(|_| HostError::Message("Channel closed"))?;
        Ok(())
    }

    fn request_show(&self) -> Result<(), HostError> {
        Ok(())
    }

    fn request_hide(&self) -> Result<(), HostError> {
        Ok(())
    }

    fn closed(&self, _was_destroyed: bool) {
        let _ = self.sender.send(MainThreadMessage::GuiClosed);
    }
}

impl HostAudioPortsImpl for DevHostMainThread<'_> {
    fn is_rescan_flag_supported(&self, _flag: RescanType) -> bool {
        false
    }

    fn rescan(&mut self, _flag: RescanType) {}
}

impl HostNotePortsImpl for DevHostMainThread<'_> {
    fn supported_dialects(&self) -> NoteDialects {
        NoteDialects::CLAP | NoteDialects::MIDI
    }

    fn rescan(&mut self, _flags: NotePortRescanFlags) {}
}

impl HostParamsImplMainThread for DevHostMainThread<'_> {
    fn rescan(&mut self, _flags: ParamRescanFlags) {}

    fn clear(&mut self, _param_id: ClapId, _flags: ParamClearFlags) {}
}

impl HostParamsImplShared for DevHostShared {
    fn request_flush(&self) {}
}
src/plugin_host/loader.rs
rust
#![allow(unsafe_code)]

use clack_host::prelude::*;
use std::path::{Path, PathBuf};

/// Represents a loaded CLAP plugin binary.
pub struct PluginBinary {
    pub entry: PluginEntry,
    pub plugin_id: String,
    pub plugin_name: String,
    pub plugin_version: Option<String>,
    pub path: PathBuf,
}

impl PluginBinary {
    /// Load a .clap file and find the first plugin in it.
    pub fn load(path: &Path) -> Result<Self, String> {
        if !path.exists() {
            return Err(format!("Plugin file not found: {}", path.display()));
        }

        let entry = unsafe { PluginEntry::load(path) }
            .map_err(|e| format!("Failed to load CLAP entry from {}: {e}", path.display()))?;

        let factory = entry
            .get_plugin_factory()
            .ok_or_else(|| "CLAP file has no plugin factory".to_string())?;

        let mut found_id = None;
        let mut found_name = None;
        let mut found_version = None;

        for descriptor in factory.plugin_descriptors() {
            if let Some(id) = descriptor.id() {
                if let Ok(id_str) = id.to_str() {
                    found_id = Some(id_str.to_string());
                    found_name = descriptor
                        .name()
                        .map(|n| n.to_string_lossy().to_string());
                    found_version = descriptor
                        .version()
                        .map(|v| v.to_string_lossy().to_string());
                    break;
                }
            }
        }

        let plugin_id = found_id.ok_or_else(|| "No valid plugin found in CLAP file".to_string())?;
        let plugin_name = found_name.unwrap_or_else(|| plugin_id.clone());

        Ok(PluginBinary {
            entry,
            plugin_id,
            plugin_name,
            plugin_version: found_version,
            path: path.to_path_buf(),
        })
    }
}

/// Find the .clap bundle in target/bundled/ after a successful `cargo nih-plug bundle`.
///
/// `project_path` is the root of the cargo project.
/// `plugin_name` is the lib name from Cargo.toml (with hyphens replaced by underscores if needed).
pub fn find_clap_bundle(project_path: &Path) -> Result<PathBuf, String> {
    let bundled_dir = project_path.join("target").join("bundled");

    if !bundled_dir.exists() {
        return Err(format!(
            "No target/bundled directory found at {}. Did the bundle command succeed?",
            bundled_dir.display()
        ));
    }

    // Look for any .clap file in the bundled directory
    let mut clap_files: Vec<PathBuf> = Vec::new();

    if let Ok(entries) = std::fs::read_dir(&bundled_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().map_or(false, |ext| ext == "clap") {
                clap_files.push(path);
            }
        }
    }

    match clap_files.len() {
        0 => Err(format!(
            "No .clap files found in {}",
            bundled_dir.display()
        )),
        1 => Ok(clap_files.into_iter().next().unwrap()),
        _ => {
            // Pick the most recently modified one
            clap_files.sort_by(|a, b| {
                let a_time = std::fs::metadata(a)
                    .and_then(|m| m.modified())
                    .unwrap_or(std::time::SystemTime::UNIX_EPOCH);
                let b_time = std::fs::metadata(b)
                    .and_then(|m| m.modified())
                    .unwrap_or(std::time::SystemTime::UNIX_EPOCH);
                b_time.cmp(&a_time)
            });
            Ok(clap_files.into_iter().next().unwrap())
        }
    }
}

/// Parse the Cargo.toml at project_path to extract the lib crate name.
/// Falls back to package name with hyphens replaced by underscores.
pub fn get_lib_name(project_path: &Path) -> Result<String, String> {
    let cargo_toml_path = project_path.join("Cargo.toml");
    let content = std::fs::read_to_string(&cargo_toml_path)
        .map_err(|e| format!("Failed to read Cargo.toml: {e}"))?;

    // Simple TOML parsing — look for [lib] name = "..."
    // or fall back to [package] name = "..."
    let mut in_lib_section = false;
    let mut in_package_section = false;
    let mut lib_name = None;
    let mut package_name = None;

    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('[') {
            in_lib_section = trimmed == "[lib]";
            in_package_section = trimmed == "[package]";
            continue;
        }

        if let Some(value) = extract_toml_string_value(trimmed, "name") {
            if in_lib_section {
                lib_name = Some(value);
            } else if in_package_section && package_name.is_none() {
                package_name = Some(value);
            }
        }
    }

    let name = lib_name
        .or(package_name)
        .ok_or_else(|| "Could not find crate name in Cargo.toml".to_string())?;

    Ok(name.replace('-', "_"))
}

fn extract_toml_string_value(line: &str, key: &str) -> Option<String> {
    let line = line.trim();
    if !line.starts_with(key) {
        return None;
    }
    let rest = line[key.len()..].trim();
    if !rest.starts_with('=') {
        return None;
    }
    let value = rest[1..].trim().trim_matches('"');
    Some(value.to_string())
}
src/plugin_host/audio.rs
rust
#![allow(unsafe_code)]

use crate::plugin_host::handlers::DevHost;
use crate::plugin_host::midi_bridge::{MidiBridge, RawMidiEvent};
use crate::plugin_host::PluginMode;

use clack_extensions::audio_ports::{
    AudioPortFlags, AudioPortInfoBuffer, AudioPortType, PluginAudioPorts,
};
use clack_extensions::note_ports::{NoteDialects, NotePortInfoBuffer, PluginNotePorts};
use clack_host::prelude::*;
use cpal::FromSample;
use std::sync::mpsc;

/// Configuration for plugin audio activation.
#[derive(Debug, Clone)]
pub struct PluginAudioConfig {
    pub sample_rate: u32,
    pub min_buffer_size: u32,
    pub max_buffer_size: u32,
    pub plugin_mode: PluginMode,
}

/// Port layout information.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum PortLayout {
    Mono,
    Stereo,
    Other(u16),
}

impl PortLayout {
    pub fn channel_count(&self) -> u16 {
        match self {
            PortLayout::Mono => 1,
            PortLayout::Stereo => 2,
            PortLayout::Other(n) => *n,
        }
    }
}

/// Info about one audio port.
#[derive(Debug, Clone)]
pub struct PortInfo {
    pub layout: PortLayout,
    pub name: String,
}

/// Config for a set of ports (input or output).
#[derive(Debug, Clone)]
pub struct PortsConfig {
    pub ports: Vec<PortInfo>,
    pub main_port_index: usize,
}

impl PortsConfig {
    fn default_stereo() -> Self {
        Self {
            ports: vec![PortInfo {
                layout: PortLayout::Stereo,
                name: "Default".into(),
            }],
            main_port_index: 0,
        }
    }

    fn empty() -> Self {
        Self {
            ports: vec![],
            main_port_index: 0,
        }
    }

    pub fn main_port(&self) -> Option<&PortInfo> {
        self.ports.get(self.main_port_index)
    }

    pub fn total_channel_count(&self) -> usize {
        self.ports.iter().map(|p| p.layout.channel_count() as usize).sum()
    }
}

/// The audio processor that lives on the audio thread.
/// Created on the main thread, then sent to cpal callback via channel.
pub struct PluginAudioProcessor {
    /// One-shot: the stopped processor is moved here initially,
    /// then taken and started on the first audio callback.
    stopped: Option<StoppedPluginAudioProcessor<DevHost>>,
    /// After start_processing(), this holds the active processor.
    started: Option<StartedPluginAudioProcessor<DevHost>>,
    /// MIDI bridge for receiving events.
    midi_bridge: MidiBridge,
    /// Audio buffer management.
    input_ports: AudioPorts,
    output_ports: AudioPorts,
    input_channels: Box<[Vec<f32>]>,
    output_channels: Box<[Vec<f32>]>,
    muxed_output: Vec<f32>,
    /// Config.
    output_channel_count: usize,
    input_port_config: PortsConfig,
    output_port_config: PortsConfig,
    actual_frame_count: usize,
    steady_counter: u64,
    plugin_mode: PluginMode,
}

// StoppedPluginAudioProcessor is Send, so this is safe as long as
// we only call start_processing (which returns a !Send started processor)
// from within the audio thread.
unsafe impl Send for PluginAudioProcessor {}

impl PluginAudioProcessor {
    /// Create the processor on the main thread. Activates the plugin but doesn't
    /// start processing yet — that happens on the first audio callback.
    pub fn create(
        instance: &mut PluginInstance<DevHost>,
        midi_consumer: rtrb::Consumer<RawMidiEvent>,
        config: PluginAudioConfig,
    ) -> Result<Self, String> {
        let input_port_config = query_ports(&mut instance.plugin_handle(), true);
        let output_port_config = query_ports(&mut instance.plugin_handle(), false);

        // Query note port info for MIDI bridge
        let (note_port_index, prefers_midi) =
            find_note_port(&mut instance.plugin_handle()).unwrap_or((0, true));

        let midi_bridge = MidiBridge::new(midi_consumer, note_port_index, prefers_midi);

        let clap_config = PluginAudioConfiguration {
            sample_rate: config.sample_rate as f64,
            min_frames_count: config.min_buffer_size,
            max_frames_count: config.max_buffer_size,
        };

        let stopped = instance
            .activate(|_, _| (), clap_config)
            .map_err(|e| format!("Failed to activate plugin: {e}"))?;

        let frame_count = config.max_buffer_size as usize;
        let total_in_channels = input_port_config.total_channel_count();
        let total_out_channels = output_port_config.total_channel_count();

        let output_channel_count = output_port_config
            .main_port()
            .map(|p| p.layout.channel_count() as usize)
            .unwrap_or(2)
            .min(2);

        let input_channels: Box<[Vec<f32>]> = input_port_config
            .ports
            .iter()
            .map(|p| vec![0.0f32; frame_count * p.layout.channel_count() as usize])
            .collect();

        let output_channels: Box<[Vec<f32>]> = output_port_config
            .ports
            .iter()
            .map(|p| vec![0.0f32; frame_count * p.layout.channel_count() as usize])
            .collect();

        let muxed_output = vec![0.0f32; frame_count * output_channel_count];

        Ok(Self {
            stopped: Some(stopped),
            started: None,
            midi_bridge,
            input_ports: AudioPorts::with_capacity(total_in_channels, input_port_config.ports.len()),
            output_ports: AudioPorts::with_capacity(total_out_channels, output_port_config.ports.len()),
            input_channels,
            output_channels,
            muxed_output,
            output_channel_count,
            input_port_config,
            output_port_config,
            actual_frame_count: frame_count,
            steady_counter: 0,
            plugin_mode: config.plugin_mode,
        })
    }

    /// Process audio. Called from the cpal output callback.
    pub fn process<S: FromSample<f32>>(&mut self, output_data: &mut [S], input_data: Option<&[f32]>) {
        // Start processing on first call (moves from stopped → started)
        if self.started.is_none() {
            if let Some(stopped) = self.stopped.take() {
                match stopped.start_processing() {
                    Ok(started) => self.started = Some(started),
                    Err(e) => {
                        eprintln!("[plugin_host] Failed to start processing: {e}");
                        output_data.iter_mut().for_each(|s| *s = f32::to_sample(0.0));
                        return;
                    }
                }
            }
        }

        let Some(ref mut processor) = self.started else {
            output_data.iter_mut().for_each(|s| *s = f32::to_sample(0.0));
            return;
        };

        // Ensure buffers are big enough
        let frame_count = output_data.len() / self.output_channel_count.max(1);
        self.ensure_buffer_size(output_data.len());

        // Copy input data into plugin input buffers (for effect mode)
        if self.plugin_mode == PluginMode::Effect {
            if let Some(input) = input_data {
                self.write_input_from_interleaved(input, frame_count);
            }
        }

        // Clear output buffers
        for buf in self.output_channels.iter_mut() {
            buf.iter_mut().for_each(|s| *s = 0.0);
        }

        // Prepare plugin buffers
        let sample_count = frame_count;
        let (ins, mut outs) = self.prepare_buffers(sample_count);

        // Get MIDI events
        let events = self.midi_bridge.drain_to_input_events(sample_count as u32);

        // Process
        match processor.process(
            &ins,
            &mut outs,
            &events,
            &mut OutputEvents::void(),
            Some(self.steady_counter),
            None,
        ) {
            Ok(_) => self.write_output_interleaved(output_data),
            Err(e) => {
                eprintln!("[plugin_host] Process error: {e}");
                output_data.iter_mut().for_each(|s| *s = f32::to_sample(0.0));
            }
        }

        self.steady_counter += sample_count as u64;
    }

    fn ensure_buffer_size(&mut self, cpal_buf_len: usize) {
        let frame_count = cpal_buf_len / self.output_channel_count.max(1);
        if frame_count <= self.actual_frame_count {
            return;
        }

        self.actual_frame_count = frame_count;

        for (buf, port) in self.input_channels.iter_mut().zip(&self.input_port_config.ports) {
            buf.resize(frame_count * port.layout.channel_count() as usize, 0.0);
        }
        for (buf, port) in self.output_channels.iter_mut().zip(&self.output_port_config.ports) {
            buf.resize(frame_count * port.layout.channel_count() as usize, 0.0);
        }
        self.muxed_output.resize(frame_count * self.output_channel_count, 0.0);
    }

    fn write_input_from_interleaved(&mut self, interleaved: &[f32], frame_count: usize) {
        // De-interleave CPAL input into plugin input buffers
        if self.input_channels.is_empty() {
            return;
        }

        let main_idx = self.input_port_config.main_port_index;
        if main_idx >= self.input_channels.len() {
            return;
        }

        let port = &self.input_port_config.ports[main_idx];
        let ch_count = port.layout.channel_count() as usize;
        let buf = &mut self.input_channels[main_idx];

        for frame in 0..frame_count {
            for ch in 0..ch_count.min(self.output_channel_count) {
                let interleaved_idx = frame * self.output_channel_count + ch;
                let channel_buf_idx = ch * self.actual_frame_count + frame;
                if interleaved_idx < interleaved.len() && channel_buf_idx < buf.len() {
                    buf[channel_buf_idx] = interleaved[interleaved_idx];
                }
            }
        }
    }

    fn prepare_buffers(&mut self, sample_count: usize) -> (InputAudioBuffers<'_>, OutputAudioBuffers<'_>) {
        let actual = self.actual_frame_count;

        let ins = self.input_ports.with_input_buffers(
            self.input_channels.iter_mut().map(|port_buf| {
                AudioPortBuffer {
                    latency: 0,
                    channels: AudioPortBufferType::f32_input_only(
                        port_buf.chunks_exact_mut(actual).map(|buffer| InputChannel {
                            buffer: &mut buffer[..sample_count],
                            is_constant: false,
                        }),
                    ),
                }
            }),
        );

        let outs = self.output_ports.with_output_buffers(
            self.output_channels.iter_mut().map(|port_buf| {
                AudioPortBuffer {
                    latency: 0,
                    channels: AudioPortBufferType::f32_output_only(
                        port_buf
                            .chunks_exact_mut(actual)
                            .map(|buf| &mut buf[..sample_count]),
                    ),
                }
            }),
        );

        (ins, outs)
    }

    fn write_output_interleaved<S: FromSample<f32>>(&mut self, destination: &mut [S]) {
        let main_idx = self.output_port_config.main_port_index;
        if main_idx >= self.output_channels.len() {
            destination.iter_mut().for_each(|s| *s = f32::to_sample(0.0));
            return;
        }

        let main_output = &self.output_channels[main_idx];
        let muxed = &mut self.muxed_output[..destination.len()];

        let plugin_ch_count = self.output_port_config.ports[main_idx]
            .layout
            .channel_count() as usize;

        match (plugin_ch_count, self.output_channel_count) {
            (1, 1) => {
                let len = muxed.len().min(main_output.len());
                muxed[..len].copy_from_slice(&main_output[..len]);
            }
            (_, 1) => {
                // Mix down to mono
                let frame_count = muxed.len();
                for i in 0..frame_count {
                    let mut total = 0.0;
                    for ch in 0..plugin_ch_count {
                        let idx = ch * self.actual_frame_count + i;
                        if idx < main_output.len() {
                            total += main_output[idx];
                        }
                    }
                    muxed[i] = total / plugin_ch_count as f32;
                }
            }
            (1, 2) => {
                // Mono to stereo
                let frame_count = muxed.len() / 2;
                for i in 0..frame_count {
                    let sample = if i < main_output.len() {
                        main_output[i]
                    } else {
                        0.0
                    };
                    muxed[i * 2] = sample;
                    muxed[i * 2 + 1] = sample;
                }
            }
            (_, 2) => {
                // Interleave first two channels
                let frame_count = muxed.len() / 2;
                for i in 0..frame_count {
                    let l_idx = i; // channel 0
                    let r_idx = self.actual_frame_count + i; // channel 1
                    muxed[i * 2] = if l_idx < main_output.len() {
                        main_output[l_idx]
                    } else {
                        0.0
                    };
                    muxed[i * 2 + 1] = if r_idx < main_output.len() {
                        main_output[r_idx]
                    } else {
                        0.0
                    };
                }
            }
            _ => muxed.fill(0.0),
        }

        for (out, &m) in destination.iter_mut().zip(muxed.iter()) {
            *out = m.to_sample();
        }
    }
}

/// Query plugin audio ports.
fn query_ports(plugin: &mut PluginMainThreadHandle, is_input: bool) -> PortsConfig {
    let Some(ports_ext) = plugin.get_extension::<PluginAudioPorts>() else {
        return if is_input {
            PortsConfig::empty()
        } else {
            PortsConfig::default_stereo()
        };
    };

    let mut buffer = AudioPortInfoBuffer::new();
    let mut main_port_index = None;
    let mut ports = Vec::new();

    let count = ports_ext.count(plugin, is_input);
    for i in 0..count {
        let Some(info) = ports_ext.get(plugin, i, is_input, &mut buffer) else {
            continue;
        };

        let port_type = info
            .port_type
            .or_else(|| AudioPortType::from_channel_count(info.channel_count));

        let layout = match port_type {
            Some(t) if t == AudioPortType::MONO => PortLayout::Mono,
            Some(t) if t == AudioPortType::STEREO => PortLayout::Stereo,
            _ => PortLayout::Other(info.channel_count as u16),
        };

        if info.flags.contains(AudioPortFlags::IS_MAIN) {
            main_port_index = Some(i as usize);
        }

        ports.push(PortInfo {
            layout,
            name: String::from_utf8_lossy(info.name).into_owned(),
        });
    }

    if ports.is_empty() {
        return if is_input {
            PortsConfig::empty()
        } else {
            PortsConfig::default_stereo()
        };
    }

    PortsConfig {
        main_port_index: main_port_index.unwrap_or(0),
        ports,
    }
}

/// Find the main note port index and whether the plugin prefers raw MIDI.
fn find_note_port(plugin: &mut PluginMainThreadHandle) -> Option<(u16, bool)> {
    let note_ports = plugin.get_extension::<PluginNotePorts>()?;
    let mut buffer = NotePortInfoBuffer::new();

    let count = note_ports.count(plugin, true).min(u16::MAX as u32);

    for i in 0..count {
        let Some(info) = note_ports.get(plugin, i, true, &mut buffer) else {
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
src/plugin_host/midi_bridge.rs
rust
use clack_host::events::event_types::{MidiEvent, NoteOffEvent, NoteOnEvent};
use clack_host::events::{EventFlags, Match};
use clack_host::prelude::*;

/// A raw 3-byte MIDI message.
#[derive(Debug, Clone, Copy)]
pub struct RawMidiEvent {
    pub data: [u8; 3],
    pub len: u8,
}

impl RawMidiEvent {
    pub fn note_on(channel: u8, note: u8, velocity: u8) -> Self {
        Self {
            data: [0x90 | (channel & 0x0F), note & 0x7F, velocity & 0x7F],
            len: 3,
        }
    }

    pub fn note_off(channel: u8, note: u8) -> Self {
        Self {
            data: [0x80 | (channel & 0x0F), note & 0x7F, 0],
            len: 3,
        }
    }

    pub fn cc(channel: u8, cc: u8, value: u8) -> Self {
        Self {
            data: [0xB0 | (channel & 0x0F), cc & 0x7F, value & 0x7F],
            len: 3,
        }
    }
}

/// Bridges MIDI events from the UI/MIDI thread to CLAP plugin events on the audio thread.
pub struct MidiBridge {
    consumer: rtrb::Consumer<RawMidiEvent>,
    event_buffer: EventBuffer,
    note_port_index: u16,
    prefers_midi: bool,
}

impl MidiBridge {
    pub fn new(
        consumer: rtrb::Consumer<RawMidiEvent>,
        note_port_index: u16,
        prefers_midi: bool,
    ) -> Self {
        Self {
            consumer,
            event_buffer: EventBuffer::with_capacity(256),
            note_port_index,
            prefers_midi,
        }
    }

    /// Drain all pending MIDI events and convert them to CLAP input events.
    /// `frame_count` is the number of audio frames in the current process block.
    pub fn drain_to_input_events(&mut self, frame_count: u32) -> InputEvents<'_> {
        self.event_buffer.clear();

        while let Ok(raw) = self.consumer.pop() {
            if raw.len < 2 {
                continue;
            }

            let status = raw.data[0] & 0xF0;
            let channel = raw.data[0] & 0x0F;

            if self.prefers_midi {
                // Send as raw MIDI
                let midi_data = [raw.data[0], raw.data[1], raw.data[2]];
                self.event_buffer.push(
                    &MidiEvent::new(0, self.note_port_index, midi_data)
                        .with_flags(EventFlags::IS_LIVE),
                );
            } else {
                // Convert to CLAP note events
                match status {
                    0x90 if raw.data[2] > 0 => {
                        let velocity = raw.data[2] as f64 / 127.0;
                        self.event_buffer.push(
                            &NoteOnEvent::new(
                                0,
                                Pckn::new(
                                    self.note_port_index,
                                    channel,
                                    raw.data[1] as u16,
                                    Match::All,
                                ),
                                velocity,
                            )
                            .with_flags(EventFlags::IS_LIVE),
                        );
                    }
                    0x80 | 0x90 => {
                        self.event_buffer.push(
                            &NoteOffEvent::new(
                                0,
                                Pckn::new(
                                    self.note_port_index,
                                    channel,
                                    raw.data[1] as u16,
                                    Match::All,
                                ),
                                0.0,
                            )
                            .with_flags(EventFlags::IS_LIVE),
                        );
                    }
                    _ => {
                        // Everything else as raw MIDI
                        let midi_data = [raw.data[0], raw.data[1], raw.data[2]];
                        self.event_buffer.push(
                            &MidiEvent::new(0, self.note_port_index, midi_data)
                                .with_flags(EventFlags::IS_LIVE),
                        );
                    }
                }
            }
        }

        self.event_buffer.as_input()
    }
}
src/plugin_host/gui.rs
rust
use clack_extensions::gui::{
    GuiApiType, GuiConfiguration, GuiError, GuiSize, PluginGui,
};
use clack_host::prelude::*;

/// Tracks a plugin's GUI state.
pub struct Gui {
    plugin_gui: PluginGui,
    pub configuration: Option<GuiConfiguration<'static>>,
    is_open: bool,
}

impl Gui {
    pub fn new(plugin_gui: PluginGui, instance: &mut PluginMainThreadHandle) -> Self {
        let configuration = Self::negotiate_configuration(&plugin_gui, instance);
        Self {
            plugin_gui,
            configuration,
            is_open: false,
        }
    }

    fn negotiate_configuration(
        gui: &PluginGui,
        plugin: &mut PluginMainThreadHandle,
    ) -> Option<GuiConfiguration<'static>> {
        let api_type = GuiApiType::default_for_current_platform()?;

        // Try embedded first
        let mut config = GuiConfiguration {
            api_type,
            is_floating: false,
        };

        if gui.is_api_supported(plugin, config) {
            return Some(config);
        }

        // Fall back to floating
        config.is_floating = true;
        if gui.is_api_supported(plugin, config) {
            Some(config)
        } else {
            None
        }
    }

    /// Returns `true` if GUI needs to be floating, `false` if embeddable, `None` if no GUI.
    pub fn needs_floating(&self) -> Option<bool> {
        self.configuration
            .map(|GuiConfiguration { is_floating, .. }| is_floating)
    }

    /// Open as a floating window (plugin manages its own window).
    pub fn open_floating(&mut self, plugin: &mut PluginMainThreadHandle) -> Result<(), GuiError> {
        let Some(configuration) = self.configuration else {
            return Err(GuiError::CreateError);
        };

        // Force floating config
        let config = GuiConfiguration {
            is_floating: true,
            ..configuration
        };

        self.plugin_gui.create(plugin, config)?;
        self.plugin_gui
            .suggest_title(plugin, c"NIH-plug Playground");
        self.plugin_gui.show(plugin)?;
        self.is_open = true;

        Ok(())
    }

    /// Destroy the GUI if it's open.
    pub fn destroy(&mut self, plugin: &mut PluginMainThreadHandle) {
        if self.is_open {
            self.plugin_gui.destroy(plugin);
            self.is_open = false;
        }
    }

    pub fn is_open(&self) -> bool {
        self.is_open
    }
}
src/plugin_host/timer.rs
rust
use clack_extensions::timer::{PluginTimer, TimerId};
use clack_host::prelude::PluginMainThreadHandle;
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::time::{Duration, Instant};

/// Manages plugin timers.
pub struct Timers {
    timers: RefCell<HashMap<TimerId, Timer>>,
    latest_id: Cell<u32>,
    smallest_duration: Cell<Option<Duration>>,
}

impl Timers {
    pub fn new() -> Self {
        Self {
            timers: RefCell::new(HashMap::new()),
            latest_id: Cell::new(0),
            smallest_duration: Cell::new(None),
        }
    }

    pub fn tick_timers(&self, timer_ext: &PluginTimer, plugin: &mut PluginMainThreadHandle) {
        let triggered: Vec<TimerId> = {
            let mut timers = self.timers.borrow_mut();
            let now = Instant::now();
            timers
                .values_mut()
                .filter_map(|t| t.tick(now).then_some(t.id))
                .collect()
        };

        for id in triggered {
            timer_ext.on_timer(plugin, id);
        }
    }

    pub fn register_new(&self, interval: Duration) -> TimerId {
        const MIN_INTERVAL: Duration = Duration::from_millis(10);
        let interval = interval.max(MIN_INTERVAL);

        let latest_id = self.latest_id.get() + 1;
        self.latest_id.set(latest_id);
        let id = TimerId(latest_id);

        self.timers
            .borrow_mut()
            .insert(id, Timer::new(id, interval));

        match self.smallest_duration.get() {
            None => self.smallest_duration.set(Some(interval)),
            Some(smallest) if smallest > interval => self.smallest_duration.set(Some(interval)),
            _ => {}
        }

        id
    }

    pub fn unregister(&self, id: TimerId) -> bool {
        let mut timers = self.timers.borrow_mut();
        if timers.remove(&id).is_some() {
            self.smallest_duration
                .set(timers.values().map(|t| t.interval).min());
            true
        } else {
            false
        }
    }

    pub fn smallest_duration(&self) -> Option<Duration> {
        self.smallest_duration.get()
    }
}

struct Timer {
    id: TimerId,
    interval: Duration,
    last_triggered_at: Option<Instant>,
}

impl Timer {
    fn new(id: TimerId, interval: Duration) -> Self {
        Self {
            id,
            interval,
            last_triggered_at: None,
        }
    }

    fn tick(&mut self, now: Instant) -> bool {
        let triggered = if let Some(last) = self.last_triggered_at {
            now.checked_duration_since(last)
                .map_or(false, |since| since > self.interval)
        } else {
            true
        };

        if triggered {
            self.last_triggered_at = Some(now);
        }

        triggered
    }
}

// Timer extension host impl — needs to be on the MainThread handler
use crate::plugin_host::handlers::DevHostMainThread;
use clack_extensions::timer::HostTimerImpl;
use clack_host::prelude::HostError;

impl HostTimerImpl for DevHostMainThread<'_> {
    fn register_timer(&mut self, period_ms: u32) -> Result<TimerId, HostError> {
        Ok(self
            .timers
            .register_new(Duration::from_millis(period_ms as u64)))
    }

    fn unregister_timer(&mut self, timer_id: TimerId) -> Result<(), HostError> {
        if self.timers.unregister(timer_id) {
            Ok(())
        } else {
            Err(HostError::Message("Unknown timer ID"))
        }
    }
}
src/audio_engine.rs
rust
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{BufferSize, Device, SampleRate, Stream, StreamConfig};
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use crate::plugin_host::audio::PluginAudioProcessor;

// ── Constants ─────────────────────────────────────────────────────────────────

pub const COMMON_SAMPLE_RATES: &[u32] = &[44100, 48000, 88200, 96000, 192000];

pub const BUFFER_SIZE_OPTIONS: &[(&str, Option<u32>)] = &[
    ("Default", None),
    ("64", Some(64)),
    ("128", Some(128)),
    ("256", Some(256)),
    ("512", Some(512)),
    ("1024", Some(1024)),
    ("2048", Some(2048)),
];

// ── Status ────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq)]
pub enum AudioStatus {
    Stopped,
    Running,
    Error(String),
}

#[derive(Debug, Clone)]
pub struct RunningInfo {
    pub input_device: String,
    pub output_device: String,
    pub sample_rate: u32,
    pub buffer_size: String,
    pub channels: u16,
}

/// What the audio engine is doing with its streams.
#[derive(Debug, Clone, PartialEq)]
pub enum AudioMode {
    /// Simple input→output passthrough.
    Passthrough,
    /// Running audio through a loaded CLAP plugin (instrument — no input needed).
    PluginInstrument,
    /// Running audio through a loaded CLAP plugin (effect — input→plugin→output).
    PluginEffect,
}

// ── Engine ────────────────────────────────────────────────────────────────────

pub struct AudioEngine {
    pub status: AudioStatus,
    pub running_info: Option<RunningInfo>,
    pub mode: AudioMode,

    pub input_device_names: Vec<String>,
    pub output_device_names: Vec<String>,

    pub selected_input_idx: usize,
    pub selected_output_idx: usize,
    pub selected_sample_rate_idx: usize,
    pub selected_buffer_size_idx: usize,

    _streams: Option<StreamHolder>,
}

enum StreamHolder {
    Passthrough(Stream, Stream),
    PluginInstrument(Stream),
    PluginEffect(Stream, Stream),
}

impl AudioEngine {
    pub fn new() -> Self {
        let (inputs, outputs) = enumerate_devices();
        Self {
            status: AudioStatus::Stopped,
            running_info: None,
            mode: AudioMode::Passthrough,
            input_device_names: inputs,
            output_device_names: outputs,
            selected_input_idx: 0,
            selected_output_idx: 0,
            selected_sample_rate_idx: 0,
            selected_buffer_size_idx: 0,
            _streams: None,
        }
    }

    pub fn refresh_devices(&mut self) {
        let (inputs, outputs) = enumerate_devices();
        self.input_device_names = inputs;
        self.output_device_names = outputs;
        if !self.input_device_names.is_empty() {
            self.selected_input_idx = self
                .selected_input_idx
                .min(self.input_device_names.len() - 1);
        }
        if !self.output_device_names.is_empty() {
            self.selected_output_idx = self
                .selected_output_idx
                .min(self.output_device_names.len() - 1);
        }
    }

    pub fn current_sample_rate(&self) -> u32 {
        COMMON_SAMPLE_RATES[self.selected_sample_rate_idx]
    }

    pub fn current_buffer_size(&self) -> u32 {
        BUFFER_SIZE_OPTIONS[self.selected_buffer_size_idx]
            .1
            .unwrap_or(512)
    }

    /// Start passthrough (no plugin).
    pub fn start(&mut self) {
        self.stop();
        self.mode = AudioMode::Passthrough;
        match build_passthrough_streams(
            self.selected_input_idx,
            &self.input_device_names,
            self.selected_output_idx,
            &self.output_device_names,
            COMMON_SAMPLE_RATES[self.selected_sample_rate_idx],
            BUFFER_SIZE_OPTIONS[self.selected_buffer_size_idx],
        ) {
            Ok((in_stream, out_stream, info)) => {
                let _ = in_stream.play();
                let _ = out_stream.play();
                self._streams = Some(StreamHolder::Passthrough(in_stream, out_stream));
                self.running_info = Some(info);
                self.status = AudioStatus::Running;
            }
            Err(e) => {
                self.status = AudioStatus::Error(e);
                self.running_info = None;
            }
        }
    }

    /// Start with a plugin audio processor (instrument mode — output only).
    pub fn start_with_plugin_instrument(
        &mut self,
        processor: PluginAudioProcessor,
    ) {
        self.stop();
        self.mode = AudioMode::PluginInstrument;

        let sample_rate = COMMON_SAMPLE_RATES[self.selected_sample_rate_idx];
        let buffer_opt = BUFFER_SIZE_OPTIONS[self.selected_buffer_size_idx];

        match build_plugin_instrument_stream(
            self.selected_output_idx,
            &self.output_device_names,
            sample_rate,
            buffer_opt,
            processor,
        ) {
            Ok((stream, info)) => {
                let _ = stream.play();
                self._streams = Some(StreamHolder::PluginInstrument(stream));
                self.running_info = Some(info);
                self.status = AudioStatus::Running;
            }
            Err(e) => {
                self.status = AudioStatus::Error(e);
                self.running_info = None;
            }
        }
    }

    /// Start with a plugin audio processor (effect mode — input→plugin→output).
    pub fn start_with_plugin_effect(
        &mut self,
        processor: PluginAudioProcessor,
    ) {
        self.stop();
        self.mode = AudioMode::PluginEffect;

        let sample_rate = COMMON_SAMPLE_RATES[self.selected_sample_rate_idx];
        let buffer_opt = BUFFER_SIZE_OPTIONS[self.selected_buffer_size_idx];

        match build_plugin_effect_stream(
            self.selected_input_idx,
            &self.input_device_names,
            self.selected_output_idx,
            &self.output_device_names,
            sample_rate,
            buffer_opt,
            processor,
        ) {
            Ok((in_stream, out_stream, info)) => {
                let _ = in_stream.play();
                let _ = out_stream.play();
                self._streams = Some(StreamHolder::PluginEffect(in_stream, out_stream));
                self.running_info = Some(info);
                self.status = AudioStatus::Running;
            }
            Err(e) => {
                self.status = AudioStatus::Error(e);
                self.running_info = None;
            }
        }
    }

    pub fn stop(&mut self) {
        self._streams = None;
        self.status = AudioStatus::Stopped;
        self.running_info = None;
    }
}

// ── Helpers ───────────────────────────────────────────────────────────────────

fn enumerate_devices() -> (Vec<String>, Vec<String>) {
    let host = cpal::default_host();

    let mut inputs = vec!["Default".to_string()];
    if let Ok(devs) = host.input_devices() {
        for d in devs {
            if let Ok(name) = d.name() {
                inputs.push(name);
            }
        }
    }

    let mut outputs = vec!["Default".to_string()];
    if let Ok(devs) = host.output_devices() {
        for d in devs {
            if let Ok(name) = d.name() {
                outputs.push(name);
            }
        }
    }

    (inputs, outputs)
}

fn get_input_device(host: &cpal::Host, idx: usize, names: &[String]) -> Result<Device, String> {
    if idx == 0 {
        host.default_input_device()
            .ok_or_else(|| "No default input device found".to_string())
    } else {
        let target = &names[idx];
        host.input_devices()
            .map_err(|e| e.to_string())?
            .find(|d| d.name().map_or(false, |n| n == *target))
            .ok_or_else(|| format!("Input device not found: {target}"))
    }
}

fn get_output_device(host: &cpal::Host, idx: usize, names: &[String]) -> Result<Device, String> {
    if idx == 0 {
        host.default_output_device()
            .ok_or_else(|| "No default output device found".to_string())
    } else {
        let target = &names[idx];
        host.output_devices()
            .map_err(|e| e.to_string())?
            .find(|d| d.name().map_or(false, |n| n == *target))
            .ok_or_else(|| format!("Output device not found: {target}"))
    }
}

// ── Passthrough ───────────────────────────────────────────────────────────────

fn build_passthrough_streams(
    in_idx: usize,
    in_names: &[String],
    out_idx: usize,
    out_names: &[String],
    sample_rate: u32,
    buffer_opt: (&str, Option<u32>),
) -> Result<(Stream, Stream, RunningInfo), String> {
    let host = cpal::default_host();
    let input_device = get_input_device(&host, in_idx, in_names)?;
    let output_device = get_output_device(&host, out_idx, out_names)?;

    let input_name = input_device.name().unwrap_or_else(|_| "Unknown".into());
    let output_name = output_device.name().unwrap_or_else(|_| "Unknown".into());

    let in_default = input_device
        .default_input_config()
        .map_err(|e| format!("Input config error: {e}"))?;
    let out_default = output_device
        .default_output_config()
        .map_err(|e| format!("Output config error: {e}"))?;
    let channels = in_default.channels().min(out_default.channels());

    let buffer_size = match buffer_opt.1 {
        Some(n) => BufferSize::Fixed(n),
        None => BufferSize::Default,
    };

    let config = StreamConfig {
        channels,
        sample_rate: SampleRate(sample_rate),
        buffer_size,
    };

    let capacity = (sample_rate as usize * channels as usize).max(65_536);
    let shared: Arc<Mutex<VecDeque<f32>>> = Arc::new(Mutex::new(VecDeque::with_capacity(capacity)));
    let shared_in = Arc::clone(&shared);
    let shared_out = Arc::clone(&shared);
    let max_fill = capacity;

    let in_stream = input_device
        .build_input_stream(
            &config,
            move |data: &[f32], _info| {
                if let Ok(mut buf) = shared_in.try_lock() {
                    for &s in data {
                        if buf.len() < max_fill {
                            buf.push_back(s);
                        }
                    }
                }
            },
            |err| eprintln!("[audio] input error: {err}"),
            None,
        )
        .map_err(|e| format!("Failed to build input stream: {e}"))?;

    let out_stream = output_device
        .build_output_stream(
            &config,
            move |data: &mut [f32], _info| {
                if let Ok(mut buf) = shared_out.try_lock() {
                    for s in data.iter_mut() {
                        *s = buf.pop_front().unwrap_or(0.0);
                    }
                } else {
                    data.fill(0.0);
                }
            },
            |err| eprintln!("[audio] output error: {err}"),
            None,
        )
        .map_err(|e| format!("Failed to build output stream: {e}"))?;

    let info = RunningInfo {
        input_device: input_name,
        output_device: output_name,
        sample_rate,
        buffer_size: buffer_opt.0.to_string(),
        channels,
    };

    Ok((in_stream, out_stream, info))
}

// ── Plugin instrument (output only) ──────────────────────────────────────────

fn build_plugin_instrument_stream(
    out_idx: usize,
    out_names: &[String],
    sample_rate: u32,
    buffer_opt: (&str, Option<u32>),
    processor: PluginAudioProcessor,
) -> Result<(Stream, RunningInfo), String> {
    let host = cpal::default_host();
    let output_device = get_output_device(&host, out_idx, out_names)?;
    let output_name = output_device.name().unwrap_or_else(|_| "Unknown".into());

    let channels: u16 = 2;
    let buffer_size = match buffer_opt.1 {
        Some(n) => BufferSize::Fixed(n),
        None => BufferSize::Default,
    };

    let config = StreamConfig {
        channels,
        sample_rate: SampleRate(sample_rate),
        buffer_size,
    };

    let processor = Arc::new(Mutex::new(processor));
    let proc_clone = Arc::clone(&processor);

    let out_stream = output_device
        .build_output_stream(
            &config,
            move |data: &mut [f32], _info| {
                if let Ok(mut proc) = proc_clone.try_lock() {
                    proc.process(data, None);
                } else {
                    data.fill(0.0);
                }
            },
            |err| eprintln!("[audio] output error: {err}"),
            None,
        )
        .map_err(|e| format!("Failed to build output stream: {e}"))?;

    let info = RunningInfo {
        input_device: "None (instrument)".into(),
        output_device: output_name,
        sample_rate,
        buffer_size: buffer_opt.0.to_string(),
        channels,
    };

    Ok((out_stream, info))
}

// ── Plugin effect (input → plugin → output) ─────────────────────────────────

fn build_plugin_effect_stream(
    in_idx: usize,
    in_names: &[String],
    out_idx: usize,
    out_names: &[String],
    sample_rate: u32,
    buffer_opt: (&str, Option<u32>),
    processor: PluginAudioProcessor,
) -> Result<(Stream, Stream, RunningInfo), String> {
    let host = cpal::default_host();
    let input_device = get_input_device(&host, in_idx, in_names)?;
    let output_device = get_output_device(&host, out_idx, out_names)?;

    let input_name = input_device.name().unwrap_or_else(|_| "Unknown".into());
    let output_name = output_device.name().unwrap_or_else(|_| "Unknown".into());

    let channels: u16 = 2;
    let buffer_size = match buffer_opt.1 {
        Some(n) => BufferSize::Fixed(n),
        None => BufferSize::Default,
    };

    let config = StreamConfig {
        channels,
        sample_rate: SampleRate(sample_rate),
        buffer_size,
    };

    // Ring buffer for input→output
    let capacity = (sample_rate as usize * channels as usize).max(65_536);
    let input_ring: Arc<Mutex<VecDeque<f32>>> =
        Arc::new(Mutex::new(VecDeque::with_capacity(capacity)));
    let input_ring_writer = Arc::clone(&input_ring);
    let input_ring_reader = Arc::clone(&input_ring);

    let processor = Arc::new(Mutex::new(processor));
    let proc_clone = Arc::clone(&processor);

    let in_stream = input_device
        .build_input_stream(
            &config,
            move |data: &[f32], _info| {
                if let Ok(mut buf) = input_ring_writer.try_lock() {
                    for &s in data {
                        if buf.len() < capacity {
                            buf.push_back(s);
                        }
                    }
                }
            },
            |err| eprintln!("[audio] input error: {err}"),
            None,
        )
        .map_err(|e| format!("Failed to build input stream: {e}"))?;

    let out_stream = output_device
        .build_output_stream(
            &config,
            move |data: &mut [f32], _info| {
                // Read input from ring buffer
                let input_samples: Vec<f32> = if let Ok(mut buf) = input_ring_reader.try_lock() {
                    (0..data.len())
                        .map(|_| buf.pop_front().unwrap_or(0.0))
                        .collect()
                } else {
                    vec![0.0; data.len()]
                };

                if let Ok(mut proc) = proc_clone.try_lock() {
                    proc.process(data, Some(&input_samples));
                } else {
                    data.fill(0.0);
                }
            },
            |err| eprintln!("[audio] output error: {err}"),
            None,
        )
        .map_err(|e| format!("Failed to build output stream: {e}"))?;

    let info = RunningInfo {
        input_device: input_name,
        output_device: output_name,
        sample_rate,
        buffer_size: buffer_opt.0.to_string(),
        channels,
    };

    Ok((in_stream, out_stream, info))
}
src/build_system.rs
rust
use std::io::{BufRead, BufReader};
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::thread;

#[derive(Debug, Clone)]
pub enum BuildMessage {
    Stdout(String),
    Stderr(String),
    Finished { success: bool },
}

#[derive(Debug, Clone, PartialEq)]
pub enum BuildStatus {
    Idle,
    Building,
    Success,
    Failed,
}

pub struct BuildSystem {
    pub status: BuildStatus,
    pub output_lines: Vec<BuildOutputLine>,
    pub receiver: Option<mpsc::Receiver<BuildMessage>>,
    /// Set to the project path on successful build, cleared when consumed.
    pub artifact_ready: Option<std::path::PathBuf>,
}

#[derive(Debug, Clone)]
pub struct BuildOutputLine {
    pub text: String,
    pub is_error: bool,
}

impl BuildSystem {
    pub fn new() -> Self {
        Self {
            status: BuildStatus::Idle,
            output_lines: Vec::new(),
            receiver: None,
            artifact_ready: None,
        }
    }

    pub fn start_build(&mut self, project_path: &Path) {
        if self.status == BuildStatus::Building {
            return;
        }

        self.status = BuildStatus::Building;
        self.artifact_ready = None;
        self.output_lines.clear();
        self.output_lines.push(BuildOutputLine {
            text: format!("Bundling plugin at {}...", project_path.display()),
            is_error: false,
        });

        let (tx, rx) = mpsc::channel();
        self.receiver = Some(rx);

        let path = project_path.to_path_buf();

        thread::spawn(move || {
            let result = run_nih_plug_bundle(&path, &tx);
            let success = result.is_ok();
            let _ = tx.send(BuildMessage::Finished { success });
        });
    }

    pub fn poll(&mut self) {
        let mut just_succeeded = false;

        if let Some(ref receiver) = self.receiver {
            while let Ok(msg) = receiver.try_recv() {
                match msg {
                    BuildMessage::Stdout(line) => {
                        self.output_lines.push(BuildOutputLine {
                            text: line,
                            is_error: false,
                        });
                    }
                    BuildMessage::Stderr(line) => {
                        let is_error = line.contains("error")
                            || line.contains("Error")
                            || line.contains("cannot find");
                        self.output_lines.push(BuildOutputLine {
                            text: line,
                            is_error,
                        });
                    }
                    BuildMessage::Finished { success } => {
                        self.status = if success {
                            just_succeeded = true;
                            BuildStatus::Success
                        } else {
                            BuildStatus::Failed
                        };
                        self.output_lines.push(BuildOutputLine {
                            text: if success {
                                "✓ Bundle succeeded".to_string()
                            } else {
                                "✗ Bundle failed".to_string()
                            },
                            is_error: !success,
                        });
                    }
                }
            }
        }

        if self.status == BuildStatus::Success || self.status == BuildStatus::Failed {
            self.receiver = None;
        }

        // Note: artifact_ready is set by the caller (app.rs) after checking status,
        // because we need the project path context.
        let _ = just_succeeded;
    }
}

/// Run `cargo nih-plug bundle <lib-name> --release` to produce a .clap file.
fn run_nih_plug_bundle(project_path: &Path, tx: &mpsc::Sender<BuildMessage>) -> Result<(), String> {
    // Determine the lib name from Cargo.toml
    let lib_name = crate::plugin_host::loader::get_lib_name(project_path)?;

    let _ = tx.send(BuildMessage::Stdout(format!(
        "Running: cargo nih-plug bundle {} --release",
        lib_name
    )));

    let mut child = Command::new("cargo")
        .arg("nih-plug")
        .arg("bundle")
        .arg(&lib_name)
        .arg("--release")
        .current_dir(project_path)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("Failed to spawn cargo nih-plug bundle: {}. Is cargo-nih-plug installed? (cargo install cargo-nih-plug)", e))?;

    if let Some(stdout) = child.stdout.take() {
        let tx_clone = tx.clone();
        thread::spawn(move || {
            let reader = BufReader::new(stdout);
            for line in reader.lines() {
                if let Ok(line) = line {
                    let _ = tx_clone.send(BuildMessage::Stdout(line));
                }
            }
        });
    }

    if let Some(stderr) = child.stderr.take() {
        let tx_clone = tx.clone();
        thread::spawn(move || {
            let reader = BufReader::new(stderr);
            for line in reader.lines() {
                if let Ok(line) = line {
                    let _ = tx_clone.send(BuildMessage::Stderr(line));
                }
            }
        });
    }

    let exit_status = child
        .wait()
        .map_err(|e| format!("Failed to wait on cargo: {}", e))?;

    if exit_status.success() {
        Ok(())
    } else {
        Err("Bundle failed".to_string())
    }
}
src/app.rs
rust
use crate::audio_engine::{AudioEngine, AudioMode, AudioStatus};
use crate::build_system::BuildSystem;
use crate::midi_engine::MidiEngine;
use crate::plugin_host::midi_bridge::RawMidiEvent;
use crate::plugin_host::{HostStatus, MidiRouting, PluginHost, PluginMode};
use crate::plugin_host::loader::find_clap_bundle;
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
    LoadPlugin,
    UnloadPlugin,
    TogglePluginGui,
}

pub struct PlaygroundApp {
    project: Option<Project>,
    build_system: BuildSystem,
    audio_engine: AudioEngine,
    midi_engine: MidiEngine,
    plugin_host: PluginHost,
    file_browser: ui::file_browser::FileBrowser,
    new_project_dialog: NewProjectDialog,
    settings_panel: SettingsPanel,
    midi_settings_panel: MidiSettingsPanel,
    piano: PianoWidget,
    left_panel_width: f32,
    build_panel_height: f32,
    error_log: Vec<String>,
    /// Auto-load plugin after successful build
    auto_load_on_build: bool,
}

impl PlaygroundApp {
    pub fn new() -> Self {
        Self {
            project: None,
            build_system: BuildSystem::new(),
            audio_engine: AudioEngine::new(),
            midi_engine: MidiEngine::new(),
            plugin_host: PluginHost::new(),
            file_browser: ui::file_browser::FileBrowser::new(),
            new_project_dialog: NewProjectDialog::default(),
            settings_panel: SettingsPanel::default(),
            midi_settings_panel: MidiSettingsPanel::default(),
            piano: PianoWidget::default(),
            left_panel_width: 300.0,
            build_panel_height: 200.0,
            error_log: Vec::new(),
            auto_load_on_build: true,
        }
    }

    fn handle_action(&mut self, action: AppAction) {
        match action {
            AppAction::NewProject => self.new_project_dialog.open(),
            AppAction::OpenProject => {
                if let Some(path) = rfd::FileDialog::new().pick_folder() {
                    match Project::open(path) {
                        Ok(project) => {
                            self.plugin_host.unload();
                            self.audio_engine.stop();
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
            AppAction::LoadPlugin => {
                self.load_plugin();
            }
            AppAction::UnloadPlugin => {
                self.audio_engine.stop();
                self.plugin_host.unload();
            }
            AppAction::TogglePluginGui => {
                if self.plugin_host.is_gui_open() {
                    self.plugin_host.close_gui();
                } else {
                    if let Err(e) = self.plugin_host.open_gui() {
                        self.error_log.push(e);
                    }
                }
            }
        }
    }

    fn load_plugin(&mut self) {
        let Some(ref project) = self.project else {
            self.error_log
                .push("No project open — cannot load plugin".into());
            return;
        };

        // Stop existing audio & unload old plugin
        self.audio_engine.stop();
        self.plugin_host.unload();

        // Find the .clap bundle
        let clap_path = match find_clap_bundle(&project.config.path) {
            Ok(p) => p,
            Err(e) => {
                self.error_log.push(e);
                return;
            }
        };

        // Load the plugin
        if let Err(e) = self.plugin_host.load(&clap_path) {
            self.error_log.push(e);
            return;
        }

        // Activate and start audio
        let sr = self.audio_engine.current_sample_rate();
        let buf_size = self.audio_engine.current_buffer_size();

        match self.plugin_host.activate(sr, 1, buf_size) {
            Ok(processor) => {
                match self.plugin_host.plugin_mode {
                    PluginMode::Instrument => {
                        self.audio_engine
                            .start_with_plugin_instrument(processor);
                    }
                    PluginMode::Effect => {
                        self.audio_engine
                            .start_with_plugin_effect(processor);
                    }
                }
            }
            Err(e) => {
                self.error_log.push(e);
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

    /// Route MIDI from the piano widget / MIDI engine to the plugin.
    fn route_midi_note_on(&mut self, channel: u8, note: u8, velocity: u8) {
        let routing = self.plugin_host.midi_routing;
        let plugin_loaded = self.plugin_host.is_active();

        if plugin_loaded
            && matches!(routing, MidiRouting::PluginOnly | MidiRouting::Both)
        {
            self.plugin_host
                .push_midi_event(RawMidiEvent::note_on(channel, note, velocity));
        }

        if !plugin_loaded
            || matches!(routing, MidiRouting::ExternalOnly | MidiRouting::Both)
        {
            self.midi_engine.send_note_on(channel, note, velocity);
        }
    }

    fn route_midi_note_off(&mut self, channel: u8, note: u8) {
        let routing = self.plugin_host.midi_routing;
        let plugin_loaded = self.plugin_host.is_active();

        if plugin_loaded
            && matches!(routing, MidiRouting::PluginOnly | MidiRouting::Both)
        {
            self.plugin_host
                .push_midi_event(RawMidiEvent::note_off(channel, note));
        }

        if !plugin_loaded
            || matches!(routing, MidiRouting::ExternalOnly | MidiRouting::Both)
        {
            self.midi_engine.send_note_off(channel, note);
        }
    }
}

impl eframe::App for PlaygroundApp {
    fn update(&mut self, ctx: &eframe::egui::Context, _frame: &mut eframe::Frame) {
        self.build_system.poll();
        self.plugin_host.poll_main_thread();

        // Auto-load plugin after successful build
        if self.build_system.status == crate::build_system::BuildStatus::Success
            && self.auto_load_on_build
            && self.project.is_some()
        {
            // Only auto-load once per build
            if self.build_system.artifact_ready.is_none() {
                let project_path = self.project.as_ref().unwrap().config.path.clone();
                self.build_system.artifact_ready = Some(project_path);
                self.load_plugin();
            }
        }

        // Keep repainting when building or plugin is active
        if self.build_system.status == crate::build_system::BuildStatus::Building
            || self.plugin_host.is_active()
        {
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
                            "⏹  Stop"
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

                ui.menu_button("Plugin", |ui| {
                    let loaded = self.plugin_host.is_loaded();

                    if ui
                        .add_enabled(
                            self.project.is_some() && !loaded,
                            egui::Button::new("▶  Load Plugin"),
                        )
                        .clicked()
                    {
                        action = Some(AppAction::LoadPlugin);
                        ui.close();
                    }

                    if ui
                        .add_enabled(loaded, egui::Button::new("⏹  Unload Plugin"))
                        .clicked()
                    {
                        action = Some(AppAction::UnloadPlugin);
                        ui.close();
                    }

                    ui.separator();

                    let has_gui = self.plugin_host.has_gui();
                    let gui_label = if self.plugin_host.is_gui_open() {
                        "Close GUI"
                    } else {
                        "Open GUI"
                    };
                    if ui
                        .add_enabled(loaded && has_gui, egui::Button::new(gui_label))
                        .clicked()
                    {
                        action = Some(AppAction::TogglePluginGui);
                        ui.close();
                    }

                    ui.separator();

                    // Plugin mode toggle
                    ui.label("Plugin Mode:");
                    let mut mode = self.plugin_host.plugin_mode;
                    ui.radio_value(&mut mode, PluginMode::Instrument, "Instrument");
                    ui.radio_value(&mut mode, PluginMode::Effect, "Effect");
                    self.plugin_host.plugin_mode = mode;

                    ui.separator();

                    // MIDI routing
                    ui.label("MIDI Routing:");
                    let mut routing = self.plugin_host.midi_routing;
                    ui.radio_value(&mut routing, MidiRouting::PluginOnly, "Plugin Only");
                    ui.radio_value(&mut routing, MidiRouting::ExternalOnly, "External Only");
                    ui.radio_value(&mut routing, MidiRouting::Both, "Both");
                    self.plugin_host.midi_routing = routing;

                    ui.separator();
                    ui.checkbox(&mut self.auto_load_on_build, "Auto-load on build");
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
                    // Plugin status pill
                    let (plugin_text, plugin_color) = match &self.plugin_host.status {
                        HostStatus::Unloaded => ("● Plugin Off", egui::Color32::DARK_GRAY),
                        HostStatus::Loaded => ("● Plugin Loaded", egui::Color32::YELLOW),
                        HostStatus::Active | HostStatus::Processing => {
                            ("● Plugin Active", egui::Color32::from_rgb(100, 220, 100))
                        }
                        HostStatus::Error(_) => {
                            ("● Plugin Err", egui::Color32::from_rgb(255, 90, 90))
                        }
                    };
                    ui.label(
                        egui::RichText::new(plugin_text)
                            .color(plugin_color)
                            .size(12.0),
                    );

                    ui.separator();

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

        // Right panel — plugin info + piano keyboard + MIDI monitor
        egui::SidePanel::right("piano_panel")
            .resizable(true)
            .default_width(700.0)
            .min_width(400.0)
            .show(ctx, |ui| {
                // ── Plugin info section ───────────────────────────────────
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    ui.label(
                        egui::RichText::new("🔌 Plugin Host")
                            .strong()
                            .size(14.0),
                    );
                });

                ui.horizontal(|ui| {
                    if self.plugin_host.is_loaded() {
                        let name = self
                            .plugin_host
                            .plugin_name
                            .as_deref()
                            .unwrap_or("Unknown");
                        ui.label(format!("Loaded: {}", name));

                        if self.plugin_host.has_gui() {
                            let gui_label = if self.plugin_host.is_gui_open() {
                                "Close GUI"
                            } else {
                                "Open GUI"
                            };
                            if ui.button(gui_label).clicked() {
                                if self.plugin_host.is_gui_open() {
                                    self.plugin_host.close_gui();
                                } else {
                                    if let Err(e) = self.plugin_host.open_gui() {
                                        self.error_log.push(e);
                                    }
                                }
                            }
                        }

                        if ui.button("Unload").clicked() {
                            self.audio_engine.stop();
                            self.plugin_host.unload();
                        }
                    } else {
                        ui.label("No plugin loaded");
                        if self.project.is_some() {
                            if ui.button("Load Plugin").clicked() {
                                self.load_plugin();
                            }
                        }
                    }
                });

                // Plugin mode toggle (inline)
                if self.plugin_host.is_loaded() {
                    ui.horizontal(|ui| {
                        ui.label("Mode:");
                        let mut mode = self.plugin_host.plugin_mode;
                        ui.selectable_value(&mut mode, PluginMode::Instrument, "Instrument");
                        ui.selectable_value(&mut mode, PluginMode::Effect, "Effect");
                        if mode != self.plugin_host.plugin_mode {
                            self.plugin_host.plugin_mode = mode;
                            // Reload to apply mode change
                            self.load_plugin();
                        }

                        ui.separator();
                        ui.label("MIDI:");
                        ui.selectable_value(
                            &mut self.plugin_host.midi_routing,
                            MidiRouting::PluginOnly,
                            "Plugin",
                        );
                        ui.selectable_value(
                            &mut self.plugin_host.midi_routing,
                            MidiRouting::ExternalOnly,
                            "External",
                        );
                        ui.selectable_value(
                            &mut self.plugin_host.midi_routing,
                            MidiRouting::Both,
                            "Both",
                        );
                    });
                }

                ui.separator();

                // ── Piano keyboard section (collapsible) ─────────────────
                egui::CollapsingHeader::new(
                    egui::RichText::new("🎹 Virtual Piano").strong().size(14.0),
                )
                .default_open(true)
                .show(ui, |ui| {
                    ui.horizontal(|ui| {
                        ui.with_layout(
                            egui::Layout::right_to_left(egui::Align::Center),
                            |ui| {
                                use crate::midi_engine::MidiStatus;
                                if self.midi_engine.status == MidiStatus::Disconnected {
                                    if ui.button("Connect MIDI").clicked() {
                                        self.midi_engine.connect();
                                    }
                                }
                            },
                        );
                    });

                    // Collect note events from the piano widget
                    let events = self.piano.show_with_events(ui, &self.midi_engine);
                    for event in events {
                        match event {
                            PianoEvent::NoteOn {
                                channel,
                                note,
                                velocity,
                            } => {
                                self.route_midi_note_on(channel, note, velocity);
                            }
                            PianoEvent::NoteOff { channel, note } => {
                                self.route_midi_note_off(channel, note);
                            }
                        }
                    }
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

/// Events emitted by the piano widget that the app needs to route.
pub enum PianoEvent {
    NoteOn {
        channel: u8,
        note: u8,
        velocity: u8,
    },
    NoteOff {
        channel: u8,
        note: u8,
    },
}
src/ui/midi_panel.rs
rust
use crate::app::PianoEvent;
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

                let (status_text, status_color) = match &engine.status {
                    MidiStatus::Disconnected => ("● Disconnected", egui::Color32::DARK_GRAY),
                    MidiStatus::InputOnly => ("● Input only", egui::Color32::YELLOW),
                    MidiStatus::OutputOnly => ("● Output only", egui::Color32::YELLOW),
                    MidiStatus::Connected => {
                        ("● Connected", egui::Color32::from_rgb(100, 220, 100))
                    }
                    MidiStatus::Error(_) => ("✗  Error", egui::Color32::from_rgb(255, 90, 90)),
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

pub struct PianoWidget {
    held_notes: std::collections::HashSet<u8>,
    hovered_note: Option<u8>,
    pub start_note: u8,
    pub white_key_count: usize,
    pub channel: u8,
    pub velocity: u8,
    pub event_log: Vec<String>,
}

impl Default for PianoWidget {
    fn default() -> Self {
        Self {
            held_notes: std::collections::HashSet::new(),
            hovered_note: None,
            start_note: 36,
            white_key_count: 28,
            channel: 0,
            velocity: 100,
            event_log: Vec::new(),
        }
    }
}

const WHITE_OFFSETS: [u8; 7] = [0, 2, 4, 5, 7, 9, 11];

fn white_key_note(start: u8, white_idx: usize) -> u8 {
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
    /// Original show method for backward compatibility — routes MIDI directly to engine.
    pub fn show(&mut self, ui: &mut egui::Ui, midi: &MidiEngine) {
        let events = self.show_with_events(ui, midi);
        // In this path, events are handled by the caller (app.rs).
        // But we still need to send to midi engine for the non-plugin path.
        // This is now handled by app.rs via route_midi_*.
        let _ = events;
    }

    /// Draw the piano and return note events for the app to route.
    pub fn show_with_events(
        &mut self,
        ui: &mut egui::Ui,
        midi: &MidiEngine,
    ) -> Vec<PianoEvent> {
        let mut output_events = Vec::new();

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
            let events = self.draw_keyboard(ui);
            output_events = events;
            ui.add_space(8.0);
            self.draw_event_log(ui);
        });

        output_events
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
            ui.add(
                egui::Slider::new(&mut self.velocity, 1..=127)
                    .clamping(egui::SliderClamping::Always),
            );

            ui.separator();
            ui.label("Channel:");
            let mut ch_display = self.channel + 1;
            ui.add(
                egui::Slider::new(&mut ch_display, 1..=16)
                    .clamping(egui::SliderClamping::Always),
            );
            self.channel = ch_display - 1;
        });
    }

    fn draw_keyboard(&mut self, ui: &mut egui::Ui) -> Vec<PianoEvent> {
        let mut events = Vec::new();

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

        // Release all held notes when mouse button is lifted
        if response.drag_stopped()
            || (!response.is_pointer_button_down_on() && !self.held_notes.is_empty())
        {
            for note in self.held_notes.drain() {
                events.push(PianoEvent::NoteOff {
                    channel: self.channel,
                    note,
                });
                let msg = format!("◀ NoteOff ch:{} {}", self.channel + 1, note_name(note));
                self.event_log.push(msg);
                if self.event_log.len() > 64 {
                    self.event_log.remove(0);
                }
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
                    events.push(PianoEvent::NoteOn {
                        channel: self.channel,
                        note,
                        velocity: self.velocity,
                    });
                    let msg = format!(
                        "► NoteOn  ch:{} {} vel:{}",
                        self.channel + 1,
                        note_name(note),
                        self.velocity
                    );
                    self.event_log.push(msg);
                    if self.event_log.len() > 64 {
                        self.event_log.remove(0);
                    }
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
                    events.push(PianoEvent::NoteOff {
                        channel: self.channel,
                        note: n,
                    });
                    let msg =
                        format!("◀ NoteOff ch:{} {}", self.channel + 1, note_name(n));
                    self.event_log.push(msg);
                    if self.event_log.len() > 64 {
                        self.event_log.remove(0);
                    }
                }
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

        events
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
Summary of all changed/new files
File	Status
Cargo.toml	Modified — added clack + rtrb deps
src/main.rs	Modified — added mod plugin_host
src/app.rs	Replaced — plugin host integration, MIDI routing, Plugin menu, auto-load
src/audio_engine.rs	Replaced — added start_with_plugin_instrument/effect, AudioMode
src/build_system.rs	Replaced — changed to cargo nih-plug bundle, added artifact_ready
src/plugin_host/mod.rs	New — orchestrator: load, activate, unload, GUI, MIDI push
src/plugin_host/handlers.rs	New — DevHost + all extension impls
src/plugin_host/loader.rs	New — load .clap, find bundle, parse lib name
src/plugin_host/audio.rs	New — PluginAudioProcessor with deferred start_processing()
src/plugin_host/midi_bridge.rs	New — rtrb consumer → CLAP events
src/plugin_host/gui.rs	New — floating GUI management
src/plugin_host/timer.rs	New — timer management + HostTimerImpl
src/ui/midi_panel.rs	Replaced — piano now returns PianoEvents instead of sending MIDI directly
Files unchanged: src/midi_engine.rs, src/project.rs, src/scaffolding.rs, src/templates/*, src/ui/build_panel.rs, src/ui/code_editor.rs, src/ui/file_browser.rs, src/ui/mod.rs, src/ui/new_project_dialog.rs, src/ui/settings_panel.rs, src/ui/top_bar.rs.

Prerequisites: Install cargo-nih-plug with cargo install cargo-nih-plug for the bundle command to work.
