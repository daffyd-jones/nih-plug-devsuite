//! Native OS window for hosting an embedded CLAP plugin GUI.
//!
//! Each platform implements the same four-method contract. The public
//! `PluginGuiWindow` type dispatches to the compiled backend via `#[path]`
//! module aliasing, so no platform code leaks into callers.
//!
//! Lifecycle contract (enforced by `PluginHost`, documented here for
//! backend implementors):
//!
//!   1. `create()`        → native window exists, visible, empty
//!   2. plugin `set_parent(clap_window())` → plugin creates child inside us
//!   3. `poll_events()` each frame → returns `true` when user hits close
//!   4. plugin `destroy()` → plugin tears down its child
//!   5. `drop()`          → native window destroyed
//!
//! Step 4 **must** happen before step 5. Backends must therefore NOT
//! destroy themselves on close-request — they set a flag and wait.

use clack_extensions::gui::Window as ClapWindow;

#[cfg(target_os = "windows")]
#[path = "win32.rs"]
mod platform;

#[cfg(target_os = "linux")]
#[path = "x11.rs"]
mod platform;

// Fallback stub so the crate still compiles on macOS etc.
// open_gui() will return a friendly error instead of a link failure.
#[cfg(not(any(target_os = "windows", target_os = "linux")))]
mod platform {
    use super::ClapWindow;

    pub struct NativeWindow;

    impl NativeWindow {
        pub fn create(_title: &str, _w: u32, _h: u32) -> Result<Self, String> {
            Err("Embedded plugin GUI is not implemented on this platform. \
                 The plugin may still offer a floating window."
                .into())
        }
        pub fn clap_window(&self) -> ClapWindow {
            unreachable!()
        }
        pub fn resize(&mut self, _w: u32, _h: u32) {}
        pub fn poll_events(&mut self) -> bool {
            false
        }
    }
}

/// A native top-level window that owns nothing but a frame and a close
/// button. The plugin paints into a child window inside its client area.
///
/// Dropping this destroys the native window. **Always** destroy the
/// plugin GUI first — see module docs.
pub struct PluginGuiWindow(platform::NativeWindow);

impl PluginGuiWindow {
    /// Create a top-level window whose *client area* is `width × height`.
    /// Frame/titlebar are added on top of that by the OS.
    pub fn create(title: &str, width: u32, height: u32) -> Result<Self, String> {
        platform::NativeWindow::create(title, width, height).map(Self)
    }

    /// The handle to pass into `clap_plugin_gui.set_parent()`.
    pub fn clap_window(&self) -> ClapWindow {
        self.0.clap_window()
    }

    /// Resize the client area. Call this when the plugin sends a
    /// `request_resize` callback.
    pub fn resize(&mut self, width: u32, height: u32) {
        self.0.resize(width, height);
    }

    /// Pump pending window events. Call once per UI frame from the main
    /// thread. Returns `true` when the user has asked to close the window
    /// (clicked ×, Alt+F4, etc.) — the caller should then tear down the
    /// plugin GUI and drop this struct.
    pub fn poll_events(&mut self) -> bool {
        self.0.poll_events()
    }
}
