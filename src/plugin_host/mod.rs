#![allow(unsafe_code)]

pub mod audio;
pub mod gui;
pub mod handlers;
pub mod loader;
pub mod midi_bridge;
pub mod timer;

use crate::plugin_host::audio::{PluginAudioConfig, PluginAudioProcessor};
use crate::plugin_host::gui::Gui;
use crate::plugin_host::handlers::{DevHost, DevHostMainThread, DevHostShared, MainThreadMessage};
use crate::plugin_host::loader::PluginBinary;
use crate::plugin_host::midi_bridge::RawMidiEvent;
use crate::plugin_host::timer::Timers;

use clack_host::prelude::*;
use crossbeam_channel::{unbounded, Receiver, Sender};
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
        println!("[plugin_host] Checking for GUI extension...");
        if let Some(gui_ext) = gui_ext {
            println!("[plugin_host] GUI extension found, initializing...");
            if let Some(ref mut instance) = self.instance {
                let gui = Gui::new(gui_ext, &mut instance.plugin_handle());
                self.gui = Some(gui);
                println!("[plugin_host] GUI initialized successfully");
            }
        } else {
            println!("[plugin_host] No GUI extension available");
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

        let instance = self.instance.as_mut().ok_or("No plugin instance")?;

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
            let (timer_ext, timers) = instance.access_handler(|h| {
                (h.timer_support, h.timers.clone())
            });
            if let Some(timer_ext) = timer_ext {
                timers.tick_timers(&timer_ext, &mut instance.plugin_handle());
            }
        }
    }

    /// Open the plugin's GUI.
    pub fn open_gui(&mut self) -> Result<(), String> {
        if self.gui_open {
            return Ok(());
        }

        let gui = self.gui.as_mut().ok_or("Plugin has no GUI")?;
        let instance = self.instance.as_mut().ok_or("No plugin instance")?;

        // Try embedded first, then fall back to floating
        match gui.needs_floating() {
            Some(false) => {
                // Try embedded GUI first
                match gui.open_embedded(&mut instance.plugin_handle()) {
                    Ok(()) => {
                        self.gui_open = true;
                        println!("[plugin_host] Opened embedded GUI");
                        Ok(())
                    }
                    Err(e) => {
                        println!("[plugin_host] Embedded GUI failed, trying floating: {e}");
                        // Fall back to floating
                        gui.open_floating(&mut instance.plugin_handle())
                            .map_err(|e| format!("Failed to open floating GUI: {e}"))?;
                        self.gui_open = true;
                        println!("[plugin_host] Opened GUI (floating fallback)");
                        Ok(())
                    }
                }
            }
            Some(true) | None => {
                // Open floating (either preferred or only option)
                gui.open_floating(&mut instance.plugin_handle())
                    .map_err(|e| format!("Failed to open floating GUI: {e}"))?;
                self.gui_open = true;
                println!("[plugin_host] Opened floating GUI");
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
