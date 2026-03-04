
I'll implement the full CLAP hosting integration. Here are all the files that need to be created or modified:

New Files
src/plugin_host/mod.rs
rust
#![allow(unsafe_code)]

pub mod audio;
pub mod gui;
pub mod handlers;
pub mod loader;
pub mod midi_bridge;
pub mod timer;

use crate::plugin_host::audio::PluginAudioProcessor;
use crate::plugin_host::gui::Gui;
use crate::plugin_host::handlers::{DevHost, DevHostShared, MainThreadMessage};
use crate::plugin_host::loader::PluginBinary;
use crate::plugin_host::midi_bridge::RawMidiEvent;
use crate::plugin_host::timer::Timers;
use clack_extensions::gui::PluginGui;
use clack_extensions::timer::PluginTimer;
use clack_host::prelude::*;
use crossbeam_channel::{Receiver, Sender, unbounded};
use std::path::Path;
use std::rc::Rc;

#[derive(Debug, Clone, PartialEq)]
pub enum HostStatus {
    Unloaded,
    Loaded,
    Activated,
    Processing,
    Error(String),
}

#[derive(Debug, Clone, PartialEq)]
pub enum PluginMode {
    Instrument,
    Effect,
}

pub struct PluginHost {
    pub status: HostStatus,
    pub plugin_name: Option<String>,
    pub plugin_id: Option<String>,
    pub mode: PluginMode,

    entry: Option<PluginEntry>,
    instance: Option<PluginInstance<DevHost>>,
    main_thread_rx: Option<Receiver<MainThreadMessage>>,
    main_thread_tx: Option<Sender<MainThreadMessage>>,

    pub midi_producer: Option<rtrb::Producer<RawMidiEvent>>,

    gui: Option<Gui>,
    gui_open: bool,

    timers: Option<Rc<Timers>>,
    timer_ext: Option<PluginTimer>,
}

impl PluginHost {
    pub fn new() -> Self {
        Self {
            status: HostStatus::Unloaded,
            plugin_name: None,
            plugin_id: None,
            mode: PluginMode::Instrument,
            entry: None,
            instance: None,
            main_thread_rx: None,
            main_thread_tx: None,
            midi_producer: None,
            gui: None,
            gui_open: false,
            timers: None,
            timer_ext: None,
        }
    }

    pub fn load(&mut self, clap_path: &Path, plugin_id: &str) -> Result<(), String> {
        self.unload();

        let binary = PluginBinary::load(clap_path, plugin_id)?;

        let (tx, rx) = unbounded();
        self.main_thread_tx = Some(tx.clone());
        self.main_thread_rx = Some(rx);

        let host_info = HostInfo::new(
            "NIH-plug Playground",
            "NIH-plug Playground",
            "https://github.com/user/nih-plug-playground",
            "0.1.0",
        )
        .map_err(|e| format!("Failed to create host info: {e}"))?;

        let pid = std::ffi::CString::new(plugin_id).map_err(|e| e.to_string())?;

        let mut instance = PluginInstance::<DevHost>::new(
            |_| DevHostShared::new(tx),
            |shared| handlers::DevHostMainThread::new(shared),
            &binary.entry,
            &pid,
            &host_info,
        )
        .map_err(|e| format!("Failed to instantiate plugin: {e}"))?;

        self.plugin_name = Some(binary.name.clone());
        self.plugin_id = Some(plugin_id.to_string());

        // Check for GUI support
        let gui_ext = instance.access_handler(|h| h.gui);
        if let Some(plugin_gui) = gui_ext {
            let gui = Gui::new(plugin_gui, &mut instance.plugin_handle());
            self.gui = Some(gui);
        }

        // Check for timer support
        self.timer_ext = instance.access_handler(|h| h.timer_support);
        self.timers = instance.access_handler(|h| Some(h.timers.clone()));

        self.entry = Some(binary.entry);
        self.instance = Some(instance);
        self.status = HostStatus::Loaded;

        Ok(())
    }

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

        let (midi_producer, midi_consumer) = rtrb::RingBuffer::new(1024);
        self.midi_producer = Some(midi_producer);

        let processor = PluginAudioProcessor::new(
            instance,
            sample_rate,
            min_buffer_size,
            max_buffer_size,
            midi_consumer,
            self.mode == PluginMode::Effect,
        )?;

        self.status = HostStatus::Activated;
        Ok(processor)
    }

    pub fn unload(&mut self) {
        if self.gui_open {
            self.close_gui();
        }

        self.midi_producer = None;

        // Drop instance before entry
        if let Some(mut instance) = self.instance.take() {
            // Ensure GUI is destroyed
            if let Some(ref mut gui) = self.gui {
                gui.destroy(&mut instance.plugin_handle());
            }
        }

        self.gui = None;
        self.entry = None;
        self.timer_ext = None;
        self.timers = None;
        self.main_thread_rx = None;
        self.main_thread_tx = None;
        self.plugin_name = None;
        self.plugin_id = None;
        self.gui_open = false;
        self.status = HostStatus::Unloaded;
    }

    pub fn poll_main_thread(&mut self) {
        if let Some(ref rx) = self.main_thread_rx {
            while let Ok(msg) = rx.try_recv() {
                match msg {
                    MainThreadMessage::RunOnMainThread => {
                        if let Some(ref mut instance) = self.instance {
                            instance.call_on_main_thread_callback();
                        }
                    }
                    MainThreadMessage::GuiClosed => {
                        self.gui_open = false;
                    }
                    MainThreadMessage::GuiRequestResized { .. } => {
                        // Floating windows handle their own resize
                    }
                }
            }
        }

        // Tick timers
        if let (Some(timers), Some(timer_ext)) = (&self.timers, &self.timer_ext) {
            if let Some(ref mut instance) = self.instance {
                timers.tick_timers(timer_ext, &mut instance.plugin_handle());
            }
        }
    }

    pub fn open_gui(&mut self) -> Result<(), String> {
        let instance = self
            .instance
            .as_mut()
            .ok_or("No plugin loaded")?;

        let gui = self
            .gui
            .as_mut()
            .ok_or("Plugin has no GUI")?;

        let needs_floating = gui
            .needs_floating()
            .ok_or("Plugin GUI not supported on this platform")?;

        if !needs_floating {
            // We only support floating for now
            // Force floating if plugin supports it at all
        }

        gui.open_floating(&mut instance.plugin_handle())
            .map_err(|e| format!("Failed to open GUI: {e}"))?;

        self.gui_open = true;
        Ok(())
    }

    pub fn close_gui(&mut self) {
        if let (Some(ref mut gui), Some(ref mut instance)) =
            (&mut self.gui, &mut self.instance)
        {
            gui.destroy(&mut instance.plugin_handle());
        }
        self.gui_open = false;
    }

    pub fn has_gui(&self) -> bool {
        self.gui
            .as_ref()
            .and_then(|g| g.needs_floating())
            .is_some()
    }

    pub fn is_gui_open(&self) -> bool {
        self.gui_open
    }

    pub fn send_note_on(&mut self, channel: u8, note: u8, velocity: u8) {
        if let Some(ref mut producer) = self.midi_producer {
            let _ = producer.push(RawMidiEvent {
                data: [0x90 | (channel & 0x0F), note & 0x7F, velocity & 0x7F],
                len: 3,
            });
        }
    }

    pub fn send_note_off(&mut self, channel: u8, note: u8) {
        if let Some(ref mut producer) = self.midi_producer {
            let _ = producer.push(RawMidiEvent {
                data: [0x80 | (channel & 0x0F), note & 0x7F, 0],
                len: 3,
            });
        }
    }

    pub fn send_raw_midi(&mut self, bytes: &[u8]) {
        if bytes.len() > 3 || bytes.is_empty() {
            return;
        }
        if let Some(ref mut producer) = self.midi_producer {
            let mut data = [0u8; 3];
            data[..bytes.len()].copy_from_slice(bytes);
            let _ = producer.push(RawMidiEvent {
                data,
                len: bytes.len() as u8,
            });
        }
    }
}
src/plugin_host/handlers.rs
rust
#![allow(unsafe_code)]

