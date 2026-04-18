use std::thread;
use std::time::Duration;

use anyhow::{Context, Result};
use arboard::Clipboard;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, KEYBDINPUT, KEYBD_EVENT_FLAGS, KEYEVENTF_KEYUP,
    KEYEVENTF_UNICODE, VIRTUAL_KEY, VK_CONTROL, VK_V,
};

pub fn clipboard_paste(text: &str) -> Result<()> {
    let mut clipboard = Clipboard::new().context("clipboard open")?;
    let previous = clipboard.get_text().ok();
    clipboard.set_text(text.to_string()).context("set clipboard")?;
    send_ctrl_v()?;
    // Give the target app a moment to consume the clipboard before restoring
    thread::sleep(Duration::from_millis(120));
    if let Some(prev) = previous {
        let _ = clipboard.set_text(prev);
    }
    Ok(())
}

pub fn send_input_text(text: &str) -> Result<()> {
    // Send each unicode code unit via KEYEVENTF_UNICODE
    for ch in text.encode_utf16() {
        let inputs = [
            make_unicode_input(ch, false),
            make_unicode_input(ch, true),
        ];
        unsafe {
            SendInput(&inputs, std::mem::size_of::<INPUT>() as i32);
        }
    }
    Ok(())
}

fn send_ctrl_v() -> Result<()> {
    let inputs = [
        make_vk_input(VK_CONTROL, false),
        make_vk_input(VK_V, false),
        make_vk_input(VK_V, true),
        make_vk_input(VK_CONTROL, true),
    ];
    unsafe {
        SendInput(&inputs, std::mem::size_of::<INPUT>() as i32);
    }
    Ok(())
}

fn make_vk_input(vk: VIRTUAL_KEY, key_up: bool) -> INPUT {
    let mut flags = KEYBD_EVENT_FLAGS(0);
    if key_up {
        flags |= KEYEVENTF_KEYUP;
    }
    INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: vk,
                wScan: 0,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    }
}

fn make_unicode_input(ch: u16, key_up: bool) -> INPUT {
    let mut flags = KEYEVENTF_UNICODE;
    if key_up {
        flags |= KEYEVENTF_KEYUP;
    }
    INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: VIRTUAL_KEY(0),
                wScan: ch,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    }
}
