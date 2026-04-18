//! Win32 modal popup that captures the next keypress and returns its binding
//! string (e.g. "Shift+F9", "Ctrl+Alt+F12", "F8").
//!
//! Runs on its own thread because it needs to spin its own `PeekMessageW` pump
//! — the tao event loop on the main thread wouldn't deliver WM_KEYDOWN events
//! to a window we didn't register through tao.
//!
//! Design:
//! * The caller fires `capture_hotkey_async(timeout)` and polls the returned
//!   channel each event-loop tick.
//! * On the worker thread we register a tiny window class, create a centered
//!   popup, and pump messages until either (a) a non-modifier keypress
//!   arrives (→ return the combined "Ctrl+Shift+F9"-style binding),
//!   (b) Escape is pressed (→ return None), or (c) timeout elapses (→ None).
//! * Modifier states (Ctrl/Shift/Alt) are snapshotted via `GetAsyncKeyState`
//!   at the moment the main key fires — that's the natural user gesture.

use std::cell::RefCell;
use std::sync::mpsc::{self, Receiver};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{anyhow, Context, Result};
use windows::core::w;
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    BeginPaint, DrawTextW, EndPaint, UpdateWindow, DT_CENTER, DT_SINGLELINE, DT_VCENTER,
    PAINTSTRUCT,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetAsyncKeyState, VK_CONTROL, VK_ESCAPE, VK_LWIN, VK_MENU, VK_RWIN, VK_SHIFT,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetClientRect,
    GetSystemMetrics, LoadCursorW, PeekMessageW, PostQuitMessage, RegisterClassW, ShowWindow,
    TranslateMessage, CS_HREDRAW, CS_VREDRAW, HMENU, IDC_ARROW, MSG, PM_REMOVE, SM_CXSCREEN,
    SM_CYSCREEN, SW_SHOW, WINDOW_EX_STYLE, WM_DESTROY, WM_KEYDOWN, WM_MBUTTONDOWN, WM_PAINT,
    WM_SYSKEYDOWN, WM_XBUTTONDOWN, WNDCLASSW, WS_CAPTION, WS_POPUP, WS_SYSMENU, WS_VISIBLE,
};
use windows::Win32::Foundation::HINSTANCE;

// Set by `wndproc` when the user finishes a capture. `Some(Some(binding))`
// = bound a key, `Some(None)` = user pressed Escape, `None` = no decision yet.
// Lives in TLS because the capture thread owns both the window and the pump.
thread_local! {
    static CAPTURE_RESULT: RefCell<Option<Option<String>>> = const { RefCell::new(None) };
}

pub struct CaptureHandle {
    pub rx: Receiver<Result<Option<String>>>,
}

/// Spawn a worker thread that opens the capture popup and reports the result
/// (Ok(Some(binding)) / Ok(None) / Err) via the returned channel.
pub fn capture_hotkey_async(timeout: Duration) -> CaptureHandle {
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let res = run_capture(timeout);
        let _ = tx.send(res);
    });
    CaptureHandle { rx }
}

fn run_capture(timeout: Duration) -> Result<Option<String>> {
    unsafe {
        let hinst: HINSTANCE = GetModuleHandleW(None)
            .context("GetModuleHandleW for capture window")?
            .into();

        // Register the window class. Re-register is a no-op (returns 0 with
        // ERROR_CLASS_ALREADY_EXISTS), so we don't bother with a OnceLock
        // — it's cheap and keeps this module self-contained.
        let class_name = w!("VibeDictateHotkeyCapture");
        let cursor = LoadCursorW(HINSTANCE::default(), IDC_ARROW)
            .context("LoadCursorW IDC_ARROW")?;
        let wc = WNDCLASSW {
            style: CS_HREDRAW | CS_VREDRAW,
            lpfnWndProc: Some(wndproc),
            hInstance: hinst,
            hCursor: cursor,
            lpszClassName: class_name,
            ..Default::default()
        };
        // A zero return can mean "already registered" (benign) or a real error;
        // only log because a duplicate registration is the expected case on
        // the second invocation of this module in the same process.
        let atom = RegisterClassW(&wc);
        if atom == 0 {
            log::debug!("RegisterClassW returned 0 (class likely already registered)");
        }

        // Reset any leftover TLS state from a previous capture on this thread.
        // Can't actually happen (each capture is a fresh thread), but cheap
        // insurance against a future refactor that pools threads.
        CAPTURE_RESULT.with(|s| *s.borrow_mut() = None);

        let width = 460;
        let height = 160;
        let sw = GetSystemMetrics(SM_CXSCREEN);
        let sh = GetSystemMetrics(SM_CYSCREEN);
        let x = (sw - width) / 2;
        let y = (sh - height) / 2;

        let hwnd = CreateWindowExW(
            WINDOW_EX_STYLE::default(),
            class_name,
            w!("vibe-dictate — press a new hotkey"),
            WS_POPUP | WS_CAPTION | WS_SYSMENU | WS_VISIBLE,
            x,
            y,
            width,
            height,
            HWND::default(),
            HMENU::default(),
            hinst,
            None,
        )
        .context("CreateWindowExW capture popup")?;
        if hwnd.0.is_null() {
            return Err(anyhow!("CreateWindowExW returned null HWND"));
        }

        let _ = ShowWindow(hwnd, SW_SHOW);
        let _ = UpdateWindow(hwnd);

        let deadline = Instant::now() + timeout;
        let outcome: Option<String> = 'outer: loop {
            // Drain any pending messages for this thread's queue.
            let mut msg = MSG::default();
            while PeekMessageW(&mut msg, HWND::default(), 0, 0, PM_REMOVE).as_bool() {
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }

            // Check if wndproc set a result.
            let cap = CAPTURE_RESULT.with(|s| s.borrow_mut().take());
            if let Some(resolved) = cap {
                break 'outer resolved;
            }

            if Instant::now() >= deadline {
                log::info!("Hotkey capture timed out after {:?}", timeout);
                break 'outer None;
            }

            thread::sleep(Duration::from_millis(15));
        };

        // DestroyWindow even if wndproc already posted WM_DESTROY — the second
        // call simply returns FALSE and doesn't affect correctness.
        let _ = DestroyWindow(hwnd);

        // Drain any remaining messages so WM_QUIT etc. don't leak into a
        // later message pump on this thread (thread exits next anyway).
        let mut msg = MSG::default();
        while PeekMessageW(&mut msg, HWND::default(), 0, 0, PM_REMOVE).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }

        Ok(outcome)
    }
}

extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    unsafe {
        match msg {
            WM_KEYDOWN | WM_SYSKEYDOWN => {
                let vk = wparam.0 as u16;
                if vk == VK_ESCAPE.0 {
                    CAPTURE_RESULT.with(|s| *s.borrow_mut() = Some(None));
                    return LRESULT(0);
                }
                if is_modifier_vk(vk) {
                    // Wait for a real key — modifiers alone aren't a binding.
                    return LRESULT(0);
                }
                if let Some(key_name) = vk_to_key_name(vk) {
                    let binding = compose_binding(&read_modifier_state(), &key_name);
                    CAPTURE_RESULT.with(|s| *s.borrow_mut() = Some(Some(binding)));
                }
                LRESULT(0)
            }
            WM_MBUTTONDOWN => {
                let binding = compose_binding(&read_modifier_state(), "Mouse3");
                CAPTURE_RESULT.with(|s| *s.borrow_mut() = Some(Some(binding)));
                LRESULT(0)
            }
            WM_XBUTTONDOWN => {
                // MK_XBUTTON1 / MK_XBUTTON2 live in the wparam high word (same
                // convention as WH_MOUSE_LL's mouseData). 1 = Mouse4 (back),
                // 2 = Mouse5 (forward).
                let xbtn = ((wparam.0 >> 16) & 0xFFFF) as u16;
                let name = match xbtn {
                    1 => "Mouse4",
                    2 => "Mouse5",
                    _ => return DefWindowProcW(hwnd, msg, wparam, lparam),
                };
                let binding = compose_binding(&read_modifier_state(), name);
                CAPTURE_RESULT.with(|s| *s.borrow_mut() = Some(Some(binding)));
                // WM_XBUTTON* docs ask us to return TRUE when handled.
                LRESULT(1)
            }
            WM_PAINT => {
                let mut ps = PAINTSTRUCT::default();
                let hdc = BeginPaint(hwnd, &mut ps);
                let mut rect = RECT::default();
                let _ = GetClientRect(hwnd, &mut rect);
                let text: Vec<u16> =
                    "Press a key or click Mouse3/4/5 in this window   (Esc cancels)"
                        .encode_utf16()
                        .collect();
                // DrawTextW expects a &mut [u16]; it doesn't actually mutate
                // when we don't pass DT_MODIFYSTRING, but the signature is
                // inherited from the Win32 ABI so we need a mutable slice.
                let mut buf = text;
                DrawTextW(hdc, &mut buf, &mut rect, DT_CENTER | DT_VCENTER | DT_SINGLELINE);
                let _ = EndPaint(hwnd, &ps);
                LRESULT(0)
            }
            WM_DESTROY => {
                PostQuitMessage(0);
                LRESULT(0)
            }
            _ => DefWindowProcW(hwnd, msg, wparam, lparam),
        }
    }
}

struct ModState {
    ctrl: bool,
    shift: bool,
    alt: bool,
}

fn read_modifier_state() -> ModState {
    unsafe {
        ModState {
            ctrl: (GetAsyncKeyState(VK_CONTROL.0 as i32) as u16 & 0x8000) != 0,
            shift: (GetAsyncKeyState(VK_SHIFT.0 as i32) as u16 & 0x8000) != 0,
            alt: (GetAsyncKeyState(VK_MENU.0 as i32) as u16 & 0x8000) != 0,
        }
    }
}

fn compose_binding(mods: &ModState, key_name: &str) -> String {
    let mut out = String::new();
    if mods.ctrl {
        out.push_str("Ctrl+");
    }
    if mods.shift {
        out.push_str("Shift+");
    }
    if mods.alt {
        out.push_str("Alt+");
    }
    out.push_str(key_name);
    out
}

fn is_modifier_vk(vk: u16) -> bool {
    vk == VK_SHIFT.0
        || vk == VK_CONTROL.0
        || vk == VK_MENU.0
        || vk == VK_LWIN.0
        || vk == VK_RWIN.0
}

/// Map a Windows virtual-key code to the binding-string name that
/// `parse_hotkey` in main.rs understands. We cover F-keys, Pause,
/// ScrollLock, digits 0–9, and letters A–Z — anything else returns None
/// and the capture ignores the press (user tries again).
fn vk_to_key_name(vk: u16) -> Option<String> {
    match vk {
        0x70..=0x7B => Some(format!("F{}", vk - 0x70 + 1)),
        0x13 => Some("Pause".to_string()),
        0x91 => Some("ScrollLock".to_string()),
        0x30..=0x39 => Some(format!("{}", (vk - 0x30) as u8 as char)),
        0x41..=0x5A => Some(((b'A' + (vk - 0x41) as u8) as char).to_string()),
        _ => None,
    }
}
