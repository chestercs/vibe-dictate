use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use tray_icon::menu::{
    CheckMenuItem, Menu, MenuEvent, MenuId, MenuItem, PredefinedMenuItem, Submenu,
};
use tray_icon::{Icon, TrayIcon, TrayIconBuilder};

use crate::audio;
use crate::autostart;
use crate::config::{Config, OutputMode, HOTKEY_OPTIONS};

// Stable menu IDs. Using prefixed strings instead of opaque generated IDs lets us
// keep the menu rebuildable without storing every MenuItem handle in shared state.
const ID_QUIT: &str = "vd:quit";
const ID_RELOAD: &str = "vd:reload";
const ID_AUTOSTART: &str = "vd:autostart";
const ID_OUT_CLIPBOARD: &str = "vd:out:clipboard";
const ID_OUT_SENDINPUT: &str = "vd:out:sendinput";
const ID_OPEN_LOG: &str = "vd:open:log";
const ID_OPEN_CONFIG: &str = "vd:open:config";
const ID_HOTKEY_CAPTURE: &str = "vd:hotkey:__capture__";
const PREFIX_HOTKEY: &str = "vd:hotkey:";
const PREFIX_MIC: &str = "vd:mic:";
const ID_MIC_DEFAULT: &str = "vd:mic:__default__";

pub struct TrayState {
    pub icon: TrayIcon,
}

/// Authoritative tray icon state. The event loop computes one of these
/// every tick from (recorder, flash_until, ...) and the reconciler only
/// re-paints the TrayIcon when the value actually changes — that way
/// a red cancel-flash can never get stranded if in_flight delays idle.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum TrayStatus {
    Idle,
    Recording,
    CancelFlash,
}

pub fn apply_status(state: &TrayState, status: TrayStatus, binding: &str) -> Result<()> {
    match status {
        TrayStatus::Idle => set_recording(state, false, binding),
        TrayStatus::Recording => set_recording(state, true, ""),
        TrayStatus::CancelFlash => set_cancel_flash(state),
    }
}

pub fn build(cfg: &Config) -> Result<TrayState> {
    let menu = build_menu(cfg)?;
    let icon = fallback_icon()?;

    let tray = TrayIconBuilder::new()
        .with_tooltip(format!("vibe-dictate — hotkey: {}", cfg.hotkey.binding))
        .with_icon(icon)
        .with_menu(Box::new(menu))
        .build()
        .context("tray build")?;

    Ok(TrayState { icon: tray })
}

