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

    /// Phase 1 of embedded open: create the plugin GUI and query its
    /// preferred size. Call this **before** creating your host window so
    /// you know how big to make it.
    ///
    /// After this returns `Ok`, `self.is_open` is `true` and you **must**
    /// eventually call `destroy()` — even if `attach_and_show()` later
    /// fails. CLAP mandates `destroy()` after every successful `create()`.
    pub fn create_for_embedding(
        &mut self,
        plugin: &mut PluginMainThreadHandle,
    ) -> Result<GuiSize, GuiError> {
        let configuration = self
            .configuration
            .filter(|c| !c.is_floating)
            .ok_or(GuiError::CreateError)?;

        self.plugin_gui.create(plugin, configuration)?;

        // Set this immediately after create() succeeds, not after show().
        // If the caller bails between here and attach_and_show(), they
        // call destroy() and this flag makes that work.
        // (Note: open_floating() has the same latent issue — it sets
        //  is_open only after show(). Left as-is for now since we're not
        //  touching the floating path.)
        self.is_open = true;

        self.is_resizable = self.plugin_gui.can_resize(plugin);
        let size = self.plugin_gui.get_size(plugin).unwrap_or(GuiSize {
            width: 640,
            height: 480,
        });
        self.initial_size = Some(size);
        Ok(size)
    }

    /// Phase 2 of embedded open: parent the plugin into the host window
    /// and show it.
    ///
    /// # Safety
    /// The caller guarantees `parent` refers to a window that will outlive
    /// this `Gui` — i.e. it won't be destroyed until after `destroy()` has
    /// been called on this struct.
    pub unsafe fn attach_and_show(
        &mut self,
        plugin: &mut PluginMainThreadHandle,
        parent: ClapWindow,
    ) -> Result<(), GuiError> {
        self.plugin_gui.set_parent(plugin, parent)?;
        // Some plugins return an error from show() but still display fine.
        // Swallow it like the old open_embedded did.
        let _ = self.plugin_gui.show(plugin);
        Ok(())
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
