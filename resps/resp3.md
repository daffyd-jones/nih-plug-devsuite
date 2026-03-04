
I'll provide all the files needed. This is a substantial implementation, so I'll work through each file methodically.

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
wmidi = "4.0.6"

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

/// Controls whether we route audio input to the plugin (effect) or not (synth).
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum PluginMode {
    Synth,
    Effect,
}

pub struct PluginHost {
    pub status: HostStatus,
    pub plugin_name: Option<String>,
    pub plugin_id: Option<String>,
    pub mode: PluginMode,
    pub gui_open: bool,

    // Keep entry alive (holds the .so/.dylib/.dll)
    _entry: Option<PluginEntry>,
    instance: Option<PluginInstance<DevHost>>,
    main_thread_rx: Receiver<MainThreadMessage>,
    main_thread_tx: Sender<MainThreadMessage>,

    // MIDI bridge: producer side for UI/MIDI input to push events
    midi_producer: Option<rtrb::Producer<RawMidiEvent>>,

    // GUI state
    gui: Option<Gui>,

    // Loaded binary path for reload
    pub loaded_path: Option<PathBuf>,
}

impl PluginHost {
    pub fn new() -> Self {
        let (tx, rx) = unbounded();
        Self {
            status: HostStatus::Unloaded,
            plugin_name: None,
            plugin_id: None,
            mode: PluginMode::Synth,
            gui_open: false,
            _entry: None,
            instance: None,
            main_thread_rx: rx,
            main_thread_tx: tx,
            midi_producer: None,
            gui: None,
            loaded_path: None,
        }
    }

    /// Load a .clap plugin from a file path. Finds the first plugin ID in the bundle.
    pub fn load(&mut self, path: &Path) -> Result<(), String> {
        self.unload();

        let binary = loader::load_clap_bundle(path)?;

        let (tx, rx) = unbounded();
        self.main_thread_tx = tx.clone();
        self.main_thread_rx = rx;

        let plugin_id_cstr = CString::new(binary.plugin_id.as_str())
            .map_err(|e| format!("Invalid plugin ID: {e}"))?;

        let host_info = HostInfo::new(
            "NIH-plug Playground",
            "NIH-plug Playground",
            "https://github.com",
            "0.1.0",
        )
        .map_err(|e| format!("Failed to create host info: {e}"))?;

        let instance = PluginInstance::<DevHost>::new(
            |_| DevHostShared::new(tx.clone()),
            |shared| DevHostMainThread::new(shared),
            &binary.entry,
            &plugin_id_cstr,
            &host_info,
        )
        .map_err(|e| format!("Failed to instantiate plugin: {e}"))?;

        // Check for GUI support
        let gui_ext = instance.access_handler(|h| h.gui);

        self.plugin_name = Some(binary.plugin_name.clone());
        self.plugin_id = Some(binary.plugin_id.clone());
        self._entry = Some(binary.entry);
        self.loaded_path = Some(path.to_path_buf());

        // Store instance, then set up GUI
        self.instance = Some(instance);

        if let Some(gui_ext) = gui_ext {
            if let Some(ref mut inst) = self.instance {
                let gui = Gui::new(gui_ext, &mut inst.plugin_handle());
                self.gui = Some(gui);
            }
        }

        self.status = HostStatus::Loaded;
        Ok(())
    }

    /// Activate the plugin and return a PluginAudioProcessor for the audio thread.
    /// The processor is Send and can be moved to the cpal callback.
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

        // Query audio port configs
        let input_port_config =
            audio::config::get_config_from_ports(&mut instance.plugin_handle(), true);
        let output_port_config =
            audio::config::get_config_from_ports(&mut instance.plugin_handle(), false);

        // Set up MIDI bridge
        let (midi_producer, midi_consumer) = rtrb::RingBuffer::new(256);
        self.midi_producer = Some(midi_producer);

        let midi_bridge = midi_bridge::MidiBridge::new(
            midi_consumer,
            sample_rate as u64,
            instance,
        );

        let plugin_config = PluginAudioConfiguration {
            sample_rate: sample_rate as f64,
            min_frames_count: min_buffer_size,
            max_frames_count: max_buffer_size,
        };

        // Activate returns a StoppedPluginAudioProcessor which IS Send
        let stopped_processor = instance
            .activate(|_, _| (), plugin_config)
            .map_err(|e| format!("Failed to activate plugin: {e}"))?;

        let config = PluginAudioConfig {
            sample_rate,
            min_buffer_size,
            max_buffer_size,
            input_port_config,
            output_port_config,
            mode: self.mode,
        };

        self.status = HostStatus::Active;

        Ok(PluginAudioProcessor::new(
            stopped_processor,
            midi_bridge,
            config,
        ))
    }

    /// Unload everything in the correct order: GUI → deactivate → drop instance → drop entry
    pub fn unload(&mut self) {
        // Close GUI first
        if self.gui_open {
            self.close_gui();
        }

        // Drop GUI state
        if let (Some(ref mut gui), Some(ref mut inst)) = (&mut self.gui, &mut self.instance) {
            gui.destroy(&mut inst.plugin_handle());
        }
        self.gui = None;

        // Drop MIDI producer
        self.midi_producer = None;

        // Drop instance (this deactivates if needed)
        self.instance = None;

        // Drop entry (unloads the .so)
        self._entry = None;

        self.status = HostStatus::Unloaded;
        self.plugin_name = None;
        self.plugin_id = None;
        self.loaded_path = None;
        self.gui_open = false;
    }

    /// Must be called every frame from the main/UI thread.
    pub fn poll_main_thread(&mut self) {
        while let Ok(msg) = self.main_thread_rx.try_recv() {
            match msg {
                MainThreadMessage::RunOnMainThread => {
                    if let Some(ref mut inst) = self.instance {
                        inst.call_on_main_thread_callback();
                    }
                }
                MainThreadMessage::GuiClosed => {
                    self.gui_open = false;
                }
                MainThreadMessage::GuiRequestResized { .. } => {
                    // Floating windows manage their own size
                }
            }
        }

        // Tick timers
        if let Some(ref mut inst) = self.instance {
            let timer_data = inst.access_handler(|h| {
                h.timer_support.map(|ext| (h.timers.clone(), ext))
            });
            if let Some((timers, timer_ext)) = timer_data {
                timers.tick_timers(&timer_ext, &mut inst.plugin_handle());
            }
        }
    }

    /// Open the plugin GUI as a floating window.
    pub fn open_gui(&mut self) -> Result<(), String> {
        let gui = self.gui.as_mut().ok_or("Plugin has no GUI")?;
        let instance = self.instance.as_mut().ok_or("No plugin loaded")?;

        match gui.needs_floating() {
            Some(true) | None => {
                gui.open_floating(&mut instance.plugin_handle())
                    .map_err(|e| format!("Failed to open GUI: {e}"))?;
            }
            Some(false) => {
                // For now, force floating even if plugin prefers embedded
                // We'd need a native child window for true embedding in egui
                gui.open_floating(&mut instance.plugin_handle())
                    .map_err(|e| format!("Failed to open GUI: {e}"))?;
            }
        }

        self.gui_open = true;
        Ok(())
    }

    /// Close the plugin GUI.
    pub fn close_gui(&mut self) {
        if let (Some(ref mut gui), Some(ref mut inst)) = (&mut self.gui, &mut self.instance) {
            gui.destroy(&mut inst.plugin_handle());
        }
        self.gui_open = false;
    }

    /// Send a note-on to the plugin via the MIDI bridge.
    pub fn send_note_on(&self, channel: u8, note: u8, velocity: u8) {
        if let Some(ref producer) = self.midi_producer {
            // Producer is not &mut, we need interior mutability — handled in push
            // rtrb::Producer requires &mut, so we use a wrapper. See midi_bridge.
        }
        // We use a separate sender pattern — see below
    }

    /// Send a note-off to the plugin via the MIDI bridge.
    pub fn send_note_off(&self, channel: u8, note: u8) {
        // Same as above
    }

    /// Check if a plugin is loaded and ready.
    pub fn is_loaded(&self) -> bool {
        matches!(
            self.status,
            HostStatus::Loaded | HostStatus::Active | HostStatus::Processing
        )
    }

    /// Check if plugin has GUI support.
    pub fn has_gui(&self) -> bool {
        self.gui
            .as_ref()
            .map(|g| g.configuration.is_some())
            .unwrap_or(false)
    }
}

