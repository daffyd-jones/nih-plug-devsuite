#![allow(unsafe_code)]

use crate::plugin_host::handlers::{DevHostMainThread, DevHostShared};
use clack_extensions::gui::{
    GuiApiType, GuiConfiguration, GuiError, GuiSize, PluginGui, Window as ClapWindow,
};
use clack_host::prelude::*;

pub struct Gui {
    pub plugin_gui: PluginGui,
    pub configuration: Option<GuiConfiguration<'static>>,
    is_open: bool,
    pub is_resizable: bool,
    /// Size reported by the plugin at creation time
    pub initial_size: Option<GuiSize>,
}

impl Gui {
    pub fn new(plugin_gui: PluginGui, instance: &mut PluginMainThreadHandle) -> Self {
        let config = Self::negotiate_configuration(&plugin_gui, instance);
        println!("[gui] GUI initialization result: {:?}", config.is_some());
        Self {
            configuration: config,
            plugin_gui,
            is_open: false,
            is_resizable: false,
            initial_size: None,
        }
    }

    fn negotiate_configuration(
        gui: &PluginGui,
        plugin: &mut PluginMainThreadHandle,
    ) -> Option<GuiConfiguration<'static>> {
        let api_type = GuiApiType::default_for_current_platform()?;

        let embedded = GuiConfiguration {
            api_type,
            is_floating: false,
        };
        if gui.is_api_supported(plugin, embedded) {
            println!("[gui] Plugin supports embedded GUI with {:?}", api_type);
            return Some(embedded);
        }

        let floating = GuiConfiguration {
            api_type,
            is_floating: true,
        };
        if gui.is_api_supported(plugin, floating) {
            println!("[gui] Plugin supports floating GUI with {:?}", api_type);
            return Some(floating);
        }

        println!("[gui] Plugin does not support any GUI API");
        None
    }

    pub fn needs_floating(&self) -> Option<bool> {
        self.configuration.map(|c| c.is_floating)
    }

    /// Open as embedded, parenting into the provided raw window handle.
    ///
    /// Returns the size the plugin wants to occupy, so the caller can
    /// resize its container accordingly.
    ///
    /// # Safety
    /// The caller must ensure the parent window outlives this GUI.
    pub unsafe fn open_embedded(
        &mut self,
        plugin: &mut PluginMainThreadHandle,
        parent: ClapWindow,
    ) -> Result<GuiSize, GuiError> {
        let configuration = match self.configuration {
            Some(c) if !c.is_floating => c,
            _ => {
                println!("[gui] No embedded configuration available");
                return Err(GuiError::CreateError);
            }
        };

        self.plugin_gui.create(plugin, configuration)?;

        // Check resize capability before set_parent
        self.is_resizable = self.plugin_gui.can_resize(plugin);

        // Get plugin's preferred size (fall back to a sane default)
        let size = self.plugin_gui.get_size(plugin).unwrap_or(GuiSize {
            width: 640,
            height: 480,
        });
        self.initial_size = Some(size);

        // This is the critical call that was missing in your original code
        // SAFETY: caller guarantees the window is valid
        unsafe {
            self.plugin_gui.set_parent(plugin, parent)?;
        }

        // Some plugins ignore show() errors, so we swallow it
        let _ = self.plugin_gui.show(plugin);
        self.is_open = true;

        Ok(size)
    }

    pub fn open_floating(&mut self, plugin: &mut PluginMainThreadHandle) -> Result<(), GuiError> {
        let configuration = match self.configuration {
            Some(c) if c.is_floating => c,
            Some(c) => GuiConfiguration {
                api_type: c.api_type,
                is_floating: true,
            },
            None => return Err(GuiError::CreateError),
        };

        self.plugin_gui.create(plugin, configuration)?;
        self.plugin_gui.suggest_title(plugin, c"Plugin");
        self.plugin_gui.show(plugin)?;
        self.is_open = true;
        Ok(())
    }

    /// Tell the plugin to resize. Returns the size it actually agreed to use.
    pub fn resize(&mut self, plugin: &mut PluginMainThreadHandle, requested: GuiSize) -> GuiSize {
        if !self.is_resizable {
            return self.plugin_gui.get_size(plugin).unwrap_or(requested);
        }
        let working = self
            .plugin_gui
            .adjust_size(plugin, requested)
            .unwrap_or(requested);
        let _ = self.plugin_gui.set_size(plugin, working);
        working
    }

    pub fn destroy(&mut self, plugin: &mut PluginMainThreadHandle) {
        if self.is_open {
            self.plugin_gui.destroy(plugin);
            self.is_open = false;
            self.initial_size = None;
        }
    }
}