use crate::plugin_host::timer::Timers;
use clack_extensions::audio_ports::{HostAudioPortsImpl, RescanType};
use clack_extensions::gui::{GuiSize, HostGui, HostGuiImpl};
use clack_extensions::log::{HostLog, HostLogImpl, LogSeverity};
use clack_extensions::note_ports::{HostNotePortsImpl, NoteDialects, NotePortRescanFlags};
use clack_extensions::params::{
    HostParams, HostParamsImplMainThread, HostParamsImplShared, ParamClearFlags, ParamRescanFlags,
};
use clack_extensions::timer::{HostTimer, HostTimerImpl, PluginTimer};
use clack_host::prelude::*;
use crossbeam_channel::Sender;
use std::rc::Rc;
use std::sync::OnceLock;
use std::time::Duration;

use clack_extensions::audio_ports::PluginAudioPorts;
use clack_extensions::gui::PluginGui;

pub enum MainThreadMessage {
    RunOnMainThread,
    GuiClosed,
    GuiRequestResized { new_size: GuiSize },
}

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

struct PluginCallbacks {
    #[allow(dead_code)]
    audio_ports: Option<PluginAudioPorts>,
}

pub struct DevHostShared {
    pub sender: Sender<MainThreadMessage>,
    callbacks: OnceLock<PluginCallbacks>,
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
    fn initializing(&self, instance: InitializingPluginHandle<'a>) {
        let _ = self.callbacks.set(PluginCallbacks {
            audio_ports: instance.get_extension(),
        });
    }

    fn request_restart(&self) {}

    fn request_process(&self) {}

    fn request_callback(&self) {
        let _ = self.sender.send(MainThreadMessage::RunOnMainThread);
    }
}

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
            gui: None,
            timers: Rc::new(Timers::new()),
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

// ── Extension Implementations ────────────────────────────────────────────────

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
use std::ffi::CString;
use std::path::{Path, PathBuf};

pub struct PluginBinary {
    pub entry: PluginEntry,
    pub name: String,
    pub id: String,
    pub path: PathBuf,
}

impl PluginBinary {
    pub fn load(clap_path: &Path, plugin_id: &str) -> Result<Self, String> {
        if !clap_path.exists() {
            return Err(format!("Plugin file not found: {}", clap_path.display()));
        }

        let entry = unsafe { PluginEntry::load(clap_path) }
            .map_err(|e| format!("Failed to load CLAP entry: {e}"))?;

        let factory = entry
            .get_plugin_factory()
            .ok_or_else(|| "No plugin factory found in CLAP file".to_string())?;

        let mut found_name = None;
        let mut found_id = None;

        for desc in factory.plugin_descriptors() {
            let Some(id) = desc.id() else { continue };
            let id_str = match id.to_str() {
                Ok(s) => s,
                Err(_) => continue,
            };

            if id_str == plugin_id {
                found_id = Some(id_str.to_string());
                found_name = desc.name().map(|n| n.to_string_lossy().to_string());
                break;
            }
        }

        let id = found_id.ok_or_else(|| {
            format!(
                "Plugin with ID '{}' not found in {}",
                plugin_id,
                clap_path.display()
            )
        })?;

        let name = found_name.unwrap_or_else(|| id.clone());

        Ok(PluginBinary {
            entry,
            name,
            id,
            path: clap_path.to_path_buf(),
        })
    }

    /// List all plugin IDs in a .clap file
    pub fn list_ids(clap_path: &Path) -> Result<Vec<(String, Option<String>)>, String> {
        if !clap_path.exists() {
            return Err(format!("File not found: {}", clap_path.display()));
        }

        let entry = unsafe { PluginEntry::load(clap_path) }
            .map_err(|e| format!("Failed to load CLAP entry: {e}"))?;

        let factory = entry
            .get_plugin_factory()
            .ok_or_else(|| "No plugin factory".to_string())?;

        let mut results = Vec::new();
        for desc in factory.plugin_descriptors() {
            let Some(id) = desc.id() else { continue };
            let Ok(id_str) = id.to_str() else { continue };
            let name = desc.name().map(|n| n.to_string_lossy().to_string());
            results.push((id_str.to_string(), name));
        }

        Ok(results)
    }
}

/// Find the .clap bundle in target/bundled after `cargo nih-plug bundle`
pub fn find_clap_bundle(project_path: &Path) -> Result<PathBuf, String> {
    let bundled_dir = project_path.join("target").join("bundled");

    if !bundled_dir.exists() {
        return Err(format!(
            "target/bundled directory not found. Run 'cargo nih-plug bundle' first.\n\
             Path: {}",
            bundled_dir.display()
        ));
    }

    // Look for .clap files
    let entries = std::fs::read_dir(&bundled_dir)
        .map_err(|e| format!("Failed to read bundled dir: {e}"))?;

    let mut clap_files: Vec<PathBuf> = Vec::new();

    for entry in entries {
        let Ok(entry) = entry else { continue };
        let path = entry.path();

        if path.extension().and_then(|e| e.to_str()) == Some("clap") {
            clap_files.push(path);
        }
    }

    match clap_files.len() {
        0 => Err(format!(
            "No .clap files found in {}",
            bundled_dir.display()
        )),
        1 => Ok(clap_files.into_iter().next().unwrap()),
        _ => {
            // Return most recently modified
            clap_files.sort_by(|a, b| {
                let ma = a.metadata().and_then(|m| m.modified()).ok();
                let mb = b.metadata().and_then(|m| m.modified()).ok();
                mb.cmp(&ma)
            });
            Ok(clap_files.into_iter().next().unwrap())
        }
    }
}

/// Extract plugin name from Cargo.toml for bundling command
pub fn get_plugin_lib_name(project_path: &Path) -> Result<String, String> {
    let cargo_toml_path = project_path.join("Cargo.toml");
    let content = std::fs::read_to_string(&cargo_toml_path)
        .map_err(|e| format!("Failed to read Cargo.toml: {e}"))?;

    // Look for [lib] name = "..." first
    let mut in_lib_section = false;
    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed == "[lib]" {
            in_lib_section = true;
            continue;
        }
        if trimmed.starts_with('[') {
            in_lib_section = false;
            continue;
        }
        if in_lib_section && trimmed.starts_with("name") {
            if let Some(val) = extract_toml_string_value(trimmed) {
                return Ok(val);
            }
        }
    }

    // Fallback: use package name
    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("name") {
            if let Some(val) = extract_toml_string_value(trimmed) {
                return Ok(val.replace('-', "_"));
            }
        }
    }

    Err("Could not determine plugin name from Cargo.toml".to_string())
}

