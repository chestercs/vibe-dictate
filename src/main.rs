#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{anyhow, Context, Result};
use global_hotkey::{
    hotkey::{Code, HotKey, Modifiers},
    GlobalHotKeyEvent, GlobalHotKeyManager, HotKeyState,
};
use tao::event_loop::{ControlFlow, EventLoopBuilder};
use tray_icon::menu::MenuEvent;

mod audio;
mod autostart;
mod config;
mod gradio;
mod injector;
mod tray;

use config::{Config, OutputMode};

#[derive(Debug, Clone)]
enum AppEvent {
    Tick,
}

fn main() -> Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    log::info!("vibe-dictate v{} starting", env!("CARGO_PKG_VERSION"));

    let cfg = Arc::new(Mutex::new(Config::load_or_default()?));

    // Enforce autostart state from config
    {
        let c = cfg.lock().unwrap();
        if let Err(e) = autostart::set_enabled(c.startup.autostart) {
            log::warn!("autostart sync failed: {e:#}");
        }
    }

    let event_loop = EventLoopBuilder::<AppEvent>::with_user_event().build();

    // Tray icon + menu
    let tray_state = tray::build(&cfg.lock().unwrap())?;

    // Global hotkey
    let hotkey_manager = GlobalHotKeyManager::new().context("hotkey manager init")?;
    let hotkey = parse_hotkey(&cfg.lock().unwrap().hotkey.binding)
        .context("parse hotkey binding")?;
    hotkey_manager
        .register(hotkey)
        .context("register global hotkey")?;
    log::info!(
        "Registered hotkey: {}",
        cfg.lock().unwrap().hotkey.binding
    );

    // Recording state
    let recorder: Arc<Mutex<Option<audio::Recorder>>> = Arc::new(Mutex::new(None));
    let press_time: Arc<Mutex<Option<Instant>>> = Arc::new(Mutex::new(None));

    let cfg_loop = cfg.clone();
    let recorder_loop = recorder.clone();
    let press_time_loop = press_time.clone();

    // Keep tray in scope for lifetime of event loop
    let _tray_keep_alive = tray_state;

    // Pump hotkey/menu events periodically
    let proxy = event_loop.create_proxy();
    thread::spawn(move || loop {
        thread::sleep(Duration::from_millis(40));
        let _ = proxy.send_event(AppEvent::Tick);
    });

    event_loop.run(move |event, _, control_flow| {
        *control_flow = ControlFlow::Wait;

        match event {
            tao::event::Event::UserEvent(AppEvent::Tick) => {
                // Drain hotkey events
                while let Ok(hk_evt) = GlobalHotKeyEvent::receiver().try_recv() {
                    match hk_evt.state {
                        HotKeyState::Pressed => {
                            let mut slot = recorder_loop.lock().unwrap();
                            if slot.is_none() {
                                let audio_cfg = cfg_loop.lock().unwrap().audio.clone();
                                match audio::Recorder::start(&audio_cfg) {
                                    Ok(r) => {
                                        log::info!("Recording started");
                                        *slot = Some(r);
                                        *press_time_loop.lock().unwrap() = Some(Instant::now());
                                    }
                                    Err(e) => log::error!("Failed to start recording: {e:#}"),
                                }
                            }
                        }
                        HotKeyState::Released => {
                            let rec = recorder_loop.lock().unwrap().take();
                            let started = press_time_loop.lock().unwrap().take();
                            if let Some(r) = rec {
                                let duration = started
                                    .map(|t| t.elapsed())
                                    .unwrap_or_else(|| Duration::from_millis(0));
                                if duration < Duration::from_millis(150) {
                                    log::info!(
                                        "Recording too short ({}ms), discarded",
                                        duration.as_millis()
                                    );
                                    drop(r);
                                } else {
                                    // cpal::Stream is !Send, encode WAV on this thread
                                    // before handing bytes off to the network worker.
                                    match r.stop_and_encode_wav() {
                                        Ok(wav) => {
                                            log::info!(
                                                "Captured {} bytes WAV ({}ms), sending",
                                                wav.len(),
                                                duration.as_millis()
                                            );
                                            let cfg_clone = cfg_loop.clone();
                                            thread::spawn(move || {
                                                if let Err(e) = send_and_inject(wav, cfg_clone) {
                                                    log::error!(
                                                        "Transcription pipeline failed: {e:#}"
                                                    );
                                                }
                                            });
                                        }
                                        Err(e) => {
                                            log::error!("WAV encode failed: {e:#}");
                                        }
                                    }
                                }
                            }
                        }
                    }
                }

                // Drain menu events
                while let Ok(me) = MenuEvent::receiver().try_recv() {
                    if let Err(e) = tray::handle_menu_event(&me, &cfg_loop) {
                        log::error!("menu event error: {e:#}");
                    }
                    if tray::is_quit(&me) {
                        *control_flow = ControlFlow::Exit;
                    }
                }
            }
            _ => {}
        }
    });
}

