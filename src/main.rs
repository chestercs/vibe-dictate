#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

/// Max gap between two successive Pressed events that still counts as a
/// double-tap. Tight enough that back-to-back dictations (which naturally
/// take >500ms between press-events) won't be read as a cancel.
const DOUBLE_TAP_WINDOW: Duration = Duration::from_millis(400);

/// How long the red "cancelled" icon stays up after a double-tap before
/// snapping back to idle blue. Short and sharp — the user wants a quick
/// ack, not a lingering error state.
const CANCEL_FLASH_DURATION: Duration = Duration::from_millis(500);

/// Max wait-time for the user to press a key in the capture popup. If they
/// walk away, the capture auto-cancels and the prior hotkey is restored.
const HOTKEY_CAPTURE_TIMEOUT: Duration = Duration::from_secs(20);

/// How long the red "error" icon stays up after a failed transcription.
/// Slightly longer than the cancel flash so the user can register it even
/// if they were looking elsewhere.
const ERROR_FLASH_DURATION: Duration = Duration::from_millis(800);

/// How long a classified error summary lingers in the tray tooltip before
/// the tooltip reverts to the default "hotkey: …" line. Long enough that
/// the user can hover to read it, short enough that stale messages don't
/// persist once they've fixed the problem.
const ERROR_NOTE_TTL: Duration = Duration::from_secs(20);

/// Heartbeat cadence for the background reachability probe. 30 s keeps the
/// gray/idle transition responsive without pointlessly hammering the
/// server when the user isn't dictating.
const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(30);

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
mod openai;
mod hotkey_capture;
mod injector;
mod mouse_hook;
mod singleton;
mod text_input;
mod tray;

use config::{Config, OutputMode};

#[derive(Debug, Clone)]
enum AppEvent {
    Tick,
    /// Sent by the singleton listener when a newer instance asks us to
    /// make way; the event loop should flip ControlFlow::Exit on receipt.
    Quit,
}

/// Unified binding owner: routes a binding string either to the keyboard
/// global-hotkey manager or to the low-level mouse hook, so the rest of
/// the app doesn't have to branch on "is it a mouse binding?" everywhere.
///
/// Exactly one of (`kb_current` active, `mouse_shared` Some) is ever set.
/// `disable()` clears both, for the capture-dialog pause.
struct BindingManager {
    kb_manager: GlobalHotKeyManager,
    kb_current: Option<HotKey>,
    mouse_shared: Arc<Mutex<Option<mouse_hook::MouseBinding>>>,
}

impl BindingManager {
    fn new(mouse_shared: Arc<Mutex<Option<mouse_hook::MouseBinding>>>) -> Result<Self> {
        let kb_manager = GlobalHotKeyManager::new().context("hotkey manager init")?;
        Ok(Self {
            kb_manager,
            kb_current: None,
            mouse_shared,
        })
    }

    fn apply(&mut self, binding: &str) -> Result<()> {
        // Fully tear down any previous binding first — saves us from
        // partial-state bugs when a user flips between kb and mouse.
        self.disable();
        if let Some(mb) = mouse_hook::parse_mouse_binding(binding) {
            *self.mouse_shared.lock().unwrap() = Some(mb);
            log::info!("Binding active (mouse): {}", binding);
        } else {
            let hk = parse_hotkey(binding)
                .with_context(|| format!("parse hotkey '{}'", binding))?;
            self.kb_manager
                .register(hk)
                .with_context(|| format!("register hotkey '{}'", binding))?;
            self.kb_current = Some(hk);
            log::info!("Binding active (keyboard): {}", binding);
        }
        Ok(())
    }