fn extract_toml_string_value(line: &str) -> Option<String> {
    let parts: Vec<&str> = line.splitn(2, '=').collect();
    if parts.len() != 2 {
        return None;
    }
    let val = parts[1].trim().trim_matches('"').trim_matches('\'');
    Some(val.to_string())
}
src/plugin_host/audio.rs
rust
#![allow(unsafe_code)]

use crate::plugin_host::handlers::DevHost;
use crate::plugin_host::midi_bridge::{MidiBridge, RawMidiEvent};
use clack_extensions::audio_ports::{
    AudioPortFlags, AudioPortInfoBuffer, AudioPortType, PluginAudioPorts,
};
use clack_extensions::note_ports::{NoteDialects, NotePortInfoBuffer, PluginNotePorts};
use clack_host::prelude::*;
use cpal::FromSample;
use std::sync::mpsc;

// ── Port configuration types ──────────────────────────────────────────────────

#[derive(Clone, Debug)]
pub struct AudioPortConfig {
    pub ports: Vec<AudioPortInfo>,
    pub main_port_index: u32,
}

impl AudioPortConfig {
    fn empty() -> Self {
        Self {
            ports: vec![],
            main_port_index: 0,
        }
    }

    fn default_stereo() -> Self {
        Self {
            ports: vec![AudioPortInfo {
                channel_count: 2,
                name: "Default".into(),
            }],
            main_port_index: 0,
        }
    }

    pub fn main_port(&self) -> &AudioPortInfo {
        &self.ports[self.main_port_index as usize]
    }

    pub fn total_channel_count(&self) -> usize {
        self.ports.iter().map(|p| p.channel_count as usize).sum()
    }
}

#[derive(Clone, Debug)]
pub struct AudioPortInfo {
    pub channel_count: u16,
    pub name: String,
}

// ── Plugin Audio Processor ────────────────────────────────────────────────────

/// This is `Send` so it can be passed to the cpal audio thread.
/// It wraps a `StoppedPluginAudioProcessor` that gets started on first use.
pub struct PluginAudioProcessor {
    stopped: Option<StoppedPluginAudioProcessor<DevHost>>,
    started: Option<StartedPluginAudioProcessor<DevHost>>,
    midi_bridge: MidiBridge,
    input_port_config: AudioPortConfig,
    output_port_config: AudioPortConfig,
    output_channel_count: usize,
    sample_rate: u32,
    max_buffer_size: u32,

    // Audio buffers
    input_ports: AudioPorts,
    output_ports: AudioPorts,
    input_port_channels: Box<[Vec<f32>]>,
    output_port_channels: Box<[Vec<f32>]>,
    muxed: Vec<f32>,
    actual_frame_count: usize,
    steady_counter: u64,
    is_effect: bool,
}

// SAFETY: StoppedPluginAudioProcessor is Send. StartedPluginAudioProcessor is !Send
// but we only start it on the audio thread and never move it after that.
unsafe impl Send for PluginAudioProcessor {}

impl PluginAudioProcessor {
    pub fn new(
        instance: &mut PluginInstance<DevHost>,
        sample_rate: u32,
        min_buffer_size: u32,
        max_buffer_size: u32,
        midi_consumer: rtrb::Consumer<RawMidiEvent>,
        is_effect: bool,
    ) -> Result<Self, String> {
        let input_port_config =
            query_audio_ports(&mut instance.plugin_handle(), true, is_effect);
        let output_port_config =
            query_audio_ports(&mut instance.plugin_handle(), false, false);

        let (note_port_index, prefers_midi) =
            find_main_note_port(instance).unwrap_or((0, true));

        let midi_bridge = MidiBridge::new(midi_consumer, note_port_index, prefers_midi);

        let total_input_channels = input_port_config.total_channel_count();
        let total_output_channels = output_port_config.total_channel_count();
        let frame_count = max_buffer_size as usize;

        let output_channel_count = if output_port_config.ports.is_empty() {
            2
        } else {
            output_port_config.main_port().channel_count as usize
        }
        .min(2);

        let plugin_config = PluginAudioConfiguration {
            sample_rate: sample_rate as f64,
            min_frames_count: min_buffer_size,
            max_frames_count: max_buffer_size,
        };

        let stopped = instance
            .activate(|_, _| (), plugin_config)
            .map_err(|e| format!("Failed to activate plugin: {e}"))?;

        let input_port_channels: Box<[Vec<f32>]> = input_port_config
            .ports
            .iter()
            .map(|p| vec![0.0; frame_count * p.channel_count as usize])
            .collect();

        let output_port_channels: Box<[Vec<f32>]> = output_port_config
            .ports
            .iter()
            .map(|p| vec![0.0; frame_count * p.channel_count as usize])
            .collect();

        Ok(Self {
            stopped: Some(stopped),
            started: None,
            midi_bridge,
            input_port_config,
            output_port_config,
            output_channel_count,
            sample_rate,
            max_buffer_size,
            input_ports: AudioPorts::with_capacity(total_input_channels, 8),
            output_ports: AudioPorts::with_capacity(total_output_channels, 8),
            input_port_channels,
            output_port_channels,
            muxed: vec![0.0; frame_count * output_channel_count.max(2)],
            actual_frame_count: frame_count,
            steady_counter: 0,
            is_effect,
        })
    }

    pub fn output_channel_count(&self) -> usize {
        self.output_channel_count
    }

    /// Called on the audio thread. Starts processing on first call.
    fn ensure_started(&mut self) {
        if self.started.is_none() {
            if let Some(stopped) = self.stopped.take() {
                match stopped.start_processing() {
                    Ok(started) => {
                        self.started = Some(started);
                    }
                    Err(e) => {
                        eprintln!("[plugin_host] Failed to start processing: {e}");
                    }
                }
            }
        }
    }

