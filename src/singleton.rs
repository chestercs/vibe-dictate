//! Single-instance enforcement with "new replaces old" semantics.
//!
//! The first copy of vibe-dictate.exe that starts creates a named mutex and
//! a named auto-reset event in the current Windows session namespace, then
//! spawns a listener thread that blocks on the event. A subsequent copy
//! launched while the first is still running detects the mutex, signals the
//! event, waits for the mutex to clear (old instance exiting), and then
//! acquires it for itself.
//!
//! This is the opposite of the classical "exit if already running" pattern
//! — the user explicitly wants a fresh rebuild to take over from the stale
//! copy without having to `Stop-Process` by hand.
//!
//! Everything here touches raw Win32 handles directly via the `windows`
//! crate; we intentionally skip RAII Drop for handles because the process
//! exits immediately after the event loop ends, and Windows cleans up
//! session-namespace handles for us.

use std::thread;
use std::time::{Duration, Instant};

use anyhow::{anyhow, Context, Result};
use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{
    CloseHandle, GetLastError, ERROR_ALREADY_EXISTS, HANDLE, WAIT_OBJECT_0,
};
use windows::Win32::System::Threading::{
    CreateEventW, CreateMutexW, SetEvent, WaitForSingleObject, INFINITE,
};

const MUTEX_NAME: PCWSTR = w!("com.chestercs.vibe-dictate.singleton");
const EVENT_NAME: PCWSTR = w!("com.chestercs.vibe-dictate.quit");


/// Acquire the singleton slot, evicting an existing instance if one is
/// running. On success spawns a listener thread that invokes
/// `on_takeover` the moment a *newer* instance asks us to make way —
/// the caller should use that callback to walk the event loop to a clean
/// Quit.
///
/// Returns Err if we couldn't evict the old instance within a few seconds
/// (it was frozen, or something else is holding the name).
pub fn acquire_or_replace<F>(on_takeover: F) -> Result<()>
where
    F: FnOnce() + Send + 'static,
{
    unsafe {
        // Auto-reset event, initially unsignalled. If an older instance
        // already created it we just get a handle to the same object —
        // which is exactly what we want for the takeover signal below.
        let event = CreateEventW(None, false, false, EVENT_NAME)
            .context("CreateEventW for quit signal")?;
        if event.is_invalid() {
            return Err(anyhow!("CreateEventW returned invalid handle"));
        }

        let mut mutex = CreateMutexW(None, true, MUTEX_NAME)
            .context("CreateMutexW for singleton")?;
        let already_running = GetLastError() == ERROR_ALREADY_EXISTS;

        if already_running {
            log::warn!(
                "Another vibe-dictate instance is running — signalling it to exit and taking over"
            );
            // We own a handle to the existing mutex but not its ownership.
            // Drop our handle and signal the owner to quit.
            let _ = CloseHandle(mutex);
            if SetEvent(event).is_err() {
                let _ = CloseHandle(event);
                return Err(anyhow!("SetEvent on quit signal failed"));
            }

            // Poll for the old instance to release the mutex. In practice
            // its listener thread fires within a handful of ms; we give it
            // up to 5s for slow shutdowns, logging progress so the user can
            // see what's happening in the log file.
            let deadline = Instant::now() + Duration::from_secs(5);
            let mut acquired = false;
            while Instant::now() < deadline {
                let m = CreateMutexW(None, true, MUTEX_NAME)
                    .context("CreateMutexW retry")?;
                if GetLastError() != ERROR_ALREADY_EXISTS {
                    mutex = m;
                    acquired = true;
                    break;
                }
                let _ = CloseHandle(m);
                thread::sleep(Duration::from_millis(100));
            }
            if !acquired {
                let _ = CloseHandle(event);
                return Err(anyhow!(
                    "Timed out (5s) waiting for previous vibe-dictate to release the singleton mutex — is it frozen?"
                ));
            }
            log::info!("Previous instance exited, singleton acquired");
        } else {
            log::info!("Singleton acquired (no prior instance)");
        }

        // We now own the mutex. `mutex` handle lives for the rest of the
        // process; on exit Windows releases it. Intentionally leak the
        // handle by not wrapping it in an RAII guard — simpler than
        // threading a Drop-only lifetime through main().
        let _ = mutex;

        // Spawn the takeover listener. When a *future* instance signals
        // the event, we call the caller's shutdown closure. Using an
        // auto-reset event means the signal is consumed here; if multiple
        // instances fire in a tight race, only the first wake matters.
        //
        // HANDLE (= *mut c_void) isn't Send, and Rust-2021 disjoint
        // captures would grab the raw pointer field rather than a Send
        // wrapper, so we transport the handle as a usize and rebuild the
        // HANDLE on the far side. This is legal on Windows: HANDLE values
        // are valid across threads for Wait* family calls.
        let event_bits = event.0 as usize;
        thread::spawn(move || {
            let handle = HANDLE(event_bits as *mut _);
            // windows-rs 0.58 declares WaitForSingleObject as a safe fn
            // (the unsafety is absorbed into Param-type bounds), so no
            // inner `unsafe` block needed. The handle is valid for the
            // process lifetime because we never close it.
            let rc = WaitForSingleObject(handle, INFINITE);
            if rc == WAIT_OBJECT_0 {
                log::warn!("Takeover signal received — a newer instance is starting, shutting down");
                on_takeover();
            } else {
                log::error!("Singleton listener WaitForSingleObject failed: rc={:?}", rc);
            }
        });
    }

    Ok(())
}
