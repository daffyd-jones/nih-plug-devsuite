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
    "clack-host", "audio-ports", "note-ports", "gui", "log", "params", "timer"
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

use crate::plugin_host::audio::{PluginAudioConfig, PluginAudioProcessor};
use crate::plugin_host::gui::Gui;
use crate::plugin_host::handlers::{DevHost, DevHostShared, MainThreadMessage};
use crate::plugin_host::midi_bridge::MidiBridge;
use clack_host::prelude::*;
use crossbeam_channel::{Receiver, Sender, unbounded};
use std::ffi::CString;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq)]
pub enum HostStatus {
    Unloaded,
    Loaded,
    Active,
    Processing,
    Error(String),
}

/// Whether plugin expects audio input, audio output, or both
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum PluginMode {
    Instrument,
    Effect,
}

pub struct PluginHost {
    pub status: HostStatus,
    pub plugin_name: Option<String>,
    pub plugin_id: Option<String>,
    pub plugin_mode: PluginMode,
    pub loaded_path: Option<PathBuf>,

    entry: Option<PluginEntry>,
    instance: Option<PluginInstance<DevHost>>,
    main_thread_rx: Option<Receiver<MainThreadMessage>>,
    main_thread_tx: Option<Sender<MainThreadMessage>>,

    /// MIDI producer — UI/MIDI thread pushes events here
    pub midi_producer: Option<rtrb::Producer<midi_bridge::RawMidiMsg>>,

    /// Whether the GUI floating window is currently open
    pub gui_open: bool,
    /// Whether the plugin supports GUI at all
    pub gui_supported: bool,
}

impl PluginHost {
    pub fn new() -> Self {
        Self {
            status: HostStatus::Unloaded,
            plugin_name: None,
            plugin_id: None,
            plugin_mode: PluginMode::Instrument,
            loaded_path: None,
            entry: None,
            instance: None,
            main_thread_rx: None,
            main_thread_tx: None,
            midi_producer: None,
            gui_open: false,
            gui_supported: false,
        }
    }

    /// Load a .clap plugin from disk. Does NOT activate — call activate() next.
    pub fn load(&mut self, clap_path: &Path, plugin_id: &str) -> Result<(), String> {
        self.unload();

        let entry = unsafe { PluginEntry::load(clap_path) }
            .map_err(|e| format!("Failed to load CLAP entry: {e}"))?;

        let factory = entry
            .get_plugin_factory()
            .ok_or_else(|| "No plugin factory in CLAP file".to_string())?;

        // Find the plugin descriptor
        let mut found_name = None;
        let c_id = CString::new(plugin_id).map_err(|e| format!("Invalid plugin ID: {e}"))?;

        for desc in factory.plugin_descriptors() {
            if let Some(id) = desc.id() {
                if let Ok(id_str) = id.to_str() {
                    if id_str == plugin_id {
                        found_name = desc.name().map(|n| n.to_string_lossy().to_string());
                        break;
                    }
                }
            }
        }

        let host_info = HostInfo::new(
            "NIH-plug Playground",
            "NIH-plug Playground",
            "https://github.com/example",
            "0.1.0",
        )
        .map_err(|e| format!("Failed to create host info: {e}"))?;

        let (tx, rx) = unbounded();
        let tx_clone = tx.clone();

        let instance = PluginInstance::<DevHost>::new(
            |_| DevHostShared::new(tx_clone),
            |shared| handlers::DevHostMainThread::new(shared),
            &entry,
            &c_id,
            &host_info,
        )
        .map_err(|e| format!("Failed to instantiate plugin: {e}"))?;

        // Check GUI support
        let gui_supported = instance
            .access_handler(|h| h.gui.is_some());

        self.plugin_name = found_name;
        self.plugin_id = Some(plugin_id.to_string());
        self.loaded_path = Some(clap_path.to_path_buf());
        self.entry = Some(entry);
        self.instance = Some(instance);
        self.main_thread_rx = Some(rx);
        self.main_thread_tx = Some(tx);
        self.gui_supported = gui_supported;
        self.status = HostStatus::Loaded;

        Ok(())
    }

    /// Activate the plugin and return a PluginAudioProcessor ready for the audio thread.
    /// The processor is Send and can be moved into the cpal callback.
    pub fn activate(
        &mut self,
        sample_rate: u32,
        min_buffer_size: u32,
        max_buffer_size: u32,
    ) -> Result<PluginAudioProcessor, String> {
        let instance = self
            .instance
            .as_mut()
            .ok_or_else(|| "No plugin loaded".to_string())?;

        // Query audio port config before activation
        let input_config =
            audio::config::get_config_from_ports(&mut instance.plugin_handle(), true);
        let output_config =
            audio::config::get_config_from_ports(&mut instance.plugin_handle(), false);

        // Determine plugin mode from ports
        self.plugin_mode = if input_config.ports.is_empty() {
            PluginMode::Instrument
        } else {
            PluginMode::Effect
        };

        // Query note ports for MIDI bridge setup
        let (note_port_index, prefers_midi) =
            midi_bridge::find_main_note_port_index(instance).unwrap_or((0, false));

        // Create MIDI ring buffer
        let (producer, consumer) = rtrb::RingBuffer::new(1024);
        self.midi_producer = Some(producer);

        let midi_bridge = MidiBridge::new(consumer, note_port_index, prefers_midi);

        let audio_config = PluginAudioConfig {
            sample_rate,
            min_buffer_size,
            max_buffer_size,
            input_port_config: input_config,
            output_port_config: output_config,
        };

        let clap_config = PluginAudioConfiguration {
            sample_rate: sample_rate as f64,
            min_frames_count: min_buffer_size,
            max_frames_count: max_buffer_size,
        };

        let stopped = instance
            .activate(|_, _| (), clap_config)
            .map_err(|e| format!("Failed to activate plugin: {e}"))?;

        self.status = HostStatus::Active;

        Ok(PluginAudioProcessor::new(
            stopped,
            midi_bridge,
            audio_config,
        ))
    }