fn send_and_inject(wav: Vec<u8>, cfg: Arc<Mutex<Config>>) -> Result<()> {
    let (gradio_cfg, stt_cfg, output_cfg) = {
        let c = cfg.lock().unwrap();
        (c.gradio.clone(), c.stt.clone(), c.output.clone())
    };

    let client = gradio::GradioClient::new(&gradio_cfg)?;
    let text = client.transcribe(
        wav,
        &stt_cfg.context_info,
        stt_cfg.max_new_tokens,
        &stt_cfg.language_hint,
    )?;

    let mut out = text.trim().to_string();
    if out.is_empty() {
        log::warn!("Empty transcription returned");
        return Ok(());
    }
    if output_cfg.trailing_space {
        out.push(' ');
    }
    log::info!("Transcription ({} chars): {}", out.len(), out);

    match output_cfg.mode {
        OutputMode::Clipboard => injector::clipboard_paste(&out)?,
        OutputMode::Sendinput => injector::send_input_text(&out)?,
    }
    Ok(())
}

fn parse_hotkey(s: &str) -> Result<HotKey> {
    let mut mods = Modifiers::empty();
    let mut code: Option<Code> = None;
    for raw in s.split('+') {
        let token = raw.trim();
        if token.is_empty() {
            continue;
        }
        match token.to_ascii_lowercase().as_str() {
            "ctrl" | "control" => mods |= Modifiers::CONTROL,
            "shift" => mods |= Modifiers::SHIFT,
            "alt" => mods |= Modifiers::ALT,
            "rightalt" | "altgr" => mods |= Modifiers::ALT_GRAPH,
            "win" | "super" | "meta" => mods |= Modifiers::META,
            other => {
                code = Some(parse_code(other).ok_or_else(|| {
                    anyhow!("unknown key token '{}' in hotkey '{}'", other, s)
                })?);
            }
        }
    }
    let code = code.ok_or_else(|| anyhow!("no key code in hotkey '{}'", s))?;
    Ok(HotKey::new(Some(mods), code))
}

fn parse_code(name: &str) -> Option<Code> {
    let lower = name.to_ascii_lowercase();
    Some(match lower.as_str() {
        "space" => Code::Space,
        "enter" | "return" => Code::Enter,
        "tab" => Code::Tab,
        "escape" | "esc" => Code::Escape,
        "backspace" => Code::Backspace,
        "capslock" => Code::CapsLock,
        "f1" => Code::F1,
        "f2" => Code::F2,
        "f3" => Code::F3,
        "f4" => Code::F4,
        "f5" => Code::F5,
        "f6" => Code::F6,
        "f7" => Code::F7,
        "f8" => Code::F8,
        "f9" => Code::F9,
        "f10" => Code::F10,
        "f11" => Code::F11,
        "f12" => Code::F12,
        s if s.len() == 1 => {
            let ch = s.chars().next().unwrap().to_ascii_uppercase();
            match ch {
                'A' => Code::KeyA,
                'B' => Code::KeyB,
                'C' => Code::KeyC,
                'D' => Code::KeyD,
                'E' => Code::KeyE,
                'F' => Code::KeyF,
                'G' => Code::KeyG,
                'H' => Code::KeyH,
                'I' => Code::KeyI,
                'J' => Code::KeyJ,
                'K' => Code::KeyK,
                'L' => Code::KeyL,
                'M' => Code::KeyM,
                'N' => Code::KeyN,
                'O' => Code::KeyO,
                'P' => Code::KeyP,
                'Q' => Code::KeyQ,
                'R' => Code::KeyR,
                'S' => Code::KeyS,
                'T' => Code::KeyT,
                'U' => Code::KeyU,
                'V' => Code::KeyV,
                'W' => Code::KeyW,
                'X' => Code::KeyX,
                'Y' => Code::KeyY,
                'Z' => Code::KeyZ,
                '0' => Code::Digit0,
                '1' => Code::Digit1,
                '2' => Code::Digit2,
                '3' => Code::Digit3,
                '4' => Code::Digit4,
                '5' => Code::Digit5,
                '6' => Code::Digit6,
                '7' => Code::Digit7,
                '8' => Code::Digit8,
                '9' => Code::Digit9,
                _ => return None,
            }
        }
        _ => return None,
    })
}
