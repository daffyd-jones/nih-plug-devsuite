#![allow(unsafe_code)]

use crate::plugin_host::handlers::{DevHostMainThread, DevHostShared};
use clack_extensions::gui::{GuiApiType, GuiConfiguration, GuiError, GuiSize, PluginGui};
use clack_host::prelude::*;

pub struct Gui {
    plugin_gui: PluginGui,
    pub configuration: Option<GuiConfiguration<'static>>,
    is_open: bool,
}

impl Gui {
    pub fn new(plugin_gui: PluginGui, instance: &mut PluginMainThreadHandle) -> Self {
        let config = Self::negotiate_configuration(&plugin_gui, instance);
        println!("[gui] GUI initialization result: {:?}", config.is_some());
        Self {
            configuration: config,
            plugin_gui,
            is_open: false,
        }
    }

    fn negotiate_configuration(
        gui: &PluginGui,
        plugin: &mut PluginMainThreadHandle,
    ) -> Option<GuiConfiguration<'static>> {
        // Check if we're running on Wayland
        let is_wayland = cfg!(target_os = "linux") && 
            (std::env::var("WAYLAND_DISPLAY").is_ok() || std::env::var("XDG_SESSION_TYPE").as_deref() == Ok("wayland"));
        
        if is_wayland {
            println!("[gui] Detected Wayland environment - floating GUI only");
        }

        // Get the default API type for the platform
        let api_type = match GuiApiType::default_for_current_platform() {
            Some(api) => api,
            None => {
                println!("[gui] No default GUI API available for this platform");
                return None;
            }
        };

        if is_wayland {
            // On Wayland, only try floating (embedding not supported)
            let config = GuiConfiguration {
                api_type,
                is_floating: true,
            };
            if gui.is_api_supported(plugin, config) {
                println!("[gui] Plugin supports floating GUI on Wayland with {:?}", api_type);
                return Some(config);
            } else {
                println!("[gui] Plugin does not support floating GUI on Wayland");
                return None;
            }
        }

        // On X11 or other platforms, try embedded first, then floating
        // Try embedded first (preferred)
        let config = GuiConfiguration {
            api_type,
            is_floating: false,
        };
        if gui.is_api_supported(plugin, config) {
            println!("[gui] Plugin supports embedded GUI with {:?}", api_type);
            return Some(config);
        }

        // Fall back to floating
        let config = GuiConfiguration {
            api_type,
            is_floating: true,
        };
        if gui.is_api_supported(plugin, config) {
            println!("[gui] Plugin supports floating GUI with {:?}", api_type);
            return Some(config);
        }

        println!("[gui] Plugin does not support any GUI API");
        None
    }

    /// Returns `Some(true)` if floating, `Some(false)` if embedded, `None` if no GUI.
    pub fn needs_floating(&self) -> Option<bool> {
        self.configuration.map(|c| c.is_floating)
    }

    /// Open the plugin's GUI as an embedded window.
    /// This is the preferred method when supported.
    pub fn open_embedded(&mut self, plugin: &mut PluginMainThreadHandle) -> Result<(), GuiError> {
        println!("[gui] Attempting to open embedded GUI");
        
        let configuration = match self.configuration {
            Some(c) if !c.is_floating => c,
            _ => {
                println!("[gui] No embedded GUI configuration available");
                return Err(GuiError::CreateError);
            }
        };

        println!("[gui] Creating embedded GUI with API type: {:?}", configuration.api_type);
        match self.plugin_gui.create(plugin, configuration) {
            Ok(()) => {
                println!("[gui] GUI created successfully");
            }
            Err(e) => {
                println!("[gui] Failed to create GUI: {:?}", e);
                return Err(e);
            }
        }
        
        // For embedded GUI, we would need to:
        // 1. Get the window handle from the egui context
        // 2. Set scaling if needed (not for X11/Wayland which use physical pixels)
        // 3. Get initial size or set size
        // 4. Set parent window
        // 5. Show the GUI
        
        // For now, we'll just show it as a simple implementation
        // A full implementation would require egui window handle integration
        self.plugin_gui.show(plugin)?;
        self.is_open = true;

        Ok(())
    }

    /// Open the plugin's GUI as a floating window.
    pub fn open_floating(&mut self, plugin: &mut PluginMainThreadHandle) -> Result<(), GuiError> {
        println!("[gui] Attempting to open floating GUI");
        
        // If no configuration supports floating, force it
        let configuration = match self.configuration {
            Some(c) if c.is_floating => {
                println!("[gui] Using existing floating configuration");
                c
            },
            Some(c) => {
                // Try floating anyway
                let floating_config = GuiConfiguration {
                    api_type: c.api_type,
                    is_floating: true,
                };
                if self.plugin_gui.is_api_supported(plugin, floating_config) {
                    println!("[gui] Plugin supports floating mode");
                    floating_config
                } else {
                    // Last resort: just use what we have — some plugins accept create() with
                    // floating even if is_api_supported returned false for it.
                    println!("[gui] Forcing floating mode as last resort");
                    floating_config
                }
            }
            None => {
                println!("[gui] No GUI configuration available");
                return Err(GuiError::CreateError);
            }
        };

        println!("[gui] Creating floating GUI with API type: {:?}", configuration.api_type);
        match self.plugin_gui.create(plugin, configuration) {
            Ok(()) => {
                println!("[gui] GUI created successfully");
            }
            Err(e) => {
                println!("[gui] Failed to create GUI: {:?}", e);
                return Err(e);
            }
        }
        
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
