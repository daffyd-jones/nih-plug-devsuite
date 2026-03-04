I'll provide all the files needed. This is a substantial implementation, so I'll go through each file systematically.

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

/// Represents which kind of plugin audio routing to use.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum PluginMode {
    /// Synth: MIDI in → audio out (no audio input)
    Instrument,
    /// Effect: audio in + optional MIDI → audio out
    Effect,
}

pub struct PluginHost {
    pub status: HostStatus,
    pub plugin_name: Option<String>,
    pub plugin_id: Option<String>,
    pub mode: PluginMode,

    // Keep the entry alive so the .so/.dylib/.dll stays loaded
    _entry: Option<PluginEntry>,
    instance: Option<PluginInstance<DevHost>>,

    main_thread_rx: Receiver<MainThreadMessage>,
    main_thread_tx: Sender<MainThreadMessage>,

    // MIDI ring buffer producer — given to UI/midi engine to push events
    midi_producer: Option<rtrb::Producer<RawMidiEvent>>,

    // GUI state
    gui: Option<Gui>,
    gui_open: bool,

    // Path to loaded binary
    loaded_path: Option<PathBuf>,
}

impl PluginHost {
    pub fn new() -> Self {
        let (tx, rx) = unbounded();
        Self {
            status: HostStatus::Unloaded,
            plugin_name: None,
            plugin_id: None,
            mode: PluginMode::Instrument,
            _entry: None,
            instance: None,
            main_thread_rx: rx,
            main_thread_tx: tx,
            midi_producer: None,
            gui: None,
            gui_open: false,
            loaded_path: None,
        }
    }

    /// Load a .clap binary and instantiate the first plugin found in it.
    pub fn load(&mut self, clap_path: &Path) -> Result<(), String> {
        self.unload();

        let binary = loader::load_clap_binary(clap_path)?;

        let host_info = HostInfo::new(
            "NIH-plug Playground",
            "NIH-plug Playground",
            "https://github.com/user/nih-plug-playground",
            "0.1.0",
        )
        .map_err(|e| format!("Failed to create host info: {e}"))?;

        let plugin_id_cstr = CString::new(binary.plugin_id.as_str())
            .map_err(|e| format!("Invalid plugin ID: {e}"))?;

        let (tx, rx) = unbounded();
        self.main_thread_tx = tx.clone();
        self.main_thread_rx = rx;

        let instance = PluginInstance::<DevHost>::new(
            |_| DevHostShared::new(tx.clone()),
            |shared| DevHostMainThread::new(shared),
            &binary.entry,
            &plugin_id_cstr,
            &host_info,
        )
        .map_err(|e| format!("Failed to instantiate plugin: {e}"))?;

        // Query GUI extension
        let gui_ext = instance.access_handler(|h| h.gui);

        self.plugin_name = Some(binary.plugin_name.clone());
        self.plugin_id = Some(binary.plugin_id.clone());
        self.loaded_path = Some(clap_path.to_path_buf());
        self._entry = Some(binary.entry);
        self.instance = Some(instance);
        self.status = HostStatus::Loaded;

        // Initialize GUI wrapper if extension is available
        if let Some(gui_ext) = gui_ext {
            if let Some(ref mut instance) = self.instance {
                let gui = Gui::new(gui_ext, &mut instance.plugin_handle());
                self.gui = Some(gui);
            }
        }

        println!(
            "[plugin_host] Loaded plugin: {} ({})",
            binary.plugin_name, binary.plugin_id
        );

        Ok(())
    }

    /// Activate the plugin and return a PluginAudioProcessor ready for the audio thread.
    /// The processor is Send and can be moved to the cpal callback.
    pub fn activate(
        &mut self,
        sample_rate: u32,
        min_buffer_size: u32,
        max_buffer_size: u32,
    ) -> Result<PluginAudioProcessor, String> {
        if self.status != HostStatus::Loaded && self.status != HostStatus::Active {
            return Err("Plugin must be loaded before activation".into());
        }

        let instance = self
            .instance
            .as_mut()
            .ok_or("No plugin instance")?;

        // Create MIDI ring buffer
        let (producer, consumer) = rtrb::RingBuffer::new(1024);
        self.midi_producer = Some(producer);

        let config = PluginAudioConfig {
            sample_rate,
            min_buffer_size,
            max_buffer_size,
            mode: self.mode,
        };

        let processor = PluginAudioProcessor::new(instance, consumer, config)?;

        self.status = HostStatus::Active;
        println!(
            "[plugin_host] Activated at {}Hz, buffer {}-{}",
            sample_rate, min_buffer_size, max_buffer_size
        );

        Ok(processor)
    }

    /// Unload the plugin completely (GUI → deactivate → drop instance → drop entry).
    pub fn unload(&mut self) {
        if self.status == HostStatus::Unloaded {
            return;
        }

        // Close GUI first
        self.close_gui();

        // Drop instance (this deactivates if needed)
        self.instance = None;
        self._entry = None;
        self.midi_producer = None;
        self.gui = None;

        self.status = HostStatus::Unloaded;
        self.plugin_name = None;
        self.plugin_id = None;
        self.loaded_path = None;

        println!("[plugin_host] Plugin unloaded");
    }

    /// Poll main-thread messages — call every frame from the UI thread.
    pub fn poll_main_thread(&mut self) {
        while let Ok(msg) = self.main_thread_rx.try_recv() {
            match msg {
                MainThreadMessage::RunOnMainThread => {
                    if let Some(ref mut instance) = self.instance {
                        instance.call_on_main_thread_callback();
                    }
                }
                MainThreadMessage::GuiClosed => {
                    self.gui_open = false;
                    println!("[plugin_host] Plugin GUI closed by plugin");
                }
                MainThreadMessage::GuiRequestResized { new_size: _ } => {
                    // Floating windows handle their own sizing
                }
            }
        }

        // Tick timers
        if let Some(ref mut instance) = self.instance {
            instance.access_handler(|h| {
                if let Some(timer_ext) = h.timer_support {
                    h.timers.tick_timers(&timer_ext, &mut PluginMainThreadHandle::from(h));
                }
            });
        }
    }

