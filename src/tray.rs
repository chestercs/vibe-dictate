use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use tray_icon::menu::{
    CheckMenuItem, Menu, MenuEvent, MenuId, MenuItem, PredefinedMenuItem, Submenu,
};
use tray_icon::{Icon, TrayIcon, TrayIconBuilder};

use crate::audio;
use crate::autostart;
use crate::config::{Config, InputMode, OutputMode, HOTKEY_OPTIONS};

// Stable menu IDs. Using prefixed strings instead of opaque generated IDs lets us
// keep the menu rebuildable without storing every MenuItem handle in shared state.
const ID_QUIT: &str = "vd:quit";
const ID_RELOAD: &str = "vd:reload";
const ID_AUTOSTART: &str = "vd:autostart";
const ID_OUT_CLIPBOARD: &str = "vd:out:clipboard";
const ID_OUT_SENDINPUT: &str = "vd:out:sendinput";
const ID_OUT_SEND_ENTER: &str = "vd:out:send_enter";
const PREFIX_SEND_DELAY: &str = "vd:out:keydelay:";
const PREFIX_SEND_HOLD: &str = "vd:out:keyhold:";
const ID_OPEN_LOG: &str = "vd:open:log";
const ID_OPEN_CONFIG: &str = "vd:open:config";
const ID_HOTKEY_CAPTURE: &str = "vd:hotkey:__capture__";
const PREFIX_HOTKEY: &str = "vd:hotkey:";
const PREFIX_MIC: &str = "vd:mic:";
const ID_MIC_DEFAULT: &str = "vd:mic:__default__";
const PREFIX_LANG: &str = "vd:lang:";
const ID_LANG_CUSTOM: &str = "vd:lang:__custom__";
const PREFIX_MAXTOK: &str = "vd:maxtok:";
const ID_CTX_EDIT: &str = "vd:stt:ctx";
const ID_SRV_URL: &str = "vd:srv:url";
const ID_SRV_KEY: &str = "vd:srv:key";
const ID_SRV_MODEL: &str = "vd:srv:model";
const ID_SRV_CA: &str = "vd:srv:ca";
const ID_MODE_PTT: &str = "vd:mode:ptt";
const ID_MODE_VAD: &str = "vd:mode:vad";

/// Language presets shown as rubber-stamp options in the tray. Anything else
/// goes via the Custom… text-input dialog. Order mirrors VibeVoice-ASR's
/// training-data distribution (Hungarian first because that's our user).
pub const LANGUAGE_OPTIONS: &[&str] = &[
    "Hungarian",
    "English",
    "German",
    "French",
    "Spanish",
    "Italian",
    "Portuguese",
    "Polish",
    "Dutch",
    "Japanese",
    "Korean",
    "Chinese",
];

/// Token-budget presets. Labels include the rough audio length at the
/// model's ~1600 tokens-per-minute rate so the user doesn't have to guess.
pub const MAXTOK_OPTIONS: &[(&str, u32)] = &[
    ("4096 (~2.5 min)", 4096),
    ("8192 (~5 min)", 8192),
    ("16384 (~10 min)", 16384),
    ("32768 (~20 min)", 32768),
];

/// SendInput inter-character pacing presets (ms). 5 ms is the default but
/// Notepad and Electron apps on slower hardware still drop characters —
/// bump to 15-30 ms for those. 0 ms = burst mode (fastest, least reliable).
pub const SEND_DELAY_OPTIONS: &[(&str, u64)] = &[
    ("0 ms (burst, fastest)", 0),
    ("5 ms", 5),
    ("10 ms", 10),
    ("15 ms", 15),
    ("20 ms (default, safe)", 20),
    ("30 ms", 30),
    ("50 ms (very slow, safest)", 50),
];

/// SendInput key-down hold presets (ms). 0 works against well-behaved
/// targets but several apps filter out zero-duration keypresses, so 10 ms
/// is the default. Raise further only if characters still drop after
/// bumping the inter-char delay.
pub const SEND_HOLD_OPTIONS: &[(&str, u64)] = &[
    ("0 ms (burst)", 0),
    ("2 ms", 2),
    ("5 ms", 5),
    ("10 ms (default)", 10),
    ("20 ms", 20),
];

