#![allow(unsafe_code)]
//! X11 backend for `PluginGuiWindow`.
//!
//! Untested port of the previous `make_child_window` helper, upgraded to
//! create a proper top-level window with:
//!   * `WM_DELETE_WINDOW` protocol (close button works)
//!   * `WM_NAME` + `_NET_WM_NAME` (window title)
//!   * `WM_CLASS` (window manager grouping / taskbar icon)
//!
//! Unlike Win32, X11 has no thread-wide message pump that automatically
//! dispatches to our connection — the plugin opens its own `Display*` and
//! runs its own event loop. So `poll_events()` here actively drains our
//! connection's queue looking for the delete-window `ClientMessage`.
//!
//! The `RustConnection` is kept alive in the struct because x11rb ties
//! the window's lifetime to it.

use clack_extensions::gui::Window as ClapWindow;

use x11rb::connection::Connection;
use x11rb::protocol::xproto::{
    AtomEnum, ConfigureWindowAux, ConnectionExt as _, CreateWindowAux, EventMask, PropMode,
    WindowClass,
};
use x11rb::protocol::Event;
use x11rb::rust_connection::RustConnection;
use x11rb::wrapper::ConnectionExt as _; // change_property8 / change_property32

pub struct NativeWindow {
    conn: RustConnection,
    window: u32,
    /// Stored so `poll_events()` can match incoming `ClientMessage.data[0]`.
    atom_wm_delete_window: u32,
    close_requested: bool,
}

impl NativeWindow {
    pub fn create(title: &str, width: u32, height: u32) -> Result<Self, String> {
        let (conn, screen_num) =
            RustConnection::connect(None).map_err(|e| format!("X11 connect failed: {e}"))?;

        // Clamp to u16 — X11 window dimensions are 16-bit. Also avoid 0,
        // which is a BadValue.
        let w = width.clamp(1, u16::MAX as u32) as u16;
        let h = height.clamp(1, u16::MAX as u32) as u16;

        let screen = &conn.setup().roots[screen_num];
        let root = screen.root;
        let window = conn
            .generate_id()
            .map_err(|e| format!("generate_id failed: {e}"))?;

        // ── Intern atoms up front ──
        let atom_wm_protocols = intern_atom(&conn, b"WM_PROTOCOLS")?;
        let atom_wm_delete_window = intern_atom(&conn, b"WM_DELETE_WINDOW")?;
        let atom_net_wm_name = intern_atom(&conn, b"_NET_WM_NAME")?;
        let atom_utf8_string = intern_atom(&conn, b"UTF8_STRING")?;

        // ── Create the window (top-level: parent = root) ──
        // STRUCTURE_NOTIFY lets us receive ConfigureNotify/DestroyNotify.
        // ClientMessage is always delivered regardless of mask, but setting
        // a sane mask is good hygiene.
        let aux = CreateWindowAux::new()
            .background_pixel(screen.black_pixel)
            .event_mask(EventMask::STRUCTURE_NOTIFY);

        conn.create_window(
            x11rb::COPY_DEPTH_FROM_PARENT,
            window,
            root,
            0,
            0,
            w,
            h,
            0, // border_width
            WindowClass::INPUT_OUTPUT,
            screen.root_visual,
            &aux,
        )
        .map_err(|e| format!("create_window failed: {e}"))?
        .check()
        .map_err(|e| format!("create_window rejected: {e}"))?;

        // ── WM_PROTOCOLS = [WM_DELETE_WINDOW] ──
        // Without this the WM kills our connection when the user clicks ×.
        // With it, we get a polite ClientMessage instead.
        conn.change_property32(
            PropMode::REPLACE,
            window,
            atom_wm_protocols,
            AtomEnum::ATOM,
            &[atom_wm_delete_window],
        )
        .map_err(|e| format!("set WM_PROTOCOLS failed: {e}"))?;

        // ── Title (legacy WM_NAME, Latin-1/ASCII) ──
        conn.change_property8(
            PropMode::REPLACE,
            window,
            AtomEnum::WM_NAME,
            AtomEnum::STRING,
            title.as_bytes(),
        )
        .map_err(|e| format!("set WM_NAME failed: {e}"))?;

        // ── Title (EWMH _NET_WM_NAME, UTF-8) ──
        // Modern WMs prefer this; also handles non-ASCII plugin names.
        conn.change_property8(
            PropMode::REPLACE,
            window,
            atom_net_wm_name,
            atom_utf8_string,
            title.as_bytes(),
        )
        .map_err(|e| format!("set _NET_WM_NAME failed: {e}"))?;

        // ── WM_CLASS (instance\0class\0) ──
        // Lets the WM group our plugin windows and pick an icon.
        conn.change_property8(
            PropMode::REPLACE,
            window,
            AtomEnum::WM_CLASS,
            AtomEnum::STRING,
            b"nih-plug-playground\0NihPlugPlayground\0",
        )
        .map_err(|e| format!("set WM_CLASS failed: {e}"))?;

        // ── Map (show) and flush ──
        conn.map_window(window)
            .map_err(|e| format!("map_window failed: {e}"))?;
        conn.flush().map_err(|e| format!("flush failed: {e}"))?;

        Ok(Self {
            conn,
            window,
            atom_wm_delete_window,
            close_requested: false,
        })
    }

