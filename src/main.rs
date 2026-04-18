#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

/// Max gap between two successive Pressed events that still counts as a
/// double-tap. Tight enough that back-to-back dictations (which naturally
/// take >500ms between press-events) won't be read as a cancel.
const DOUBLE_TAP_WINDOW: Duration = Duration::from_millis(400);

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

struct HotkeyState {
    manager: GlobalHotKeyManager,
    current: HotKey,
}

impl HotkeyState {
    fn rebind(&mut self, new_binding: &str) -> Result<()> {
        let new_hk = parse_hotkey(new_binding)
            .with_context(|| format!("parse hotkey '{}'", new_binding))?;
        self.manager
            .unregister(self.current)
            .context("unregister previous hotkey")?;
        if let Err(e) = self.manager.register(new_hk) {
            // Try to restore the previous binding so the app stays usable.
            let _ = self.manager.register(self.current);
            return Err(anyhow!("register new hotkey '{}' failed: {:#}", new_binding, e));
        }
        self.current = new_hk;
        log::info!("Hotkey rebound to {}", new_binding);
        Ok(())
    }
}

fn main() -> Result<()> {
    init_logger();
    log::info!("vibe-dictate v{} starting", env!("CARGO_PKG_VERSION"));
    if let Ok(p) = Config::log_path() {
        log::info!("Log file: {}", p.display());
    }

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

    // Global hotkey — manager + currently registered HotKey kept together so the
    // tray menu callback can rebind on the fly.
    let hotkey_manager = GlobalHotKeyManager::new().context("hotkey manager init")?;
    let initial_hotkey = parse_hotkey(&cfg.lock().unwrap().hotkey.binding)
        .context("parse hotkey binding")?;
    hotkey_manager
        .register(initial_hotkey)
        .context("register global hotkey")?;
    log::info!(
        "Registered hotkey: {}",
        cfg.lock().unwrap().hotkey.binding
    );
    let hotkey_state = HotkeyState {
        manager: hotkey_manager,
        current: initial_hotkey,
    };

    // Recording state
    let recorder: Arc<Mutex<Option<audio::Recorder>>> = Arc::new(Mutex::new(None));
    let press_time: Arc<Mutex<Option<Instant>>> = Arc::new(Mutex::new(None));
    // Double-tap detection state. `last_press_at` tracks the previous Pressed
    // timestamp (regardless of whether it was a single tap or the start of a
    // tap-tap). `cancel_flag` is set the moment a double-tap is detected and
    // checked by the in-flight transcription worker right before paste —
    // that's the "still in flight" cancel window. `in_flight` tells the
    // Pressed handler whether there is actually something to cancel.
    let last_press_at: Arc<Mutex<Option<Instant>>> = Arc::new(Mutex::new(None));
    let cancel_flag: Arc<AtomicBool> = Arc::new(AtomicBool::new(false));
    let in_flight: Arc<AtomicBool> = Arc::new(AtomicBool::new(false));

    let cfg_loop = cfg.clone();
    let recorder_loop = recorder.clone();
    let press_time_loop = press_time.clone();
    let last_press_at_loop = last_press_at.clone();
    let cancel_flag_loop = cancel_flag.clone();
    let in_flight_loop = in_flight.clone();

    // Keep tray + hotkey state alive for the event loop. The closure takes
    // ownership; HotkeyState is mutated via &mut self when rebinding.
    let tray_keep_alive = tray_state;
    let mut hotkey_state = hotkey_state;

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
                            let now = Instant::now();
                            let prev = last_press_at_loop.lock().unwrap().replace(now);
                            let is_double_tap = prev
                                .map(|t| now.duration_since(t) <= DOUBLE_TAP_WINDOW)
                                .unwrap_or(false);
                            let currently_recording =
                                recorder_loop.lock().unwrap().is_some();
                            let currently_in_flight =
                                in_flight_loop.load(Ordering::SeqCst);

                            if is_double_tap
                                && (currently_recording || currently_in_flight)
                            {
                                // Cancel path — drop any live recording and arm the
                                // flag so the in-flight worker (if any) skips paste
                                // once Gradio returns.
                                cancel_flag_loop.store(true, Ordering::SeqCst);
                                let dropped = {
                                    let mut slot = recorder_loop.lock().unwrap();
                                    slot.take().is_some()
                                };
                                if dropped {
                                    log::info!("Double-tap cancel: recording aborted");
                                } else if currently_in_flight {
                                    log::info!(
                                        "Double-tap cancel: in-flight transcription will be dropped"
                                    );
                                }
                                *press_time_loop.lock().unwrap() = None;
                                let binding = cfg_loop.lock().unwrap().hotkey.binding.clone();
                                let _ = tray::set_recording(
                                    &tray_keep_alive,
                                    false,
                                    &binding,
                                );
                            } else {
                                let mut slot = recorder_loop.lock().unwrap();
                                if slot.is_none() {
                                    // New session — clear any stale cancel flag from a
                                    // prior aborted run.
                                    cancel_flag_loop.store(false, Ordering::SeqCst);
                                    let audio_cfg = cfg_loop.lock().unwrap().audio.clone();
                                    match audio::Recorder::start(&audio_cfg) {
                                        Ok(r) => {
                                            log::info!("Recording started");
                                            *slot = Some(r);
                                            *press_time_loop.lock().unwrap() =
                                                Some(Instant::now());
                                            let _ = tray::set_recording(
                                                &tray_keep_alive,
                                                true,
                                                "",
                                            );
                                        }
                                        Err(e) => {
                                            log::error!("Failed to start recording: {e:#}")
                                        }
                                    }
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
                                let binding = cfg_loop.lock().unwrap().hotkey.binding.clone();
                                let _ = tray::set_recording(&tray_keep_alive, false, &binding);
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
                                            let cancel_clone = cancel_flag_loop.clone();
                                            let in_flight_clone = in_flight_loop.clone();
                                            in_flight_clone.store(true, Ordering::SeqCst);
                                            thread::spawn(move || {
                                                let res = send_and_inject(
                                                    wav,
                                                    cfg_clone,
                                                    cancel_clone,
                                                );
                                                in_flight_clone.store(false, Ordering::SeqCst);
                                                if let Err(e) = res {
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
                    match tray::handle_menu_event(&me, &cfg_loop) {
                        Ok(outcome) => {
                            if outcome.hotkey_changed {
                                let new_binding = cfg_loop.lock().unwrap().hotkey.binding.clone();
                                if let Err(e) = hotkey_state.rebind(&new_binding) {
                                    log::error!("hotkey rebind failed: {e:#}");
                                }
                            }
                            if outcome.menu_dirty {
                                let snapshot = cfg_loop.lock().unwrap().clone();
                                if let Err(e) = tray::rebuild_menu(&tray_keep_alive, &snapshot) {
                                    log::error!("tray menu rebuild failed: {e:#}");
                                }
                            }
                        }
                        Err(e) => log::error!("menu event error: {e:#}"),
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

fn send_and_inject(
    wav: Vec<u8>,
    cfg: Arc<Mutex<Config>>,
    cancel_flag: Arc<AtomicBool>,
) -> Result<()> {
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

    // Final cancel check: user double-tapped while Gradio was crunching. Drop
    // the result instead of pasting it. We don't try to abort the HTTP call
    // itself — reqwest blocking has no cheap cancellation — but swallowing
    // the output is what the user actually cares about.
    if cancel_flag.load(Ordering::SeqCst) {
        log::info!("Double-tap cancel honored, transcription discarded");
        return Ok(());
    }

    let mut out = text.trim().to_string();
    if out.is_empty() {
        log::warn!("Empty transcription returned");
        return Ok(());
    }
    // Filter out single non-speech meta tags like "[Music]", "[Noise]", "[Silence]".
    // VibeVoice ASR emits these when no speech is detected — pasting them into
    // the focused window is never what the user wants.
    if is_meta_only(&out) {
        log::warn!("Non-speech transcription '{}', skipping paste", out);
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

/// VibeVoice ASR returns bracketed meta tags ("[Music]", "[Noise]",
/// "[Silence]", "[Unintelligible Speech]") when no actual speech is detected.
/// Pasting them into the focused window is never useful — drop them silently.
fn is_meta_only(text: &str) -> bool {
    let t = text.trim();
    let inner = match (t.strip_prefix('['), t.strip_suffix(']')) {
        (Some(_), Some(_)) if t.len() >= 2 => &t[1..t.len() - 1],
        _ => return false,
    };
    !inner.is_empty()
        && inner
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-' || c == ' ')
}

fn init_logger() {
    let env = env_logger::Env::default().default_filter_or("info");
    let mut builder = env_logger::Builder::from_env(env);

    // In windowed builds there is no console, so stderr is dropped. Always
    // try to also tee logs into a rotating-on-startup file in the cache dir.
    if let Ok(path) = Config::log_path() {
        // Truncate on each start so the file never grows unbounded; the user
        // can keep an external tail open if they want history.
        let file = std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&path);
        match file {
            Ok(f) => {
                builder.target(env_logger::Target::Pipe(Box::new(f)));
            }
            Err(e) => {
                eprintln!("vibe-dictate: cannot open log file {}: {e:#}", path.display());
            }
        }
    }
    builder.init();
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
        "pause" => Code::Pause,
        "scrolllock" | "scroll" => Code::ScrollLock,
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