    /// Open the plugin's floating GUI window.
    pub fn open_gui(&mut self) -> Result<(), String> {
        if self.gui_open {
            return Ok(());
        }

        let gui = self.gui.as_mut().ok_or("Plugin has no GUI")?;
        let instance = self.instance.as_mut().ok_or("No plugin instance")?;

        match gui.needs_floating() {
            Some(true) | None => {
                // Open floating
                gui.open_floating(&mut instance.plugin_handle())
                    .map_err(|e| format!("Failed to open floating GUI: {e}"))?;
                self.gui_open = true;
                println!("[plugin_host] Opened floating GUI");
                Ok(())
            }
            Some(false) => {
                // For now, fall back to floating even if plugin prefers embedded.
                // True embedding requires platform-specific child window creation.
                gui.open_floating(&mut instance.plugin_handle())
                    .map_err(|e| format!("Failed to open GUI: {e}"))?;
                self.gui_open = true;
                println!("[plugin_host] Opened GUI (floating fallback)");
                Ok(())
            }
        }
    }

    /// Close the plugin's GUI.
    pub fn close_gui(&mut self) {
        if !self.gui_open {
            return;
        }
        if let (Some(gui), Some(instance)) = (self.gui.as_mut(), self.instance.as_mut()) {
            gui.destroy(&mut instance.plugin_handle());
        }
        self.gui_open = false;
    }

    pub fn is_gui_open(&self) -> bool {
        self.gui_open
    }

    /// Send a MIDI note-on to the plugin via the ring buffer.
    pub fn send_note_on(&mut self, channel: u8, note: u8, velocity: u8) {
        if let Some(ref mut producer) = self.midi_producer {
            let _ = producer.push(RawMidiEvent {
                data: [0x90 | (channel & 0x0F), note & 0x7F, velocity & 0x7F],
                len: 3,
            });
        }
    }

    /// Send a MIDI note-off to the plugin via the ring buffer.
    pub fn send_note_off(&mut self, channel: u8, note: u8) {
        if let Some(ref mut producer) = self.midi_producer {
            let _ = producer.push(RawMidiEvent {
                data: [0x80 | (channel & 0x0F), note & 0x7F, 0],
                len: 3,
            });
        }
    }

    /// Send raw MIDI bytes to the plugin via the ring buffer.
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

    pub fn is_loaded(&self) -> bool {
        self.status != HostStatus::Unloaded
    }
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

/// Messages sent from any plugin thread to the main thread.
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

// ── Shared (thread-safe) ─────────────────────────────────────────────────────

pub struct DevHostShared {
    pub sender: Sender<MainThreadMessage>,
    callbacks: OnceLock<SharedCallbacks>,
}

struct SharedCallbacks {
    // Reserved for future extension callbacks
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
        let _ = self.callbacks.set(SharedCallbacks {});
    }

    fn request_restart(&self) {
        // Not supported in this host
    }

    fn request_process(&self) {
        // CPAL is always running, nothing to do
    }

    fn request_callback(&self) {
        let _ = self.sender.send(MainThreadMessage::RunOnMainThread);
    }
}

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
            .map_err(|_| HostError::Message("Failed to send resize request"))?;
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

impl HostParamsImplShared for DevHostShared {
    fn request_flush(&self) {
        // We're always processing, nothing to flush
    }
}

// ── Main Thread ──────────────────────────────────────────────────────────────

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
src/plugin_host/timer.rs

rust
use clack_extensions::timer::{PluginTimer, TimerId};
use clack_host::prelude::{HostError, PluginMainThreadHandle};
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::time::{Duration, Instant};

use crate::plugin_host::handlers::DevHostMainThread;
use clack_extensions::timer::HostTimerImpl;

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
                .map_or(false, |d| d > self.interval)
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

use crate::plugin_host::handlers::{DevHostShared, DevHostMainThread};
use clack_extensions::gui::{
    GuiApiType, GuiConfiguration, GuiError, GuiSize, PluginGui,
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
            return Some(config);
        }

        // Fallback to floating
        let config = GuiConfiguration {
            api_type,
            is_floating: true,
        };
        if gui.is_api_supported(plugin, config) {
            return Some(config);
        }

        None
    }

    /// Returns `Some(true)` if floating, `Some(false)` if embedded, `None` if no GUI.
    pub fn needs_floating(&self) -> Option<bool> {
        self.configuration.map(|c| c.is_floating)
    }

    /// Open the plugin's GUI as a floating window.
    pub fn open_floating(
        &mut self,
        plugin: &mut PluginMainThreadHandle,
    ) -> Result<(), GuiError> {
        // If no configuration supports floating, force it
        let configuration = match self.configuration {
            Some(c) if c.is_floating => c,
            Some(c) => {
                // Try floating anyway
                let floating_config = GuiConfiguration {
                    api_type: c.api_type,
                    is_floating: true,
                };
                if self.plugin_gui.is_api_supported(plugin, floating_config) {
                    floating_config
                } else {
                    // Last resort: just use what we have — some plugins accept create() with
                    // floating even if is_api_supported returned false for it.
                    floating_config
                }
            }
            None => return Err(GuiError::CreateError),
        };

        self.plugin_gui.create(plugin, configuration)?;
        self.plugin_gui
            .suggest_title(plugin, c"NIH-plug Playground");
        self.plugin_gui.show(plugin)?;
        self.is_open = true;

        Ok(())
    }

    /// Destroy the plugin's GUI resources if open.
    pub fn destroy(&mut self, plugin: &mut PluginMainThreadHandle) {
        if self.is_open {
            self.plugin_gui.destroy(plugin);
            self.is_open = false;
        }
    }
}
src/plugin_host/loader.rs

