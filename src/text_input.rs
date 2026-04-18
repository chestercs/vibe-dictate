//! Win32 modal popup that asks the user for a single line of text.
//!
//! Mirrors the shape of `hotkey_capture.rs`: worker thread owns the window
//! + message pump, caller polls a channel. We re-use Win32 common controls
//! (EDIT + BUTTON) instead of pulling in a GUI crate — the tray-menu UX
//! only needs one-field dialogs (URL, token, CA cert path, language, etc.)
//! and the footprint stays minimal.
//!
//! `IsDialogMessageW` gives us Tab / Enter / Escape navigation for free
//! (Enter → default OK button, Escape → IDCANCEL, Tab cycles between
//! controls), so we don't have to subclass the EDIT control.

use std::cell::RefCell;
use std::sync::mpsc::{self, Receiver};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{anyhow, Context, Result};
use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Input::KeyboardAndMouse::SetFocus;
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetSystemMetrics,
    GetWindowTextLengthW, GetWindowTextW, IsDialogMessageW, LoadCursorW, PeekMessageW,
    PostQuitMessage, RegisterClassW, SendMessageW, ShowWindow, TranslateMessage, CS_HREDRAW,
    CS_VREDRAW, HMENU, IDC_ARROW, MSG, PM_REMOVE, SM_CXSCREEN, SM_CYSCREEN, SW_SHOW,
    WINDOW_EX_STYLE, WINDOW_STYLE, WM_COMMAND, WM_CREATE, WM_DESTROY, WNDCLASSW,
    WS_CAPTION, WS_CHILD, WS_EX_CLIENTEDGE, WS_POPUP, WS_SYSMENU, WS_TABSTOP, WS_VISIBLE,
};

// Win32 control IDs + constants — hardcoded because the `windows` crate
// exposes these as typed constants in scattered modules and we only need
// three numeric IDs.
const IDC_OK: i32 = 1; // == IDOK in Windows headers
const IDC_CANCEL: i32 = 2; // == IDCANCEL
const IDC_EDIT: i32 = 1001;

// Static style bits (EDIT ES_AUTOHSCROLL, BUTTON BS_DEFPUSHBUTTON, STATIC SS_LEFT).
// Declared as plain u32 so we can OR them into WINDOW_STYLE without chasing
// the windows-rs module paths.
const ES_AUTOHSCROLL: u32 = 0x0080;
const BS_DEFPUSHBUTTON: u32 = 0x0001;
// EM_SETSEL = 0x00B1 — selects the whole EDIT content so the user can
// over-type the prefilled value immediately.
const EM_SETSEL: u32 = 0x00B1;

// TLS carries the caller's prompt + initial value into WM_CREATE (the
// handler has no parameter bag). CAPTURE_RESULT is also TLS because the
// wndproc runs on the same worker thread as the pump loop.
thread_local! {
    static PROMPT_TEXT: RefCell<String> = const { RefCell::new(String::new()) };
    static INITIAL_TEXT: RefCell<String> = const { RefCell::new(String::new()) };
    static EDIT_HWND: RefCell<Option<HWND>> = const { RefCell::new(None) };
    static CAPTURE_RESULT: RefCell<Option<Option<String>>> = const { RefCell::new(None) };
}

pub struct TextInputHandle {
    pub rx: Receiver<Result<Option<String>>>,
}

pub fn ask_text_async(title: &str, prompt: &str, initial: &str) -> TextInputHandle {
    let (tx, rx) = mpsc::channel();
    let title = title.to_string();
    let prompt = prompt.to_string();
    let initial = initial.to_string();
    thread::spawn(move || {
        let res = run_input(title, prompt, initial);
        let _ = tx.send(res);
    });
    TextInputHandle { rx }
}