fn build_menu(cfg: &Config) -> Result<Menu> {
    let menu = Menu::new();

    let reload = MenuItem::with_id(MenuId::new(ID_RELOAD), "Reload config", true, None);
    menu.append(&reload)?;
    menu.append(&PredefinedMenuItem::separator())?;

    // Hotkey submenu — F-keys + Pause/ScrollLock; Alt-based bindings intentionally
    // omitted (they collide with Windows app menus / AltGr stuck-key issues).
    // "Rebind…" opens a pure-Win32 capture popup that accepts any key + modifiers.
    let hotkey_sub = Submenu::new("Hotkey", true);
    let current_matches_preset = HOTKEY_OPTIONS
        .iter()
        .any(|opt| cfg.hotkey.binding.eq_ignore_ascii_case(opt));
    for opt in HOTKEY_OPTIONS {
        let id = MenuId::new(format!("{PREFIX_HOTKEY}{opt}"));
        let item = CheckMenuItem::with_id(
            id,
            *opt,
            true,
            cfg.hotkey.binding.eq_ignore_ascii_case(opt),
            None,
        );
        hotkey_sub.append(&item)?;
    }
    hotkey_sub.append(&PredefinedMenuItem::separator())?;
    // When the active binding isn't in the preset list (because the user
    // captured a custom combo), we show it as a checked but disabled entry
    // so the tray makes it obvious what's active without letting the user
    // re-click an already-active combo.
    if !current_matches_preset && !cfg.hotkey.binding.is_empty() {
        let label = format!("Custom: {}", cfg.hotkey.binding);
        let current = CheckMenuItem::new(label, false, true, None);
        hotkey_sub.append(&current)?;
    }
    let rebind = MenuItem::with_id(
        MenuId::new(ID_HOTKEY_CAPTURE),
        "Rebind…",
        true,
        None,
    );
    hotkey_sub.append(&rebind)?;
    menu.append(&hotkey_sub)?;

    // Microphone submenu — default + every enumerated input device.
    let mic_sub = Submenu::new("Microphone", true);
    let default_item = CheckMenuItem::with_id(
        MenuId::new(ID_MIC_DEFAULT),
        "(System default)",
        true,
        cfg.audio.mic_device.is_empty(),
        None,
    );
    mic_sub.append(&default_item)?;
    let devices = audio::list_input_devices();
    if !devices.is_empty() {
        mic_sub.append(&PredefinedMenuItem::separator())?;
    }
    for name in &devices {
        let id = MenuId::new(format!("{PREFIX_MIC}{name}"));
        let item = CheckMenuItem::with_id(
            id,
            name.as_str(),
            true,
            cfg.audio.mic_device == *name,
            None,
        );
        mic_sub.append(&item)?;
    }
    menu.append(&mic_sub)?;

    menu.append(&PredefinedMenuItem::separator())?;

    let autostart_item = CheckMenuItem::with_id(
        MenuId::new(ID_AUTOSTART),
        "Start with Windows",
        true,
        cfg.startup.autostart,
        None,
    );
    menu.append(&autostart_item)?;

    let mode_clipboard = CheckMenuItem::with_id(
        MenuId::new(ID_OUT_CLIPBOARD),
        "Output: Clipboard + Ctrl+V",
        true,
        cfg.output.mode == OutputMode::Clipboard,
        None,
    );
    let mode_sendinput = CheckMenuItem::with_id(
        MenuId::new(ID_OUT_SENDINPUT),
        "Output: SendInput (direct typing)",
        true,
        cfg.output.mode == OutputMode::Sendinput,
        None,
    );
    menu.append(&mode_clipboard)?;
    menu.append(&mode_sendinput)?;

    menu.append(&PredefinedMenuItem::separator())?;

    let open_log = MenuItem::with_id(MenuId::new(ID_OPEN_LOG), "Open log file", true, None);
    let open_config = MenuItem::with_id(MenuId::new(ID_OPEN_CONFIG), "Open config file", true, None);
    menu.append(&open_log)?;
    menu.append(&open_config)?;

    menu.append(&PredefinedMenuItem::separator())?;

    let quit = MenuItem::with_id(MenuId::new(ID_QUIT), "Quit", true, None);
    menu.append(&quit)?;

    Ok(menu)
}

fn open_path_in_default_app(path: &std::path::Path) -> Result<()> {
    // notepad.exe is universally present on Windows and handles both .log and
    // .toml plaintext files. We deliberately avoid `cmd /C start "" "<path>"`
    // because the empty title-quoting trick is fragile under non-console apps
    // (cmd exit code 1 in practice on windowed builds).
    std::process::Command::new("notepad.exe")
        .arg(path)
        .spawn()
        .with_context(|| format!("spawn notepad for {}", path.display()))?;
    Ok(())
}

pub fn rebuild_menu(state: &TrayState, cfg: &Config) -> Result<()> {
    let menu = build_menu(cfg)?;
    state.icon.set_menu(Some(Box::new(menu)));
    let _ = state
        .icon
        .set_tooltip(Some(format!("vibe-dictate — hotkey: {}", cfg.hotkey.binding)));
    Ok(())
}

pub fn is_quit(e: &MenuEvent) -> bool {
    e.id().0 == ID_QUIT
}

pub fn set_recording(state: &TrayState, recording: bool, binding: &str) -> Result<()> {
    let icon = if recording {
        indicator_icon(60, 180, 80)?
    } else {
        indicator_icon(30, 120, 220)?
    };
    state.icon.set_icon(Some(icon)).context("tray set_icon")?;
    let tip = if recording {
        "vibe-dictate — recording… release to send".to_string()
    } else {
        format!("vibe-dictate — hotkey: {}", binding)
    };
    let _ = state.icon.set_tooltip(Some(tip));
    Ok(())
}

/// Flash a red tray icon to acknowledge a double-tap cancel. The caller is
/// expected to restore the idle icon after a short delay via the usual
/// `set_recording(&tray, false, ..)` path — we don't spawn a timer here
/// because TrayIcon isn't Send and the main event loop already ticks often.
pub fn set_cancel_flash(state: &TrayState) -> Result<()> {
    let icon = indicator_icon(220, 50, 50)?;
    state.icon.set_icon(Some(icon)).context("tray set_icon cancel")?;
    let _ = state
        .icon
        .set_tooltip(Some("vibe-dictate — cancelled".to_string()));
    Ok(())
}