rust
#![allow(unsafe_code)]

use clack_host::prelude::*;
use std::path::{Path, PathBuf};

pub struct PluginBinary {
    pub entry: PluginEntry,
    pub plugin_id: String,
    pub plugin_name: String,
    pub path: PathBuf,
}

/// Load a .clap binary and find the first plugin in it.
pub fn load_clap_binary(clap_path: &Path) -> Result<PluginBinary, String> {
    if !clap_path.exists() {
        return Err(format!("CLAP file not found: {}", clap_path.display()));
    }

    let entry = unsafe { PluginEntry::load(clap_path) }
        .map_err(|e| format!("Failed to load CLAP entry: {e}"))?;

    let factory = entry
        .get_plugin_factory()
        .ok_or_else(|| "CLAP file has no plugin factory".to_string())?;

    let mut first_plugin = None;
    for descriptor in factory.plugin_descriptors() {
        let id = descriptor
            .id()
            .and_then(|id| id.to_str().ok())
            .map(|s| s.to_string());
        let name = descriptor
            .name()
            .map(|n| n.to_string_lossy().to_string());

        if let Some(id) = id {
            first_plugin = Some((
                id,
                name.unwrap_or_else(|| "Unknown Plugin".to_string()),
            ));
            break;
        }
    }

    let (plugin_id, plugin_name) =
        first_plugin.ok_or_else(|| "No plugins found in CLAP file".to_string())?;

    Ok(PluginBinary {
        entry,
        plugin_id,
        plugin_name,
        path: clap_path.to_path_buf(),
    })
}

/// Find the .clap bundle produced by `cargo nih-plug bundle` in a project directory.
///
/// nih-plug puts bundles under `target/bundled/<plugin_name>.clap`.
/// On Linux the .clap is a directory containing the shared library.
/// On Windows/macOS it's a single file or app bundle.
pub fn find_clap_bundle(project_path: &Path) -> Result<PathBuf, String> {
    let bundled_dir = project_path.join("target").join("bundled");

    if !bundled_dir.exists() {
        return Err(format!(
            "No target/bundled directory found at {}. Run 'cargo nih-plug bundle' first.",
            bundled_dir.display()
        ));
    }

    // Look for any .clap file/directory in bundled/
    let mut clap_files: Vec<PathBuf> = Vec::new();

    let entries = std::fs::read_dir(&bundled_dir)
        .map_err(|e| format!("Failed to read bundled dir: {e}"))?;

    for entry in entries.flatten() {
        let path = entry.path();
        if path
            .extension()
            .map_or(false, |ext| ext == "clap")
        {
            clap_files.push(path);
        }
    }

    if clap_files.is_empty() {
        return Err(format!(
            "No .clap files found in {}",
            bundled_dir.display()
        ));
    }

    if clap_files.len() > 1 {
        eprintln!(
            "[loader] Warning: multiple .clap files found, using first: {}",
            clap_files[0].display()
        );
    }

    let clap_path = clap_files.into_iter().next().unwrap();

    // On Linux, .clap is a directory — the actual .so is inside
    if clap_path.is_dir() {
        // Look for the .so inside
        let inner_entries = std::fs::read_dir(&clap_path)
            .map_err(|e| format!("Failed to read .clap directory: {e}"))?;

        for entry in inner_entries.flatten() {
            let p = entry.path();
            if p.extension().map_or(false, |e| e == "so") {
                return Ok(p);
            }
        }

        // Some nih-plug versions put the .so at the top level with .clap extension directly
        // Try loading the directory path itself (clack may handle it)
        return Ok(clap_path);
    }

    Ok(clap_path)
}

/// Get the plugin library name from Cargo.toml in the project directory.
/// Parses [lib] name or falls back to [package] name with hyphens → underscores.
pub fn get_plugin_lib_name(project_path: &Path) -> Result<String, String> {
    let cargo_toml_path = project_path.join("Cargo.toml");
    let content = std::fs::read_to_string(&cargo_toml_path)
        .map_err(|e| format!("Failed to read Cargo.toml: {e}"))?;

    // Simple parsing — look for [lib] name = "..."
    let mut in_lib_section = false;
    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('[') {
            in_lib_section = trimmed == "[lib]";
            continue;
        }
        if in_lib_section && trimmed.starts_with("name") {
            if let Some(val) = trimmed.split('=').nth(1) {
                let name = val.trim().trim_matches('"').trim_matches('\'');
                return Ok(name.to_string());
            }
        }
    }

    // Fallback: package name
    let mut in_package_section = false;
    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('[') {
            in_package_section = trimmed == "[package]";
            continue;
        }
        if in_package_section && trimmed.starts_with("name") {
            if let Some(val) = trimmed.split('=').nth(1) {
                let name = val.trim().trim_matches('"').trim_matches('\'');
                return Ok(name.replace('-', "_"));
            }
        }
    }

    Err("Could not determine plugin name from Cargo.toml".into())
}
src/plugin_host/midi_bridge.rs