/// Thread-safe MIDI sender that can be cloned and shared.
/// Wraps a crossbeam sender since rtrb::Producer is not Clone/shared.
#[derive(Clone)]
pub struct MidiSender {
    tx: crossbeam_channel::Sender<RawMidiEvent>,
}

impl MidiSender {
    pub fn send_note_on(&self, channel: u8, note: u8, velocity: u8) {
        let _ = self.tx.send(RawMidiEvent {
            data: [0x90 | (channel & 0x0F), note & 0x7F, velocity & 0x7F],
            len: 3,
        });
    }

    pub fn send_note_off(&self, channel: u8, note: u8) {
        let _ = self.tx.send(RawMidiEvent {
            data: [0x80 | (channel & 0x0F), note & 0x7F, 0],
            len: 3,
        });
    }

    pub fn send_raw(&self, bytes: &[u8]) {
        if bytes.len() <= 3 {
            let mut data = [0u8; 3];
            data[..bytes.len()].copy_from_slice(bytes);
            let _ = self.tx.send(RawMidiEvent {
                data,
                len: bytes.len() as u8,
            });
        }
    }
}

/// Create a linked MidiSender + consumer pair.
/// The consumer feeds into the rtrb ring buffer on the forwarding thread.
pub fn create_midi_channel() -> (MidiSender, crossbeam_channel::Receiver<RawMidiEvent>) {
    let (tx, rx) = crossbeam_channel::unbounded();
    (MidiSender { tx }, rx)
}
src/plugin_host/handlers.rs

rust
#![allow(unsafe_code)]

use crate::plugin_host::gui::Gui;
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

/// Messages sent from plugin threads to the main thread.
pub enum MainThreadMessage {
    RunOnMainThread,
    GuiClosed,
    GuiRequestResized { new_size: GuiSize },
}

/// The host type marker.
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

/// Shared data accessible from any thread.
pub struct DevHostShared {
    pub sender: Sender<MainThreadMessage>,
    callbacks: OnceLock<PluginCallbacks>,
}

struct PluginCallbacks {
    _audio_ports: Option<clack_extensions::audio_ports::PluginAudioPorts>,
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
            _audio_ports: instance.get_extension(),
        });
    }

    fn request_restart(&self) {
        // Not supported
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

// ── Extension implementations ─────────────────────────────────────────────────

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

/// A loaded CLAP plugin binary with its entry and metadata.
pub struct PluginBinary {
    pub entry: PluginEntry,
    pub plugin_id: String,
    pub plugin_name: String,
    pub plugin_version: Option<String>,
    pub path: PathBuf,
}

/// Load a .clap bundle file and return the first plugin found in it.
pub fn load_clap_bundle(path: &Path) -> Result<PluginBinary, String> {
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

    let plugin_id = found_id.ok_or_else(|| "No valid plugin ID found in CLAP file".to_string())?;
    let plugin_name = found_name.unwrap_or_else(|| plugin_id.clone());

    Ok(PluginBinary {
        entry,
        plugin_id,
        plugin_name,
        plugin_version: found_version,
        path: path.to_path_buf(),
    })
}

/// Find the .clap bundle produced by `cargo nih-plug bundle` for a given project.
///
/// Searches `target/bundled/` for a `.clap` file.
pub fn find_plugin_clap(project_path: &Path) -> Result<PathBuf, String> {
    let bundled_dir = project_path.join("target").join("bundled");

    if !bundled_dir.exists() {
        return Err(format!(
            "No target/bundled directory found at {}. Run 'cargo nih-plug bundle' first.",
            bundled_dir.display()
        ));
    }

    // Look for .clap files
    let entries = std::fs::read_dir(&bundled_dir)
        .map_err(|e| format!("Failed to read bundled dir: {e}"))?;

    let mut clap_files: Vec<PathBuf> = Vec::new();

    for entry in entries.flatten() {
        let path = entry.path();
        // On macOS .clap is a directory (bundle), on Linux/Windows it's a file
        let is_clap = path
            .extension()
            .map(|ext| ext == "clap")
            .unwrap_or(false);
        if is_clap {
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
            // Pick the most recently modified
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

use clack_extensions::timer::HostTimerImpl;
use clack_host::prelude::HostError;

use crate::plugin_host::handlers::DevHostMainThread;

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
                .map(|since| since > self.interval)
                .unwrap_or(false)
        } else {
            true
        };

        if triggered {
            self.last_triggered_at = Some(now);
        }

        triggered
    }
}
src/plugin_host/gui.rs

rust
#![allow(unsafe_code)]

use clack_extensions::gui::{
    GuiApiType, GuiConfiguration, GuiError, GuiSize, PluginGui,
};
use clack_host::prelude::*;

/// Tracks a plugin's GUI state and configuration.
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

        // Try floating first since that's all we support right now
        let floating_config = GuiConfiguration {
            api_type,
            is_floating: true,
        };

        if gui.is_api_supported(plugin, floating_config) {
            return Some(floating_config);
        }

        // Try embedded as fallback (we'll still open it floating for now)
        let embedded_config = GuiConfiguration {
            api_type,
            is_floating: false,
        };

        if gui.is_api_supported(plugin, embedded_config) {
            Some(embedded_config)
        } else {
            None
        }
    }

    pub fn needs_floating(&self) -> Option<bool> {
        self.configuration.map(|c| c.is_floating)
    }

    pub fn open_floating(
        &mut self,
        plugin: &mut PluginMainThreadHandle,
    ) -> Result<(), GuiError> {
        let configuration = self
            .configuration
            .ok_or(GuiError::CreateError)?;

        // If the negotiated config is embedded, re-create as floating
        let config = if !configuration.is_floating {
            GuiConfiguration {
                is_floating: true,
                ..configuration
            }
        } else {
            configuration
        };

        self.plugin_gui.create(plugin, config)?;
        self.plugin_gui
            .suggest_title(plugin, c"NIH-plug Playground - Plugin");

        // Get initial size
        let _size = self.plugin_gui.get_size(plugin).unwrap_or(GuiSize {
            width: 640,
            height: 480,
        });

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

    pub fn is_open(&self) -> bool {
        self.is_open
    }
}
src/plugin_host/midi_bridge.rs