    /// Process audio. Called from the cpal output callback.
    /// `input_data` is interleaved input samples (or empty for synth mode).
    /// `output_data` is the interleaved output buffer to fill.
    pub fn process<S: FromSample<f32> + cpal::Sample>(
        &mut self,
        input_data: &[f32],
        output_data: &mut [S],
    ) {
        self.ensure_started();

        let Some(ref mut processor) = self.started else {
            // Fill silence if not started
            output_data.iter_mut().for_each(|s| *s = S::EQUILIBRIUM);
            return;
        };

        // Ensure buffers are large enough
        let frame_count = output_data.len() / self.output_channel_count.max(1);
        if frame_count > self.actual_frame_count {
            self.actual_frame_count = frame_count;
            for (buf, port) in self
                .input_port_channels
                .iter_mut()
                .zip(&self.input_port_config.ports)
            {
                buf.resize(frame_count * port.channel_count as usize, 0.0);
            }
            for (buf, port) in self
                .output_port_channels
                .iter_mut()
                .zip(&self.output_port_config.ports)
            {
                buf.resize(frame_count * port.channel_count as usize, 0.0);
            }
            self.muxed
                .resize(frame_count * self.output_channel_count, 0.0);
        }

        // De-interleave input data into input port buffers (for effects)
        if self.is_effect && !input_data.is_empty() && !self.input_port_channels.is_empty() {
            let input_channels = self
                .input_port_config
                .ports
                .first()
                .map(|p| p.channel_count as usize)
                .unwrap_or(2);
            let buf = &mut self.input_port_channels[0];
            let input_frames = input_data.len() / input_channels.max(1);
            let frames_to_copy = input_frames.min(frame_count);

            for frame in 0..frames_to_copy {
                for ch in 0..input_channels {
                    let interleaved_idx = frame * input_channels + ch;
                    let deinterleaved_idx = ch * self.actual_frame_count + frame;
                    if interleaved_idx < input_data.len() && deinterleaved_idx < buf.len() {
                        buf[deinterleaved_idx] = input_data[interleaved_idx];
                    }
                }
            }
        } else {
            // Silence input for synths
            self.input_port_channels.iter_mut().for_each(|b| b.fill(0.0));
        }

        // Clear output
        self.output_port_channels.iter_mut().for_each(|b| b.fill(0.0));

        // Build plugin buffers
        let (ins, mut outs) = self.prepare_plugin_buffers(frame_count);

        // Get MIDI events
        let events = self.midi_bridge.drain_to_input_events(frame_count as u64);

        match processor.process(
            &ins,
            &mut outs,
            &events,
            &mut OutputEvents::void(),
            Some(self.steady_counter),
            None,
        ) {
            Ok(_) => {
                self.write_output(output_data, frame_count);
            }
            Err(e) => {
                eprintln!("[plugin_host] process error: {e}");
                output_data.iter_mut().for_each(|s| *s = S::EQUILIBRIUM);
            }
        }

        self.steady_counter += frame_count as u64;
    }

    fn prepare_plugin_buffers(
        &mut self,
        sample_count: usize,
    ) -> (InputAudioBuffers<'_>, OutputAudioBuffers<'_>) {
        let actual = self.actual_frame_count;

        (
            self.input_ports.with_input_buffers(
                self.input_port_channels.iter_mut().map(|port_buf| {
                    AudioPortBuffer {
                        latency: 0,
                        channels: AudioPortBufferType::f32_input_only(
                            port_buf
                                .chunks_exact_mut(actual)
                                .map(|buffer| InputChannel {
                                    buffer: &mut buffer[..sample_count],
                                    is_constant: false,
                                }),
                        ),
                    }
                }),
            ),
            self.output_ports.with_output_buffers(
                self.output_port_channels.iter_mut().map(|port_buf| {
                    AudioPortBuffer {
                        latency: 0,
                        channels: AudioPortBufferType::f32_output_only(
                            port_buf
                                .chunks_exact_mut(actual)
                                .map(|buf| &mut buf[..sample_count]),
                        ),
                    }
                }),
            ),
        )
    }

    fn write_output<S: FromSample<f32>>(&mut self, destination: &mut [S], frame_count: usize) {
        if self.output_port_channels.is_empty() {
            destination.iter_mut().for_each(|s| *s = S::EQUILIBRIUM);
            return;
        }

        let main_idx = self.output_port_config.main_port_index as usize;
        let main_output = &self.output_port_channels[main_idx];
        let plugin_channels = self.output_port_config.main_port().channel_count as usize;
        let out_channels = self.output_channel_count;
        let actual = self.actual_frame_count;

        // Interleave output
        for frame in 0..frame_count {
            for ch in 0..out_channels {
                let out_idx = frame * out_channels + ch;
                if out_idx >= destination.len() {
                    break;
                }

                let src_ch = if ch < plugin_channels { ch } else { 0 };
                let src_idx = src_ch * actual + frame;

                let sample = if src_idx < main_output.len() {
                    main_output[src_idx]
                } else {
                    0.0
                };

                destination[out_idx] = S::from_sample(sample);
            }
        }
    }
}

// ── Port querying ─────────────────────────────────────────────────────────────

fn query_audio_ports(
    plugin: &mut PluginMainThreadHandle,
    is_input: bool,
    needs_ports: bool,
) -> AudioPortConfig {
    let Some(ports_ext) = plugin.get_extension::<PluginAudioPorts>() else {
        return if needs_ports {
            AudioPortConfig::default_stereo()
        } else if is_input {
            AudioPortConfig::empty()
        } else {
            AudioPortConfig::default_stereo()
        };
    };

    let mut buffer = AudioPortInfoBuffer::new();
    let mut main_port_index = None;
    let mut discovered = vec![];

    let count = ports_ext.count(plugin, is_input);
    for i in 0..count {
        let Some(info) = ports_ext.get(plugin, i, is_input, &mut buffer) else {
            continue;
        };

        let port_type = info
            .port_type
            .or_else(|| AudioPortType::from_channel_count(info.channel_count));

        let channel_count = match port_type {
            Some(t) if t == AudioPortType::MONO => 1,
            Some(t) if t == AudioPortType::STEREO => 2,
            _ => info.channel_count as u16,
        };

        if info.flags.contains(AudioPortFlags::IS_MAIN) {
            main_port_index = Some(i);
        }

        discovered.push(AudioPortInfo {
            channel_count,
            name: String::from_utf8_lossy(info.name).into_owned(),
        });
    }

    if discovered.is_empty() {
        return if is_input {
            AudioPortConfig::empty()
        } else {
            AudioPortConfig::default_stereo()
        };
    }

    AudioPortConfig {
        main_port_index: main_port_index.unwrap_or(0),
        ports: discovered,
    }
}

