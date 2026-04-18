use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use once_cell::sync::OnceCell;
use tray_icon::menu::{CheckMenuItem, Menu, MenuEvent, MenuItem, PredefinedMenuItem};
use tray_icon::{Icon, TrayIcon, TrayIconBuilder};

use crate::autostart;
use crate::config::{Config, OutputMode};

pub struct TrayState {
    _icon: TrayIcon,
    ids: TrayIds,
}

pub struct TrayIds {
    quit: String,
    reload: String,
    autostart: String,
    mode_clipboard: String,
    mode_sendinput: String,
}

static IDS: OnceCell<TrayIds> = OnceCell::new();

pub fn build(cfg: &Config) -> Result<TrayState> {
    let menu = Menu::new();

    let reload = MenuItem::new("Reload config", true, None);
    let autostart_item =
        CheckMenuItem::new("Start with Windows", true, cfg.startup.autostart, None);
    let mode_clipboard = CheckMenuItem::new(
        "Output: Clipboard + Ctrl+V",
        true,
        cfg.output.mode == OutputMode::Clipboard,
        None,
    );
    let mode_sendinput = CheckMenuItem::new(
        "Output: SendInput (direct typing)",
        true,
        cfg.output.mode == OutputMode::Sendinput,
        None,
    );
    let quit = MenuItem::new("Quit", true, None);

    let ids = TrayIds {
        quit: quit.id().0.clone(),
        reload: reload.id().0.clone(),
        autostart: autostart_item.id().0.clone(),
        mode_clipboard: mode_clipboard.id().0.clone(),
        mode_sendinput: mode_sendinput.id().0.clone(),
    };

    menu.append(&reload)?;
    menu.append(&PredefinedMenuItem::separator())?;
    menu.append(&autostart_item)?;
    menu.append(&mode_clipboard)?;
    menu.append(&mode_sendinput)?;
    menu.append(&PredefinedMenuItem::separator())?;
    menu.append(&quit)?;

    let icon = fallback_icon()?;

    let tray = TrayIconBuilder::new()
        .with_tooltip(format!(
            "vibe-dictate — hotkey: {}",
            cfg.hotkey.binding
        ))
        .with_icon(icon)
        .with_menu(Box::new(menu))
        .build()
        .context("tray build")?;

    let state = TrayState {
        _icon: tray,
        ids: TrayIds {
            quit: ids.quit.clone(),
            reload: ids.reload.clone(),
            autostart: ids.autostart.clone(),
            mode_clipboard: ids.mode_clipboard.clone(),
            mode_sendinput: ids.mode_sendinput.clone(),
        },
    };
    let _ = IDS.set(ids);
    Ok(state)
}

pub fn is_quit(e: &MenuEvent) -> bool {
    IDS.get()
        .map(|ids| e.id().0 == ids.quit)
        .unwrap_or(false)
}

pub fn handle_menu_event(e: &MenuEvent, cfg: &Arc<Mutex<Config>>) -> Result<()> {
    let ids = match IDS.get() {
        Some(i) => i,
        None => return Ok(()),
    };
    let id = &e.id().0;

    if id == &ids.reload {
        let reloaded = Config::load_or_default()?;
        *cfg.lock().unwrap() = reloaded;
        log::info!("Config reloaded from disk");
    } else if id == &ids.autostart {
        let new_val = !cfg.lock().unwrap().startup.autostart;
        cfg.lock().unwrap().startup.autostart = new_val;
        cfg.lock().unwrap().save()?;
        autostart::set_enabled(new_val)?;
        log::info!("Autostart set to {}", new_val);
    } else if id == &ids.mode_clipboard {
        cfg.lock().unwrap().output.mode = OutputMode::Clipboard;
        cfg.lock().unwrap().save()?;
        log::info!("Output mode: Clipboard");
    } else if id == &ids.mode_sendinput {
        cfg.lock().unwrap().output.mode = OutputMode::Sendinput;
        cfg.lock().unwrap().save()?;
        log::info!("Output mode: SendInput");
    }
    Ok(())
}

fn fallback_icon() -> Result<Icon> {
    // 32x32 solid blue-ish square with a white dot center — good enough for v0.1.
    // Real icon gets embedded via build.rs / winres when assets/icon.ico exists.
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
                rgba.extend_from_slice(&[30, 120, 220, 255]);
            }
        }
    }
    Icon::from_rgba(rgba, SIZE, SIZE).context("icon from rgba")
}