rust
use crate::plugin_host::handlers::DevHost;

use clack_extensions::note_ports::{NoteDialects, NotePortInfoBuffer, PluginNotePorts};
use clack_host::events::event_types::{MidiEvent, NoteOffEvent, NoteOnEvent};
use clack_host::events::{EventFlags, Match};
use clack_host::prelude::*;

/// A raw MIDI event with up to 3 bytes.
#[derive(Clone, Copy, Debug)]
pub struct RawMidiEvent {
    pub data: [u8; 3],
    pub len: u8,
}

/// Receives MIDI events from the UI/MIDI thread and converts them to CLAP events
/// for the audio thread.
pub struct MidiBridge {
    consumer: rtrb::Consumer<RawMidiEvent>,
    /// Backup channel for events from the crossbeam MIDI sender
    backup_consumer: crossbeam_channel::Receiver<RawMidiEvent>,
    event_buffer: EventBuffer,
    sample_rate: u64,
    note_port_index: u16,
    prefers_midi: bool,
}

impl MidiBridge {
    pub fn new(
        consumer: rtrb::Consumer<RawMidiEvent>,
        sample_rate: u64,
        instance: &mut PluginInstance<DevHost>,
    ) -> Option<Self> {
        let (note_port_index, prefers_midi) = find_main_note_port_index(instance)?;

        Some(Self {
            consumer,
            backup_consumer: crossbeam_channel::never(),
            event_buffer: EventBuffer::with_capacity(256),
            sample_rate,
            note_port_index,
            prefers_midi,
        })
    }

    /// Attach a crossbeam receiver as a secondary MIDI source.
    /// This allows the MidiSender (which is Clone + Send) to feed events.
    pub fn set_backup_receiver(&mut self, rx: crossbeam_channel::Receiver<RawMidiEvent>) {
        self.backup_consumer = rx;
    }

    /// Drain all pending MIDI events and return them as CLAP InputEvents.
    pub fn drain_to_input_events(&mut self, frame_count: u32) -> InputEvents<'_> {
        self.event_buffer.clear();

        // Drain rtrb ring buffer (lock-free, audio-safe)
        while let Ok(evt) = self.consumer.pop() {
            self.push_event(evt, 0); // timestamp 0 = start of buffer
        }

        // Drain crossbeam backup channel
        while let Ok(evt) = self.backup_consumer.try_recv() {
            self.push_event(evt, 0);
        }

        self.event_buffer.as_input()
    }

    fn push_event(&mut self, evt: RawMidiEvent, sample_time: u32) {
        if evt.len == 0 {
            return;
        }

        let data = &evt.data[..evt.len as usize];

        if !self.prefers_midi && data.len() >= 2 {
            let status = data[0] & 0xF0;
            let channel = data[0] & 0x0F;

            match status {
                0x90 if data.len() >= 3 && data[2] > 0 => {
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
                    return;
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
                    return;
                }
                _ => {}
            }
        }

        // Fallback: send as raw MIDI
        if data.len() >= 1 && data.len() <= 3 {
            let mut buf = [0u8; 3];
            buf[..data.len()].copy_from_slice(data);
            self.event_buffer.push(
                &MidiEvent::new(sample_time, self.note_port_index, buf)
                    .with_flags(EventFlags::IS_LIVE),
            );
        }
    }
}