rust
use clack_host::events::event_types::{MidiEvent, NoteOffEvent, NoteOnEvent};
use clack_host::events::{EventBuffer, EventFlags, Match};
use clack_host::prelude::*;
use clack_extensions::note_ports::{NoteDialects, NotePortInfoBuffer, PluginNotePorts};
use crate::plugin_host::handlers::DevHost;
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
        let (port_index, prefers_midi) =
            find_main_note_port_index(instance).unwrap_or((0, true));

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

        while let Ok(raw) = self.consumer.pop() {
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
src/plugin_host/audio.rs

rust
#![allow(unsafe_code)]

use crate::plugin_host::handlers::DevHost;
use crate::plugin_host::midi_bridge::{MidiBridge, RawMidiEvent};
use crate::plugin_host::PluginMode;

use clack_extensions::audio_ports::{
    AudioPortFlags, AudioPortInfoBuffer, AudioPortType, PluginAudioPorts,
};
use clack_host::prelude::*;
use cpal::FromSample;
use rtrb::Consumer;

// ── Port Configuration ───────────────────────────────────────────────────────

#[derive(Clone, Debug)]
pub struct PortConfig {
    pub ports: Vec<PortInfo>,
    pub main_port_index: u32,
}

#[derive(Clone, Debug)]
pub struct PortInfo {
    pub channel_count: u16,
    pub name: String,
}

impl PortConfig {
    fn empty() -> Self {
        Self {
            ports: vec![],
            main_port_index: 0,
        }
    }

    fn default_stereo() -> Self {
        Self {
            ports: vec![PortInfo {
                channel_count: 2,
                name: "Default".into(),
            }],
            main_port_index: 0,
        }
    }

    pub fn main_port(&self) -> &PortInfo {
        &self.ports[self.main_port_index as usize]
    }

    pub fn total_channel_count(&self) -> usize {
        self.ports.iter().map(|p| p.channel_count as usize).sum()
    }
}

fn query_ports(plugin: &mut PluginMainThreadHandle, is_input: bool) -> PortConfig {
    let Some(audio_ports) = plugin.get_extension::<PluginAudioPorts>() else {
        return if is_input {
            PortConfig::empty()
        } else {
            PortConfig::default_stereo()
        };
    };

    let mut buffer = AudioPortInfoBuffer::new();
    let mut ports = vec![];
    let mut main_idx = None;

    for i in 0..audio_ports.count(plugin, is_input) {
        let Some(info) = audio_ports.get(plugin, i, is_input, &mut buffer) else {
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
            main_idx = Some(i);
        }

        ports.push(PortInfo {
            channel_count,
            name: String::from_utf8_lossy(info.name).into_owned(),
        });
    }

    if ports.is_empty() {
        return if is_input {
            PortConfig::empty()
        } else {
            PortConfig::default_stereo()
        };
    }

    PortConfig {
        main_port_index: main_idx.unwrap_or(0),
        ports,
    }
}

// ── Audio Config ─────────────────────────────────────────────────────────────

pub struct PluginAudioConfig {
    pub sample_rate: u32,
    pub min_buffer_size: u32,
    pub max_buffer_size: u32,
    pub mode: PluginMode,
}

// ── Plugin Audio Processor (lives on audio thread) ───────────────────────────

/// This struct is `Send` so it can be moved to the cpal audio thread.
/// It holds a `StoppedPluginAudioProcessor` that will be started on first process call.
pub struct PluginAudioProcessor {
    /// Initially Some — consumed on first process() call
    stopped: Option<StoppedPluginAudioProcessor<DevHost>>,
    /// Populated after start_processing() succeeds
    started: Option<StartedPluginAudioProcessor<DevHost>>,

    midi_bridge: MidiBridge,

    input_ports: AudioPorts,
    output_ports: AudioPorts,
    input_port_channels: Box<[Vec<f32>]>,
    output_port_channels: Box<[Vec<f32>]>,
    muxed: Vec<f32>,

    output_channel_count: usize,
    input_port_config: PortConfig,
    output_port_config: PortConfig,
    frame_capacity: usize,

    steady_counter: u64,
    mode: PluginMode,
}

// SAFETY: StoppedPluginAudioProcessor is Send. Once we call start_processing()
// on the audio thread, the StartedPluginAudioProcessor stays there forever.
unsafe impl Send for PluginAudioProcessor {}

impl PluginAudioProcessor {
    pub fn new(
        instance: &mut PluginInstance<DevHost>,
        midi_consumer: Consumer<RawMidiEvent>,
        config: PluginAudioConfig,
    ) -> Result<Self, String> {
        let input_port_config = query_ports(&mut instance.plugin_handle(), true);
        let output_port_config = query_ports(&mut instance.plugin_handle(), false);

        let midi_bridge = MidiBridge::new(midi_consumer, instance);

        let plugin_config = PluginAudioConfiguration {
            sample_rate: config.sample_rate as f64,
            min_frames_count: config.min_buffer_size,
            max_frames_count: config.max_buffer_size,
        };

        let stopped = instance
            .activate(|_, _| (), plugin_config)
            .map_err(|e| format!("Failed to activate plugin: {e}"))?;

        let frame_capacity = config.max_buffer_size as usize;

        let output_channel_count = if output_port_config.ports.is_empty() {
            2
        } else {
            output_port_config.main_port().channel_count as usize
        };

        let total_in_channels = input_port_config.total_channel_count();
        let total_out_channels = output_port_config.total_channel_count();

        let input_port_channels: Box<[Vec<f32>]> = input_port_config
            .ports
            .iter()
            .map(|p| vec![0.0f32; frame_capacity * p.channel_count as usize])
            .collect();

        let output_port_channels: Box<[Vec<f32>]> = output_port_config
            .ports
            .iter()
            .map(|p| vec![0.0f32; frame_capacity * p.channel_count as usize])
            .collect();

        let muxed = vec![0.0f32; frame_capacity * output_channel_count.max(2)];

        Ok(Self {
            stopped: Some(stopped),
            started: None,
            midi_bridge,
            input_ports: AudioPorts::with_capacity(total_in_channels, input_port_config.ports.len()),
            output_ports: AudioPorts::with_capacity(total_out_channels, output_port_config.ports.len()),
            input_port_channels,
            output_port_channels,
            muxed,
            output_channel_count: output_channel_count.max(1),
            input_port_config,
            output_port_config,
            frame_capacity,
            steady_counter: 0,
            mode: config.mode,
        })
    }

    /// Returns the number of output channels the plugin will produce.
    pub fn output_channel_count(&self) -> usize {
        self.output_channel_count
    }

    /// Process audio. Called from the cpal output callback.
    /// `input_data` may be empty for instrument mode.
    /// Writes interleaved output into `output_data`.
    pub fn process<S: FromSample<f32>>(
        &mut self,
        input_data: &[f32],
        output_data: &mut [S],
    ) {
        // Start processing on first call
        if self.started.is_none() {
            if let Some(stopped) = self.stopped.take() {
                match stopped.start_processing() {
                    Ok(started) => self.started = Some(started),
                    Err(e) => {
                        eprintln!("[plugin_audio] Failed to start processing: {e}");
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

        let cpal_frames = output_data.len() / self.output_channel_count.max(1);
        let frame_count = cpal_frames.min(self.frame_capacity);

        if frame_count == 0 {
            return;
        }

        // Ensure buffers are large enough
        self.ensure_capacity(frame_count);

        // Clear buffers
        self.output_port_channels.iter_mut().for_each(|b| b.fill(0.0));
        self.input_port_channels.iter_mut().for_each(|b| b.fill(0.0));

        // Copy input audio (de-interleave) for effect mode
        if self.mode == PluginMode::Effect && !input_data.is_empty() && !self.input_port_config.ports.is_empty() {
            let main_idx = self.input_port_config.main_port_index as usize;
            let in_ch_count = self.input_port_config.main_port().channel_count as usize;
            let buf = &mut self.input_port_channels[main_idx];

            for frame in 0..frame_count {
                for ch in 0..in_ch_count.min(self.output_channel_count) {
                    let interleaved_idx = frame * self.output_channel_count + ch;
                    let deinterleaved_idx = ch * self.frame_capacity + frame;
                    if interleaved_idx < input_data.len() && deinterleaved_idx < buf.len() {
                        buf[deinterleaved_idx] = input_data[interleaved_idx];
                    }
                }
            }
        }

        // Prepare CLAP buffers
        let (ins, mut outs) = self.prepare_buffers(frame_count);

        // Get MIDI events
        let events = self.midi_bridge.drain_to_input_events(frame_count as u32);

        // Process
        match processor.process(
            &ins,
            &mut outs,
            &events,
            &mut OutputEvents::void(),
            Some(self.steady_counter),
            None,
        ) {
            Ok(_) => {}
            Err(e) => {
                eprintln!("[plugin_audio] Process error: {e}");
                output_data.iter_mut().for_each(|s| *s = f32::to_sample(0.0));
                return;
            }
        }

        self.steady_counter += frame_count as u64;

        // Interleave output
        self.write_output(output_data, frame_count);
    }

    fn ensure_capacity(&mut self, needed: usize) {
        if needed <= self.frame_capacity {
            return;
        }

        self.frame_capacity = needed;

        for (buf, port) in self.input_port_channels.iter_mut().zip(&self.input_port_config.ports) {
            buf.resize(needed * port.channel_count as usize, 0.0);
        }
        for (buf, port) in self.output_port_channels.iter_mut().zip(&self.output_port_config.ports) {
            buf.resize(needed * port.channel_count as usize, 0.0);
        }
        self.muxed.resize(needed * self.output_channel_count, 0.0);
    }

    fn prepare_buffers(
        &mut self,
        frame_count: usize,
    ) -> (InputAudioBuffers<'_>, OutputAudioBuffers<'_>) {
        let cap = self.frame_capacity;

        let inputs = self.input_ports.with_input_buffers(
            self.input_port_channels.iter_mut().map(|port_buf| {
                AudioPortBuffer {
                    latency: 0,
                    channels: AudioPortBufferType::f32_input_only(
                        port_buf.chunks_exact_mut(cap).map(|buffer| InputChannel {
                            buffer: &mut buffer[..frame_count],
                            is_constant: false,
                        }),
                    ),
                }
            }),
        );

        let outputs = self.output_ports.with_output_buffers(
            self.output_port_channels.iter_mut().map(|port_buf| {
                AudioPortBuffer {
                    latency: 0,
                    channels: AudioPortBufferType::f32_output_only(
                        port_buf
                            .chunks_exact_mut(cap)
                            .map(|buf| &mut buf[..frame_count]),
                    ),
                }
            }),
        );

        (inputs, outputs)
    }

    fn write_output<S: FromSample<f32>>(&mut self, output: &mut [S], frame_count: usize) {
        if self.output_port_config.ports.is_empty() {
            output.iter_mut().for_each(|s| *s = f32::to_sample(0.0));
            return;
        }

        let main_idx = self.output_port_config.main_port_index as usize;
        let main_buf = &self.output_port_channels[main_idx];
        let plugin_ch = self.output_port_config.main_port().channel_count as usize;
        let out_ch = self.output_channel_count;

        // Interleave from de-interleaved plugin output into cpal's interleaved buffer
        for frame in 0..frame_count {
            for ch in 0..out_ch {
                let out_idx = frame * out_ch + ch;
                if out_idx >= output.len() {
                    break;
                }

                let src_ch = if ch < plugin_ch { ch } else { 0 }; // mono→stereo duplication
                let src_idx = src_ch * self.frame_capacity + frame;

                let sample = if src_idx < main_buf.len() {
                    main_buf[src_idx]
                } else {
                    0.0
                };

                output[out_idx] = sample.to_sample();
            }
        }
    }
}
src/audio_engine.rs (replaced — adds plugin mode)

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
    PluginInsert(Stream, Stream),
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

    pub fn current_sample_rate(&self) -> u32 {
        COMMON_SAMPLE_RATES[self.selected_sample_rate_idx]
    }

    pub fn current_buffer_size(&self) -> (u32, u32) {
        match BUFFER_SIZE_OPTIONS[self.selected_buffer_size_idx].1 {
            Some(n) => (n, n),
            None => (256, 1024),
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

    /// Start with a plugin processor (instrument mode — output only, MIDI → audio).
    pub fn start_with_plugin_instrument(&mut self, processor: PluginAudioProcessor) {
        self.stop();

        let host = cpal::default_host();
        let sample_rate = COMMON_SAMPLE_RATES[self.selected_sample_rate_idx];
        let buffer_opt = BUFFER_SIZE_OPTIONS[self.selected_buffer_size_idx];

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

        let output_name = output_device
            .name()
            .unwrap_or_else(|_| "Unknown".to_string());

        let out_ch = processor.output_channel_count().max(1).min(2) as u16;

        let buffer_size = match buffer_opt.1 {
            Some(n) => BufferSize::Fixed(n),
            None => BufferSize::Default,
        };

        let config = StreamConfig {
            channels: out_ch,
            sample_rate: SampleRate(sample_rate),
            buffer_size,
        };

        let processor = Arc::new(Mutex::new(processor));
        let proc_clone = Arc::clone(&processor);

        let out_stream = match output_device.build_output_stream(
            &config,
            move |data: &mut [f32], _info| {
                if let Ok(mut proc) = proc_clone.try_lock() {
                    let empty_input: &[f32] = &[];
                    proc.process::<f32>(empty_input, data);
                } else {
                    data.fill(0.0);
                }
            },
            |err| eprintln!("[audio] output error: {err}"),
            None,
        ) {
            Ok(s) => s,
            Err(e) => {
                self.status = AudioStatus::Error(format!("Failed to build output stream: {e}"));
                return;
            }
        };

        if let Err(e) = out_stream.play() {
            self.status = AudioStatus::Error(format!("Failed to play output stream: {e}"));
            return;
        }

        self.running_info = Some(RunningInfo {
            input_device: "(none — instrument mode)".into(),
            output_device: output_name,
            sample_rate,
            buffer_size: buffer_opt.0.to_string(),
            channels: out_ch,
        });
        self._streams = Some(StreamHolder::PluginOutput(out_stream));
        self.status = AudioStatus::Running;
    }

    /// Start with a plugin processor (effect mode — audio in → plugin → audio out).
    pub fn start_with_plugin_effect(&mut self, processor: PluginAudioProcessor) {
        self.stop();

        let host = cpal::default_host();
        let sample_rate = COMMON_SAMPLE_RATES[self.selected_sample_rate_idx];
        let buffer_opt = BUFFER_SIZE_OPTIONS[self.selected_buffer_size_idx];

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

        let input_name = input_device.name().unwrap_or_else(|_| "Unknown".to_string());
        let output_name = output_device.name().unwrap_or_else(|_| "Unknown".to_string());

        let out_ch = processor.output_channel_count().max(1).min(2) as u16;

        let buffer_size = match buffer_opt.1 {
            Some(n) => BufferSize::Fixed(n),
            None => BufferSize::Default,
        };

        let config = StreamConfig {
            channels: out_ch,
            sample_rate: SampleRate(sample_rate),
            buffer_size,
        };

        // Ring buffer for input → output routing
        let capacity = (sample_rate as usize * out_ch as usize).max(65_536);
        let shared: Arc<Mutex<VecDeque<f32>>> =
            Arc::new(Mutex::new(VecDeque::with_capacity(capacity)));
        let shared_in = Arc::clone(&shared);
        let shared_out = Arc::clone(&shared);
        let max_fill = capacity;

        let in_stream = match input_device.build_input_stream(
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
        ) {
            Ok(s) => s,
            Err(e) => {
                self.status = AudioStatus::Error(format!("Failed to build input stream: {e}"));
                return;
            }
        };

        let processor = Arc::new(Mutex::new(processor));
        let proc_clone = Arc::clone(&processor);
        let ch_count = out_ch as usize;

        let out_stream = match output_device.build_output_stream(
            &config,
            move |data: &mut [f32], _info| {
                // Collect input samples
                let frame_count = data.len();
                let mut input_buf = vec![0.0f32; frame_count];

                if let Ok(mut ring) = shared_out.try_lock() {
                    for s in input_buf.iter_mut() {
                        *s = ring.pop_front().unwrap_or(0.0);
                    }
                }

                if let Ok(mut proc) = proc_clone.try_lock() {
                    proc.process::<f32>(&input_buf, data);
                } else {
                    data.fill(0.0);
                }
            },
            |err| eprintln!("[audio] output error: {err}"),
            None,
        ) {
            Ok(s) => s,
            Err(e) => {
                self.status = AudioStatus::Error(format!("Failed to build output stream: {e}"));
                return;
            }
        };

        let _ = in_stream.play();
        let _ = out_stream.play();

        self.running_info = Some(RunningInfo {
            input_device: input_name,
            output_device: output_name,
            sample_rate,
            buffer_size: buffer_opt.0.to_string(),
            channels: out_ch,
        });
        self._streams = Some(StreamHolder::PluginInsert(in_stream, out_stream));
        self.status = AudioStatus::Running;
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
    };

    Ok((in_stream, out_stream, info))
}
src/build_system.rs (replaced — adds bundle command + artifact notification)

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
    /// Set to Some after a successful build — consumed by the app to trigger reload.
    pub artifact_ready: Option<PathBuf>,
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

    /// Start a build using `cargo nih-plug bundle <name> --release`.
    pub fn start_build(&mut self, project_path: &Path) {
        if self.status == BuildStatus::Building {
            return;
        }

        self.status = BuildStatus::Building;
        self.output_lines.clear();
        self.artifact_ready = None;
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
    }
}

/// Run `cargo nih-plug bundle <lib-name> --release`.
///
/// We first figure out the library name from Cargo.toml, then invoke the bundler.
fn run_nih_plug_bundle(project_path: &Path, tx: &mpsc::Sender<BuildMessage>) -> Result<(), String> {
    // Determine lib name
    let lib_name = get_lib_name(project_path)?;

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
                "Failed to spawn 'cargo nih-plug bundle'. Is cargo-nih-plug installed? Error: {}",
                e
            )
        })?;

    if let Some(stdout) = child.stdout.take() {
        let tx_clone = tx.clone();
        thread::spawn(move || {
            let reader = BufReader::new(stdout);
            for line in reader.lines().flatten() {
                let _ = tx_clone.send(BuildMessage::Stdout(line));
            }
        });
    }

    if let Some(stderr) = child.stderr.take() {
        let tx_clone = tx.clone();
        thread::spawn(move || {
            let reader = BufReader::new(stderr);
            for line in reader.lines().flatten() {
                let _ = tx_clone.send(BuildMessage::Stderr(line));
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

/// Parse Cargo.toml for [lib] name, falling back to [package] name.
fn get_lib_name(project_path: &Path) -> Result<String, String> {
    let cargo_toml = project_path.join("Cargo.toml");
    let content =
        std::fs::read_to_string(&cargo_toml).map_err(|e| format!("Can't read Cargo.toml: {e}"))?;

    // Look for [lib] name
    let mut in_lib = false;
    for line in content.lines() {
        let t = line.trim();
        if t.starts_with('[') {
            in_lib = t == "[lib]";
            continue;
        }
        if in_lib && t.starts_with("name") {
            if let Some(val) = t.split('=').nth(1) {
                return Ok(val.trim().trim_matches('"').trim_matches('\'').to_string());
            }
        }
    }

    // Fallback to package name
    let mut in_pkg = false;
    for line in content.lines() {
        let t = line.trim();
        if t.starts_with('[') {
            in_pkg = t == "[package]";
            continue;
        }
        if in_pkg && t.starts_with("name") {
            if let Some(val) = t.split('=').nth(1) {
                return Ok(val
                    .trim()
                    .trim_matches('"')
                    .trim_matches('\'')
                    .replace('-', "_"));
            }
        }
    }

    Err("Could not determine lib name from Cargo.toml".into())
}
src/ui/mod.rs (replaced — adds plugin_panel module)

rust
pub mod build_panel;
pub mod code_editor;
pub mod file_browser;
pub mod midi_panel;
pub mod new_project_dialog;
pub mod plugin_panel;
pub mod settings_panel;
pub mod top_bar;
src/ui/plugin_panel.rs (new file)

rust
use crate::audio_engine::AudioStatus;
use crate::midi_engine::MidiEngine;
use crate::plugin_host::{HostStatus, PluginHost, PluginMode};
use crate::ui::midi_panel::PianoWidget;
use eframe::egui;

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
            ui.selectable_value(&mut plugin_host.mode, PluginMode::Instrument, "🎹 Instrument");
            ui.selectable_value(&mut plugin_host.mode, PluginMode::Effect, "🎛 Effect");
        });

        ui.add_space(4.0);

        // GUI button
        if plugin_host.is_loaded() {
            ui.horizontal(|ui| {
                if plugin_host.is_gui_open() {
                    if ui.button("Close GUI").clicked() {
                        plugin_host.close_gui();
                    }
                } else {
                    if ui.button("Open GUI").clicked() {
                        if let Err(e) = plugin_host.open_gui() {
                            eprintln!("[plugin_panel] GUI error: {e}");
                        }
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
src/ui/midi_panel.rs (replaced — PianoWidget now routes to plugin when loaded)

rust
use crate::midi_engine::{MidiEngine, MidiMessageKind, MidiStatus};
use crate::plugin_host::PluginHost;
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
    /// Show the piano widget. Routes MIDI to plugin if loaded, otherwise to midi engine.
    pub fn show(
        &mut self,
        ui: &mut egui::Ui,
        midi: &MidiEngine,
        plugin_host: &mut PluginHost,
    ) {
        let plugin_loaded = plugin_host.is_loaded();

        // Drain incoming MIDI events into the log
        for event in midi.drain_events() {
            let desc = match &event.kind {
                MidiMessageKind::NoteOn {
                    channel,
                    note,
                    velocity,
                } => {
                    // Forward hardware MIDI to plugin
                    if plugin_loaded {
                        plugin_host.send_note_on(*channel, *note, *velocity);
                    }
                    format!(
                        "NoteOn  ch:{} note:{} ({}) vel:{}",
                        channel + 1,
                        note,
                        note_name(*note),
                        velocity
                    )
                }
                MidiMessageKind::NoteOff { channel, note } => {
                    if plugin_loaded {
                        plugin_host.send_note_off(*channel, *note);
                    }
                    format!(
                        "NoteOff ch:{} note:{} ({})",
                        channel + 1,
                        note,
                        note_name(*note)
                    )
                }
                MidiMessageKind::ControlChange { channel, cc, value } => {
                    if plugin_loaded {
                        plugin_host
                            .send_raw_midi(&[0xB0 | (channel & 0x0F), *cc, *value]);
                    }
                    format!("CC      ch:{} cc:{} val:{}", channel + 1, cc, value)
                }
                MidiMessageKind::PitchBend { channel, value } => {
                    if plugin_loaded {
                        let raw = (*value + 8192) as u16;
                        plugin_host.send_raw_midi(&[
                            0xE0 | (channel & 0x0F),
                            (raw & 0x7F) as u8,
                            ((raw >> 7) & 0x7F) as u8,
                        ]);
                    }
                    format!("PBend   ch:{} val:{}", channel + 1, value)
                }
                MidiMessageKind::Other(bytes) => {
                    if plugin_loaded && bytes.len() <= 3 {
                        plugin_host.send_raw_midi(bytes);
                    }
                    format!("Other   {:?}", bytes)
                }
            };
            self.event_log.push(desc);
            if self.event_log.len() > 64 {
                self.event_log.remove(0);
            }
        }

        ui.vertical(|ui| {
            self.draw_controls(ui);
            ui.add_space(6.0);
            self.draw_keyboard(ui, midi, plugin_host);
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
    ) {
        let plugin_loaded = plugin_host.is_loaded();

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
                if plugin_loaded {
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
                    if plugin_loaded {
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
                    if plugin_loaded {
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
src/app.rs (replaced — integrates plugin host, auto-reload on build)

rust
use crate::audio_engine::{AudioEngine, AudioStatus};
use crate::build_system::{BuildStatus, BuildSystem};
use crate::midi_engine::MidiEngine;
use crate::plugin_host::loader::find_clap_bundle;
use crate::plugin_host::{HostStatus, PluginHost, PluginMode};
use crate::project::Project;
use crate::scaffolding::{scaffold_project, ScaffoldOptions};
use crate::ui;
use crate::ui::midi_panel::MidiSettingsPanel;
use crate::ui::new_project_dialog::{NewProjectDialog, NewProjectResult};
use crate::ui::plugin_panel::PluginPanel;
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
    plugin_host: PluginHost,
    file_browser: ui::file_browser::FileBrowser,
    new_project_dialog: NewProjectDialog,
    settings_panel: SettingsPanel,
    midi_settings_panel: MidiSettingsPanel,
    plugin_panel: PluginPanel,
    left_panel_width: f32,
    build_panel_height: f32,
    error_log: Vec<String>,
    /// Whether to auto-load the plugin after a successful build.
    auto_load_after_build: bool,
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
            plugin_panel: PluginPanel::default(),
            left_panel_width: 300.0,
            build_panel_height: 200.0,
            error_log: Vec::new(),
            auto_load_after_build: true,
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
                    // Unload current plugin before rebuilding
                    self.plugin_host.unload();
                    self.audio_engine.stop();
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
                    // Restart audio with plugin
                    self.try_activate_plugin();
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

    /// After a successful build, find and load the .clap, then activate and start audio.
    fn try_load_and_activate_after_build(&mut self) {
        let project_path = match &self.project {
            Some(p) => p.config.path.clone(),
            None => return,
        };

        // Find the .clap bundle
        match find_clap_bundle(&project_path) {
            Ok(clap_path) => {
                if let Err(e) = self.plugin_host.load(&clap_path) {
                    self.error_log
                        .push(format!("Failed to load plugin: {}", e));
                    return;
                }
                self.try_activate_plugin();
            }
            Err(e) => {
                self.error_log
                    .push(format!("Failed to find .clap bundle: {}", e));
            }
        }
    }

    /// Activate the loaded plugin and start the audio engine with it.
    fn try_activate_plugin(&mut self) {
        let sr = self.audio_engine.current_sample_rate();
        let (min_buf, max_buf) = self.audio_engine.current_buffer_size();

        match self.plugin_host.activate(sr, min_buf, max_buf) {
            Ok(processor) => match self.plugin_host.mode {
                PluginMode::Instrument => {
                    self.audio_engine
                        .start_with_plugin_instrument(processor);
                }
                PluginMode::Effect => {
                    self.audio_engine.start_with_plugin_effect(processor);
                }
            },
            Err(e) => {
                self.error_log
                    .push(format!("Failed to activate plugin: {}", e));
            }
        }
    }
}

impl eframe::App for PlaygroundApp {
    fn update(&mut self, ctx: &eframe::egui::Context, _frame: &mut eframe::Frame) {
        // Poll plugin host main-thread tasks
        self.plugin_host.poll_main_thread();

        self.build_system.poll();

        // Auto-load after successful build
        if self.build_system.status == BuildStatus::Success && self.auto_load_after_build {
            if self.build_system.artifact_ready.is_none() {
                // Mark as handled by setting artifact_ready to a sentinel
                self.build_system.artifact_ready = Some(std::path::PathBuf::new());
                self.try_load_and_activate_after_build();
            }
        }

        if self.build_system.status == BuildStatus::Building
            || self.plugin_host.is_loaded()
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
                        .button(if running { "⏹  Stop" } else { "▶  Start" })
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

                // Auto-load checkbox
                ui.checkbox(&mut self.auto_load_after_build, "Auto-load");

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

        // Right panel — plugin host + piano + MIDI monitor
        egui::SidePanel::right("plugin_panel")
            .resizable(true)
            .default_width(700.0)
            .min_width(400.0)
            .show(ctx, |ui| {
                let audio_running = self.audio_engine.status == AudioStatus::Running;
                self.plugin_panel.show(
                    ui,
                    &mut self.plugin_host,
                    &self.midi_engine,
                    audio_running,
                );
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
Summary of changes
File	Status	Key Changes
Cargo.toml	Modified	Added clack-host, clack-extensions, rtrb, wmidi
src/main.rs	Modified	Added mod plugin_host
src/plugin_host/mod.rs	New	PluginHost lifecycle: load/activate/unload/gui/midi routing
src/plugin_host/handlers.rs	New	DevHost/DevHostShared/DevHostMainThread + extension impls
src/plugin_host/timer.rs	New	Timer management (from example)
src/plugin_host/gui.rs	New	Floating GUI open/close
src/plugin_host/loader.rs	New	Load .clap binary, find bundle in target/bundled/
src/plugin_host/midi_bridge.rs	New	RTRB ring buffer → CLAP events on audio thread
src/plugin_host/audio.rs	New	PluginAudioProcessor with Send transfer + deferred start_processing()
src/audio_engine.rs	Modified	Added start_with_plugin_instrument(), start_with_plugin_effect()
src/build_system.rs	Modified	Now runs cargo nih-plug bundle, exposes artifact_ready
src/ui/mod.rs	Modified	Added pub mod plugin_panel
src/ui/plugin_panel.rs	New	Right panel with plugin controls, mode toggle, collapsible piano
src/ui/midi_panel.rs	Modified	PianoWidget::show() now takes &mut PluginHost, routes MIDI to plugin when loaded
src/app.rs	Modified	Integrated PluginHost, auto-reload on build, plugin panel
Prerequisites: Install cargo-nih-plug with cargo install cargo-nih-plug so the build system's cargo nih-plug bundle command works.
