//! Low-level mouse hook for push-to-talk on Mouse3/4/5.
//!
//! `global-hotkey` doesn't support mouse buttons, so we install our own
//! `WH_MOUSE_LL` hook on a dedicated thread and forward matching press /
//! release events to the main event loop via an mpsc channel.
//!
//! The hook always runs (the install cost is negligible and the per-event
//! cost is a few compares). When the user's binding is keyboard-only, the
//! hook sees nothing matching and stays silent. When it's mouse-based, it
//! checks button + modifiers and fires.
//!
//! We do NOT suppress the mouse event — the user still expects middle-
//! click to paste in editors, and XButton1/2 to navigate browser history.
//! Push-to-talk is additive, not exclusive.

use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread;

use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetAsyncKeyState, VK_CONTROL, VK_MENU, VK_SHIFT,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, DispatchMessageW, GetMessageW, SetWindowsHookExW, TranslateMessage,
    UnhookWindowsHookEx, HHOOK, MSG, MSLLHOOKSTRUCT, WH_MOUSE_LL, WM_MBUTTONDOWN, WM_MBUTTONUP,
    WM_XBUTTONDOWN, WM_XBUTTONUP,
};

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum MouseButton {
    /// Middle (scroll-wheel click). VK = 0x04 = VK_MBUTTON.
    Middle,
    /// XButton1 — "back" side-button on most mice.
    X1,
    /// XButton2 — "forward" side-button on most mice.
    X2,
}

/// Which mouse button + which modifiers the user has currently bound. When
/// set to `None`, the hook fires no events (keyboard binding active).
#[derive(Clone, Debug)]
pub struct MouseBinding {
    pub button: MouseButton,
    pub ctrl: bool,
    pub shift: bool,
    pub alt: bool,
}

#[derive(Copy, Clone, Debug)]
pub enum MouseEvent {
    Pressed,
    Released,
}

pub struct MouseHookHandle {
    pub rx: Receiver<MouseEvent>,
    /// Main thread writes the current binding here; hook thread reads it
    /// on every qualifying mouse event.
    pub binding: Arc<Mutex<Option<MouseBinding>>>,
}

struct HookState {
    tx: Sender<MouseEvent>,
    binding: Arc<Mutex<Option<MouseBinding>>>,
}

static HOOK_STATE: OnceLock<HookState> = OnceLock::new();

/// Spawn the hook thread. Returns a handle with the event receiver + a
/// shared binding slot that the main thread updates whenever config changes.
pub fn start() -> MouseHookHandle {
    let (tx, rx) = mpsc::channel();
    let binding = Arc::new(Mutex::new(None));
    let handle = MouseHookHandle {
        rx,
        binding: binding.clone(),
    };

    // The hook proc reaches this via a process-wide OnceLock — Windows
    // doesn't give you a user-data pointer for low-level hooks, so the
    // usual workaround is "static mut or OnceLock".
    let state = HookState { tx, binding };
    if HOOK_STATE.set(state).is_err() {
        log::error!("mouse hook already initialized — ignoring second start()");
        return handle;
    }

    thread::spawn(|| {
        unsafe {
            let hook = match SetWindowsHookExW(WH_MOUSE_LL, Some(hook_proc), None, 0) {
                Ok(h) => h,
                Err(e) => {
                    log::error!("SetWindowsHookExW WH_MOUSE_LL failed: {e:?}");
                    return;
                }
            };
            log::info!("Low-level mouse hook installed");

            // A message loop is required on the installing thread for the
            // hook to keep receiving callbacks. We don't post any messages
            // ourselves — we just need GetMessageW to keep pumping.
            let mut msg = MSG::default();
            while GetMessageW(&mut msg, HWND::default(), 0, 0).as_bool() {
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
            let _ = UnhookWindowsHookEx(hook);
        }
    });

    handle
}

extern "system" fn hook_proc(ncode: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    unsafe {
        if ncode >= 0 {
            // Safe: the hook contract guarantees `lparam` is a valid
            // MSLLHOOKSTRUCT pointer for the lifetime of this call.
            let ms = *(lparam.0 as *const MSLLHOOKSTRUCT);
            let wm = wparam.0 as u32;
            let (button, is_down) = decode_button(wm, ms.mouseData);
            if let (Some(btn), Some(down)) = (button, is_down) {
                if let Some(state) = HOOK_STATE.get() {
                    let should_fire = match state.binding.lock().unwrap().as_ref() {
                        Some(b) => b.button == btn && modifiers_satisfy(b),
                        None => false,
                    };
                    if should_fire {
                        let ev = if down {
                            MouseEvent::Pressed
                        } else {
                            MouseEvent::Released
                        };
                        let _ = state.tx.send(ev);
                    }
                }
            }
        }
        CallNextHookEx(HHOOK::default(), ncode, wparam, lparam)
    }
}

fn decode_button(wm: u32, mouse_data: u32) -> (Option<MouseButton>, Option<bool>) {
    match wm {
        WM_MBUTTONDOWN => (Some(MouseButton::Middle), Some(true)),
        WM_MBUTTONUP => (Some(MouseButton::Middle), Some(false)),
        WM_XBUTTONDOWN => (x_button_from_data(mouse_data), Some(true)),
        WM_XBUTTONUP => (x_button_from_data(mouse_data), Some(false)),
        _ => (None, None),
    }
}

fn x_button_from_data(mouse_data: u32) -> Option<MouseButton> {
    // MSLLHOOKSTRUCT.mouseData high word carries the XBUTTON id (1 or 2).
    match (mouse_data >> 16) & 0xFFFF {
        1 => Some(MouseButton::X1),
        2 => Some(MouseButton::X2),
        _ => None,
    }
}

fn modifiers_satisfy(b: &MouseBinding) -> bool {
    unsafe {
        let shift = (GetAsyncKeyState(VK_SHIFT.0 as i32) as u16 & 0x8000) != 0;
        let ctrl = (GetAsyncKeyState(VK_CONTROL.0 as i32) as u16 & 0x8000) != 0;
        let alt = (GetAsyncKeyState(VK_MENU.0 as i32) as u16 & 0x8000) != 0;
        shift == b.shift && ctrl == b.ctrl && alt == b.alt
    }
}

/// Parse a binding string like "Mouse4", "Shift+Mouse3", "Ctrl+Alt+Mouse5"
/// into a MouseBinding. Returns None if the string isn't a mouse binding
/// (so the caller falls through to the keyboard path).
pub fn parse_mouse_binding(s: &str) -> Option<MouseBinding> {
    let mut ctrl = false;
    let mut shift = false;
    let mut alt = false;
    let mut button: Option<MouseButton> = None;
    for raw in s.split('+') {
        let token = raw.trim();
        if token.is_empty() {
            continue;
        }
        match token.to_ascii_lowercase().as_str() {
            "ctrl" | "control" => ctrl = true,
            "shift" => shift = true,
            "alt" => alt = true,
            "mouse3" | "middle" => button = Some(MouseButton::Middle),
            "mouse4" | "xbutton1" => button = Some(MouseButton::X1),
            "mouse5" | "xbutton2" => button = Some(MouseButton::X2),
            _ => return None,
        }
    }
    button.map(|b| MouseBinding {
        button: b,
        ctrl,
        shift,
        alt,
    })
}