/// Result of handling a menu event so main can react (rebind hotkey, rebuild menu).
/// `request_capture` tells main to pop the Win32 capture dialog — handling that
/// can't live inside `handle_menu_event` because the dialog pumps its own
/// messages on a worker thread and needs a channel back to the event loop.
#[derive(Debug, Default)]
pub struct MenuOutcome {
    pub hotkey_changed: bool,
    pub menu_dirty: bool,
    pub request_capture: bool,
}

pub fn handle_menu_event(
    e: &MenuEvent,
    cfg: &Arc<Mutex<Config>>,
) -> Result<MenuOutcome> {
    let id = e.id().0.as_str();
    let mut outcome = MenuOutcome::default();

    if id == ID_RELOAD {
        let reloaded = Config::load_or_default()?;
        *cfg.lock().unwrap() = reloaded;
        log::info!("Config reloaded from disk");
        outcome.hotkey_changed = true;
        outcome.menu_dirty = true;
    } else if id == ID_AUTOSTART {
        let new_val = !cfg.lock().unwrap().startup.autostart;
        {
            let mut c = cfg.lock().unwrap();
            c.startup.autostart = new_val;
            c.save()?;
        }
        autostart::set_enabled(new_val)?;
        log::info!("Autostart set to {}", new_val);
        outcome.menu_dirty = true;
    } else if id == ID_OUT_CLIPBOARD {
        let mut c = cfg.lock().unwrap();
        c.output.mode = OutputMode::Clipboard;
        c.save()?;
        log::info!("Output mode: Clipboard");
        outcome.menu_dirty = true;
    } else if id == ID_OUT_SENDINPUT {
        let mut c = cfg.lock().unwrap();
        c.output.mode = OutputMode::Sendinput;
        c.save()?;
        log::info!("Output mode: SendInput");
        outcome.menu_dirty = true;
    } else if id == ID_OPEN_LOG {
        let p = Config::log_path()?;
        if let Err(e) = open_path_in_default_app(&p) {
            log::error!("open log failed: {e:#}");
        }
    } else if id == ID_OPEN_CONFIG {
        let p = Config::config_path()?;
        if let Err(e) = open_path_in_default_app(&p) {
            log::error!("open config failed: {e:#}");
        }
    } else if id == ID_HOTKEY_CAPTURE {
        outcome.request_capture = true;
    } else if let Some(rest) = id.strip_prefix(PREFIX_HOTKEY) {
        let mut c = cfg.lock().unwrap();
        if !c.hotkey.binding.eq_ignore_ascii_case(rest) {
            c.hotkey.binding = rest.to_string();
            c.save()?;
            log::info!("Hotkey set to {}", rest);
            outcome.hotkey_changed = true;
        }
        outcome.menu_dirty = true;
    } else if id == ID_MIC_DEFAULT {
        let mut c = cfg.lock().unwrap();
        if !c.audio.mic_device.is_empty() {
            c.audio.mic_device = String::new();
            c.save()?;
            log::info!("Microphone reset to system default");
        }
        outcome.menu_dirty = true;
    } else if let Some(rest) = id.strip_prefix(PREFIX_MIC) {
        let mut c = cfg.lock().unwrap();
        if c.audio.mic_device != rest {
            c.audio.mic_device = rest.to_string();
            c.save()?;
            log::info!("Microphone set to '{}'", rest);
        }
        outcome.menu_dirty = true;
    }
    Ok(outcome)
}

fn fallback_icon() -> Result<Icon> {
    indicator_icon(30, 120, 220)
}

fn indicator_icon(r: u8, g: u8, b: u8) -> Result<Icon> {
    // 32x32 coloured square with a white dot center. Idle = blue, recording =
    // red, chosen by the caller. Real branded icon gets embedded via build.rs /
    // winres when assets/icon.ico exists.
    const SIZE: u32 = 32;
    let mut rgba = Vec::with_capacity((SIZE * SIZE * 4) as usize);
    for y in 0..SIZE {
        for x in 0..SIZE {
            let cx = x as i32 - (SIZE as i32 / 2);
            let cy = y as i32 - (SIZE as i32 / 2);
            let dist2 = cx * cx + cy * cy;
            if dist2 < 16 {
                rgba.extend_from_slice(&[255, 255, 255, 255]);
            } else {
                rgba.extend_from_slice(&[r, g, b, 255]);
            }
        }
    }
    Icon::from_rgba(rgba, SIZE, SIZE).context("icon from rgba")
}