/// Which scalar config field the user is editing via the Win32 text-input
/// popup. `main.rs` maps this back to a config mutation + menu rebuild when
/// the popup returns.
#[derive(Copy, Clone, Debug)]
pub enum TextField {
    LanguageHint,
    ContextInfo,
    ServerUrl,
    ServerKey,
    ServerModel,
    ServerCaCert,
}

pub struct TrayState {
    pub icon: TrayIcon,
}

/// Authoritative tray icon state. The event loop computes one of these
/// every tick from (recorder, flash_until, in_flight, connection_ok, ...)
/// and the reconciler only re-paints the TrayIcon when the value actually
/// changes — that way a red flash can never get stranded if in_flight
/// delays idle.
///
/// Priority order (highest first), enforced in the main-loop reconciler:
/// ErrorFlash → CancelFlash → Recording → Processing → Disconnected → Idle.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum TrayStatus {
    /// Default, no active work, backend reachable (blue).
    Idle,
    /// Actively capturing audio (green).
    Recording,
    /// Voice-activation mode is armed and listening but not speaking
    /// (teal). Distinct from Idle so the user sees at a glance that the
    /// mic is actually hot in VAD mode.
    VadListening,
    /// Audio sent, waiting on server response (orange).
    Processing,
    /// Heartbeat / last request failed with a connection issue (gray).
    Disconnected,
    /// Brief red overlay after a double-tap cancel.
    CancelFlash,
    /// Brief red overlay after a transcription error.
    ErrorFlash,
}