fn find_main_note_port(instance: &mut PluginInstance<DevHost>) -> Option<(u16, bool)> {
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
src/plugin_host/midi_bridge.rs
rust
use clack_host::events::event_types::{MidiEvent, NoteOffEvent, NoteOnEvent};
use clack_host::events::{EventFlags, Match};
use clack_host::prelude::*;
use rtrb::Consumer;

#[derive(Clone, Copy, Debug)]
pub struct RawMidiEvent {
    pub data: [u8; 3],
    pub len: u8,
}

pub struct MidiBridge {
    consumer: Consumer<RawMidiEvent>,
    event_buffer: EventBuffer,
    note_port_index: u16,
    prefers_midi: bool,
}

impl MidiBridge {
    pub fn new(consumer: Consumer<RawMidiEvent>, note_port_index: u16, prefers_midi: bool) -> Self {
        Self {
            consumer,
            event_buffer: EventBuffer::with_capacity(256),
            note_port_index,
            prefers_midi,
        }
    }

    pub fn drain_to_input_events(&mut self, _frame_count: u64) -> InputEvents<'_> {
        self.event_buffer.clear();

        while let Ok(raw) = self.consumer.pop() {
            let bytes = &raw.data[..raw.len as usize];
            if bytes.is_empty() {
                continue;
            }

            let status = bytes[0] & 0xF0;
            let channel = bytes[0] & 0x0F;

            if !self.prefers_midi && bytes.len() >= 3 {
                match status {
                    0x90 if bytes[2] > 0 => {
                        let velocity = bytes[2] as f64 / 127.0;
                        self.event_buffer.push(
                            &NoteOnEvent::new(
                                0,
                                Pckn::new(
                                    self.note_port_index,
                                    channel,
                                    bytes[1] as u16,
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
                                0,
                                Pckn::new(
                                    self.note_port_index,
                                    channel,
                                    bytes[1] as u16,
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

            // Fallback: send as raw MIDI
            if bytes.len() >= 3 {
                let mut buf = [0u8; 3];
                buf.copy_from_slice(&bytes[..3]);
                self.event_buffer.push(
                    &MidiEvent::new(0, self.note_port_index, buf)
                        .with_flags(EventFlags::IS_LIVE),
                );
            }
        }

        self.event_buffer.as_input()
    }
}
src/plugin_host/gui.rs
rust
use crate::plugin_host::handlers::{DevHostShared, MainThreadMessage};
use clack_extensions::gui::{
    GuiApiType, GuiConfiguration, GuiError, GuiSize, HostGuiImpl, PluginGui,
};
use clack_host::prelude::*;

pub struct Gui {
    plugin_gui: PluginGui,
    pub configuration: Option<GuiConfiguration<'static>>,
    is_open: bool,
}

impl Gui {
    pub fn new(plugin_gui: PluginGui, instance: &mut PluginMainThreadHandle) -> Self {
        Self {
            configuration: Self::negotiate_configuration(&plugin_gui, instance),
            plugin_gui,
            is_open: false,
        }
    }

    fn negotiate_configuration(
        gui: &PluginGui,
        plugin: &mut PluginMainThreadHandle,
    ) -> Option<GuiConfiguration<'static>> {
        let api_type = GuiApiType::default_for_current_platform()?;

        // Try embedded first
        let config = GuiConfiguration {
            api_type,
            is_floating: false,
        };

        if gui.is_api_supported(plugin, config) {
            // We'll use floating anyway for now, but the plugin supports the platform
            return Some(GuiConfiguration {
                api_type,
                is_floating: false,
            });
        }

        // Try floating
        let floating = GuiConfiguration {
            api_type,
            is_floating: true,
        };

        if gui.is_api_supported(plugin, floating) {
            Some(floating)
        } else {
            None
        }
    }

    pub fn needs_floating(&self) -> Option<bool> {
        // We always use floating for now
        self.configuration.map(|_| true)
    }

    pub fn open_floating(&mut self, plugin: &mut PluginMainThreadHandle) -> Result<(), GuiError> {
        let Some(mut configuration) = self.configuration else {
            return Err(GuiError::CreateError);
        };

        // Force floating mode
        configuration.is_floating = true;

        self.plugin_gui.create(plugin, configuration)?;
        self.plugin_gui
            .suggest_title(plugin, c"NIH-plug Playground - Plugin Editor");
        self.plugin_gui.show(plugin)?;

        self.is_open = true;
        Ok(())
    }

    pub fn destroy(&mut self, plugin: &mut PluginMainThreadHandle) {
        if self.is_open {
            self.plugin_gui.destroy(plugin);
            self.is_open = false;
        }
    }
}
src/plugin_host/timer.rs
rust
use clack_extensions::timer::{PluginTimer, TimerId};
use clack_host::prelude::PluginMainThreadHandle;
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::time::{Duration, Instant};

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

    fn tick_all(&self) -> Vec<TimerId> {
        let mut timers = self.timers.borrow_mut();
        let now = Instant::now();

        timers
            .values_mut()
            .filter_map(|t| t.tick(now).then_some(t.id))
            .collect()
    }

    pub fn tick_timers(&self, timer_ext: &PluginTimer, plugin: &mut PluginMainThreadHandle) {
        for triggered in self.tick_all() {
            timer_ext.on_timer(plugin, triggered);
        }
    }

    pub fn register_new(&self, interval: Duration) -> TimerId {
        const MAX_INTERVAL: Duration = Duration::from_millis(10);
        let interval = interval.max(MAX_INTERVAL);

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
Modified Files
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
    "clack-host",
    "audio-ports",
    "note-ports",
    "gui",
    "log",
    "params",
    "timer",
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
src/audio_engine.rs
rust
use crate::plugin_host::audio::PluginAudioProcessor;
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{BufferSize, Device, SampleRate, Stream, StreamConfig};
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

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

#[derive(Debug, Clone, PartialEq)]
pub enum AudioStatus {
    Stopped,
    Running,
    Error(String),
}

#[derive(Debug, Clone, PartialEq)]
pub enum AudioMode {
    Passthrough,
    PluginInstrument,
    PluginEffect,
}

#[derive(Debug, Clone)]
pub struct RunningInfo {
    pub input_device: String,
    pub output_device: String,
    pub sample_rate: u32,
    pub buffer_size: String,
    pub channels: u16,
    pub mode: AudioMode,
}

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
    PluginOutput(Stream),
    PluginInputOutput(Stream, Stream),
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

    /// Start passthrough (no plugin)
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
                if let Err(e) = in_stream.play() {
                    eprintln!("[audio] input play(): {e}");
                }
                if let Err(e) = out_stream.play() {
                    eprintln!("[audio] output play(): {e}");
                }
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

    /// Start with a plugin processor (instrument mode — output only)
    pub fn start_with_plugin_instrument(
        &mut self,
        processor: PluginAudioProcessor,
    ) {
        self.stop();
        self.mode = AudioMode::PluginInstrument;

        match build_plugin_instrument_stream(
            self.selected_output_idx,
            &self.output_device_names,
            COMMON_SAMPLE_RATES[self.selected_sample_rate_idx],
            BUFFER_SIZE_OPTIONS[self.selected_buffer_size_idx],
            processor,
        ) {
            Ok((stream, info)) => {
                if let Err(e) = stream.play() {
                    eprintln!("[audio] output play(): {e}");
                }
                self._streams = Some(StreamHolder::PluginOutput(stream));
                self.running_info = Some(info);
                self.status = AudioStatus::Running;
            }
            Err(e) => {
                self.status = AudioStatus::Error(e);
                self.running_info = None;
            }
        }
    }

    /// Start with a plugin processor (effect mode — input + output)
    pub fn start_with_plugin_effect(
        &mut self,
        processor: PluginAudioProcessor,
    ) {
        self.stop();
        self.mode = AudioMode::PluginEffect;

        match build_plugin_effect_stream(
            self.selected_input_idx,
            &self.input_device_names,
            self.selected_output_idx,
            &self.output_device_names,
            COMMON_SAMPLE_RATES[self.selected_sample_rate_idx],
            BUFFER_SIZE_OPTIONS[self.selected_buffer_size_idx],
            processor,
        ) {
            Ok((in_stream, out_stream, info)) => {
                if let Err(e) = in_stream.play() {
                    eprintln!("[audio] input play(): {e}");
                }
                if let Err(e) = out_stream.play() {
                    eprintln!("[audio] output play(): {e}");
                }
                self._streams = Some(StreamHolder::PluginInputOutput(in_stream, out_stream));
                self.running_info = Some(info);
                self.status = AudioStatus::Running;
            }
            Err(e) => {
                self.status = AudioStatus::Error(e);
                self.running_info = None;
            }
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

    pub fn stop(&mut self) {
        self._streams = None;
        self.status = AudioStatus::Stopped;
        self.running_info = None;
    }
}

// ── Private helpers ───────────────────────────────────────────────────────────

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

    let input_name = input_device
        .name()
        .unwrap_or_else(|_| "Unknown".to_string());
    let output_name = output_device
        .name()
        .unwrap_or_else(|_| "Unknown".to_string());

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
        mode: AudioMode::Passthrough,
    };

    Ok((in_stream, out_stream, info))
}

fn build_plugin_instrument_stream(
    out_idx: usize,
    out_names: &[String],
    sample_rate: u32,
    buffer_opt: (&str, Option<u32>),
    processor: PluginAudioProcessor,
) -> Result<(Stream, RunningInfo), String> {
    let host = cpal::default_host();
    let output_device = get_output_device(&host, out_idx, out_names)?;
    let output_name = output_device
        .name()
        .unwrap_or_else(|_| "Unknown".to_string());

    let channels = processor.output_channel_count().min(2).max(1) as u16;

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

    let out_stream = output_device
        .build_output_stream(
            &config,
            move |data: &mut [f32], _info| {
                if let Ok(mut proc) = processor.try_lock() {
                    proc.process::<f32>(&[], data);
                } else {
                    data.fill(0.0);
                }
            },
            |err| eprintln!("[audio] output error: {err}"),
            None,
        )
        .map_err(|e| format!("Failed to build output stream: {e}"))?;

    let info = RunningInfo {
        input_device: "None".to_string(),
        output_device: output_name,
        sample_rate,
        buffer_size: buffer_opt.0.to_string(),
        channels,
        mode: AudioMode::PluginInstrument,
    };

    Ok((out_stream, info))
}

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

    let input_name = input_device
        .name()
        .unwrap_or_else(|_| "Unknown".to_string());
    let output_name = output_device
        .name()
        .unwrap_or_else(|_| "Unknown".to_string());

    let channels = processor.output_channel_count().min(2).max(1) as u16;

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
    let input_ring: Arc<Mutex<VecDeque<f32>>> =
        Arc::new(Mutex::new(VecDeque::with_capacity(capacity)));

    let input_ring_writer = Arc::clone(&input_ring);
    let input_ring_reader = Arc::clone(&input_ring);
    let max_fill = capacity;

    let in_stream = input_device
        .build_input_stream(
            &config,
            move |data: &[f32], _info| {
                if let Ok(mut buf) = input_ring_writer.try_lock() {
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

    let processor = Arc::new(Mutex::new(processor));

    let out_stream = output_device
        .build_output_stream(
            &config,
            move |data: &mut [f32], _info| {
                // Collect input samples
                let mut input_buf = vec![0.0f32; data.len()];
                if let Ok(mut ring) = input_ring_reader.try_lock() {
                    for s in input_buf.iter_mut() {
                        *s = ring.pop_front().unwrap_or(0.0);
                    }
                }

                if let Ok(mut proc) = processor.try_lock() {
                    proc.process::<f32>(&input_buf, data);
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
        mode: AudioMode::PluginEffect,
    };

    Ok((in_stream, out_stream, info))
}
src/build_system.rs
rust
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::thread;

#[derive(Debug, Clone)]
pub enum BuildMessage {
    Stdout(String),
    Stderr(String),
    Finished { success: bool },
    ArtifactReady(PathBuf),
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
    pub last_artifact: Option<PathBuf>,
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
            last_artifact: None,
        }
    }

    /// Start a nih-plug bundle build
    pub fn start_build(&mut self, project_path: &Path) {
        if self.status == BuildStatus::Building {
            return;
        }

        self.status = BuildStatus::Building;
        self.output_lines.clear();
        self.last_artifact = None;
        self.output_lines.push(BuildOutputLine {
            text: format!("Building plugin at {}...", project_path.display()),
            is_error: false,
        });

        let (tx, rx) = mpsc::channel();
        self.receiver = Some(rx);

        let path = project_path.to_path_buf();

        thread::spawn(move || {
            let result = run_nih_plug_bundle(&path, &tx);
            let success = result.is_ok();

            // If successful, look for the .clap artifact
            if success {
                if let Ok(clap_path) =
                    crate::plugin_host::loader::find_clap_bundle(&path)
                {
                    let _ = tx.send(BuildMessage::ArtifactReady(clap_path));
                }
            }

            let _ = tx.send(BuildMessage::Finished { success });
        });
    }

    pub fn poll(&mut self) {
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
                            BuildStatus::Success
                        } else {
                            BuildStatus::Failed
                        };
                        self.output_lines.push(BuildOutputLine {
                            text: if success {
                                "✓ Build succeeded".to_string()
                            } else {
                                "✗ Build failed".to_string()
                            },
                            is_error: !success,
                        });
                    }
                    BuildMessage::ArtifactReady(path) => {
                        self.last_artifact = Some(path.clone());
                        self.output_lines.push(BuildOutputLine {
                            text: format!(
                                "✓ Plugin artifact: {}",
                                path.display()
                            ),
                            is_error: false,
                        });
                    }
                }
            }
        }

        if self.status == BuildStatus::Success || self.status == BuildStatus::Failed {
            self.receiver = None;
        }
    }
}

fn run_nih_plug_bundle(
    project_path: &Path,
    tx: &mpsc::Sender<BuildMessage>,
) -> Result<(), String> {
    // First, get the plugin lib name for the bundle command
    let lib_name = crate::plugin_host::loader::get_plugin_lib_name(project_path)?;

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
        .map_err(|e| {
            format!(
                "Failed to spawn 'cargo nih-plug bundle'. \
                 Make sure cargo-nih-plug is installed: \
                 cargo install --git https://github.com/robbert-vdh/nih-plug.git cargo-nih-plug\n\
                 Error: {}",
                e
            )
        })?;

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
        Err("Build failed".to_string())
    }
}
src/app.rs
rust
use crate::audio_engine::{AudioEngine, AudioMode, AudioStatus};
use crate::build_system::{BuildStatus, BuildSystem};
use crate::midi_engine::MidiEngine;
use crate::plugin_host::loader::{find_clap_bundle, PluginBinary};
use crate::plugin_host::{HostStatus, PluginHost, PluginMode};
use crate::project::Project;
use crate::scaffolding::{scaffold_project, ScaffoldOptions};
use crate::ui;
use crate::ui::midi_panel::{MidiSettingsPanel, PianoWidget};
use crate::ui::new_project_dialog::{NewProjectDialog, NewProjectResult};
use crate::ui::settings_panel::SettingsPanel;
use eframe::egui;
use std::time::Duration;

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
    OpenPluginGui,
    ClosePluginGui,
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
    auto_load_after_build: bool,
    midi_to_plugin: bool,
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
            auto_load_after_build: true,
            midi_to_plugin: true,
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
            AppAction::OpenPluginGui => {
                if let Err(e) = self.plugin_host.open_gui() {
                    self.error_log.push(e);
                }
            }
            AppAction::ClosePluginGui => {
                self.plugin_host.close_gui();
            }
        }
    }

    fn load_plugin(&mut self) {
        let project_path = match &self.project {
            Some(p) => p.config.path.clone(),
            None => {
                self.error_log
                    .push("No project open".to_string());
                return;
            }
        };

        // Stop existing audio
        self.audio_engine.stop();
        self.plugin_host.unload();

        // Find .clap artifact
        let clap_path = match find_clap_bundle(&project_path) {
            Ok(p) => p,
            Err(e) => {
                self.error_log.push(e);
                return;
            }
        };

        // List plugin IDs in the .clap
        let ids = match PluginBinary::list_ids(&clap_path) {
            Ok(ids) => ids,
            Err(e) => {
                self.error_log.push(e);
                return;
            }
        };

        if ids.is_empty() {
            self.error_log
                .push("No plugins found in .clap file".to_string());
            return;
        }

        let (plugin_id, plugin_name) = &ids[0];

        // Load plugin
        if let Err(e) = self.plugin_host.load(&clap_path, plugin_id) {
            self.error_log.push(e);
            return;
        }

        // Set mode
        self.plugin_host.mode = if self.plugin_host.mode == PluginMode::Effect {
            PluginMode::Effect
        } else {
            PluginMode::Instrument
        };

        // Activate and start audio
        let sr = self.audio_engine.current_sample_rate();
        let buf = self.audio_engine.current_buffer_size();

        match self.plugin_host.activate(sr, 1, buf) {
            Ok(processor) => {
                match self.plugin_host.mode {
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
                return;
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

    fn check_auto_load(&mut self) {
        if self.auto_load_after_build
            && self.build_system.status == BuildStatus::Success
            && self.build_system.last_artifact.is_some()
        {
            let artifact = self.build_system.last_artifact.take();
            if artifact.is_some() {
                self.load_plugin();
            }
        }
    }
}

impl eframe::App for PlaygroundApp {
    fn update(&mut self, ctx: &eframe::egui::Context, _frame: &mut eframe::Frame) {
        self.build_system.poll();

        // Poll plugin host for main thread callbacks
        self.plugin_host.poll_main_thread();

        // Forward MIDI to plugin if loaded
        if self.midi_to_plugin && self.plugin_host.status != HostStatus::Unloaded {
            for event in self.midi_engine.drain_events() {
                use crate::midi_engine::MidiMessageKind;
                match event.kind {
                    MidiMessageKind::NoteOn {
                        channel,
                        note,
                        velocity,
                    } => {
                        self.plugin_host.send_note_on(channel, note, velocity);
                    }
                    MidiMessageKind::NoteOff { channel, note } => {
                        self.plugin_host.send_note_off(channel, note);
                    }
                    MidiMessageKind::ControlChange { channel, cc, value } => {
                        self.plugin_host
                            .send_raw_midi(&[0xB0 | channel, cc, value]);
                    }
                    MidiMessageKind::PitchBend { channel, value } => {
                        let raw = (value + 8192) as u16;
                        self.plugin_host.send_raw_midi(&[
                            0xE0 | channel,
                            (raw & 0x7F) as u8,
                            ((raw >> 7) & 0x7F) as u8,
                        ]);
                    }
                    MidiMessageKind::Other(bytes) => {
                        self.plugin_host.send_raw_midi(&bytes);
                    }
                }
            }
        }

        // Check if we should auto-load after a successful build
        self.check_auto_load();

        if self.build_system.status == BuildStatus::Building
            || self.plugin_host.status != HostStatus::Unloaded
        {
            ctx.request_repaint_after(Duration::from_millis(16));
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
                    let loaded = self.plugin_host.status != HostStatus::Unloaded;
                    let has_artifact = self.build_system.last_artifact.is_some()
                        || self.project.as_ref().map_or(false, |p| {
                            find_clap_bundle(&p.config.path).is_ok()
                        });

                    if ui
                        .add_enabled(
                            self.project.is_some() && has_artifact && !loaded,
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
                    if ui
                        .add_enabled(
                            loaded && self.plugin_host.has_gui(),
                            egui::Button::new(if self.plugin_host.is_gui_open() {
                                "Close GUI"
                            } else {
                                "Open GUI"
                            }),
                        )
                        .clicked()
                    {
                        if self.plugin_host.is_gui_open() {
                            action = Some(AppAction::ClosePluginGui);
                        } else {
                            action = Some(AppAction::OpenPluginGui);
                        }
                        ui.close();
                    }
                    ui.separator();

                    // Plugin mode toggle
                    ui.label("Mode:");
                    let mut is_effect = self.plugin_host.mode == PluginMode::Effect;
                    if ui
                        .selectable_label(!is_effect, "🎹 Instrument")
                        .clicked()
                    {
                        self.plugin_host.mode = PluginMode::Instrument;
                    }
                    if ui
                        .selectable_label(is_effect, "🔊 Effect")
                        .clicked()
                    {
                        self.plugin_host.mode = PluginMode::Effect;
                    }

                    ui.separator();
                    ui.checkbox(&mut self.auto_load_after_build, "Auto-load after build");
                    ui.checkbox(&mut self.midi_to_plugin, "Route MIDI to plugin");
                });

                ui.separator();

                // Build button
                let build_enabled = self.project.is_some()
                    && self.build_system.status != BuildStatus::Building;
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
                    BuildStatus::Idle => ("Ready", egui::Color32::GRAY),
                    BuildStatus::Building => ("Building...", egui::Color32::YELLOW),
                    BuildStatus::Success => ("Build OK", egui::Color32::GREEN),
                    BuildStatus::Failed => ("Build Failed", egui::Color32::RED),
                };
                ui.label(egui::RichText::new(build_text).color(build_color));

                // Status pills (right side)
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    // Plugin pill
                    let (plugin_text, plugin_color) = match &self.plugin_host.status {
                        HostStatus::Unloaded => ("● Plugin Off", egui::Color32::DARK_GRAY),
                        HostStatus::Loaded => ("● Plugin Loaded", egui::Color32::YELLOW),
                        HostStatus::Activated | HostStatus::Processing => {
                            let name = self
                                .plugin_host
                                .plugin_name
                                .as_deref()
                                .unwrap_or("Plugin");
                            // Can't format into RichText easily, so we'll just use a fixed label
                            ("● Plugin Active", egui::Color32::from_rgb(100, 220, 100))
                        }
                        HostStatus::Error(_) => {
                            ("● Plugin Err", egui::Color32::from_rgb(255, 90, 90))
                        }
                    };
                    if ui
                        .add(
                            egui::Label::new(
                                egui::RichText::new(plugin_text)
                                    .color(plugin_color)
                                    .size(12.0),
                            )
                            .sense(egui::Sense::click()),
                        )
                        .on_hover_text(
                            self.plugin_host
                                .plugin_name
                                .as_deref()
                                .unwrap_or("No plugin loaded"),
                        )
                        .clicked()
                    {
                        if self.plugin_host.status == HostStatus::Unloaded {
                            action = Some(AppAction::LoadPlugin);
                        }
                    }

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

        // Right panel — plugin controls + piano keyboard + MIDI monitor
        egui::SidePanel::right("plugin_panel")
            .resizable(true)
            .default_width(700.0)
            .min_width(400.0)
            .show(ctx, |ui| {
                // ── Plugin controls ───────────────────────────────────────
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    ui.label(
                        egui::RichText::new("🔌 Plugin Host")
                            .strong()
                            .size(14.0),
                    );

                    ui.with_layout(
                        egui::Layout::right_to_left(egui::Align::Center),
                        |ui| {
                            let loaded =
                                self.plugin_host.status != HostStatus::Unloaded;

                            if loaded {
                                if ui.button("⏹ Unload").clicked() {
                                    self.audio_engine.stop();
                                    self.plugin_host.unload();
                                }

                                if self.plugin_host.has_gui() {
                                    if self.plugin_host.is_gui_open() {
                                        if ui.button("Close GUI").clicked() {
                                            self.plugin_host.close_gui();
                                        }
                                    } else {
                                        if ui.button("Open GUI").clicked() {
                                            if let Err(e) =
                                                self.plugin_host.open_gui()
                                            {
                                                self.error_log.push(e);
                                            }
                                        }
                                    }
                                }
                            } else {
                                let can_load = self.project.is_some();
                                if ui
                                    .add_enabled(
                                        can_load,
                                        egui::Button::new("▶ Load"),
                                    )
                                    .clicked()
                                {
                                    self.load_plugin();
                                }
                            }
                        },
                    );
                });

                // Plugin status info
                match &self.plugin_host.status {
                    HostStatus::Unloaded => {
                        ui.label(
                            egui::RichText::new("No plugin loaded")
                                .color(egui::Color32::GRAY)
                                .size(12.0),
                        );
                    }
                    HostStatus::Loaded
                    | HostStatus::Activated
                    | HostStatus::Processing => {
                        if let Some(ref name) = self.plugin_host.plugin_name {
                            ui.label(
                                egui::RichText::new(format!("Plugin: {}", name))
                                    .color(egui::Color32::from_rgb(100, 220, 100))
                                    .size(12.0),
                            );
                        }

                        ui.horizontal(|ui| {
                            ui.label("Mode:");
                            let is_instrument =
                                self.plugin_host.mode == PluginMode::Instrument;
                            if ui
                                .selectable_label(is_instrument, "🎹 Instrument")
                                .clicked()
                            {
                                self.plugin_host.mode = PluginMode::Instrument;
                            }
                            if ui
                                .selectable_label(!is_instrument, "🔊 Effect")
                                .clicked()
                            {
                                self.plugin_host.mode = PluginMode::Effect;
                            }
                        });

                        ui.horizontal(|ui| {
                            ui.checkbox(
                                &mut self.midi_to_plugin,
                                "Route MIDI → Plugin",
                            );
                            ui.checkbox(
                                &mut self.auto_load_after_build,
                                "Auto-load on build",
                            );
                        });
                    }
                    HostStatus::Error(e) => {
                        ui.label(
                            egui::RichText::new(format!("Error: {}", e))
                                .color(egui::Color32::from_rgb(255, 90, 90))
                                .size(12.0),
                        );
                    }
                }

                ui.separator();

                // ── Piano keyboard (collapsible) ──────────────────────────
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
                                if self.midi_engine.status
                                    == MidiStatus::Disconnected
                                {
                                    if ui.button("Connect MIDI").clicked() {
                                        self.midi_engine.connect();
                                    }
                                }
                            },
                        );
                    });

                    self.piano
                        .show(ui, &self.midi_engine, &mut self.plugin_host, self.midi_to_plugin);
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
src/ui/midi_panel.rs
rust
use crate::midi_engine::{MidiEngine, MidiMessageKind, MidiStatus};
use crate::plugin_host::{HostStatus, PluginHost};
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
    pub fn show(
        &mut self,
        ui: &mut egui::Ui,
        midi: &MidiEngine,
        plugin_host: &mut PluginHost,
        midi_to_plugin: bool,
    ) {
        // Drain incoming MIDI events into the log (only when not routing to plugin,
        // since the app handles that separately)
        if !midi_to_plugin {
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
        }

        ui.vertical(|ui| {
            self.draw_controls(ui);
            ui.add_space(6.0);
            self.draw_keyboard(ui, midi, plugin_host, midi_to_plugin);
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

    fn draw_keyboard(
        &mut self,
        ui: &mut egui::Ui,
        midi: &MidiEngine,
        plugin_host: &mut PluginHost,
        midi_to_plugin: bool,
    ) {
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

        let plugin_loaded = plugin_host.status != HostStatus::Unloaded;

        let mut newly_pressed: Option<u8> = None;
        let mut newly_released: Vec<u8> = Vec::new();

        if response.drag_stopped()
            || (!response.is_pointer_button_down_on() && !self.held_notes.is_empty())
        {
            for note in self.held_notes.drain() {
                if plugin_loaded && midi_to_plugin {
                    plugin_host.send_note_off(self.channel, note);
                } else {
                    midi.send_note_off(self.channel, note);
                }
                newly_released.push(note);
            }
        }

        let mut hovered: Option<u8> = None;

        if let Some(pos) = mouse_pos {
            let local = pos - origin;
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
            if hovered.is_none() {
                let wi = (local.x / white_w) as usize;
                if wi < self.white_key_count {
                    hovered = Some(white_key_note(self.start_note, wi));
                }
            }
        }

        self.hovered_note = hovered;

        if response.is_pointer_button_down_on() {
            if let Some(note) = hovered {
                if !self.held_notes.contains(&note) {
                    self.held_notes.insert(note);
                    if plugin_loaded && midi_to_plugin {
                        plugin_host.send_note_on(self.channel, note, self.velocity);
                    } else {
                        midi.send_note_on(self.channel, note, self.velocity);
                    }
                    newly_pressed = Some(note);
                }
                let to_release: Vec<u8> = self
                    .held_notes
                    .iter()
                    .copied()
                    .filter(|&n| n != note)
                    .collect();
                for n in to_release {
                    self.held_notes.remove(&n);
                    if plugin_loaded && midi_to_plugin {
                        plugin_host.send_note_off(self.channel, n);
                    } else {
                        midi.send_note_off(self.channel, n);
                    }
                }
            }
        }

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
Summary of changes
File	Status
Cargo.toml	Modified — added clack-host, clack-extensions, rtrb
src/main.rs	Modified — added mod plugin_host
src/app.rs	Replaced — plugin host integration, auto-load, MIDI routing, Plugin menu
src/audio_engine.rs	Replaced — added start_with_plugin_instrument(), start_with_plugin_effect(), AudioMode
src/build_system.rs	Replaced — uses cargo nih-plug bundle, emits ArtifactReady
src/ui/midi_panel.rs	Replaced — piano routes to plugin when loaded, collapsible
src/plugin_host/mod.rs	New — lifecycle orchestrator
src/plugin_host/handlers.rs	New — DevHost + all extension impls
src/plugin_host/loader.rs	New — .clap discovery + loading
src/plugin_host/audio.rs	New — PluginAudioProcessor with port config + processing
src/plugin_host/midi_bridge.rs	New — rtrb MIDI → CLAP event conversion
src/plugin_host/gui.rs	New — floating GUI management
src/plugin_host/timer.rs	New — timer support for plugin UI