    /// Open the plugin's floating GUI window.
    pub fn open_gui(&mut self) -> Result<(), String> {
        let instance = self
            .instance
            .as_mut()
            .ok_or_else(|| "No plugin loaded".to_string())?;

        let plugin_gui = instance
            .access_handler(|h| h.gui)
            .ok_or_else(|| "Plugin does not support GUI".to_string())?;

        let mut gui = Gui::new(plugin_gui, &mut instance.plugin_handle());

        match gui.needs_floating() {
            Some(true) | Some(false) => {
                // We always use floating for now
                gui.open_floating(&mut instance.plugin_handle())
                    .map_err(|e| format!("Failed to open GUI: {e}"))?;
                self.gui_open = true;

                // Store gui back in handler
                instance.access_handler(|h| {
                    h.gui_state = Some(gui);
                });

                Ok(())
            }
            None => Err("Plugin GUI not supported on this platform".to_string()),
        }
    }

    /// Close the plugin's GUI if open.
    pub fn close_gui(&mut self) {
        if let Some(ref mut instance) = self.instance {
            instance.access_handler(|h| {
                if let Some(ref mut gui) = h.gui_state {
                    gui.destroy(&mut h.plugin.as_mut().unwrap().as_plugin_handle_mut());
                }
                h.gui_state = None;
            });
        }
        self.gui_open = false;
    }

    /// Send a note-on through the MIDI bridge ring buffer.
    pub fn send_note_on(&mut self, channel: u8, note: u8, velocity: u8) {
        if let Some(ref mut producer) = self.midi_producer {
            let _ = producer.push(midi_bridge::RawMidiMsg {
                data: [0x90 | (channel & 0x0F), note & 0x7F, velocity & 0x7F],
                len: 3,
            });
        }
    }

    /// Send a note-off through the MIDI bridge ring buffer.
    pub fn send_note_off(&mut self, channel: u8, note: u8) {
        if let Some(ref mut producer) = self.midi_producer {
            let _ = producer.push(midi_bridge::RawMidiMsg {
                data: [0x80 | (channel & 0x0F), note & 0x7F, 0],
                len: 3,
            });
        }
    }

    /// Send raw MIDI bytes through the bridge.
    pub fn send_midi_raw(&mut self, bytes: &[u8]) {
        if bytes.len() > 3 {
            return;
        }
        if let Some(ref mut producer) = self.midi_producer {
            let mut data = [0u8; 3];
            data[..bytes.len()].copy_from_slice(bytes);
            let _ = producer.push(midi_bridge::RawMidiMsg {
                data,
                len: bytes.len() as u8,
            });
        }
    }

    /// Poll main-thread messages. Call every frame from the UI thread.
    pub fn poll_main_thread(&mut self) {
        let rx = match self.main_thread_rx {
            Some(ref rx) => rx,
            None => return,
        };

        while let Ok(msg) = rx.try_recv() {
            match msg {
                MainThreadMessage::RunOnMainThread => {
                    if let Some(ref mut instance) = self.instance {
                        instance.call_on_main_thread_callback();
                    }
                }
                MainThreadMessage::GuiClosed => {
                    self.gui_open = false;
                    if let Some(ref mut instance) = self.instance {
                        instance.access_handler(|h| {
                            h.gui_state = None;
                        });
                    }
                }
                MainThreadMessage::GuiRequestResized { .. } => {
                    // Floating window handles its own sizing
                }
            }
        }

        // Tick timers
        if let Some(ref mut instance) = self.instance {
            instance.access_handler(|h| {
                if let (Some(ref timers), Some(timer_ext)) = (&h.timers_rc, h.timer_support) {
                    if let Some(ref mut plugin) = h.plugin {
                        timers.tick_timers(&timer_ext, &mut plugin.as_plugin_handle_mut());
                    }
                }
            });
        }
    }

    /// Fully unload the plugin. Order: close GUI → deactivate → drop instance → drop entry.
    pub fn unload(&mut self) {
        self.close_gui();

        // Drop instance (deactivates automatically)
        self.instance = None;
        self.entry = None;
        self.main_thread_rx = None;
        self.main_thread_tx = None;
        self.midi_producer = None;

        self.status = HostStatus::Unloaded;
        self.plugin_name = None;
        self.plugin_id = None;
        self.loaded_path = None;
        self.gui_open = false;
        self.gui_supported = false;
    }

    pub fn is_loaded(&self) -> bool {
        !matches!(self.status, HostStatus::Unloaded)
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

/// Messages sent from plugin threads to the main thread.
pub enum MainThreadMessage {
    RunOnMainThread,
    GuiClosed,
    GuiRequestResized { new_size: GuiSize },
}

/// Marker type for our host implementation.
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

/// Thread-safe shared host data.
pub struct DevHostShared {
    pub sender: Sender<MainThreadMessage>,
    callbacks: OnceLock<SharedCallbacks>,
}

struct SharedCallbacks {
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
        let _ = self.callbacks.set(SharedCallbacks {
            _audio_ports: instance.get_extension(),
        });
    }

    fn request_restart(&self) {
        // Not supported in dev tool
    }

    fn request_process(&self) {
        // Always processing via cpal
    }

    fn request_callback(&self) {
        let _ = self.sender.send(MainThreadMessage::RunOnMainThread);
    }
}

/// Main-thread-only host data.
pub struct DevHostMainThread<'a> {
    pub _shared: &'a DevHostShared,
    pub plugin: Option<InitializedPluginHandle<'a>>,
    pub gui: Option<PluginGui>,
    pub gui_state: Option<Gui>,
    pub timer_support: Option<PluginTimer>,
    pub timers_rc: Option<Rc<Timers>>,
}