/// Apply a desired tray status. `binding` is the current hotkey string
/// (for the default tooltip). `note` is an optional extra line appended to
/// the tooltip — used to surface error summaries ("Bad API key", …) or
/// the "Transcribing…" progress hint.
pub fn apply_status(
    state: &TrayState,
    status: TrayStatus,
    binding: &str,
    note: Option<&str>,
) -> Result<()> {
    let (icon, default_tip) = match status {
        TrayStatus::Idle => (
            indicator_icon(30, 120, 220)?,
            format!("vibe-dictate — hotkey: {}", binding),
        ),
        TrayStatus::Recording => (
            indicator_icon(60, 180, 80)?,
            "vibe-dictate — recording… release to send".to_string(),
        ),
        TrayStatus::VadListening => (
            indicator_icon(40, 180, 180)?,
            "vibe-dictate — listening (voice activation)".to_string(),
        ),
        TrayStatus::Processing => (
            indicator_icon(230, 160, 30)?,
            "vibe-dictate — transcribing…".to_string(),
        ),
        TrayStatus::Disconnected => (
            indicator_icon(140, 140, 140)?,
            format!("vibe-dictate — offline (hotkey: {})", binding),
        ),
        TrayStatus::CancelFlash => (
            indicator_icon(220, 50, 50)?,
            "vibe-dictate — cancelled".to_string(),
        ),
        TrayStatus::ErrorFlash => (
            indicator_icon(220, 50, 50)?,
            "vibe-dictate — transcription error".to_string(),
        ),
    };
    state.icon.set_icon(Some(icon)).context("tray set_icon")?;
    let tip = match note {
        Some(n) if !n.is_empty() => format!("{}\n{}", default_tip, n),
        _ => default_tip,
    };
    let _ = state.icon.set_tooltip(Some(tip));
    Ok(())
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

    // Input mode — radio pick between classic push-to-talk (hotkey/mouse
    // drives the recording) and voice activation (mic stays hot, an energy
    // VAD opens/closes utterances automatically). Mutually exclusive by
    // design; main.rs tears down the other mode on switch.
    let mode_label = match cfg.input.mode {
        InputMode::PushToTalk => "Input: Push-to-talk",
        InputMode::VoiceActivation => "Input: Voice activation",
    };
    let mode_sub = Submenu::new(mode_label, true);
    let mode_ptt = CheckMenuItem::with_id(
        MenuId::new(ID_MODE_PTT),
        "Push-to-talk (hold hotkey)",
        true,
        cfg.input.mode == InputMode::PushToTalk,
        None,
    );
    let mode_vad = CheckMenuItem::with_id(
        MenuId::new(ID_MODE_VAD),
        "Voice activation (always-listening VAD)",
        true,
        cfg.input.mode == InputMode::VoiceActivation,
        None,
    );
    mode_sub.append(&mode_ptt)?;
    mode_sub.append(&mode_vad)?;
    menu.append(&mode_sub)?;

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

    // STT server submenu — opens the text-input popup per field. We don't
    // show the current key value (sensitive) in the submenu label, but
    // URL, model, and CA path are safe to include as a tooltip-ish suffix.
    let server_sub = Submenu::new("STT server", true);
    let url_label = if cfg.server.base_url.is_empty() {
        "Edit URL…".to_string()
    } else {
        format!("Edit URL…  ({})", truncate_middle(&cfg.server.base_url, 48))
    };
    server_sub.append(&MenuItem::with_id(MenuId::new(ID_SRV_URL), url_label, true, None))?;
    let key_label = if cfg.server.api_key.is_empty() {
        "Edit API key…  (empty)"
    } else {
        "Edit API key…  (set)"
    };
    server_sub.append(&MenuItem::with_id(
        MenuId::new(ID_SRV_KEY),
        key_label,
        true,
        None,
    ))?;
    let model_label = if cfg.server.model.is_empty() {
        "Edit model…".to_string()
    } else {
        format!("Edit model…  ({})", truncate_middle(&cfg.server.model, 48))
    };
    server_sub.append(&MenuItem::with_id(MenuId::new(ID_SRV_MODEL), model_label, true, None))?;
    let ca_label = if cfg.server.extra_ca_cert.is_empty() {
        "Edit CA cert path…  (empty)".to_string()
    } else {
        format!("Edit CA cert path…  ({})", truncate_middle(&cfg.server.extra_ca_cert, 48))
    };
    server_sub.append(&MenuItem::with_id(MenuId::new(ID_SRV_CA), ca_label, true, None))?;
    menu.append(&server_sub)?;

    // Language submenu — preset checkmarks + Custom… for anything exotic.
    let lang_sub = Submenu::new("Language", true);
    let lang_cur = cfg.stt.language_hint.clone();
    let lang_in_presets = LANGUAGE_OPTIONS
        .iter()
        .any(|opt| lang_cur.eq_ignore_ascii_case(opt));
    for opt in LANGUAGE_OPTIONS {
        let id = MenuId::new(format!("{PREFIX_LANG}{opt}"));
        let item = CheckMenuItem::with_id(
            id,
            *opt,
            true,
            lang_cur.eq_ignore_ascii_case(opt),
            None,
        );
        lang_sub.append(&item)?;
    }
    lang_sub.append(&PredefinedMenuItem::separator())?;
    if !lang_in_presets && !lang_cur.is_empty() {
        let label = format!("Current: {}", lang_cur);
        let current = CheckMenuItem::new(label, false, true, None);
        lang_sub.append(&current)?;
    }
    let lang_custom =
        MenuItem::with_id(MenuId::new(ID_LANG_CUSTOM), "Custom…", true, None);
    lang_sub.append(&lang_custom)?;
    menu.append(&lang_sub)?;

    // Context info free-form prompt — direct item, opens the dialog with
    // the current value prefilled.
    menu.append(&MenuItem::with_id(
        MenuId::new(ID_CTX_EDIT),
        "Edit context info…",
        true,
        None,
    ))?;

    // Max tokens submenu — fixed presets only, no custom. The spread covers
    // push-to-talk bursts (4096) up to long dictations (32768); finer grain
    // isn't worth the menu clutter.
    let maxtok_sub = Submenu::new("Max tokens", true);
    for (label, val) in MAXTOK_OPTIONS {
        let id = MenuId::new(format!("{PREFIX_MAXTOK}{val}"));
        let item = CheckMenuItem::with_id(
            id,
            *label,
            true,
            cfg.stt.max_new_tokens == *val,
            None,
        );
        maxtok_sub.append(&item)?;
    }
    // Show current non-preset value (if any) as a disabled marker so the
    // user can see what they've got even if they set it via config.toml.
    let maxtok_in_presets = MAXTOK_OPTIONS.iter().any(|(_, v)| *v == cfg.stt.max_new_tokens);
    if !maxtok_in_presets {
        maxtok_sub.append(&PredefinedMenuItem::separator())?;
        let cur = CheckMenuItem::new(
            format!("Current: {}", cfg.stt.max_new_tokens),
            false,
            true,
            None,
        );
        maxtok_sub.append(&cur)?;
    }
    menu.append(&maxtok_sub)?;

    menu.append(&PredefinedMenuItem::separator())?;

    let autostart_item = CheckMenuItem::with_id(
        MenuId::new(ID_AUTOSTART),
        "Start with Windows",
        true,
        cfg.startup.autostart,
        None,
    );
    menu.append(&autostart_item)?;

    // Output mode is a one-of-two pick, so grouping the radio-style options
    // inside their own submenu makes the mutual exclusion visually obvious —
    // flat CheckMenuItems at the root looked like independent toggles.
    let output_sub = Submenu::new(
        match cfg.output.mode {
            OutputMode::Clipboard => "Output mode: Clipboard",
            OutputMode::Sendinput => "Output mode: SendInput",
        },
        true,
    );
    let mode_clipboard = CheckMenuItem::with_id(
        MenuId::new(ID_OUT_CLIPBOARD),
        "Clipboard + Ctrl+V",
        true,
        cfg.output.mode == OutputMode::Clipboard,
        None,
    );
    let mode_sendinput = CheckMenuItem::with_id(
        MenuId::new(ID_OUT_SENDINPUT),
        "SendInput (direct typing)",
        true,
        cfg.output.mode == OutputMode::Sendinput,
        None,
    );
    output_sub.append(&mode_clipboard)?;
    output_sub.append(&mode_sendinput)?;
    menu.append(&output_sub)?;

    // SendInput pacing presets — only meaningful when mode=SendInput, so
    // we grey them out under Clipboard mode (the entry stays visible to
    // keep the menu layout stable on mode switches, but can't be clicked).
    // Label carries the current value so the user doesn't have to open
    // the submenu to check.
    let sendinput_active = cfg.output.mode == OutputMode::Sendinput;
    let delay_sub = Submenu::new(
        format!("SendInput char delay: {} ms", cfg.output.send_key_delay_ms),
        sendinput_active,
    );
    for (label, val) in SEND_DELAY_OPTIONS {
        let id = MenuId::new(format!("{PREFIX_SEND_DELAY}{val}"));
        let item = CheckMenuItem::with_id(
            id,
            *label,
            true,
            cfg.output.send_key_delay_ms == *val,
            None,
        );
        delay_sub.append(&item)?;
    }
    let delay_in_presets = SEND_DELAY_OPTIONS
        .iter()
        .any(|(_, v)| *v == cfg.output.send_key_delay_ms);
    if !delay_in_presets {
        delay_sub.append(&PredefinedMenuItem::separator())?;
        delay_sub.append(&CheckMenuItem::new(
            format!("Current: {} ms", cfg.output.send_key_delay_ms),
            false,
            true,
            None,
        ))?;
    }
    menu.append(&delay_sub)?;

    let hold_sub = Submenu::new(
        format!("SendInput key hold: {} ms", cfg.output.send_key_down_delay_ms),
        sendinput_active,
    );
    for (label, val) in SEND_HOLD_OPTIONS {
        let id = MenuId::new(format!("{PREFIX_SEND_HOLD}{val}"));
        let item = CheckMenuItem::with_id(
            id,
            *label,
            true,
            cfg.output.send_key_down_delay_ms == *val,
            None,
        );
        hold_sub.append(&item)?;
    }
    let hold_in_presets = SEND_HOLD_OPTIONS
        .iter()
        .any(|(_, v)| *v == cfg.output.send_key_down_delay_ms);
    if !hold_in_presets {
        hold_sub.append(&PredefinedMenuItem::separator())?;
        hold_sub.append(&CheckMenuItem::new(
            format!("Current: {} ms", cfg.output.send_key_down_delay_ms),
            false,
            true,
            None,
        ))?;
    }
    menu.append(&hold_sub)?;

    let send_enter = CheckMenuItem::with_id(
        MenuId::new(ID_OUT_SEND_ENTER),
        "Append Enter after dictation",
        true,
        cfg.output.send_enter,
        None,
    );
    menu.append(&send_enter)?;

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

/// Result of handling a menu event so main can react (rebind hotkey, rebuild menu).
/// `request_capture` tells main to pop the Win32 capture dialog — handling that
/// can't live inside `handle_menu_event` because the dialog pumps its own
/// messages on a worker thread and needs a channel back to the event loop.
#[derive(Debug, Default)]
pub struct MenuOutcome {
    pub hotkey_changed: bool,
    pub menu_dirty: bool,
    pub request_capture: bool,
    /// Some(field) → main should open the Win32 text-input popup for that
    /// config field (URL, token, language, etc.). None → nothing to do.
    pub text_input_request: Option<TextField>,
    /// Set when the user switched between PTT and VAD. Main has to tear
    /// down the current input path and bring up the other — the two are
    /// mutually exclusive, so the routing is all-or-nothing.
    pub input_mode_changed: bool,
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
        outcome.input_mode_changed = true;
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
    } else if id == ID_OUT_SEND_ENTER {
        let mut c = cfg.lock().unwrap();
        c.output.send_enter = !c.output.send_enter;
        let new_val = c.output.send_enter;
        c.save()?;
        log::info!("Append Enter after dictation: {}", new_val);
        outcome.menu_dirty = true;
    } else if let Some(rest) = id.strip_prefix(PREFIX_SEND_DELAY) {
        let val: u64 = rest.parse().unwrap_or(5);
        let mut c = cfg.lock().unwrap();
        if c.output.send_key_delay_ms != val {
            c.output.send_key_delay_ms = val;
            c.save()?;
            log::info!("SendInput char delay set to {} ms", val);
        }
        outcome.menu_dirty = true;
    } else if let Some(rest) = id.strip_prefix(PREFIX_SEND_HOLD) {
        let val: u64 = rest.parse().unwrap_or(0);
        let mut c = cfg.lock().unwrap();
        if c.output.send_key_down_delay_ms != val {
            c.output.send_key_down_delay_ms = val;
            c.save()?;
            log::info!("SendInput key hold set to {} ms", val);
        }
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
    } else if id == ID_LANG_CUSTOM {
        outcome.text_input_request = Some(TextField::LanguageHint);
    } else if let Some(rest) = id.strip_prefix(PREFIX_LANG) {
        let mut c = cfg.lock().unwrap();
        if !c.stt.language_hint.eq_ignore_ascii_case(rest) {
            c.stt.language_hint = rest.to_string();
            c.save()?;
            log::info!("Language hint set to '{}'", rest);
        }
        outcome.menu_dirty = true;
    } else if let Some(rest) = id.strip_prefix(PREFIX_MAXTOK) {
        // Parse-fail only happens if we wrote a malformed preset ID ourselves,
        // so fall back to the default (8192, ~5 min) rather than silently
        // ignoring the click.
        let val: u32 = rest.parse().unwrap_or(8192);
        let mut c = cfg.lock().unwrap();
        if c.stt.max_new_tokens != val {
            c.stt.max_new_tokens = val;
            c.save()?;
            log::info!("max_new_tokens set to {}", val);
        }
        outcome.menu_dirty = true;
    } else if id == ID_CTX_EDIT {
        outcome.text_input_request = Some(TextField::ContextInfo);
    } else if id == ID_SRV_URL {
        outcome.text_input_request = Some(TextField::ServerUrl);
    } else if id == ID_SRV_KEY {
        outcome.text_input_request = Some(TextField::ServerKey);
    } else if id == ID_SRV_MODEL {
        outcome.text_input_request = Some(TextField::ServerModel);
    } else if id == ID_SRV_CA {
        outcome.text_input_request = Some(TextField::ServerCaCert);
    } else if id == ID_MODE_PTT {
        let mut c = cfg.lock().unwrap();
        if c.input.mode != InputMode::PushToTalk {
            c.input.mode = InputMode::PushToTalk;
            c.save()?;
            log::info!("Input mode: Push-to-talk");
            outcome.input_mode_changed = true;
        }
        outcome.menu_dirty = true;
    } else if id == ID_MODE_VAD {
        let mut c = cfg.lock().unwrap();
        if c.input.mode != InputMode::VoiceActivation {
            c.input.mode = InputMode::VoiceActivation;
            c.save()?;
            log::info!("Input mode: Voice activation");
            outcome.input_mode_changed = true;
        }
        outcome.menu_dirty = true;
    }
    Ok(outcome)
}

/// Shorten a long string for inline menu labels — keeps head + tail,
/// drops the middle with `…`. Used so gradio URLs or long paths don't
/// blow the menu width; the full value lives in the popup anyway.
fn truncate_middle(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let keep = (max.saturating_sub(1)) / 2;
    let head: String = s.chars().take(keep).collect();
    let tail: String = s.chars().rev().take(keep).collect::<String>().chars().rev().collect();
    format!("{head}…{tail}")
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