fn find_main_note_port_index(
    instance: &mut PluginInstance<DevHost>,
) -> Option<(u16, bool)> {
    let mut handle = instance.plugin_handle();
    let plugin_note_ports = handle.get_extension::<PluginNotePorts>()?;

    let mut buffer = NotePortInfoBuffer::new();
    let count = plugin_note_ports
        .count(&mut handle, true)
        .min(u16::MAX as u32);

    for i in 0..count {
        let Some(info) = plugin_note_ports.get(&mut handle, i, true, &mut buffer) else {
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
src/plugin_host/audio/mod.rs

rust
pub mod config;

use crate::plugin_host::handlers::DevHost;
use crate::plugin_host::midi_bridge::MidiBridge;
use crate::plugin_host::PluginMode;
use config::PluginAudioPortsConfig;

use clack_host::prelude::*;
use cpal::FromSample;
use std::sync::Mutex;

/// Full audio configuration for the plugin.
pub struct PluginAudioConfig {
    pub sample_rate: u32,
    pub min_buffer_size: u32,
    pub max_buffer_size: u32,
    pub input_port_config: PluginAudioPortsConfig,
    pub output_port_config: PluginAudioPortsConfig,
    pub mode: PluginMode,
}

/// The audio processor that lives on the audio thread.
/// Created in a "stopped" state and started on first process call.
///
/// This is Send because it holds a StoppedPluginAudioProcessor (which is Send)
/// until start_processing() is called on the audio thread.
pub struct PluginAudioProcessor {
    state: Mutex<ProcessorState>,
    midi_bridge: Option<MidiBridge>,
    config: PluginAudioConfig,

    // Audio buffers
    input_ports: AudioPorts,
    output_ports: AudioPorts,
    input_port_channels: Box<[Vec<f32>]>,
    output_port_channels: Box<[Vec<f32>]>,
    muxed: Vec<f32>,
    actual_frame_count: usize,
    output_channel_count: usize,
    steady_counter: u64,
}

enum ProcessorState {
    Stopped(StoppedPluginAudioProcessor<DevHost>),
    Started(StartedPluginAudioProcessor<DevHost>),
    Transitioning, // temporary state during start
}

// We implement Send manually. StoppedPluginAudioProcessor is Send.
// Once started, the processor must stay on the audio thread.
// The Mutex ensures safe transition.
unsafe impl Send for PluginAudioProcessor {}

impl PluginAudioProcessor {
    pub fn new(
        stopped: StoppedPluginAudioProcessor<DevHost>,
        midi_bridge: Option<MidiBridge>,
        config: PluginAudioConfig,
    ) -> Self {
        let frame_count = config.max_buffer_size as usize;

        let total_input_channels = config.input_port_config.total_channel_count();
        let total_output_channels = config.output_port_config.total_channel_count();

        let output_channel_count = if config.output_port_config.ports.is_empty() {
            2 // default stereo
        } else {
            config
                .output_port_config
                .main_port()
                .port_layout
                .channel_count() as usize
        }
        .min(2); // cap at stereo

        let input_port_channels: Box<[Vec<f32>]> = config
            .input_port_config
            .ports
            .iter()
            .map(|p| vec![0.0; frame_count * p.port_layout.channel_count() as usize])
            .collect();

        let output_port_channels: Box<[Vec<f32>]> = config
            .output_port_config
            .ports
            .iter()
            .map(|p| vec![0.0; frame_count * p.port_layout.channel_count() as usize])
            .collect();

        let muxed = vec![0.0; frame_count * output_channel_count];

        Self {
            state: Mutex::new(ProcessorState::Stopped(stopped)),
            midi_bridge,
            config,
            input_ports: AudioPorts::with_capacity(
                total_input_channels,
                config.input_port_config.ports.len().max(1),
            ),
            output_ports: AudioPorts::with_capacity(
                total_output_channels,
                config.output_port_config.ports.len().max(1),
            ),
            input_port_channels,
            output_port_channels,
            muxed,
            actual_frame_count: frame_count,
            output_channel_count,
            steady_counter: 0,
        }
    }

    /// Get the output channel count for cpal configuration.
    pub fn output_channel_count(&self) -> usize {
        self.output_channel_count
    }

    /// Process audio. Call this from the cpal output callback.
    pub fn process<S: FromSample<f32>>(&mut self, output: &mut [S], input: Option<&[f32]>) {
        // Ensure we're in Started state
        self.ensure_started();

        let cpal_frame_count = output.len() / self.output_channel_count;
        self.ensure_buffer_size(cpal_frame_count);

        // Fill input buffers from cpal input if in effect mode
        if let (PluginMode::Effect, Some(input_data)) = (self.config.mode, input) {
            self.fill_input_from_cpal(input_data, cpal_frame_count);
        }

        // Clear output buffers
        for buf in self.output_port_channels.iter_mut() {
            buf.fill(0.0);
        }

        // Prepare plugin buffers
        let (ins, mut outs) =
            self.prepare_plugin_buffers(cpal_frame_count);

        // Get MIDI events
        let events = if let Some(ref mut midi) = self.midi_bridge {
            midi.drain_to_input_events(cpal_frame_count as u32)
        } else {
            InputEvents::empty()
        };

        // Process
        let state = self.state.get_mut().unwrap();
        if let ProcessorState::Started(ref mut processor) = state {
            match processor.process(
                &ins,
                &mut outs,
                &events,
                &mut OutputEvents::void(),
                Some(self.steady_counter),
                None,
            ) {
                Ok(_) => self.write_output(output),
                Err(e) => {
                    eprintln!("[plugin process error] {e}");
                    output.iter_mut().for_each(|s| *s = S::EQUILIBRIUM);
                }
            }
        } else {
            output.iter_mut().for_each(|s| *s = S::EQUILIBRIUM);
        }

        self.steady_counter += cpal_frame_count as u64;
    }

    fn ensure_started(&mut self) {
        let state = self.state.get_mut().unwrap();
        match state {
            ProcessorState::Started(_) => {}
            ProcessorState::Stopped(_) => {
                let old = std::mem::replace(state, ProcessorState::Transitioning);
                if let ProcessorState::Stopped(stopped) = old {
                    match stopped.start_processing() {
                        Ok(started) => {
                            *state = ProcessorState::Started(started);
                        }
                        Err(e) => {
                            eprintln!("[plugin] Failed to start processing: {e}");
                            // Put it back as stopped
                            *state = ProcessorState::Stopped(e.into_stopped_processor());
                        }
                    }
                }
            }
            ProcessorState::Transitioning => {}
        }
    }

    fn ensure_buffer_size(&mut self, frame_count: usize) {
        if frame_count > self.actual_frame_count {
            self.actual_frame_count = frame_count;

            for (buf, port) in self
                .input_port_channels
                .iter_mut()
                .zip(&self.config.input_port_config.ports)
            {
                buf.resize(
                    frame_count * port.port_layout.channel_count() as usize,
                    0.0,
                );
            }

            for (buf, port) in self
                .output_port_channels
                .iter_mut()
                .zip(&self.config.output_port_config.ports)
            {
                buf.resize(
                    frame_count * port.port_layout.channel_count() as usize,
                    0.0,
                );
            }

            self.muxed
                .resize(frame_count * self.output_channel_count, 0.0);
        }
    }

    fn fill_input_from_cpal(&mut self, cpal_input: &[f32], frame_count: usize) {
        if self.input_port_channels.is_empty() {
            return;
        }

        let input_buf = &mut self.input_port_channels[0];
        let port_channels = self
            .config
            .input_port_config
            .ports
            .first()
            .map(|p| p.port_layout.channel_count() as usize)
            .unwrap_or(2);

        // De-interleave cpal input into separate channel buffers
        let cpal_channels = cpal_input.len() / frame_count;
        for frame in 0..frame_count {
            for ch in 0..port_channels.min(cpal_channels) {
                let src_idx = frame * cpal_channels + ch;
                let dst_idx = ch * self.actual_frame_count + frame;
                if src_idx < cpal_input.len() && dst_idx < input_buf.len() {
                    input_buf[dst_idx] = cpal_input[src_idx];
                }
            }
        }
    }

    fn prepare_plugin_buffers(
        &mut self,
        frame_count: usize,
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
                                    buffer: &mut buffer[..frame_count],
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
                                .map(|buf| &mut buf[..frame_count]),
                        ),
                    }
                }),
            ),
        )
    }

    fn write_output<S: FromSample<f32>>(&mut self, destination: &mut [S]) {
        let frame_count = destination.len() / self.output_channel_count;

        if self.output_port_channels.is_empty() {
            destination.iter_mut().for_each(|s| *s = S::EQUILIBRIUM);
            return;
        }

        let main_idx = self.config.output_port_config.main_port_index as usize;
        let main_output = &self.output_port_channels[main_idx];
        let plugin_ch_count = self
            .config
            .output_port_config
            .main_port()
            .port_layout
            .channel_count() as usize;

        let muxed = &mut self.muxed[..destination.len()];

        match (plugin_ch_count, self.output_channel_count) {
            (1, 1) => {
                muxed[..frame_count].copy_from_slice(&main_output[..frame_count]);
            }
            (_, 1) => {
                // Mix down to mono
                for i in 0..frame_count {
                    let mut sum = 0.0;
                    for ch in 0..plugin_ch_count {
                        sum += main_output[ch * self.actual_frame_count + i];
                    }
                    muxed[i] = sum / plugin_ch_count as f32;
                }
            }
            (1, 2) => {
                // Mono to stereo
                for i in 0..frame_count {
                    let s = main_output[i];
                    muxed[i * 2] = s;
                    muxed[i * 2 + 1] = s;
                }
            }
            (_, 2) => {
                // Interleave first two channels
                let actual = self.actual_frame_count;
                for i in 0..frame_count {
                    muxed[i * 2] = main_output[i]; // L
                    muxed[i * 2 + 1] = if plugin_ch_count >= 2 {
                        main_output[actual + i] // R
                    } else {
                        main_output[i]
                    };
                }
            }
            _ => {
                muxed.fill(0.0);
            }
        }

        for (out, &m) in destination.iter_mut().zip(muxed.iter()) {
            *out = S::from_sample_(m);
        }
    }

    /// Attach a crossbeam MIDI receiver for the MidiSender pattern.
    pub fn set_midi_receiver(
        &mut self,
        rx: crossbeam_channel::Receiver<crate::plugin_host::midi_bridge::RawMidiEvent>,
    ) {
        if let Some(ref mut bridge) = self.midi_bridge {
            bridge.set_backup_receiver(rx);
        }
    }
}
src/plugin_host/audio/config.rs