fn run_input(title: String, prompt: String, initial: String) -> Result<Option<String>> {
    unsafe {
        let hinst: HINSTANCE = GetModuleHandleW(None)
            .context("GetModuleHandleW for text input window")?
            .into();

        let class_name = w!("VibeDictateTextInput");
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
        let atom = RegisterClassW(&wc);
        if atom == 0 {
            log::debug!("RegisterClassW returned 0 (class likely already registered)");
        }

        PROMPT_TEXT.with(|p| *p.borrow_mut() = prompt);
        INITIAL_TEXT.with(|t| *t.borrow_mut() = initial);
        EDIT_HWND.with(|e| *e.borrow_mut() = None);
        CAPTURE_RESULT.with(|s| *s.borrow_mut() = None);

        let width = 560;
        let height = 200;
        let sw = GetSystemMetrics(SM_CXSCREEN);
        let sh = GetSystemMetrics(SM_CYSCREEN);
        let x = (sw - width) / 2;
        let y = (sh - height) / 2;

        let title_w: Vec<u16> = title.encode_utf16().chain(std::iter::once(0)).collect();
        let hwnd = CreateWindowExW(
            WINDOW_EX_STYLE::default(),
            class_name,
            PCWSTR::from_raw(title_w.as_ptr()),
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
        .context("CreateWindowExW text input")?;
        if hwnd.0.is_null() {
            return Err(anyhow!("CreateWindowExW returned null HWND"));
        }

        let _ = ShowWindow(hwnd, SW_SHOW);

        let deadline = Instant::now() + Duration::from_secs(600);
        let outcome: Option<String> = 'outer: loop {
            let mut msg = MSG::default();
            while PeekMessageW(&mut msg, HWND::default(), 0, 0, PM_REMOVE).as_bool() {
                // IsDialogMessage handles Tab / Enter / Escape on controls
                // with WS_TABSTOP — so we get keyboard nav without subclassing.
                if !IsDialogMessageW(hwnd, &msg).as_bool() {
                    let _ = TranslateMessage(&msg);
                    DispatchMessageW(&msg);
                }
            }

            let cap = CAPTURE_RESULT.with(|s| s.borrow_mut().take());
            if let Some(r) = cap {
                break 'outer r;
            }
            if Instant::now() >= deadline {
                log::info!("Text input timed out");
                break 'outer None;
            }
            thread::sleep(Duration::from_millis(15));
        };

        let _ = DestroyWindow(hwnd);
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
            WM_CREATE => {
                let hinst: HINSTANCE = match GetModuleHandleW(None) {
                    Ok(h) => h.into(),
                    Err(_) => return LRESULT(-1),
                };

                // Prompt label above the edit.
                let prompt = PROMPT_TEXT.with(|p| p.borrow().clone());
                let prompt_w: Vec<u16> =
                    prompt.encode_utf16().chain(std::iter::once(0)).collect();
                let _ = CreateWindowExW(
                    WINDOW_EX_STYLE::default(),
                    w!("STATIC"),
                    PCWSTR::from_raw(prompt_w.as_ptr()),
                    WS_CHILD | WS_VISIBLE,
                    14,
                    12,
                    520,
                    36,
                    hwnd,
                    HMENU::default(),
                    hinst,
                    None,
                );

                // The EDIT control. ES_AUTOHSCROLL lets long URLs / tokens
                // scroll horizontally instead of wrapping; single-line.
                let initial = INITIAL_TEXT.with(|t| t.borrow().clone());
                let initial_w: Vec<u16> =
                    initial.encode_utf16().chain(std::iter::once(0)).collect();
                let edit = CreateWindowExW(
                    WS_EX_CLIENTEDGE,
                    w!("EDIT"),
                    PCWSTR::from_raw(initial_w.as_ptr()),
                    WS_CHILD | WS_VISIBLE | WS_TABSTOP | WINDOW_STYLE(ES_AUTOHSCROLL),
                    14,
                    56,
                    530,
                    28,
                    hwnd,
                    HMENU(IDC_EDIT as isize as *mut _),
                    hinst,
                    None,
                )
                .unwrap_or_default();
                EDIT_HWND.with(|e| *e.borrow_mut() = Some(edit));
                let _ = SetFocus(edit);
                // Select all so the user can overwrite immediately.
                let _ = SendMessageW(edit, EM_SETSEL, WPARAM(0), LPARAM(-1));

                // Cancel first in tab order, OK as default button.
                let _ = CreateWindowExW(
                    WINDOW_EX_STYLE::default(),
                    w!("BUTTON"),
                    w!("Cancel"),
                    WS_CHILD | WS_VISIBLE | WS_TABSTOP,
                    360,
                    110,
                    84,
                    30,
                    hwnd,
                    HMENU(IDC_CANCEL as isize as *mut _),
                    hinst,
                    None,
                );
                let _ = CreateWindowExW(
                    WINDOW_EX_STYLE::default(),
                    w!("BUTTON"),
                    w!("OK"),
                    WS_CHILD | WS_VISIBLE | WS_TABSTOP | WINDOW_STYLE(BS_DEFPUSHBUTTON),
                    456,
                    110,
                    84,
                    30,
                    hwnd,
                    HMENU(IDC_OK as isize as *mut _),
                    hinst,
                    None,
                );

                LRESULT(0)
            }
            WM_COMMAND => {
                // WPARAM low-word is the control ID for menu / button clicks.
                let id = (wparam.0 & 0xFFFF) as i32;
                if id == IDC_OK {
                    let edit = EDIT_HWND.with(|e| *e.borrow());
                    if let Some(edit) = edit {
                        let len = GetWindowTextLengthW(edit);
                        // +1 for the null terminator that GetWindowTextW writes
                        // even if the content is empty.
                        let mut buf = vec![0u16; (len as usize) + 1];
                        let written = GetWindowTextW(edit, &mut buf);
                        let text = String::from_utf16_lossy(&buf[..written as usize]);
                        CAPTURE_RESULT.with(|s| *s.borrow_mut() = Some(Some(text)));
                    } else {
                        CAPTURE_RESULT.with(|s| *s.borrow_mut() = Some(None));
                    }
                    LRESULT(0)
                } else if id == IDC_CANCEL {
                    CAPTURE_RESULT.with(|s| *s.borrow_mut() = Some(None));
                    LRESULT(0)
                } else {
                    DefWindowProcW(hwnd, msg, wparam, lparam)
                }
            }
            WM_DESTROY => {
                PostQuitMessage(0);
                LRESULT(0)
            }
            _ => DefWindowProcW(hwnd, msg, wparam, lparam),
        }
    }
}
