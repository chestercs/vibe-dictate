use std::thread;
use std::time::Duration;

use anyhow::{Context, Result};
use arboard::Clipboard;
use windows::Win32::Foundation::GetLastError;
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
    // Build the full event list up-front (down+up per UTF-16 code unit) so the
    // foreground app receives coherent bursts instead of hundreds of tiny
    // SendInput calls racing with its message pump. Some apps (browsers,
    // terminals) drop injected events when hammered one by one.
    let mut inputs: Vec<INPUT> = Vec::with_capacity(text.encode_utf16().count() * 2);
    for ch in text.encode_utf16() {
        inputs.push(make_unicode_input(ch, false));
        inputs.push(make_unicode_input(ch, true));
    }
    if inputs.is_empty() {
        return Ok(());
    }

    let cbsize = std::mem::size_of::<INPUT>() as i32;
    let total = inputs.len();
    // 100 events per chunk is a conservative safe size; it also gives the
    // target window a chance to drain between chunks for long dictations.
    let mut sent: u32 = 0;
    for chunk in inputs.chunks(100) {
        let n = unsafe { SendInput(chunk, cbsize) };
        sent += n;
        if (n as usize) < chunk.len() {
            let err = unsafe { GetLastError() };
            log::warn!(
                "SendInput partial: {}/{} events sent (last error: {:?}) — \
                 focused window may block injected input (UIPI, admin, game)",
                n,
                chunk.len(),
                err
            );
            // Don't continue into the next chunk — whatever's blocking us will
            // block that too, and we'd produce garbled partial output.
            break;
        }
        thread::sleep(Duration::from_millis(1));
    }

    log::info!(
        "SendInput typed {} chars ({}/{} events delivered)",
        text.chars().count(),
        sent,
        total
    );
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