rust
use crate::plugin_host::handlers::DevHost;

use clack_extensions::audio_ports::{
    AudioPortFlags, AudioPortInfoBuffer, AudioPortType, PluginAudioPorts,
};
use clack_host::prelude::*;

#[derive(Clone, Debug)]
pub struct PluginAudioPortsConfig {
    pub ports: Vec<PluginAudioPortInfo>,
    pub main_port_index: u32,
}

impl PluginAudioPortsConfig {
    pub fn empty() -> Self {
        Self {
            main_port_index: 0,
            ports: vec![],
        }
    }

    pub fn default_stereo() -> Self {
        Self {
            main_port_index: 0,
            ports: vec![PluginAudioPortInfo {
                _id: None,
                port_layout: AudioPortLayout::Stereo,
                name: "Default".into(),
            }],
        }
    }

    pub fn main_port(&self) -> &PluginAudioPortInfo {
        if self.ports.is_empty() {
            // Return a default to avoid panics
            static DEFAULT: PluginAudioPortInfo = PluginAudioPortInfo {
                _id: None,
                port_layout: AudioPortLayout::Stereo,
                name: String::new(),
            };
            &DEFAULT
        } else {
            &self.ports[self.main_port_index as usize]
        }
    }

    pub fn total_channel_count(&self) -> usize {
        self.ports
            .iter()
            .map(|p| p.port_layout.channel_count() as usize)
            .sum()
    }
}

#[derive(Clone, Debug)]
pub struct PluginAudioPortInfo {
    pub _id: Option<ClapId>,
    pub port_layout: AudioPortLayout,
    pub name: String,
}

#[derive(Eq, PartialEq, Copy, Clone, Debug)]
pub enum AudioPortLayout {
    Mono,
    Stereo,
    Unsupported { channel_count: u16 },
}

impl AudioPortLayout {
    pub fn channel_count(&self) -> u16 {
        match self {
            AudioPortLayout::Mono => 1,
            AudioPortLayout::Stereo => 2,
            AudioPortLayout::Unsupported { channel_count } => *channel_count,
        }
    }
}

impl std::fmt::Display for AudioPortLayout {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AudioPortLayout::Mono => f.write_str("mono"),
            AudioPortLayout::Stereo => f.write_str("stereo"),
            AudioPortLayout::Unsupported { channel_count } => {
                write!(f, "{channel_count}-channels")
            }
        }
    }
}

pub fn get_config_from_ports(
    plugin: &mut PluginMainThreadHandle,
    is_input: bool,
) -> PluginAudioPortsConfig {
    let Some(ports) = plugin.get_extension::<PluginAudioPorts>() else {
        if is_input {
            return PluginAudioPortsConfig::empty();
        }
        return PluginAudioPortsConfig::default_stereo();
    };

    let mut buffer = AudioPortInfoBuffer::new();
    let mut main_port_index = None;
    let mut discovered = vec![];

    for i in 0..ports.count(plugin, is_input) {
        let Some(info) = ports.get(plugin, i, is_input, &mut buffer) else {
            continue;
        };

        let port_type = info
            .port_type
            .or_else(|| AudioPortType::from_channel_count(info.channel_count));

        let port_layout = match port_type {
            Some(l) if l == AudioPortType::MONO => AudioPortLayout::Mono,
            Some(l) if l == AudioPortType::STEREO => AudioPortLayout::Stereo,
            _ => AudioPortLayout::Unsupported {
                channel_count: info.channel_count as u16,
            },
        };

        if info.flags.contains(AudioPortFlags::IS_MAIN) {
            main_port_index = Some(i);
        }

        discovered.push(PluginAudioPortInfo {
            _id: Some(info.id),
            port_layout,
            name: String::from_utf8_lossy(info.name).into_owned(),
        });
    }

    if discovered.is_empty() {
        if is_input {
            return PluginAudioPortsConfig::empty();
        }
        return PluginAudioPortsConfig::default_stereo();
    }

    let main_port_index = main_port_index.unwrap_or(0);

    PluginAudioPortsConfig {
        main_port_index,
        ports: discovered,
    }
}
src/audio_engine.rs

rust
use crate::plugin_host::audio::PluginAudioProcessor;
use crate::plugin_host::midi_bridge::RawMidiEvent;
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

#[derive(Debug, Clone)]
pub struct RunningInfo {
    pub input_device: String,
    pub output_device: String,
    pub sample_rate: u32,
    pub buffer_size: String,
    pub channels: u16,
}

pub struct AudioEngine {
    pub status: AudioStatus,
    pub running_info: Option<RunningInfo>,

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
    PluginWithInput(Stream, Stream),
}

