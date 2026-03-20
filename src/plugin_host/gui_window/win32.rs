#![allow(unsafe_code)]
//! Win32 backend for `PluginGuiWindow`.
//!
//! Design notes:
//!
//! * **Message pumping** — we don't run our own `PeekMessage` loop. eframe's
//!   winit backend already pumps the thread-wide message queue every frame,
//!   and Windows dispatches to *every* window on the thread, including ours.
//!   Our `wnd_proc` runs during that dispatch. `poll_events()` just reads
//!   the flag the proc sets.
//!
//! * **WM_CLOSE** — we swallow it and raise a flag instead of calling
//!   `DestroyWindow`. The host reads the flag, destroys the *plugin* GUI
//!   first, then drops us (which finally calls `DestroyWindow`). This
//!   ordering stops the plugin from painting into a freed HWND.
//!
//! * **WS_CLIPCHILDREN** — without this, Windows repaints our background
//!   *over* the plugin's child window on every WM_PAINT, causing flicker.
//!
//! * **Close flag storage** — `Box<AtomicBool>` gives a stable heap address
//!   we can stash in `GWLP_USERDATA`. The box is pinned for the lifetime of
//!   the struct; we null the userdata slot in `Drop` before the box dies.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::OnceLock;

use clack_extensions::gui::Window as ClapWindow;

