//! Global show/hide hotkey.
//!
//! `RegisterHotKey` without a window posts `WM_HOTKEY` to the registering
//! thread's queue, so a dedicated thread registers it and waits in
//! `GetMessageW`. Showing and hiding go straight to Win32: a window hidden
//! through eframe stops running `update`, so it could never show itself
//! again.

use std::sync::mpsc;
use std::thread::{self, JoinHandle};

use windows_sys::Win32::Foundation::HWND;
use windows_sys::Win32::System::Threading::GetCurrentThreadId;
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
    RegisterHotKey, UnregisterHotKey, MOD_ALT, MOD_CONTROL, MOD_NOREPEAT,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    GetForegroundWindow, GetMessageW, IsIconic, IsWindowVisible, PostThreadMessageW,
    SetForegroundWindow, ShowWindow, MSG, SW_HIDE, SW_RESTORE, SW_SHOW, WM_HOTKEY, WM_QUIT,
};

pub const LABEL: &str = "Ctrl+Alt+T";
const ID: i32 = 1;

/// The registered hotkey; unregistered on drop.
pub struct Hotkey {
    thread_id: u32,
    thread: Option<JoinHandle<()>>,
}

impl Hotkey {
    /// Registers the hotkey to toggle the window `hwnd`.
    pub fn register(hwnd: isize) -> Result<Self, String> {
        let (tx, rx) = mpsc::channel();
        let thread = thread::Builder::new()
            .name("hotkey".into())
            .spawn(move || unsafe {
                let modifiers = MOD_CONTROL | MOD_ALT | MOD_NOREPEAT;
                if RegisterHotKey(std::ptr::null_mut(), ID, modifiers, u32::from(b'T')) == 0 {
                    let _ = tx.send(None);
                    return;
                }
                // Registering created this thread's message queue, so
                // `Drop` can post WM_QUIT to it from now on.
                let _ = tx.send(Some(GetCurrentThreadId()));
                let mut msg: MSG = std::mem::zeroed();
                while GetMessageW(&mut msg, std::ptr::null_mut(), 0, 0) > 0 {
                    if msg.message == WM_HOTKEY {
                        toggle(hwnd as HWND);
                    }
                }
                UnregisterHotKey(std::ptr::null_mut(), ID);
            })
            .map_err(|e| format!("Couldn't start the hotkey thread: {e}"))?;
        match rx.recv() {
            Ok(Some(thread_id)) => Ok(Self {
                thread_id,
                thread: Some(thread),
            }),
            _ => {
                let _ = thread.join();
                Err(format!("{LABEL} is already used by another program; the global hotkey is off."))
            }
        }
    }
}

impl Drop for Hotkey {
    fn drop(&mut self) {
        unsafe {
            PostThreadMessageW(self.thread_id, WM_QUIT, 0, 0);
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// Hides the window when it is in front; otherwise shows it and brings it
/// to the front.
fn toggle(hwnd: HWND) {
    unsafe {
        if IsWindowVisible(hwnd) == 0 {
            ShowWindow(hwnd, SW_SHOW);
        } else if IsIconic(hwnd) != 0 {
            ShowWindow(hwnd, SW_RESTORE);
        } else if GetForegroundWindow() == hwnd {
            ShowWindow(hwnd, SW_HIDE);
            return;
        }
        // Allowed: the thread that received the hotkey may take the foreground.
        SetForegroundWindow(hwnd);
    }
}

pub fn hide(hwnd: isize) {
    unsafe {
        ShowWindow(hwnd as HWND, SW_HIDE);
    }
}