impl AudioEngine {
    pub fn new() -> Self {
        let (inputs, outputs) = enumerate_devices();
        Self {
            status: AudioStatus::Stopped,
            running_info: None,
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

    /// Start passthrough (no plugin).
    pub fn start(&mut self) {
        self.stop();
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

    /// Start with a plugin audio processor (synth mode: output only).
    pub fn start_with_plugin_synth(
        &mut self,
        mut processor: PluginAudioProcessor,
        midi_rx: Option<crossbeam_channel::Receiver<RawMidiEvent>>,
    ) {
        self.stop();

        // Attach MIDI receiver if provided
        if let Some(rx) = midi_rx {
            processor.set_midi_receiver(rx);
        }

        let host = cpal::default_host();
        let output_device = match get_output_device(
            &host,
            self.selected_output_idx,
            &self.output_device_names,
        ) {
            Ok(d) => d,
            Err(e) => {
                self.status = AudioStatus::Error(e);
                return;
            }
        };

        let sample_rate = COMMON_SAMPLE_RATES[self.selected_sample_rate_idx];
        let channels = processor.output_channel_count().min(2) as u16;
        let channels = channels.max(1);

        let buffer_size = match BUFFER_SIZE_OPTIONS[self.selected_buffer_size_idx].1 {
            Some(n) => BufferSize::Fixed(n),
            None => BufferSize::Default,
        };

        let config = StreamConfig {
            channels,
            sample_rate: SampleRate(sample_rate),
            buffer_size,
        };

        let processor = Arc::new(Mutex::new(processor));

        let out_stream = {
            let proc = Arc::clone(&processor);
            output_device
                .build_output_stream(
                    &config,
                    move |data: &mut [f32], _info| {
                        if let Ok(mut p) = proc.lock() {
                            p.process(data, None);
                        } else {
                            data.fill(0.0);
                        }
                    },
                    |err| eprintln!("[audio] output error: {err}"),
                    None,
                )
                .map_err(|e| format!("Failed to build output stream: {e}"))
        };

        match out_stream {
            Ok(stream) => {
                if let Err(e) = stream.play() {
                    eprintln!("[audio] output play(): {e}");
                }
                let output_name = output_device
                    .name()
                    .unwrap_or_else(|_| "Unknown".to_string());
                self.running_info = Some(RunningInfo {
                    input_device: "None (Synth)".to_string(),
                    output_device: output_name,
                    sample_rate,
                    buffer_size: BUFFER_SIZE_OPTIONS[self.selected_buffer_size_idx]
                        .0
                        .to_string(),
                    channels,
                });
                self._streams = Some(StreamHolder::PluginOutput(stream));
                self.status = AudioStatus::Running;
            }
            Err(e) => {
                self.status = AudioStatus::Error(e);
            }
        }
    }

    /// Start with a plugin audio processor (effect mode: input + output).
    pub fn start_with_plugin_effect(
        &mut self,
        mut processor: PluginAudioProcessor,
        midi_rx: Option<crossbeam_channel::Receiver<RawMidiEvent>>,
    ) {
        self.stop();

        if let Some(rx) = midi_rx {
            processor.set_midi_receiver(rx);
        }

        let host = cpal::default_host();

        let input_device = match get_input_device(
            &host,
            self.selected_input_idx,
            &self.input_device_names,
        ) {
            Ok(d) => d,
            Err(e) => {
                self.status = AudioStatus::Error(e);
                return;
            }
        };

        let output_device = match get_output_device(
            &host,
            self.selected_output_idx,
            &self.output_device_names,
        ) {
            Ok(d) => d,
            Err(e) => {
                self.status = AudioStatus::Error(e);
                return;
            }
        };

        let sample_rate = COMMON_SAMPLE_RATES[self.selected_sample_rate_idx];
        let channels = processor.output_channel_count().min(2) as u16;
        let channels = channels.max(1);

        let buffer_size = match BUFFER_SIZE_OPTIONS[self.selected_buffer_size_idx].1 {
            Some(n) => BufferSize::Fixed(n),
            None => BufferSize::Default,
        };

        let config = StreamConfig {
            channels,
            sample_rate: SampleRate(sample_rate),
            buffer_size,
        };

        // Ring buffer for input → output forwarding
        let capacity = (sample_rate as usize * channels as usize).max(65_536);
        let shared_input: Arc<Mutex<VecDeque<f32>>> =
            Arc::new(Mutex::new(VecDeque::with_capacity(capacity)));

        let shared_in = Arc::clone(&shared_input);
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
            .map_err(|e| format!("Failed to build input stream: {e}"));

        let processor = Arc::new(Mutex::new(processor));
        let proc = Arc::clone(&processor);
        let shared_out = Arc::clone(&shared_input);
        let ch_count = channels as usize;

        let out_stream = output_device
            .build_output_stream(
                &config,
                move |data: &mut [f32], _info| {
                    // Collect input data
                    let mut input_buf = vec![0.0f32; data.len()];
                    if let Ok(mut buf) = shared_out.try_lock() {
                        for s in input_buf.iter_mut() {
                            *s = buf.pop_front().unwrap_or(0.0);
                        }
                    }

                    if let Ok(mut p) = proc.lock() {
                        p.process(data, Some(&input_buf));
                    } else {
                        data.fill(0.0);
                    }
                },
                |err| eprintln!("[audio] output error: {err}"),
                None,
            )
            .map_err(|e| format!("Failed to build output stream: {e}"));

        match (in_stream, out_stream) {
            (Ok(i_stream), Ok(o_stream)) => {
                if let Err(e) = i_stream.play() {
                    eprintln!("[audio] input play(): {e}");
                }
                if let Err(e) = o_stream.play() {
                    eprintln!("[audio] output play(): {e}");
                }
                let input_name = input_device
                    .name()
                    .unwrap_or_else(|_| "Unknown".to_string());
                let output_name = output_device
                    .name()
                    .unwrap_or_else(|_| "Unknown".to_string());
                self.running_info = Some(RunningInfo {
                    input_device: input_name,
                    output_device: output_name,
                    sample_rate,
                    buffer_size: BUFFER_SIZE_OPTIONS[self.selected_buffer_size_idx]
                        .0
                        .to_string(),
                    channels,
                });
                self._streams = Some(StreamHolder::PluginWithInput(i_stream, o_stream));
                self.status = AudioStatus::Running;
            }
            (Err(e), _) | (_, Err(e)) => {
                self.status = AudioStatus::Error(e);
            }
        }
    }

    pub fn stop(&mut self) {
        self._streams = None;
        self.status = AudioStatus::Stopped;
        self.running_info = None;
    }

    /// Get the currently selected sample rate.
    pub fn current_sample_rate(&self) -> u32 {
        COMMON_SAMPLE_RATES[self.selected_sample_rate_idx]
    }

    /// Get the currently selected buffer size (fixed value or a reasonable default).
    pub fn current_buffer_size(&self) -> u32 {
        BUFFER_SIZE_OPTIONS[self.selected_buffer_size_idx]
            .1
            .unwrap_or(512)
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
    let shared: Arc<Mutex<VecDeque<f32>>> =
        Arc::new(Mutex::new(VecDeque::with_capacity(capacity)));

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
    /// The name of the plugin lib (derived from project Cargo.toml).
    pub plugin_lib_name: Option<String>,
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
            plugin_lib_name: None,
        }
    }

    /// Start a `cargo nih-plug bundle <name> --release` build.
    pub fn start_build(&mut self, project_path: &Path) {
        if self.status == BuildStatus::Building {
            return;
        }

        // Try to determine the lib name from the project's Cargo.toml
        let lib_name = detect_lib_name(project_path);
        self.plugin_lib_name = lib_name.clone();

        self.status = BuildStatus::Building;
        self.output_lines.clear();
        self.output_lines.push(BuildOutputLine {
            text: format!("Building project at {}...", project_path.display()),
            is_error: false,
        });

        let (tx, rx) = mpsc::channel();
        self.receiver = Some(rx);

        let path = project_path.to_path_buf();

        thread::spawn(move || {
            let result = run_nih_plug_bundle(&path, lib_name.as_deref(), &tx);
            let success = result.is_ok();
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
                }
            }
        }

        if self.status == BuildStatus::Success || self.status == BuildStatus::Failed {
            self.receiver = None;
        }
    }
}

/// Try to extract the [lib] name or package name from a project's Cargo.toml.
fn detect_lib_name(project_path: &Path) -> Option<String> {
    let cargo_toml_path = project_path.join("Cargo.toml");
    let content = std::fs::read_to_string(cargo_toml_path).ok()?;

    // Simple TOML parsing — look for [lib] name = "..." first
    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("name") && trimmed.contains('=') {
            if let Some(val) = trimmed.split('=').nth(1) {
                let name = val.trim().trim_matches('"').trim_matches('\'');
                if !name.is_empty() {
                    return Some(name.to_string());
                }
            }
        }
    }