impl<'a> DevHostMainThread<'a> {
    pub fn new(shared: &'a DevHostShared) -> Self {
        Self {
            _shared: shared,
            plugin: None,
            gui: None,
            gui_state: None,
            timer_support: None,
            timers_rc: Some(Rc::new(Timers::new())),
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

impl clack_extensions::timer::HostTimerImpl for DevHostMainThread<'_> {
    fn register_timer(&mut self, period_ms: u32) -> Result<clack_extensions::timer::TimerId, HostError> {
        if let Some(ref timers) = self.timers_rc {
            Ok(timers.register_new(std::time::Duration::from_millis(period_ms as u64)))
        } else {
            Err(HostError::Message("Timers not initialized"))
        }
    }

    fn unregister_timer(&mut self, timer_id: clack_extensions::timer::TimerId) -> Result<(), HostError> {
        if let Some(ref timers) = self.timers_rc {
            if timers.unregister(timer_id) {
                Ok(())
            } else {
                Err(HostError::Message("Unknown timer ID"))
            }
        } else {
            Err(HostError::Message("Timers not initialized"))
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
src/plugin_host/gui.rs

rust
#![allow(unsafe_code)]

use clack_extensions::gui::{GuiApiType, GuiConfiguration, GuiError, GuiSize, PluginGui};
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
        let mut config = GuiConfiguration {
            api_type,
            is_floating: false,
        };

        // Try embedded first, then floating
        if gui.is_api_supported(plugin, config) {
            // We still use floating, but good to know embedded is possible
            config.is_floating = true;
            if gui.is_api_supported(plugin, config) {
                Some(config)
            } else {
                config.is_floating = false;
                Some(config)
            }
        } else {
            config.is_floating = true;
            if gui.is_api_supported(plugin, config) {
                Some(config)
            } else {
                None
            }
        }
    }

    pub fn needs_floating(&self) -> Option<bool> {
        self.configuration
            .map(|GuiConfiguration { is_floating, .. }| is_floating)
    }

    /// Open the plugin GUI as a floating window (plugin manages its own window).
    pub fn open_floating(&mut self, plugin: &mut PluginMainThreadHandle) -> Result<(), GuiError> {
        let configuration = self.configuration.ok_or(GuiError::CreateError)?;

        // Force floating
        let float_config = GuiConfiguration {
            api_type: configuration.api_type,
            is_floating: true,
        };

        self.plugin_gui.create(plugin, float_config)?;
        self.plugin_gui
            .suggest_title(plugin, c"NIH-plug Playground - Plugin");
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
src/plugin_host/loader.rs

rust
use std::path::{Path, PathBuf};

/// Find the .clap bundle in target/bundled after `cargo nih-plug bundle`.
/// Returns the path to the .clap file.
pub fn find_clap_bundle(project_path: &Path) -> Result<PathBuf, String> {
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

    for entry in entries {
        let entry = entry.map_err(|e| format!("Failed to read dir entry: {e}"))?;
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
            // Return the most recently modified one
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

/// Discover the plugin ID from a .clap file by loading it and reading the factory.
#[allow(unsafe_code)]
pub fn discover_plugin_id(clap_path: &Path) -> Result<(String, Option<String>), String> {
    use clack_host::prelude::*;

    let entry = unsafe { PluginEntry::load(clap_path) }
        .map_err(|e| format!("Failed to load CLAP file: {e}"))?;

    let factory = entry
        .get_plugin_factory()
        .ok_or_else(|| "No plugin factory found".to_string())?;

    for desc in factory.plugin_descriptors() {
        if let Some(id) = desc.id() {
            if let Ok(id_str) = id.to_str() {
                let name = desc.name().map(|n| n.to_string_lossy().to_string());
                return Ok((id_str.to_string(), name));
            }
        }
    }

    Err("No plugins found in CLAP file".to_string())
}

/// Parse Cargo.toml to find the plugin package name for the bundle command.
pub fn find_package_name(project_path: &Path) -> Result<String, String> {
    let cargo_toml = project_path.join("Cargo.toml");
    let content = std::fs::read_to_string(&cargo_toml)
        .map_err(|e| format!("Failed to read Cargo.toml: {e}"))?;

    // Simple TOML parsing — look for name = "..." under [package]
    let mut in_package = false;
    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed == "[package]" {
            in_package = true;
            continue;
        }
        if trimmed.starts_with('[') {
            in_package = false;
            continue;
        }
        if in_package && trimmed.starts_with("name") {
            if let Some(val) = trimmed.split('=').nth(1) {
                let name = val.trim().trim_matches('"').trim_matches('\'');
                return Ok(name.to_string());
            }
        }
    }

    Err("Could not find package name in Cargo.toml".to_string())
}
src/plugin_host/midi_bridge.rs

rust
use crate::plugin_host::handlers::DevHost;
use clack_extensions::note_ports::{NoteDialects, NotePortInfoBuffer, PluginNotePorts};
use clack_host::events::event_types::{MidiEvent, NoteOffEvent, NoteOnEvent};
use clack_host::events::{EventFlags, Match};
use clack_host::prelude::*;

/// A raw 3-byte MIDI message to pass through the ring buffer.
#[derive(Debug, Clone, Copy)]
pub struct RawMidiMsg {
    pub data: [u8; 3],
    pub len: u8,
}

/// Sits on the audio thread. Drains MIDI from the ring buffer and converts to CLAP events.
pub struct MidiBridge {
    consumer: rtrb::Consumer<RawMidiMsg>,
    event_buffer: EventBuffer,
    note_port_index: u16,
    prefers_midi: bool,
}

impl MidiBridge {
    pub fn new(consumer: rtrb::Consumer<RawMidiMsg>, note_port_index: u16, prefers_midi: bool) -> Self {
        Self {
            consumer,
            event_buffer: EventBuffer::with_capacity(256),
            note_port_index,
            prefers_midi,
        }
    }

    /// Drain all pending MIDI messages and return CLAP input events for this process block.
    pub fn drain_to_input_events(&mut self, _frame_count: u32) -> InputEvents<'_> {
        self.event_buffer.clear();

        while let Ok(msg) = self.consumer.pop() {
            let data = &msg.data[..msg.len as usize];
            if data.is_empty() {
                continue;
            }

            let status = data[0] & 0xF0;
            let channel = data[0] & 0x0F;

            if !self.prefers_midi && data.len() >= 3 {
                match status {
                    0x90 if data[2] > 0 => {
                        let velocity = data[2] as f64 / 127.0;
                        self.event_buffer.push(
                            &NoteOnEvent::new(
                                0,
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
                                0,
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
                let mut buf = [0u8; 3];
                buf[..3].copy_from_slice(&data[..3]);
                self.event_buffer.push(
                    &MidiEvent::new(0, self.note_port_index, buf)
                        .with_flags(EventFlags::IS_LIVE),
                );
            }
        }

        self.event_buffer.as_input()
    }
}

/// Query the plugin for its main note port index and whether it prefers MIDI dialect.
pub fn find_main_note_port_index(
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
use clack_host::prelude::*;
use cpal::FromSample;

pub use config::PluginAudioPortsConfig;

/// Full audio configuration for plugin activation.
pub struct PluginAudioConfig {
    pub sample_rate: u32,
    pub min_buffer_size: u32,
    pub max_buffer_size: u32,
    pub input_port_config: PluginAudioPortsConfig,
    pub output_port_config: PluginAudioPortsConfig,
}

/// Holds a stopped (but activated) plugin audio processor, ready to be sent to the audio thread.
/// This type is Send because StartedPluginAudioProcessor is !Send, but
/// StoppedPluginAudioProcessor IS Send.
pub struct PluginAudioProcessor {
    stopped: Option<StoppedPluginAudioProcessor<DevHost>>,
    started: Option<StartedPluginAudioProcessor<DevHost>>,
    midi_bridge: MidiBridge,
    config: PluginAudioConfig,

    // Audio buffers
    input_ports: AudioPorts,
    output_ports: AudioPorts,
    input_channel_bufs: Box<[Vec<f32>]>,
    output_channel_bufs: Box<[Vec<f32>]>,
    actual_frame_count: usize,
    output_channel_count: usize,
    steady_counter: u64,
}

impl PluginAudioProcessor {
    pub fn new(
        stopped: StoppedPluginAudioProcessor<DevHost>,
        midi_bridge: MidiBridge,
        config: PluginAudioConfig,
    ) -> Self {
        let frame_count = config.max_buffer_size as usize;

        let total_in_channels = config.input_port_config.total_channel_count();
        let total_out_channels = config.output_port_config.total_channel_count();

        let output_channel_count = if config.output_port_config.ports.is_empty() {
            2 // fallback stereo
        } else {
            config.output_port_config.main_port().port_layout.channel_count() as usize
        };

        let input_channel_bufs: Box<[Vec<f32>]> = config
            .input_port_config
            .ports
            .iter()
            .map(|p| vec![0.0f32; frame_count * p.port_layout.channel_count() as usize])
            .collect();

        let output_channel_bufs: Box<[Vec<f32>]> = config
            .output_port_config
            .ports
            .iter()
            .map(|p| vec![0.0f32; frame_count * p.port_layout.channel_count() as usize])
            .collect();

        Self {
            stopped: Some(stopped),
            started: None,
            midi_bridge,
            input_ports: AudioPorts::with_capacity(
                total_in_channels,
                config.input_port_config.ports.len(),
            ),
            output_ports: AudioPorts::with_capacity(
                total_out_channels,
                config.output_port_config.ports.len(),
            ),
            input_channel_bufs,
            output_channel_bufs,
            actual_frame_count: frame_count,
            output_channel_count,
            config,
            steady_counter: 0,
        }
    }

    /// Call start_processing on the first audio callback. This transitions
    /// from Stopped → Started on the audio thread.
    fn ensure_started(&mut self) {
        if self.started.is_none() {
            if let Some(stopped) = self.stopped.take() {
                match stopped.start_processing() {
                    Ok(started) => self.started = Some(started),
                    Err(e) => {
                        eprintln!("[plugin_host] Failed to start processing: {e}");
                    }
                }
            }
        }
    }

    /// Main process function. Called from the cpal output callback.
    /// `output_data` is the interleaved cpal output buffer (any sample type).
    pub fn process<S: FromSample<f32>>(&mut self, output_data: &mut [S], input_data: Option<&[f32]>) {
        self.ensure_started();

        let Some(ref mut processor) = self.started else {
            // Not yet started or failed — output silence
            for s in output_data.iter_mut() {
                *s = S::from_sample(0.0f32);
            }
            return;
        };

        let frame_count = output_data.len() / self.output_channel_count.max(1);

        // Resize buffers if needed
        if frame_count > self.actual_frame_count {
            self.actual_frame_count = frame_count;
            for (buf, port) in self
                .input_channel_bufs
                .iter_mut()
                .zip(&self.config.input_port_config.ports)
            {
                buf.resize(
                    frame_count * port.port_layout.channel_count() as usize,
                    0.0,
                );
            }
            for (buf, port) in self
                .output_channel_bufs
                .iter_mut()
                .zip(&self.config.output_port_config.ports)
            {
                buf.resize(
                    frame_count * port.port_layout.channel_count() as usize,
                    0.0,
                );
            }
        }

        // Fill input buffers from cpal input (if effect mode and input provided)
        if let Some(input) = input_data {
            if !self.input_channel_bufs.is_empty() {
                let in_buf = &mut self.input_channel_bufs[0];
                let in_port = &self.config.input_port_config.ports[0];
                let in_channels = in_port.port_layout.channel_count() as usize;

                // De-interleave cpal input into channel buffers
                for frame_idx in 0..frame_count {
                    for ch in 0..in_channels {
                        let cpal_idx = frame_idx * in_channels + ch;
                        let buf_idx = ch * self.actual_frame_count + frame_idx;
                        if cpal_idx < input.len() && buf_idx < in_buf.len() {
                            in_buf[buf_idx] = input[cpal_idx];
                        }
                    }
                }
            }
        } else {
            // Zero input buffers for instrument mode
            for buf in self.input_channel_bufs.iter_mut() {
                buf.fill(0.0);
            }
        }

        // Zero output buffers
        for buf in self.output_channel_bufs.iter_mut() {
            buf.fill(0.0);
        }

        // Prepare plugin buffers
        let (ins, mut outs) = self.prepare_plugin_buffers(frame_count);

        // Get MIDI events
        let events = self.midi_bridge.drain_to_input_events(frame_count as u32);

        // Process!
        match processor.process(
            &ins,
            &mut outs,
            &events,
            &mut OutputEvents::void(),
            Some(self.steady_counter),
            None,
        ) {
            Ok(_) => {}
            Err(e) => eprintln!("[plugin_host] process error: {e}"),
        }

        self.steady_counter += frame_count as u64;

        // Interleave output to cpal buffer
        self.write_output(output_data, frame_count);
    }

    fn prepare_plugin_buffers(
        &mut self,
        frame_count: usize,
    ) -> (InputAudioBuffers<'_>, OutputAudioBuffers<'_>) {
        let actual = self.actual_frame_count;
        (
            self.input_ports.with_input_buffers(
                self.input_channel_bufs.iter_mut().map(|port_buf| {
                    AudioPortBuffer {
                        latency: 0,
                        channels: AudioPortBufferType::f32_input_only(
                            port_buf.chunks_exact_mut(actual).map(|buffer| {
                                InputChannel {
                                    buffer: &mut buffer[..frame_count],
                                    is_constant: false,
                                }
                            }),
                        ),
                    }
                }),
            ),
            self.output_ports.with_output_buffers(
                self.output_channel_bufs.iter_mut().map(|port_buf| {
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

    fn write_output<S: FromSample<f32>>(&self, output: &mut [S], frame_count: usize) {
        if self.output_channel_bufs.is_empty() || self.config.output_port_config.ports.is_empty() {
            for s in output.iter_mut() {
                *s = S::from_sample(0.0f32);
            }
            return;
        }

        let main_idx = self.config.output_port_config.main_port_index as usize;
        let main_buf = &self.output_channel_bufs[main_idx];
        let plugin_channels = self.config.output_port_config.main_port().port_layout.channel_count() as usize;
        let out_channels = self.output_channel_count;

        for frame_idx in 0..frame_count {
            for out_ch in 0..out_channels {
                let cpal_idx = frame_idx * out_channels + out_ch;
                if cpal_idx >= output.len() {
                    break;
                }

                // Pick the source channel (wrap/clamp)
                let src_ch = if out_ch < plugin_channels {
                    out_ch
                } else {
                    // Mono-to-stereo: repeat channel 0
                    0
                };

                let buf_idx = src_ch * self.actual_frame_count + frame_idx;
                let sample = if buf_idx < main_buf.len() {
                    main_buf[buf_idx]
                } else {
                    0.0
                };

                output[cpal_idx] = S::from_sample(sample);
            }
        }
    }

    /// Number of output channels the plugin produces.
    pub fn output_channel_count(&self) -> usize {
        self.output_channel_count
    }

    /// Whether this plugin wants audio input (effect mode).
    pub fn wants_input(&self) -> bool {
        !self.config.input_port_config.ports.is_empty()
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

    fn default_stereo() -> Self {
        Self {
            main_port_index: 0,
            ports: vec![PluginAudioPortInfo {
                port_layout: AudioPortLayout::Stereo,
                name: "Default".into(),
            }],
        }
    }

    pub fn main_port(&self) -> &PluginAudioPortInfo {
        &self.ports[self.main_port_index as usize]
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
            port_layout,
            name: String::from_utf8_lossy(info.name).into_owned(),
        });
    }

    if discovered.is_empty() {
        return if is_input {
            PluginAudioPortsConfig::empty()
        } else {
            PluginAudioPortsConfig::default_stereo()
        };
    }

    let main_port_index = main_port_index.unwrap_or(0);

    PluginAudioPortsConfig {
        main_port_index,
        ports: discovered,
    }
}
src/audio_engine.rs

rust
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{BufferSize, Device, SampleRate, Stream, StreamConfig};
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use crate::plugin_host::audio::PluginAudioProcessor;

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
    Plugin,
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
    PluginBoth(Stream, Stream),
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

    /// Start audio in passthrough mode (no plugin).
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

    /// Start audio with a plugin processor. The processor is consumed and moved to the audio thread.
    pub fn start_with_plugin(&mut self, processor: PluginAudioProcessor) {
        self.stop();
        self.mode = AudioMode::Plugin;

        let sample_rate = COMMON_SAMPLE_RATES[self.selected_sample_rate_idx];
        let buffer_opt = BUFFER_SIZE_OPTIONS[self.selected_buffer_size_idx];
        let wants_input = processor.wants_input();
        let out_channels = processor.output_channel_count().max(1) as u16;

        match build_plugin_streams(
            self.selected_input_idx,
            &self.input_device_names,
            self.selected_output_idx,
            &self.output_device_names,
            sample_rate,
            buffer_opt,
            out_channels,
            processor,
            wants_input,
        ) {
            Ok((holder, info)) => {
                self._streams = Some(holder);
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

fn build_plugin_streams(
    in_idx: usize,
    in_names: &[String],
    out_idx: usize,
    out_names: &[String],
    sample_rate: u32,
    buffer_opt: (&str, Option<u32>),
    out_channels: u16,
    processor: PluginAudioProcessor,
    wants_input: bool,
) -> Result<(StreamHolder, RunningInfo), String> {
    let host = cpal::default_host();
    let output_device = get_output_device(&host, out_idx, out_names)?;
    let output_name = output_device
        .name()
        .unwrap_or_else(|_| "Unknown".to_string());

    let buffer_size = match buffer_opt.1 {
        Some(n) => BufferSize::Fixed(n),
        None => BufferSize::Default,
    };

    let out_config = StreamConfig {
        channels: out_channels,
        sample_rate: SampleRate(sample_rate),
        buffer_size: buffer_size.clone(),
    };

    let processor = Arc::new(Mutex::new(processor));
    let input_name;

    let holder = if wants_input {
        // Effect mode: capture input → feed to plugin → output
        let input_device = get_input_device(&host, in_idx, in_names)?;
        input_name = input_device
            .name()
            .unwrap_or_else(|_| "Unknown".to_string());

        let in_channels = out_channels; // match channels
        let in_config = StreamConfig {
            channels: in_channels,
            sample_rate: SampleRate(sample_rate),
            buffer_size: buffer_size.clone(),
        };

        // Ring buffer for input audio → output callback
        let capacity = (sample_rate as usize * in_channels as usize).max(65_536);
        let input_ring: Arc<Mutex<VecDeque<f32>>> =
            Arc::new(Mutex::new(VecDeque::with_capacity(capacity)));
        let ring_writer = Arc::clone(&input_ring);
        let ring_reader = Arc::clone(&input_ring);
        let max_fill = capacity;

        let in_stream = input_device
            .build_input_stream(
                &in_config,
                move |data: &[f32], _| {
                    if let Ok(mut buf) = ring_writer.try_lock() {
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

        let proc_clone = Arc::clone(&processor);
        let out_stream = output_device
            .build_output_stream(
                &out_config,
                move |data: &mut [f32], _| {
                    // Read input from ring
                    let input_buf: Vec<f32> = if let Ok(mut ring) = ring_reader.try_lock() {
                        let needed = data.len();
                        let mut buf = Vec::with_capacity(needed);
                        for _ in 0..needed {
                            buf.push(ring.pop_front().unwrap_or(0.0));
                        }
                        buf
                    } else {
                        vec![0.0; data.len()]
                    };

                    if let Ok(mut proc) = proc_clone.lock() {
                        proc.process(data, Some(&input_buf));
                    } else {
                        data.fill(0.0);
                    }
                },
                |err| eprintln!("[audio] output error: {err}"),
                None,
            )
            .map_err(|e| format!("Failed to build output stream: {e}"))?;

        let _ = in_stream.play();
        let _ = out_stream.play();

        StreamHolder::PluginBoth(in_stream, out_stream)
    } else {
        // Instrument mode: no audio input, just MIDI → plugin → output
        input_name = "None (Instrument)".to_string();

        let proc_clone = Arc::clone(&processor);
        let out_stream = output_device
            .build_output_stream(
                &out_config,
                move |data: &mut [f32], _| {
                    if let Ok(mut proc) = proc_clone.lock() {
                        proc.process(data, None);
                    } else {
                        data.fill(0.0);
                    }
                },
                |err| eprintln!("[audio] output error: {err}"),
                None,
            )
            .map_err(|e| format!("Failed to build output stream: {e}"))?;

        let _ = out_stream.play();

        StreamHolder::PluginOutput(out_stream)
    };

    let info = RunningInfo {
        input_device: input_name,
        output_device: output_name,
        sample_rate,
        buffer_size: buffer_opt.0.to_string(),
        channels: out_channels,
        mode: AudioMode::Plugin,
    };

    Ok((holder, info))
}
src/build_system.rs

rust
use std::io::{BufRead, BufReader};
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::thread;

use crate::plugin_host::loader;

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
        }
    }

    /// Start `cargo nih-plug bundle <name> --release`
    pub fn start_build(&mut self, project_path: &Path) {
        if self.status == BuildStatus::Building {
            return;
        }

        self.status = BuildStatus::Building;
        self.output_lines.clear();
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

fn run_nih_plug_bundle(project_path: &Path, tx: &mpsc::Sender<BuildMessage>) -> Result<(), String> {
    // Find the package name for the bundle command
    let package_name = loader::find_package_name(project_path)?;

    let _ = tx.send(BuildMessage::Stdout(format!(
        "Running: cargo nih-plug bundle {} --release",
        package_name
    )));

    let mut child = Command::new("cargo")
        .arg("nih-plug")
        .arg("bundle")
        .arg(&package_name)
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
use crate::build_system::{BuildStatus, BuildSystem};
use crate::midi_engine::MidiEngine;
use crate::plugin_host::loader;
use crate::plugin_host::{HostStatus, PluginHost, PluginMode};
use crate::project::Project;
use crate::scaffolding::{scaffold_project, ScaffoldOptions};
use crate::ui;
use crate::ui::midi_panel::{MidiSettingsPanel, PianoWidget};
use crate::ui::new_project_dialog::{NewProjectDialog, NewProjectResult};
use crate::ui::settings_panel::SettingsPanel;
use eframe::egui;

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum MidiRouting {
    /// MIDI goes to plugin when loaded, hardware output when not
    Auto,
    /// Always to plugin
    PluginOnly,
    /// Always to hardware
    HardwareOnly,
    /// Both plugin and hardware
    Both,
}

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
    midi_routing: MidiRouting,
    auto_reload_on_build: bool,
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
            midi_routing: MidiRouting::Auto,
            auto_reload_on_build: true,
        }
    }

    /// Whether MIDI should be sent to the plugin
    fn midi_to_plugin(&self) -> bool {
        match self.midi_routing {
            MidiRouting::Auto => self.plugin_host.is_loaded(),
            MidiRouting::PluginOnly | MidiRouting::Both => true,
            MidiRouting::HardwareOnly => false,
        }
    }

    /// Whether MIDI should be sent to hardware output
    fn midi_to_hardware(&self) -> bool {
        match self.midi_routing {
            MidiRouting::Auto => !self.plugin_host.is_loaded(),
            MidiRouting::HardwareOnly | MidiRouting::Both => true,
            MidiRouting::PluginOnly => false,
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
            AppAction::LoadPlugin => {
                self.try_load_plugin();
            }
            AppAction::UnloadPlugin => {
                self.audio_engine.stop();
                self.plugin_host.unload();
            }
            AppAction::TogglePluginGui => {
                if self.plugin_host.gui_open {
                    self.plugin_host.close_gui();
                } else {
                    if let Err(e) = self.plugin_host.open_gui() {
                        self.error_log.push(e);
                    }
                }
            }
        }
    }

    fn try_load_plugin(&mut self) {
        let project_path = match &self.project {
            Some(p) => p.config.path.clone(),
            None => {
                self.error_log.push("No project open".to_string());
                return;
            }
        };

        // Stop existing audio
        self.audio_engine.stop();
        self.plugin_host.unload();

        // Find .clap bundle
        let clap_path = match loader::find_clap_bundle(&project_path) {
            Ok(p) => p,
            Err(e) => {
                self.error_log.push(e);
                return;
            }
        };

        // Discover plugin ID
        let (plugin_id, _name) = match loader::discover_plugin_id(&clap_path) {
            Ok(r) => r,
            Err(e) => {
                self.error_log.push(e);
                return;
            }
        };

        // Load
        if let Err(e) = self.plugin_host.load(&clap_path, &plugin_id) {
            self.error_log.push(e);
            return;
        }

        // Activate
        let sample_rate = crate::audio_engine::COMMON_SAMPLE_RATES
            [self.audio_engine.selected_sample_rate_idx];
        let buffer_size = crate::audio_engine::BUFFER_SIZE_OPTIONS
            [self.audio_engine.selected_buffer_size_idx]
            .1
            .unwrap_or(512);

        let processor = match self.plugin_host.activate(sample_rate, 1, buffer_size) {
            Ok(p) => p,
            Err(e) => {
                self.error_log.push(e);
                self.plugin_host.unload();
                return;
            }
        };

        // Start audio with plugin
        self.audio_engine.start_with_plugin(processor);
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

    /// Check if build just succeeded and auto-reload plugin
    fn check_auto_reload(&mut self) {
        if self.auto_reload_on_build
            && self.build_system.status == BuildStatus::Success
            && self.project.is_some()
        {
            // Only reload once per build success — use a simple flag
            // We check if plugin path would differ or just reload anyway
            self.try_load_plugin();
        }
    }
}

impl eframe::App for PlaygroundApp {
    fn update(&mut self, ctx: &eframe::egui::Context, _frame: &mut eframe::Frame) {
        let prev_build_status = self.build_system.status.clone();
        self.build_system.poll();

        // Poll plugin host main thread callbacks + timers
        self.plugin_host.poll_main_thread();

        if self.build_system.status == BuildStatus::Building || self.plugin_host.is_loaded() {
            ctx.request_repaint();
        }

        // Auto-reload on build success
        if prev_build_status == BuildStatus::Building
            && self.build_system.status == BuildStatus::Success
        {
            if self.auto_reload_on_build {
                self.try_load_plugin();
            }
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
                    ui.separator();
                    ui.menu_button("MIDI Routing", |ui| {
                        ui.radio_value(&mut self.midi_routing, MidiRouting::Auto, "Auto");
                        ui.radio_value(
                            &mut self.midi_routing,
                            MidiRouting::PluginOnly,
                            "Plugin Only",
                        );
                        ui.radio_value(
                            &mut self.midi_routing,
                            MidiRouting::HardwareOnly,
                            "Hardware Only",
                        );
                        ui.radio_value(&mut self.midi_routing, MidiRouting::Both, "Both");
                    });
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
                    if ui
                        .add_enabled(
                            loaded && self.plugin_host.gui_supported,
                            egui::Button::new(if self.plugin_host.gui_open {
                                "✕  Close GUI"
                            } else {
                                "🖼  Open GUI"
                            }),
                        )
                        .clicked()
                    {
                        action = Some(AppAction::TogglePluginGui);
                        ui.close();
                    }
                    ui.separator();
                    ui.checkbox(&mut self.auto_reload_on_build, "Auto-reload on build");
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
                        HostStatus::Loaded => ("● Loaded", egui::Color32::YELLOW),
                        HostStatus::Active | HostStatus::Processing => {
                            ("● Plugin On", egui::Color32::from_rgb(100, 220, 100))
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
                // ── Plugin status section ──────────────────────────────────
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    ui.label(egui::RichText::new("🔌 Plugin").strong().size(14.0));
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if self.plugin_host.is_loaded() {
                            if self.plugin_host.gui_supported {
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
                    });
                });

                // Plugin info
                match &self.plugin_host.status {
                    HostStatus::Unloaded => {
                        ui.label(
                            egui::RichText::new("No plugin loaded")
                                .color(egui::Color32::GRAY)
                                .size(11.0),
                        );
                    }
                    HostStatus::Error(e) => {
                        ui.label(
                            egui::RichText::new(format!("Error: {e}"))
                                .color(egui::Color32::RED)
                                .size(11.0),
                        );
                    }
                    _ => {
                        if let Some(ref name) = self.plugin_host.plugin_name {
                            ui.label(
                                egui::RichText::new(format!("Plugin: {name}"))
                                    .size(12.0)
                                    .strong(),
                            );
                        }
                        let mode_str = match self.plugin_host.plugin_mode {
                            PluginMode::Instrument => "🎹 Instrument",
                            PluginMode::Effect => "🔊 Effect",
                        };
                        ui.label(
                            egui::RichText::new(mode_str)
                                .color(egui::Color32::from_rgb(150, 200, 255))
                                .size(11.0),
                        );
                    }
                }

                ui.separator();

                // ── Piano section (collapsible) ────────────────────────────
                ui.add_space(4.0);
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
                    self.piano
                        .show(ui, &self.midi_engine, &mut self.plugin_host, self.midi_to_plugin(), self.midi_to_hardware());
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
                                 Use Audio/MIDI menus to configure engines\n\
                                 Build → auto-loads plugin for testing",
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
    /// Draw piano and route MIDI to plugin and/or hardware based on routing flags.
    pub fn show(
        &mut self,
        ui: &mut egui::Ui,
        midi: &MidiEngine,
        plugin_host: &mut PluginHost,
        send_to_plugin: bool,
        send_to_hardware: bool,
    ) {
        // Drain incoming hardware MIDI events into the log and forward to plugin
        for event in midi.drain_events() {
            let desc = match &event.kind {
                MidiMessageKind::NoteOn {
                    channel,
                    note,
                    velocity,
                } => {
                    // Forward hardware MIDI to plugin
                    if send_to_plugin {
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
                    if send_to_plugin {
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
                    if send_to_plugin {
                        plugin_host
                            .send_midi_raw(&[0xB0 | (channel & 0x0F), *cc, *value]);
                    }
                    format!("CC      ch:{} cc:{} val:{}", channel + 1, cc, value)
                }
                MidiMessageKind::PitchBend { channel, value } => {
                    let raw = (*value + 8192) as u16;
                    if send_to_plugin {
                        plugin_host.send_midi_raw(&[
                            0xE0 | (channel & 0x0F),
                            (raw & 0x7F) as u8,
                            ((raw >> 7) & 0x7F) as u8,
                        ]);
                    }
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
            self.draw_keyboard(ui, midi, plugin_host, send_to_plugin, send_to_hardware);
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
                egui::Slider::new(&mut ch_display, 1..=16).clamping(egui::SliderClamping::Always),
            );
            self.channel = ch_display - 1;
        });
    }

    fn draw_keyboard(
        &mut self,
        ui: &mut egui::Ui,
        midi: &MidiEngine,
        plugin_host: &mut PluginHost,
        send_to_plugin: bool,
        send_to_hardware: bool,
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

        let mut newly_pressed: Option<u8> = None;
        let mut newly_released: Vec<u8> = Vec::new();

        if response.drag_stopped()
            || (!response.is_pointer_button_down_on() && !self.held_notes.is_empty())
        {
            for note in self.held_notes.drain() {
                if send_to_plugin {
                    plugin_host.send_note_off(self.channel, note);
                }
                if send_to_hardware {
                    midi.send_note_off(self.channel, note);
                }
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
                    if send_to_plugin {
                        plugin_host.send_note_on(self.channel, note, self.velocity);
                    }
                    if send_to_hardware {
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
                    if send_to_plugin {
                        plugin_host.send_note_off(self.channel, n);
                    }
                    if send_to_hardware {
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
Summary of new/changed files:

File	Status
Cargo.toml	Modified — added clack + rtrb deps
src/main.rs	Modified — added mod plugin_host
src/plugin_host/mod.rs	New
src/plugin_host/handlers.rs	New
src/plugin_host/timer.rs	New
src/plugin_host/gui.rs	New
src/plugin_host/loader.rs	New
src/plugin_host/midi_bridge.rs	New
src/plugin_host/audio/mod.rs	New
src/plugin_host/audio/config.rs	New
src/audio_engine.rs	Modified — added start_with_plugin(), plugin stream modes
src/build_system.rs	Modified — uses cargo nih-plug bundle
src/app.rs	Modified — plugin host integration, routing, auto-reload
src/ui/midi_panel.rs	Modified — dual routing, plugin_host param, collapsible piano
Unchanged files (not included): midi_engine.rs, project.rs, scaffolding.rs, templates/*, ui/mod.rs, ui/build_panel.rs, ui/code_editor.rs, ui/file_browser.rs, ui/new_project_dialog.rs, ui/settings_panel.rs, ui/top_bar.rs.