use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{HINSTANCE, HMODULE, HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{GetStockObject, BLACK_BRUSH, HBRUSH};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::WindowsAndMessaging::{
    AdjustWindowRectEx, CreateWindowExW, DefWindowProcW, DestroyWindow, GetWindowLongPtrW,
    RegisterClassW, SetWindowLongPtrW, SetWindowPos, ShowWindow, CS_HREDRAW, CS_VREDRAW,
    CW_USEDEFAULT, GWLP_USERDATA, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOZORDER, SW_SHOWNORMAL,
    WINDOW_EX_STYLE, WINDOW_STYLE, WM_CLOSE, WNDCLASSW, WS_CAPTION, WS_CLIPCHILDREN,
    WS_MINIMIZEBOX, WS_OVERLAPPED, WS_SYSMENU,
};

// ── Window class registration (once per process) ─────────────────────────────

/// Stores the atom returned by `RegisterClassW`. 0 = failure.
static CLASS_ATOM: OnceLock<u16> = OnceLock::new();
const CLASS_NAME: PCWSTR = w!("NihPlaygroundPluginHostWnd");

/// Non-resizable frame with titlebar, close button, and minimize.
/// `WS_CLIPCHILDREN` is load-bearing — see module docs.
///
/// To make the window user-resizable later (when the plugin reports
/// `can_resize() == true`), add `WS_THICKFRAME | WS_MAXIMIZEBOX` and
/// handle `WM_SIZE` to forward the new size to the plugin.
const STYLE: WINDOW_STYLE = WINDOW_STYLE(
    WS_OVERLAPPED.0 | WS_CAPTION.0 | WS_SYSMENU.0 | WS_MINIMIZEBOX.0 | WS_CLIPCHILDREN.0,
);

// ── NativeWindow ─────────────────────────────────────────────────────────────

pub struct NativeWindow {
    hwnd: HWND,
    /// Heap-allocated so the raw pointer in GWLP_USERDATA remains valid
    /// for the window's entire lifetime. Cleared in `Drop` before the
    /// box is freed.
    close_requested: Box<AtomicBool>,
}

impl NativeWindow {
    pub fn create(title: &str, width: u32, height: u32) -> Result<Self, String> {
        unsafe {
            let hmodule: HMODULE =
                GetModuleHandleW(None).map_err(|e| format!("GetModuleHandleW failed: {e}"))?;
            // HINSTANCE and HMODULE wrap the same underlying handle; the
            // windows crate provides `From<HMODULE> for HINSTANCE`.
            // If your windows-crate version lacks this impl, use
            // `HINSTANCE(hmodule.0)` instead.
            let hinstance: HINSTANCE = hmodule.into();

            // ── Register class once ──
            let bg_brush = GetStockObject(BLACK_BRUSH);
            let atom = *CLASS_ATOM.get_or_init(|| {
                let wc = WNDCLASSW {
                    style: CS_HREDRAW | CS_VREDRAW,
                    lpfnWndProc: Some(wnd_proc),
                    hInstance: hinstance,
                    lpszClassName: CLASS_NAME,
                    // Black background so there's no white flash before
                    // the plugin paints. HGDIOBJ → HBRUSH is a safe cast
                    // for stock brushes.
                    hbrBackground: HBRUSH(bg_brush.0),
                    ..Default::default()
                };
                RegisterClassW(&wc)
            });
            if atom == 0 {
                return Err("RegisterClassW failed (atom == 0)".into());
            }

            // ── Compute outer size from desired client size ──
            let mut rect = RECT {
                left: 0,
                top: 0,
                right: width.max(1) as i32,
                bottom: height.max(1) as i32,
            };
            let _ = AdjustWindowRectEx(&mut rect, STYLE, false, WINDOW_EX_STYLE(0));
            let outer_w = rect.right - rect.left;
            let outer_h = rect.bottom - rect.top;

            // ── Encode title as null-terminated UTF-16 ──
            let title_w: Vec<u16> = title.encode_utf16().chain(std::iter::once(0)).collect();

            // ── Allocate the close flag before creating the window so we
            //    can attach it immediately after creation ──
            let close_flag = Box::new(AtomicBool::new(false));

            // ── Create ──
            // Note: the hinstance parameter is `Option<HINSTANCE>` in
            // windows 0.58. If your version takes `HINSTANCE` directly,
            // drop the `Some(...)`.
            let hwnd = CreateWindowExW(
                WINDOW_EX_STYLE(0),
                CLASS_NAME,
                PCWSTR(title_w.as_ptr()),
                STYLE,
                CW_USEDEFAULT,
                CW_USEDEFAULT,
                outer_w,
                outer_h,
                None, // no parent — top-level
                None, // no menu
                hinstance,
                None, // no lpParam
            )
            .map_err(|e| format!("CreateWindowExW failed: {e}"))?;

            // ── Wire up userdata → close flag ──
            let flag_ptr = &*close_flag as *const AtomicBool;
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, flag_ptr as isize);

            let _ = ShowWindow(hwnd, SW_SHOWNORMAL);

            Ok(Self {
                hwnd,
                close_requested: close_flag,
            })
        }
    }

    pub fn clap_window(&self) -> ClapWindow {
        // HWND.0 is either `isize` or `*mut c_void` depending on windows
        // crate version — `as *mut _` handles both.
        ClapWindow::from_win32_hwnd(self.hwnd.0 as *mut std::ffi::c_void)
    }

    pub fn resize(&mut self, width: u32, height: u32) {
        unsafe {
            let mut rect = RECT {
                left: 0,
                top: 0,
                right: width.max(1) as i32,
                bottom: height.max(1) as i32,
            };
            let _ = AdjustWindowRectEx(&mut rect, STYLE, false, WINDOW_EX_STYLE(0));
            let _ = SetWindowPos(
                self.hwnd,
                None,
                0,
                0,
                rect.right - rect.left,
                rect.bottom - rect.top,
                SWP_NOMOVE | SWP_NOZORDER | SWP_NOACTIVATE,
            );
        }
    }

    pub fn poll_events(&mut self) -> bool {
        // No explicit pump — see module docs. winit dispatches for us.
        self.close_requested.load(Ordering::Relaxed)
    }
}

impl Drop for NativeWindow {
    fn drop(&mut self) {
        unsafe {
            // Detach the userdata pointer BEFORE DestroyWindow. Destruction
            // sends WM_DESTROY/WM_NCDESTROY synchronously via our wnd_proc;
            // if any path in there (or a plugin hook) reads userdata, it
            // must see null rather than a pointer into a box that's about
            // to be freed.
            SetWindowLongPtrW(self.hwnd, GWLP_USERDATA, 0);
            let _ = DestroyWindow(self.hwnd);
        }
    }
}

// ── Window procedure ─────────────────────────────────────────────────────────

/// Runs on the main thread, invoked by winit's `DispatchMessageW`.
///
/// Only `WM_CLOSE` is intercepted. Everything else — including `WM_DESTROY`
/// during teardown — goes to `DefWindowProcW`.
unsafe extern "system" fn wnd_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match msg {
        WM_CLOSE => {
            // Don't destroy. Signal the host; it will tear down the plugin
            // GUI first, then drop us, which calls DestroyWindow.
            let flag_ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *const AtomicBool;
            if !flag_ptr.is_null() {
                (*flag_ptr).store(true, Ordering::Relaxed);
            }
            LRESULT(0) // handled — suppress default (which would DestroyWindow)
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}