    None
}

/// Run `cargo nih-plug bundle <name> --release`.
fn run_nih_plug_bundle(
    project_path: &Path,
    lib_name: Option<&str>,
    tx: &mpsc::Sender<BuildMessage>,
) -> Result<(), String> {
    let mut cmd = Command::new("cargo");
    cmd.arg("nih-plug").arg("bundle");

    if let Some(name) = lib_name {
        cmd.arg(name);
    }

    cmd.arg("--release");
    cmd.current_dir(project_path);
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());

    let mut child = cmd
        .spawn()
        .map_err(|e| format!("Failed to spawn cargo nih-plug bundle: {}. Is cargo-nih-plug installed?", e))?;

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
use crate::audio_engine::{AudioEngine, AudioStatus};
use crate::build_system::{BuildStatus, BuildSystem};
use crate::midi_engine::MidiEngine;
use crate::plugin_host::loader::find_plugin_clap;
use crate::plugin_host::{self, HostStatus, MidiSender, PluginHost, PluginMode};
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
    midi_sender: Option<MidiSender>,
    midi_rx: Option<crossbeam_channel::Receiver<plugin_host::midi_bridge::RawMidiEvent>>,
    file_browser: ui::file_browser::FileBrowser,
    new_project_dialog: NewProjectDialog,
    settings_panel: SettingsPanel,
    midi_settings_panel: MidiSettingsPanel,
    piano: PianoWidget,
    left_panel_width: f32,
    build_panel_height: f32,
    error_log: Vec<String>,
    auto_load_on_build: bool,
    /// Whether the previous build status was Building (for detecting completion)
    was_building: bool,
}