    /// Pause both routes. Used while the capture popup is open so neither
    /// the kb manager nor the mouse hook fires recording events.
    fn disable(&mut self) {
        if let Some(hk) = self.kb_current.take() {
            let _ = self.kb_manager.unregister(hk);
        }
        *self.mouse_shared.lock().unwrap() = None;
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

    // Single-instance lock — if another vibe-dictate.exe is running, ask it
    // to quit so we can take its place. The listener thread fires Quit
    // through the event loop proxy when a *future* instance (after us)
    // asks the same of us.
    let quit_proxy = event_loop.create_proxy();
    singleton::acquire_or_replace(move || {
        let _ = quit_proxy.send_event(AppEvent::Quit);
    })
    .context("singleton acquire")?;

    // Tray icon + menu
    let tray_state = tray::build(&cfg.lock().unwrap())?;

    // Mouse hook — runs as a dedicated thread with its own message pump.
    // Always active, but only fires MouseEvent when the current binding
    // matches a mouse button; otherwise stays silent.
    let mouse_handle = mouse_hook::start();

    // Binding router — covers both keyboard (via global-hotkey) and mouse
    // (via the hook) paths behind a single `apply(binding)` entrypoint.
    let mut binding_manager = BindingManager::new(mouse_handle.binding.clone())?;
    let initial_binding = cfg.lock().unwrap().hotkey.binding.clone();
    binding_manager
        .apply(&initial_binding)
        .with_context(|| format!("apply initial binding '{}'", initial_binding))?;

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
    // When Some(t), the tray should show the red cancel-flash icon until `t`;
    // the event-loop reconciler snaps it back to idle blue once the deadline
    // passes.
    let flash_until: Arc<Mutex<Option<Instant>>> = Arc::new(Mutex::new(None));
    // Same shape as flash_until but for transcription-error flashes
    // (red icon + tooltip briefly after a failed request).
    let flash_error_until: Arc<Mutex<Option<Instant>>> = Arc::new(Mutex::new(None));
    // Sticky error summary for the tray tooltip. `(expires_at, message)`;
    // cleared by the reconciler once expires_at has passed, or when a
    // successful transcription clears the state.
    let last_error_note: Arc<Mutex<Option<(Instant, String)>>> = Arc::new(Mutex::new(None));
    // Connection health flag. Flipped false by connect/TLS failures during
    // a real transcription or by the heartbeat probe; flipped true on
    // any successful request. Drives the gray Disconnected state.
    let connection_ok: Arc<AtomicBool> = Arc::new(AtomicBool::new(true));
    // Last-applied tray status + tooltip note. The reconciler only repaints
    // when the tuple changes, so a red flash can never get stranded if
    // in_flight delays idle.
    let last_status: Arc<Mutex<(tray::TrayStatus, Option<String>)>> =
        Arc::new(Mutex::new((tray::TrayStatus::Idle, None)));

    let cfg_loop = cfg.clone();
    let recorder_loop = recorder.clone();
    let press_time_loop = press_time.clone();
    let last_press_at_loop = last_press_at.clone();
    let cancel_flag_loop = cancel_flag.clone();
    let in_flight_loop = in_flight.clone();
    let flash_until_loop = flash_until.clone();
    let flash_error_until_loop = flash_error_until.clone();
    let last_error_note_loop = last_error_note.clone();
    let connection_ok_loop = connection_ok.clone();
    let last_status_loop = last_status.clone();

    // Keep tray + binding + mouse-hook state alive for the event loop.
    let tray_keep_alive = tray_state;
    let mouse_rx = mouse_handle.rx;

    // In-flight hotkey capture. When a menu event fires "Rebind…" we
    // disable the current binding, spawn the capture worker, and stash both
    // the channel and the previous binding string here. Each tick polls —
    // on success we save + re-apply, on cancel we restore the previous.
    struct PendingCapture {
        handle: hotkey_capture::CaptureHandle,
        previous: String,
    }
    let mut pending_capture: Option<PendingCapture> = None;

    // Same shape as PendingCapture but for scalar config fields (language,
    // context info, STT server URL/key/model/CA path). We remember which
    // field the popup is for so the callback at result-time knows where
    // to store.
    struct PendingTextInput {
        handle: text_input::TextInputHandle,
        field: tray::TextField,
    }
    let mut pending_text_input: Option<PendingTextInput> = None;

    // Pump hotkey/menu events periodically
    let proxy = event_loop.create_proxy();
    thread::spawn(move || loop {
        thread::sleep(Duration::from_millis(40));
        let _ = proxy.send_event(AppEvent::Tick);
    });

    // Background reachability probe. Build a fresh SttClient per cycle so
    // mid-run config changes (URL, API key, extra_ca_cert) are picked up
    // without needing to reach into the main loop. The probe itself is a
    // short-timeout GET /v1/models; classified errors flip connection_ok
    // and, for long-running outages, populate last_error_note so the
    // tooltip tells the user *why* the tray went gray.
    {
        let cfg_hb = cfg.clone();
        let connection_ok_hb = connection_ok.clone();
        let last_error_note_hb = last_error_note.clone();
        thread::spawn(move || loop {
            let server_cfg = cfg_hb.lock().unwrap().server.clone();
            let probe = openai::SttClient::new(&server_cfg)
                .map_err(|e| e.to_string())
                .and_then(|c| c.health_check().map_err(|e| e.short_summary()));
            match probe {
                Ok(()) => {
                    let was_offline = !connection_ok_hb.swap(true, Ordering::SeqCst);
                    if was_offline {
                        log::info!("Heartbeat: STT server reachable again");
                        // Only clear a *connection*-flavoured note; if the
                        // user had a recent auth / endpoint error they
                        // probably still want to see it.
                        let mut note = last_error_note_hb.lock().unwrap();
                        if let Some((_, msg)) = note.as_ref() {
                            if msg.contains("Cannot reach")
                                || msg.contains("TLS")
                                || msg.contains("certificate")
                            {
                                *note = None;
                            }
                        }
                    }
                }
                Err(summary) => {
                    let was_online = connection_ok_hb.swap(false, Ordering::SeqCst);
                    if was_online {
                        log::warn!("Heartbeat: STT server unreachable ({})", summary);
                        *last_error_note_hb.lock().unwrap() =
                            Some((Instant::now() + ERROR_NOTE_TTL, summary));
                    }
                }
            }
            thread::sleep(HEARTBEAT_INTERVAL);
        });
    }

    event_loop.run(move |event, _, control_flow| {
        *control_flow = ControlFlow::Wait;

        match event {
            tao::event::Event::UserEvent(AppEvent::Quit) => {
                *control_flow = ControlFlow::Exit;
            }
            tao::event::Event::UserEvent(AppEvent::Tick) => {
                // Reconcile tray icon to the single source of truth.
                // Priority (highest first):
                //   error flash   → ErrorFlash    (red, transcription failed)
                //   cancel flash  → CancelFlash   (red, double-tap cancel)
                //   recording     → Recording     (green)
                //   in-flight     → Processing    (orange)
                //   !connection   → Disconnected  (gray)
                //   otherwise     → Idle          (blue)
                let now = Instant::now();
                let flash_cancel = expire_instant(&flash_until_loop, now);
                let flash_error = expire_instant(&flash_error_until_loop, now);
                // Fade out a stale error note so the tooltip reverts to
                // the default after ERROR_NOTE_TTL elapses.
                let note_text = {
                    let mut g = last_error_note_loop.lock().unwrap();
                    match g.as_ref() {
                        Some((t, _)) if now >= *t => {
                            *g = None;
                            None
                        }
                        Some((_, msg)) => Some(msg.clone()),
                        None => None,
                    }
                };
                let recording = recorder_loop.lock().unwrap().is_some();
                let processing = !recording && in_flight_loop.load(Ordering::SeqCst);
                let disconnected = !connection_ok_loop.load(Ordering::SeqCst);

                let desired = if flash_error {
                    tray::TrayStatus::ErrorFlash
                } else if flash_cancel {
                    tray::TrayStatus::CancelFlash
                } else if recording {
                    tray::TrayStatus::Recording
                } else if processing {
                    tray::TrayStatus::Processing
                } else if disconnected {
                    tray::TrayStatus::Disconnected
                } else {
                    tray::TrayStatus::Idle
                };
                // Only surface the note when we have something relevant to
                // say: any red/gray state, or idle with a still-fresh note.
                let tip_note = match desired {
                    tray::TrayStatus::Recording | tray::TrayStatus::Processing => None,
                    _ => note_text,
                };
                {
                    let mut last = last_status_loop.lock().unwrap();
                    if last.0 != desired || last.1 != tip_note {
                        let binding = cfg_loop.lock().unwrap().hotkey.binding.clone();
                        if let Err(e) = tray::apply_status(
                            &tray_keep_alive,
                            desired,
                            &binding,
                            tip_note.as_deref(),
                        ) {
                            log::warn!("tray apply failed: {e:#}");
                        }
                        *last = (desired, tip_note);
                    }
                }

                // Collapse keyboard + mouse events into a single stream of
                // Press/Release actions so the downstream recording logic
                // doesn't care which input device triggered the user.
                #[derive(Copy, Clone)]
                enum PushAction {
                    Press,
                    Release,
                }
                let mut actions: Vec<PushAction> = Vec::new();
                while let Ok(hk_evt) = GlobalHotKeyEvent::receiver().try_recv() {
                    actions.push(match hk_evt.state {
                        HotKeyState::Pressed => PushAction::Press,
                        HotKeyState::Released => PushAction::Release,
                    });
                }
                while let Ok(m_evt) = mouse_rx.try_recv() {
                    actions.push(match m_evt {
                        mouse_hook::MouseEvent::Pressed => PushAction::Press,
                        mouse_hook::MouseEvent::Released => PushAction::Release,
                    });
                }

                for action in actions {
                    match action {
                        PushAction::Press => {
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
                                *flash_until_loop.lock().unwrap() =
                                    Some(Instant::now() + CANCEL_FLASH_DURATION);
                            } else {
                                let mut slot = recorder_loop.lock().unwrap();
                                if slot.is_none() {
                                    // New session — clear any stale cancel flag from a
                                    // prior aborted run, and snap the tray off any
                                    // leftover red flash so the icon goes straight
                                    // from red → green when the user re-presses
                                    // shortly after a cancel.
                                    cancel_flag_loop.store(false, Ordering::SeqCst);
                                    *flash_until_loop.lock().unwrap() = None;
                                    let audio_cfg = cfg_loop.lock().unwrap().audio.clone();
                                    match audio::Recorder::start(&audio_cfg) {
                                        Ok(r) => {
                                            log::info!("Recording started");
                                            *slot = Some(r);
                                            *press_time_loop.lock().unwrap() =
                                                Some(Instant::now());
                                        }
                                        Err(e) => {
                                            log::error!("Failed to start recording: {e:#}")
                                        }
                                    }
                                }
                            }
                        }
                        PushAction::Release => {
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
                                            let cancel_clone = cancel_flag_loop.clone();
                                            let in_flight_clone = in_flight_loop.clone();
                                            let conn_clone = connection_ok_loop.clone();
                                            let flash_err_clone = flash_error_until_loop.clone();
                                            let note_clone = last_error_note_loop.clone();
                                            in_flight_clone.store(true, Ordering::SeqCst);
                                            thread::spawn(move || {
                                                let res = send_and_inject(
                                                    wav,
                                                    cfg_clone,
                                                    cancel_clone,
                                                    conn_clone,
                                                    flash_err_clone,
                                                    note_clone,
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
                                if let Err(e) = binding_manager.apply(&new_binding) {
                                    log::error!("apply new binding failed: {e:#}");
                                }
                            }
                            if outcome.request_capture {
                                if pending_capture.is_some() {
                                    log::info!(
                                        "Hotkey capture already in progress, ignoring duplicate request"
                                    );
                                } else {
                                    let previous =
                                        cfg_loop.lock().unwrap().hotkey.binding.clone();
                                    binding_manager.disable();
                                    log::info!("Opening hotkey capture popup");
                                    pending_capture = Some(PendingCapture {
                                        handle: hotkey_capture::capture_hotkey_async(
                                            HOTKEY_CAPTURE_TIMEOUT,
                                        ),
                                        previous,
                                    });
                                }
                            }
                            if let Some(field) = outcome.text_input_request {
                                if pending_text_input.is_some() {
                                    log::info!(
                                        "Text input already in progress, ignoring duplicate request"
                                    );
                                } else {
                                    let (title, prompt, initial) =
                                        text_input_params(field, &cfg_loop);
                                    log::info!("Opening text input popup: {}", title);
                                    pending_text_input = Some(PendingTextInput {
                                        handle: text_input::ask_text_async(
                                            &title, &prompt, &initial,
                                        ),
                                        field,
                                    });
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

                // Poll any in-flight hotkey capture. try_recv is non-blocking
                // and Disconnected means the worker panicked / exited without
                // sending — treat that as cancel + restore.
                if let Some(pending) = pending_capture.as_ref() {
                    match pending.handle.rx.try_recv() {
                        Ok(Ok(Some(new_binding))) => {
                            log::info!("Hotkey capture returned: {}", new_binding);
                            if let Err(e) = binding_manager.apply(&new_binding) {
                                log::error!("apply captured binding failed: {e:#}");
                                // Fall back to previous so the app isn't left silent.
                                if let Err(e2) = binding_manager.apply(&pending.previous) {
                                    log::error!("restore previous binding failed: {e2:#}");
                                }
                            } else {
                                let save_res = {
                                    let mut c = cfg_loop.lock().unwrap();
                                    c.hotkey.binding = new_binding.clone();
                                    c.save()
                                };
                                if let Err(e) = save_res {
                                    log::error!("save config after capture failed: {e:#}");
                                }
                            }
                            let snapshot = cfg_loop.lock().unwrap().clone();
                            if let Err(e) = tray::rebuild_menu(&tray_keep_alive, &snapshot) {
                                log::error!("tray menu rebuild failed after capture: {e:#}");
                            }
                            pending_capture = None;
                        }
                        Ok(Ok(None)) => {
                            log::info!("Hotkey capture cancelled, restoring previous binding");
                            if let Err(e) = binding_manager.apply(&pending.previous) {
                                log::error!("restore previous binding failed: {e:#}");
                            }
                            pending_capture = None;
                        }
                        Ok(Err(e)) => {
                            log::error!("Hotkey capture errored: {e:#}");
                            if let Err(e2) = binding_manager.apply(&pending.previous) {
                                log::error!("restore previous binding failed: {e2:#}");
                            }
                            pending_capture = None;
                        }
                        Err(std::sync::mpsc::TryRecvError::Empty) => {}
                        Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                            log::error!(
                                "Hotkey capture worker disconnected without sending a result"
                            );
                            if let Err(e) = binding_manager.apply(&pending.previous) {
                                log::error!("restore previous binding failed: {e:#}");
                            }
                            pending_capture = None;
                        }
                    }
                }

                // Poll pending text-input popup. Same polling shape as the
                // hotkey capture above.
                if let Some(pending) = pending_text_input.as_ref() {
                    match pending.handle.rx.try_recv() {
                        Ok(Ok(Some(new_val))) => {
                            apply_text_input(pending.field, &new_val, &cfg_loop);
                            let snapshot = cfg_loop.lock().unwrap().clone();
                            if let Err(e) = tray::rebuild_menu(&tray_keep_alive, &snapshot) {
                                log::error!(
                                    "tray menu rebuild failed after text input: {e:#}"
                                );
                            }
                            pending_text_input = None;
                        }
                        Ok(Ok(None)) => {
                            log::info!("Text input cancelled");
                            pending_text_input = None;
                        }
                        Ok(Err(e)) => {
                            log::error!("Text input errored: {e:#}");
                            pending_text_input = None;
                        }
                        Err(std::sync::mpsc::TryRecvError::Empty) => {}
                        Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                            log::error!(
                                "Text input worker disconnected without sending a result"
                            );
                            pending_text_input = None;
                        }
                    }
                }
            }
            _ => {}
        }
    });
}

/// Compose (title, prompt, initial value) for the Win32 text-input popup
/// based on which config field the user asked to edit.
fn text_input_params(
    field: tray::TextField,
    cfg: &Arc<Mutex<Config>>,
) -> (String, String, String) {
    let c = cfg.lock().unwrap();
    match field {
        tray::TextField::LanguageHint => (
            "vibe-dictate — language hint".to_string(),
            "Preferred language name (e.g. Hungarian, English, Finnish):".to_string(),
            c.stt.language_hint.clone(),
        ),
        tray::TextField::ContextInfo => (
            "vibe-dictate — context info".to_string(),
            "Prompt describing speaker + expected vocabulary (fed to ASR):".to_string(),
            c.stt.context_info.clone(),
        ),
        tray::TextField::ServerUrl => (
            "vibe-dictate — STT server URL".to_string(),
            "Base URL of the OpenAI-compatible STT server (http/https, no trailing slash):"
                .to_string(),
            c.server.base_url.clone(),
        ),
        tray::TextField::ServerKey => (
            "vibe-dictate — API key".to_string(),
            "Bearer token for the STT server (leave empty for local http://localhost):"
                .to_string(),
            c.server.api_key.clone(),
        ),
        tray::TextField::ServerModel => (
            "vibe-dictate — STT model".to_string(),
            "Model identifier sent in the request (e.g. microsoft/VibeVoice-ASR-HF, whisper-1):"
                .to_string(),
            c.server.model.clone(),
        ),
        tray::TextField::ServerCaCert => (
            "vibe-dictate — extra CA cert path".to_string(),
            "Absolute path to an extra PEM CA cert (leave empty for public/system CAs):"
                .to_string(),
            c.server.extra_ca_cert.clone(),
        ),
    }
}

/// Persist the user's text-input answer into the shared Config and save to
/// disk. The trim matters because users often paste values with trailing
/// whitespace — which would e.g. break Bearer auth silently.
fn apply_text_input(field: tray::TextField, value: &str, cfg: &Arc<Mutex<Config>>) {
    let trimmed = value.trim().to_string();
    let mut c = cfg.lock().unwrap();
    match field {
        tray::TextField::LanguageHint => {
            if trimmed.is_empty() {
                log::info!("Empty language hint, keeping previous value");
                return;
            }
            c.stt.language_hint = trimmed.clone();
            log::info!("Language hint set to '{}'", trimmed);
        }
        tray::TextField::ContextInfo => {
            // Keep the raw (untrimmed) value — the user might intentionally
            // want leading/trailing whitespace in prompt wording.
            c.stt.context_info = value.to_string();
            log::info!("Context info updated ({} chars)", value.len());
        }
        tray::TextField::ServerUrl => {
            c.server.base_url = trimmed.clone();
            log::info!("STT server URL set to '{}'", trimmed);
        }
        tray::TextField::ServerKey => {
            c.server.api_key = trimmed;
            log::info!("STT API key updated (length {})", c.server.api_key.len());
        }
        tray::TextField::ServerModel => {
            if trimmed.is_empty() {
                log::info!("Empty STT model, keeping previous value");
                return;
            }
            c.server.model = trimmed.clone();
            log::info!("STT model set to '{}'", trimmed);
        }
        tray::TextField::ServerCaCert => {
            c.server.extra_ca_cert = trimmed.clone();
            log::info!("STT extra CA cert path set to '{}'", trimmed);
        }
    }
    if let Err(e) = c.save() {
        log::error!("save config after text input failed: {e:#}");
    }
}

fn send_and_inject(
    wav: Vec<u8>,
    cfg: Arc<Mutex<Config>>,
    cancel_flag: Arc<AtomicBool>,
    connection_ok: Arc<AtomicBool>,
    flash_error_until: Arc<Mutex<Option<Instant>>>,
    last_error_note: Arc<Mutex<Option<(Instant, String)>>>,
) -> Result<()> {
    let (server_cfg, stt_cfg, output_cfg) = {
        let c = cfg.lock().unwrap();
        (c.server.clone(), c.stt.clone(), c.output.clone())
    };

    let client = match openai::SttClient::new(&server_cfg) {
        Ok(c) => c,
        Err(e) => {
            // Usually a bad extra_ca_cert path — treat as a config-level
            // error, flash red + surface the message for the user.
            let summary = format!("STT client init failed: {}", e);
            log::error!("{}", summary);
            report_pipeline_error(&summary, false, &flash_error_until, &last_error_note);
            return Ok(());
        }
    };
    let text = match client.transcribe(
        wav,
        &stt_cfg.language_hint,
        &stt_cfg.context_info,
    ) {
        Ok(t) => {
            connection_ok.store(true, Ordering::SeqCst);
            t
        }
        Err(e) => {
            log::error!("Transcription failed: {} — {}", e.short_summary(), e);
            if e.is_connection_issue() {
                connection_ok.store(false, Ordering::SeqCst);
            }
            report_pipeline_error(
                &e.short_summary(),
                e.is_connection_issue(),
                &flash_error_until,
                &last_error_note,
            );
            return Ok(());
        }
    };
    let _ = stt_cfg.max_new_tokens; // server-enforced; see ServerConfig docs

    // Final cancel check: user double-tapped while the server was crunching.
    // Drop the result instead of pasting it. We don't try to abort the HTTP
    // call itself — reqwest blocking has no cheap cancellation — but
    // swallowing the output is what the user actually cares about.
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
        OutputMode::Sendinput => injector::send_input_text(
            &out,
            output_cfg.send_key_delay_ms,
            output_cfg.send_key_down_delay_ms,
        )?,
    }
    if output_cfg.send_enter {
        injector::send_enter()?;
    }
    // Successful end-to-end — clear any stale error note so the tooltip
    // doesn't keep showing last run's problem after it was fixed.
    *last_error_note.lock().unwrap() = None;
    Ok(())
}

/// Fire the red ErrorFlash and stash a short summary for the tooltip.
/// Connection issues get a slightly longer TTL because the user may need
/// time to bring the backend back online.
fn report_pipeline_error(
    summary: &str,
    is_connection_issue: bool,
    flash_error_until: &Arc<Mutex<Option<Instant>>>,
    last_error_note: &Arc<Mutex<Option<(Instant, String)>>>,
) {
    let now = Instant::now();
    *flash_error_until.lock().unwrap() = Some(now + ERROR_FLASH_DURATION);
    let ttl = if is_connection_issue {
        ERROR_NOTE_TTL * 2
    } else {
        ERROR_NOTE_TTL
    };
    *last_error_note.lock().unwrap() = Some((now + ttl, summary.to_string()));
}

/// Shared helper for "drain a deadline-based flash": returns true if the
/// deadline is still in the future, false otherwise (and clears the slot
/// once the deadline passes so the reconciler can see a clean edge).
fn expire_instant(cell: &Arc<Mutex<Option<Instant>>>, now: Instant) -> bool {
    let mut g = cell.lock().unwrap();
    match *g {
        Some(t) if now < t => true,
        Some(_) => {
            *g = None;
            false
        }
        None => false,
    }
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