    pub fn clap_window(&self) -> ClapWindow {
        // CLAP's X11 window type is Xlib's `Window` (c_ulong). xcb uses u32.
        // On every platform we care about this widening cast is lossless.
        ClapWindow::from_x11_handle(self.window as std::ffi::c_ulong)
    }

    pub fn resize(&mut self, width: u32, height: u32) {
        let aux = ConfigureWindowAux::new()
            .width(width.clamp(1, u16::MAX as u32))
            .height(height.clamp(1, u16::MAX as u32));
        // Errors here are non-fatal; log and move on.
        if let Err(e) = self.conn.configure_window(self.window, &aux) {
            eprintln!("[gui_window/x11] configure_window failed: {e}");
        }
        let _ = self.conn.flush();
    }

    pub fn poll_events(&mut self) -> bool {
        // Drain everything queued on our connection. We only care about
        // the WM_DELETE_WINDOW ClientMessage; everything else is dropped.
        // (ConfigureNotify etc. could be handled here later if we add
        // user-resizable windows.)
        loop {
            match self.conn.poll_for_event() {
                Ok(Some(Event::ClientMessage(ev))) => {
                    // format 32 + data32[0] == WM_DELETE_WINDOW is the
                    // ICCCM-specified close request.
                    if ev.window == self.window
                        && ev.format == 32
                        && ev.data.as_data32()[0] == self.atom_wm_delete_window
                    {
                        self.close_requested = true;
                    }
                }
                Ok(Some(_other)) => {
                    // Ignore. Plugin child windows use their own connection
                    // so their events don't land here.
                }
                Ok(None) => break, // queue empty
                Err(e) => {
                    // Connection died — treat as close so the host tears
                    // down cleanly rather than spinning on a dead socket.
                    eprintln!("[gui_window/x11] poll_for_event error: {e}");
                    self.close_requested = true;
                    break;
                }
            }
        }
        self.close_requested
    }
}

impl Drop for NativeWindow {
    fn drop(&mut self) {
        // Best-effort cleanup. If the connection is already dead these
        // will error, which is fine — the server cleans up orphaned
        // resources when the socket closes.
        let _ = self.conn.destroy_window(self.window);
        let _ = self.conn.flush();
        // `conn` drops here, closing the socket.
    }
}

// ── Helpers ──────────────────────────────────────────────────────────────────

/// Intern an atom by name, blocking on the reply. All the atoms we need
/// are well-known and will always succeed unless the server is broken.
fn intern_atom(conn: &RustConnection, name: &[u8]) -> Result<u32, String> {
    conn.intern_atom(false, name)
        .map_err(|e| format!("intern_atom({}) send: {e}", String::from_utf8_lossy(name)))?
        .reply()
        .map_err(|e| format!("intern_atom({}) reply: {e}", String::from_utf8_lossy(name)))
        .map(|r| r.atom)
}