impl PlaygroundApp {
    pub fn new() -> Self {
        let (midi_sender, midi_rx) = plugin_host::create_midi_channel();
        Self {
            project: None,
            build_system: BuildSystem::new(),
            audio_engine: AudioEngine::new(),
            midi_engine: MidiEngine::new(),
            plugin_host: PluginHost::new(),
            midi_sender: Some(midi_sender),
            midi_rx: Some(midi_rx),
            file_browser: ui::file_browser::FileBrowser::new(),
            new_project_dialog: NewProjectDialog::default(),
            settings_panel: SettingsPanel::default(),
            midi_settings_panel: MidiSettingsPanel::default(),
            piano: PianoWidget::default(),
            left_panel_width: 300.0,
            build_panel_height: 200.0,
            error_log: Vec::new(),
            auto_load_on_build: true,
            was_building: false,
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
                } else if self.plugin_host.is_loaded() {
                    // If plugin is loaded, start with plugin
                    self.start_plugin_audio();
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
                if self.plugin_host.gui_open {
                    self.plugin_host.close_gui();
                } else if let Err(e) = self.plugin_host.open_gui() {
                    self.error_log.push(e);
                }
            }
        }
    }

    fn load_plugin(&mut self) {
        let project = match &self.project {
            Some(p) => p,
            None => {
                self.error_log.push("No project open".to_string());
                return;
            }
        };

        // Stop audio if running
        self.audio_engine.stop();

        // Unload previous plugin
        self.plugin_host.unload();

        // Find the .clap file
        let clap_path = match find_plugin_clap(&project.config.path) {
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

        // Auto-start audio with plugin
        self.start_plugin_audio();
    }

    fn start_plugin_audio(&mut self) {
        let sample_rate = self.audio_engine.current_sample_rate();
        let buffer_size = self.audio_engine.current_buffer_size();

        // Activate the plugin
        let processor = match self.plugin_host.activate(sample_rate, 1, buffer_size) {
            Ok(p) => p,
            Err(e) => {
                self.error_log
                    .push(format!("Failed to activate plugin: {e}"));
                return;
            }
        };

        // Recreate MIDI channel for this session
        let (sender, rx) = plugin_host::create_midi_channel();
        self.midi_sender = Some(sender);

        // Start audio with the plugin processor
        match self.plugin_host.mode {
            PluginMode::Synth => {
                self.audio_engine
                    .start_with_plugin_synth(processor, Some(rx));
            }
            PluginMode::Effect => {
                self.audio_engine
                    .start_with_plugin_effect(processor, Some(rx));
            }
        }

        // Auto-open GUI if available
        if self.plugin_host.has_gui() && !self.plugin_host.gui_open {
            if let Err(e) = self.plugin_host.open_gui() {
                self.error_log
                    .push(format!("Failed to open plugin GUI: {e}"));
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

    /// Check if build just completed and auto-load plugin.
    fn check_build_completion(&mut self) {
        let is_building = self.build_system.status == BuildStatus::Building;

        if self.was_building && !is_building {
            // Build just finished
            if self.build_system.status == BuildStatus::Success && self.auto_load_on_build {
                self.load_plugin();
            }
        }

        self.was_building = is_building;
    }
}

impl eframe::App for PlaygroundApp {
    fn update(&mut self, ctx: &eframe::egui::Context, _frame: &mut eframe::Frame) {
        self.build_system.poll();
        self.plugin_host.poll_main_thread();
        self.check_build_completion();

        if self.build_system.status == BuildStatus::Building || self.plugin_host.is_loaded() {
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
                            "▶  Start"
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
                            egui::Button::new("🔌  Load Plugin"),
                        )
                        .clicked()
                    {
                        action = Some(AppAction::LoadPlugin);
                        ui.close();
                    }
                    if ui
                        .add_enabled(loaded, egui::Button::new("⏏  Unload Plugin"))
                        .clicked()
                    {
                        action = Some(AppAction::UnloadPlugin);
                        ui.close();
                    }
                    ui.separator();
                    if ui
                        .add_enabled(
                            loaded && self.plugin_host.has_gui(),
                            egui::Button::new(if self.plugin_host.gui_open {
                                "🖵  Close GUI"
                            } else {
                                "🖵  Open GUI"
                            }),
                        )
                        .clicked()
                    {
                        action = Some(AppAction::TogglePluginGui);
                        ui.close();
                    }
                    ui.separator();
                    ui.checkbox(&mut self.auto_load_on_build, "Auto-load on build");

                    // Plugin mode toggle
                    ui.separator();
                    ui.label("Plugin mode:");
                    ui.radio_value(
                        &mut self.plugin_host.mode,
                        PluginMode::Synth,
                        "🎹 Synth (MIDI→Audio)",
                    );
                    ui.radio_value(
                        &mut self.plugin_host.mode,
                        PluginMode::Effect,
                        "🎚 Effect (Audio→Audio)",
                    );
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
                        HostStatus::Unloaded => ("● No Plugin", egui::Color32::DARK_GRAY),
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
                // ── Plugin control section ────────────────────────────────
                ui.add_space(4.0);
                ui.horizontal(|ui| {
                    ui.label(
                        egui::RichText::new("🔌 Plugin")
                            .strong()
                            .size(14.0),
                    );
                    ui.with_layout(
                        egui::Layout::right_to_left(egui::Align::Center),
                        |ui| {
                            if self.plugin_host.is_loaded() {
                                if self.plugin_host.has_gui() {
                                    if ui
                                        .button(if self.plugin_host.gui_open {
                                            "Close GUI"
                                        } else {
                                            "Open GUI"
                                        })
                                        .clicked()
                                    {
                                        self.handle_action(AppAction::TogglePluginGui);
                                    }
                                }
                                if ui.button("Unload").clicked() {
                                    self.handle_action(AppAction::UnloadPlugin);
                                }
                            } else if self.project.is_some() {
                                if ui.button("Load Plugin").clicked() {
                                    self.handle_action(AppAction::LoadPlugin);
                                }
                            }
                        },
                    );
                });

                // Plugin info
                if let Some(ref name) = self.plugin_host.plugin_name {
                    ui.horizontal(|ui| {
                        ui.label(
                            egui::RichText::new(format!("  {}", name))
                                .size(12.0)
                                .color(egui::Color32::from_rgb(180, 220, 180)),
                        );
                        let (status_text, status_color) = match &self.plugin_host.status {
                            HostStatus::Unloaded => ("Unloaded", egui::Color32::DARK_GRAY),
                            HostStatus::Loaded => ("Loaded", egui::Color32::YELLOW),
                            HostStatus::Active => ("Active", egui::Color32::from_rgb(100, 220, 100)),
                            HostStatus::Processing => {
                                ("Processing", egui::Color32::from_rgb(100, 220, 100))
                            }
                            HostStatus::Error(e) => {
                                let _ = e;
                                ("Error", egui::Color32::from_rgb(255, 90, 90))
                            }
                        };
                        ui.label(
                            egui::RichText::new(format!("[{}]", status_text))
                                .size(11.0)
                                .color(status_color),
                        );
                    });
                }

                ui.separator();

                // ── Piano section (collapsible) ───────────────────────────
                egui::CollapsingHeader::new(
                    egui::RichText::new("🎹 Virtual Piano").strong().size(14.0),
                )
                .default_open(true)
                .show(ui, |ui| {
                    ui.horizontal(|ui| {
                        use crate::midi_engine::MidiStatus;
                        if self.midi_engine.status == MidiStatus::Disconnected {
                            if ui.button("Connect MIDI").clicked() {
                                self.midi_engine.connect();
                            }
                        }
                    });
                    // Pass the MidiSender if plugin is loaded, otherwise use MidiEngine
                    self.piano.show_with_routing(
                        ui,
                        &self.midi_engine,
                        self.midi_sender.as_ref(),
                        self.plugin_host.is_loaded(),
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
use crate::plugin_host::MidiSender;
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

/// Helper that routes note events to either the plugin or the MIDI engine.
struct NoteRouter<'a> {
    midi_engine: &'a MidiEngine,
    midi_sender: Option<&'a MidiSender>,
    plugin_loaded: bool,
}

impl<'a> NoteRouter<'a> {
    fn send_note_on(&self, channel: u8, note: u8, velocity: u8) {
        if self.plugin_loaded {
            if let Some(sender) = self.midi_sender {
                sender.send_note_on(channel, note, velocity);
            }
        } else {
            self.midi_engine.send_note_on(channel, note, velocity);
        }
    }

    fn send_note_off(&self, channel: u8, note: u8) {
        if self.plugin_loaded {
            if let Some(sender) = self.midi_sender {
                sender.send_note_off(channel, note);
            }
        } else {
            self.midi_engine.send_note_off(channel, note);
        }
    }
}

impl PianoWidget {
    /// Original show method for backwards compatibility.
    pub fn show(&mut self, ui: &mut egui::Ui, midi: &MidiEngine) {
        self.show_with_routing(ui, midi, None, false);
    }

    /// Show with optional plugin MIDI routing.
    pub fn show_with_routing(
        &mut self,
        ui: &mut egui::Ui,
        midi: &MidiEngine,
        midi_sender: Option<&MidiSender>,
        plugin_loaded: bool,
    ) {
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

            // Forward hardware MIDI to plugin when loaded
            if plugin_loaded {
                if let Some(sender) = midi_sender {
                    match &event.kind {
                        MidiMessageKind::NoteOn {
                            channel,
                            note,
                            velocity,
                        } => sender.send_note_on(*channel, *note, *velocity),
                        MidiMessageKind::NoteOff { channel, note } => {
                            sender.send_note_off(*channel, *note)
                        }
                        MidiMessageKind::ControlChange { channel, cc, value } => {
                            sender.send_raw(&[0xB0 | channel, *cc, *value]);
                        }
                        MidiMessageKind::PitchBend { channel, value } => {
                            let raw = (*value + 8192) as u16;
                            let lsb = (raw & 0x7F) as u8;
                            let msb = ((raw >> 7) & 0x7F) as u8;
                            sender.send_raw(&[0xE0 | channel, lsb, msb]);
                        }
                        MidiMessageKind::Other(bytes) => {
                            sender.send_raw(bytes);
                        }
                    }
                }
            }
        }

        let router = NoteRouter {
            midi_engine: midi,
            midi_sender,
            plugin_loaded,
        };

        ui.vertical(|ui| {
            self.draw_controls(ui);
            ui.add_space(6.0);
            self.draw_keyboard(ui, &router);
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

    fn draw_keyboard(&mut self, ui: &mut egui::Ui, router: &NoteRouter) {
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

        let mut newly_pressed: Option<u8> = None;
        let mut newly_released: Vec<u8> = Vec::new();

        if response.drag_stopped()
            || (!response.is_pointer_button_down_on() && !self.held_notes.is_empty())
        {
            for note in self.held_notes.drain() {
                router.send_note_off(self.channel, note);
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
                    router.send_note_on(self.channel, note, self.velocity);
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
                    router.send_note_off(self.channel, n);
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

        // Draw white keys
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

        // Draw black keys
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
Summary of all files to add/replace:

File	Action
Cargo.toml	Replace
src/main.rs	Replace (adds plugin_host mod)
src/plugin_host/mod.rs	New
src/plugin_host/handlers.rs	New
src/plugin_host/loader.rs	New
src/plugin_host/timer.rs	New
src/plugin_host/gui.rs	New
src/plugin_host/midi_bridge.rs	New
src/plugin_host/audio/mod.rs	New
src/plugin_host/audio/config.rs	New
src/audio_engine.rs	Replace
src/build_system.rs	Replace (uses cargo nih-plug bundle)
src/app.rs	Replace
src/ui/midi_panel.rs	Replace
Unchanged files: src/midi_engine.rs, src/project.rs, src/scaffolding.rs, src/templates/*, src/ui/build_panel.rs, src/ui/code_editor.rs, src/ui/file_browser.rs, src/ui/new_project_dialog.rs, src/ui/settings_panel.rs, src/ui/top_bar.rs, src/ui/mod.rs.
