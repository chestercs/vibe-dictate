# vibe-dictate

A Windows tray-resident push-to-talk dictation tool that talks to a locally-
or remotely-hosted **VibeVoice ASR** Gradio endpoint. Hold a hotkey, speak,
release — the transcription appears in whatever window has focus.

Built for Hungarian dictation but works in any of the 50+ languages
VibeVoice-ASR supports.

---

## Table of contents

- [What is VibeVoice?](#what-is-vibevoice)
- [System requirements](#system-requirements)
- [Hosting the VibeVoice backend](#hosting-the-vibevoice-backend)
- [Building the client](#building-the-client)
- [Running](#running)
- [Configuration reference](#configuration-reference)
- [Hotkey reference](#hotkey-reference)
- [Tray menu reference](#tray-menu-reference)
- [Output modes](#output-modes)
- [Troubleshooting](#troubleshooting)
- [Architecture](#architecture)
- [Development](#development)

---

## What is VibeVoice?

**VibeVoice** is Microsoft's open-source speech model family released in
late 2025. The relevant variant for this tool is **`microsoft/VibeVoice-ASR`**:

- **Type**: ~7B parameter encoder-decoder ASR model (bf16, ~14 GB on disk)
- **Languages**: 50+ including English, Hungarian, German, French,
  Spanish, Italian, Polish, Dutch, Japanese, Korean, Chinese, etc.
- **Output**: structured transcription with optional speaker diarization,
  hotword biasing, language anchoring and free-form context prompts.
- **License**: MIT (see upstream repo)
- **Repo**: https://github.com/microsoft/VibeVoice
- **HF model card**: https://huggingface.co/microsoft/VibeVoice-ASR

Microsoft also ships a 0.5B realtime TTS sibling (`VibeVoice-Realtime-0.5B`),
which is unrelated to this tool — vibe-dictate only consumes the ASR side.

The official Gradio demo (`demo/vibevoice_asr_gradio_demo.py`) is what
vibe-dictate talks to over HTTP. Endpoint contract: multipart upload of
a WAV file + a JSON call with `(file_path, context_info, max_new_tokens,
language_hint)`, then SSE long-poll for the transcription string.

### Hungarian quality

Hungarian has only ~0.18% representation in VibeVoice-ASR's training corpus
(20th in the language list, comparable to Turkish / Thai / Czech). Out of
the box accuracy is usable but benefits from:

- A precise `language_hint` ("Hungarian", not "hu")
- A `context_info` prompt that anchors language and allows code-mixing
  (the default prompt does this — see `config.rs::default_context_info`)
- Clean audio (close-talk mic, low background noise)

For specialized vocabularies (medical, legal, custom jargon) consider
LoRA fine-tuning via the upstream `finetuning-asr/` recipes.

---

## System requirements

### Client (vibe-dictate, Windows)

- **OS**: Windows 10 (1903+) or Windows 11
- **Architecture**: x86_64
- **CPU**: any modern x86_64 (the client is a thin push-to-talk wrapper,
  no inference happens locally)
- **RAM**: ~30 MB resident
- **Audio**: any input device WASAPI exposes (USB headsets, built-in
  mics, virtual cables, etc.)
- **Network**: TCP reachability to the Gradio backend
  (default `http://localhost:7860`)
- **Privileges**: regular user — no admin needed unless the target window
  you want to type into is itself elevated (UIPI: a non-elevated process
  can't SendInput into an elevated one)

### Backend (VibeVoice ASR, Linux + NVIDIA GPU)

- **GPU VRAM**: ~14 GB minimum for bf16 weights, ~16-18 GB peak with KV
  cache. RTX 4090 (24 GB) is comfortable; A6000 / L40S work fine; data
  center cards (H100, B200) are overkill but supported.
- **CUDA**: 12.x or 13.x. NVIDIA Blackwell (sm_120/sm_121) needs CUDA 13
  and `TORCH_CUDA_ARCH_LIST="12.0+PTX"`.
- **Driver**: 550+ for CUDA 12, 560+ for CUDA 13.
- **CPU inference**: technically possible, practically pointless
  (~1 token/sec → minutes per utterance).
- **OS**: any Linux with Docker + NVIDIA Container Toolkit. Bare-metal
  install also works if you can reproduce the upstream `pip install -e .`
  recipe.
- **Disk**: ~30 GB for HuggingFace model cache + Docker image layers.

---

## Hosting the VibeVoice backend

vibe-dictate doesn't bundle the ASR server — you point it at a Gradio
endpoint someone (you, your team, Azure Foundry, etc.) runs. The compose
files needed to stand one up live in this repo; the upstream VibeVoice
source itself isn't vendored and has to be cloned separately.

### Expected repo layout

```
vibe-dictate/                        <- this repo
  docker-compose-vibevoice.yml       <- x86_64 / consumer NVIDIA backend
  docker-compose-vibevoice-asr-gb10.yml  <- GB10 DGX Spark backend
  docker-compose-vibedictate-build.yml   <- Rust cross-compile pipeline
  Dockerfile.build                   <- builder image for the .exe
  Dockerfile.vibevoice-gb10          <- B-opció (prebuilt) GB10 image
  setup_vibevoice.sh                 <- one-shot clone + .env + pull (Linux/macOS)
  setup_vibevoice.bat                <- one-shot clone + .env + pull (Windows)
  scripts/vibevoice_entrypoint.sh    <- runtime pip-install on first boot
  .env.vibevoice.example             <- copy to .env for x86_64
  .env.vibevoice-gb10.example        <- copy to .env for GB10
  src/                               <- Rust client source
  VibeVoice/                         <- upstream microsoft/VibeVoice (gitignored)
    demo/ vibevoice/ vllm_plugin/ ...
```

### One-shot setup

The backend host has a helper that clones the upstream tree, seeds
`.env`, pre-pulls the base image, and optionally brings the stack up.
Linux/macOS hosts use the bash version, Windows hosts the batch twin:

```bash
# Linux/macOS (default --arch gb10)
./setup_vibevoice.sh                    # clone + pull
./setup_vibevoice.sh --arch x86 --up    # consumer NVIDIA + compose up -d
./setup_vibevoice.sh --build            # B-opció prebuilt image (GB10 only)
```

```bat
:: Windows (default --arch x86)
setup_vibevoice.bat
setup_vibevoice.bat --up
setup_vibevoice.bat --arch gb10
```

The sections below walk through the same steps manually.

### Option A — local Docker (RTX 4090 / dev workstation)

```bash
git clone https://github.com/microsoft/VibeVoice VibeVoice
cp .env.vibevoice.example .env
# edit HUGGING_FACE_HUB_TOKEN if you have one
docker compose -f docker-compose-vibevoice.yml --profile asr up -d
# Gradio UI on http://localhost:7860
```

First boot downloads the ~14 GB model from HuggingFace; a token avoids
the anon rate limit. The runtime-install entrypoint runs once per
container lifetime (marker file at `/root/.vibevoice_installed`) —
subsequent restarts skip pip entirely.

### Option B — NVIDIA GB10 DGX Spark (aarch64, sm_121)

For Grace Blackwell unified-memory machines:

```bash
git clone https://github.com/microsoft/VibeVoice VibeVoice
cp .env.vibevoice-gb10.example .env
# edit VIBEVOICE_BASE_DIR (USB mount on GB10) + token
docker compose up -d    # COMPOSE_FILE is set in .env
```

Runs on `nvcr.io/nvidia/pytorch:25.12-py3` (CUDA 13, aarch64) and coexists
with other GPU workloads (e.g. a Gemma vLLM container) within the 128 GB
unified pool. Default `up -d` brings both the ASR service (port 7860)
and the 0.5B realtime TTS (port 3000); combined VRAM is ~18 GB.

### Option C — managed cloud

VibeVoice is available as an Azure AI Foundry endpoint. Point
`gradio.url` at the deployed URL and put the bearer token in
`gradio.api_token`. (Azure Foundry uses an OpenAI-compat interface, not
the Gradio one — vibe-dictate currently only speaks Gradio. PRs welcome.)

### Sanity-checking the backend

Before pointing vibe-dictate at it:

```bash
curl -fsS http://your-host:7860/ >/dev/null && echo "Gradio reachable"
```

The Gradio web UI itself is also a useful smoke test — upload a WAV and
verify a transcription appears. vibe-dictate uses the same internal API
the web UI does.

---

## Building the client

There's no toolchain install required on the host — a Docker-based
cross-compile pipeline produces the Windows .exe from any Linux/macOS/
Windows machine that has Docker.

From this repo's root:

```bash
docker compose -f docker-compose-vibedictate-build.yml run --rm vibedictate-build
```

Output: `target/x86_64-pc-windows-msvc/release/vibe-dictate.exe`

- First build: ~5–10 min (cargo-xwin downloads MSVC headers + cold compile)
- Incremental: ~10–30 sec (cargo cache + xwin cache live in named volumes)

If you'd rather build natively on Windows, the standard Rust toolchain
works: `rustup target add x86_64-pc-windows-msvc && cargo build --release`.

### Build dependencies (auto-installed by the Docker image)

- Rust 1.89+ (edition 2021)
- `cargo-xwin` 0.18.6 for the MSVC target
- clang / lld / llvm

See `Dockerfile.build` for the exact recipe.

---

## Running

Double-click the produced `vibe-dictate.exe`. It goes straight to the tray
(blue square icon by default). Right-click for the menu.

On first launch, a config file is created at
`%APPDATA%\chestercs\vibe-dictate\config\config.toml` and a log file at
`%LOCALAPPDATA%\chestercs\vibe-dictate\cache\vibe-dictate.log`. Both are
openable from the tray menu ("Open config file" / "Open log file" — they
launch in `notepad.exe`).

### First-run checklist

1. **Backend up**: confirm `curl http://localhost:7860/` returns 200, or
   set `gradio.url` to wherever yours runs.
2. **Hotkey**: default is `F8`. Change via tray → Hotkey → either pick a
   preset or `Rebind…` and press the combination you want.
3. **Microphone**: tray → Microphone → pick a device, or leave on
   `(System default)`.
4. **Hold-to-talk**: hold the hotkey, speak, release. Tray icon turns
   green while recording. Transcription should appear in the focused
   window 1-3 seconds after release.
5. **Cancel**: double-tap the hotkey within 400ms while recording — the
   tray flashes red and the buffered audio is dropped without sending.

---

## Configuration reference

All settings live in `%APPDATA%\chestercs\vibe-dictate\config\config.toml`.
Most are also exposed in the tray menu, so you rarely need to edit the
file by hand. After a manual edit: tray → "Reload config".

```toml
[gradio]
url           = "http://localhost:7860"  # base URL of the Gradio app
function      = "transcribe_audio"        # Gradio API name (don't change unless you forked)
api_token     = ""                         # Bearer token for remote deployments
extra_ca_cert = ""                         # absolute path to a PEM cert/bundle for self-signed CAs

[stt]
context_info    = "..."         # free-form prompt fed to the model (see default in config.rs)
max_new_tokens  = 8192          # generation budget. ~1600 tok/min audio. 4096=2.5min, 32768=20min
language_hint   = "Hungarian"   # full English language name; ISO codes ("hu") auto-expand

[audio]
mic_device  = ""        # WASAPI device name; empty = system default
sample_rate = 16000     # VibeVoice expects 16 kHz mono

[hotkey]
binding = "F8"          # see "Hotkey reference" below

[output]
mode                    = "clipboard"  # "clipboard" or "sendinput"
trailing_space          = true         # append " " after transcription (helps when chaining utterances)
send_enter              = false        # press Enter after the text — useful in chat clients / terminals
send_key_delay_ms       = 20           # SendInput: ms sleep between characters; safe default, lower for faster typing on well-behaved editors
send_key_down_delay_ms  = 0            # SendInput: ms to hold each key "down" (raise for legacy apps that filter zero-duration presses)

[startup]
autostart       = false   # add to HKCU\...\Run on next save
start_minimized = true    # always-true for tray apps; reserved for future
```

### Editing remote-backend settings

For a remote VibeVoice host (e.g. behind Tailscale, internal CA, or a
public Cloudflare tunnel):

```toml
[gradio]
url           = "https://vibevoice.example.internal"
api_token     = "sk-xxxxxxxxxxxxxxxxxxxxxxxxxxxx"
extra_ca_cert = "C:/Users/you/certs/internal-ca.pem"
```

The bearer token is added to every `/upload`, `/call`, and `/poll` request
as `Authorization: Bearer <token>`. The CA file may be a single PEM cert
or a concatenated bundle — reqwest's rustls backend handles both.

---

## Hotkey reference

vibe-dictate accepts a single push-to-talk binding, configured via
`hotkey.binding` or via the tray "Rebind…" capture popup.

### Accepted forms

- **Function keys**: `F1` … `F12`
- **Letters**: `A` … `Z`
- **Digits**: `0` … `9`
- **System keys**: `Pause`, `ScrollLock`
- **Mouse buttons**:
  - `Mouse3` (also `Middle`) — middle / scroll-wheel click
  - `Mouse4` (also `XButton1`) — back side button
  - `Mouse5` (also `XButton2`) — forward side button
- **Modifiers**: prefix any of the above with `Ctrl+`, `Shift+`, `Alt+`
  (combinable, e.g. `Ctrl+Shift+F12`, `Shift+Mouse4`)

### Why no Alt-only bindings?

Plain Alt collides with Windows app menus and AltGr (which is
`Ctrl+Alt` on Hungarian/German/etc. layouts). Alt-based hotkeys are
auto-migrated to `F8` on config load to avoid stuck-modifier bugs.
Use `Ctrl+Alt+...` if you must combine Alt with something else.

### Mouse buttons don't suppress the click

Mouse3/4/5 are intercepted by a low-level `WH_MOUSE_LL` hook but the
event still bubbles through to the foreground window. So middle-click
still pastes in editors, Mouse4/5 still navigate browser history, and
push-to-talk is purely additive.

---

## Tray menu reference

Right-click the tray icon to get:

- **Reload config** — re-reads `config.toml` from disk; useful after
  hand-edits or for picking up changes from other tools.
- **Hotkey** — submenu with preset bindings (F7-F12, Pause, ScrollLock)
  + `Rebind…` (Win32 capture popup that accepts any key/mouse +
  modifiers). The active binding is checked. Custom captured bindings
  show up as "Custom: …".
- **Microphone** — system default + every WASAPI input device. Re-listed
  on each menu open so newly-plugged USB mics appear.
- **Gradio server** — opens text-input popups for URL / token / CA cert
  path. Token isn't shown in the label (just "set" / "empty").
- **Language** — preset list of common languages + Custom… for ISO
  codes / less common names.
- **Edit context info…** — multi-purpose prompt the model uses for
  language anchoring, register, code-mixing rules.
- **Max tokens** — generation budget presets (4k / 8k / 16k / 32k tokens
  ≈ 2.5 / 5 / 10 / 20 min audio).
- **Start with Windows** — toggles HKCU\Software\Microsoft\Windows\
  CurrentVersion\Run.
- **Output: Clipboard + Ctrl+V** / **Output: SendInput (direct typing)** —
  see [Output modes](#output-modes).
- **Append Enter after dictation** — toggles `output.send_enter`.
- **Open log file** / **Open config file** — both via notepad.exe.
- **Quit**.

---

## Output modes

Two ways to deliver the transcription to the focused window:

### Clipboard + Ctrl+V (default)

Copies the transcription to the clipboard, sends `Ctrl+V`, sleeps 120ms,
then restores the previous clipboard contents. Pros: works in essentially
every Windows app. Cons: uses the clipboard slot briefly; some apps with
custom paste handlers may behave unexpectedly.

### SendInput (direct typing)

Injects each UTF-16 code unit as a `KEYEVENTF_UNICODE` keystroke via the
Win32 `SendInput` API. The whole text is batched into a single SendInput
call (chunked at 100 events for very long dictations) so the target
app's message pump receives a coherent burst. Errors and partial
deliveries are logged.

Pros: no clipboard side-effects, clean for chat clients and terminals.

Cons:

- Slightly slower for very long dictations.
- Some applications filter injected input:
  - **Elevated targets**: a non-elevated vibe-dictate can't type into an
    admin-elevated window (Windows UIPI). Either run vibe-dictate as
    admin too, or stick with Clipboard mode for elevated apps.
  - **Games using DirectInput**: bypass SendInput entirely.
  - **Some hardened apps**: may reject `VK_PACKET` injected keys.

If SendInput silently drops events, the log will show
`SendInput partial: N/M events sent (last error: ...)`.

### Append Enter

Independent toggle that fires a single `VK_RETURN` keystroke after the
text is delivered. Works in both output modes. Useful when dictation is
also the "send" gesture (Discord, Slack, terminals, …).

---

## Troubleshooting

| Symptom | Likely cause | Fix |
|---|---|---|
| Tray icon never appears | Another instance is already running | Singleton check holds it back; check Task Manager for `vibe-dictate.exe` |
| Hotkey does nothing | Another app grabs it (Discord, OBS, GeForce Experience) | Pick a different binding via tray → Rebind… |
| Recording starts but transcription is empty / `[Silence]` | Wrong mic selected | Tray → Microphone → pick the actual physical input |
| `Gradio: poll failed` in log | Backend down or unreachable | `curl <url>/` from the same machine; check `gradio.url` |
| TLS errors against remote backend | Self-signed / private CA | Set `gradio.extra_ca_cert` to the PEM path |
| 401 / 403 from remote backend | Missing / wrong bearer | Set `gradio.api_token` |
| SendInput types nothing into focused window | Target window is elevated, or filters injected input | Switch to Clipboard mode, or run vibe-dictate as admin |
| Tray icon stuck on red after a cancel | Bug — please file an issue with logs | Reload config or restart |
| Hungarian text comes back in English | Language anchoring failed | Set `language_hint = "Hungarian"`, ensure `context_info` mentions Hungarian |
| Truncated transcription on long audio | `max_new_tokens` too low | Tray → Max tokens → bump to 16384 or 32768 |

Logs live at `%LOCALAPPDATA%\chestercs\vibe-dictate\cache\vibe-dictate.log`.
The log level is `info` by default.

---

## Architecture

```
┌─────────────────┐    ┌─────────────────┐    ┌──────────────────┐
│   tao event     │    │  global-hotkey  │    │ WH_MOUSE_LL hook │
│   loop (main)   │◄───┤  (kb bindings)  │    │  (Mouse3/4/5)    │
│                 │    └─────────────────┘    └────────┬─────────┘
│   ticks every   │             ▲                      │
│   ~33ms,        │             │                      │
│   dispatches    │             │ BindingManager       │
│   PushAction    │◄────────────┴──── routes binding ──┘
│   ::Press/      │                  string to either
│   Release       │
│                 │
│                 │   ┌────────────┐   ┌──────────┐   ┌──────────┐
│                 │──►│ audio.rs   │──►│ gradio.rs│──►│injector  │
│                 │   │ (cpal/     │   │ (reqwest │   │.rs       │
│                 │   │  WASAPI)   │   │  + SSE)  │   │ (clip /  │
│                 │   │  WAV bytes │   │  text    │   │  SendIn) │
│                 │   └────────────┘   └──────────┘   └──────────┘
└─────────────────┘
        │
        ▼
┌─────────────────┐   ┌──────────────────────┐   ┌──────────────┐
│  tray.rs        │   │ hotkey_capture.rs    │   │ text_input.rs│
│  (right-click   │   │ (Win32 modal popup,  │   │ (Win32 modal │
│   menu, status  │   │  PeekMessage pump,   │   │  text-entry  │
│   icon)         │   │  worker thread)      │   │  popup)      │
└─────────────────┘   └──────────────────────┘   └──────────────┘
```

Module map:

| File | Responsibility |
|---|---|
| `src/main.rs` | tao event loop, BindingManager, dispatch glue |
| `src/audio.rs` | WASAPI capture via cpal, WAV encoding (hound) |
| `src/gradio.rs` | reqwest blocking client: upload + call + SSE poll |
| `src/injector.rs` | clipboard paste (arboard) + SendInput (windows-rs) |
| `src/tray.rs` | tray icon + menu construction + menu event handling |
| `src/hotkey_capture.rs` | Win32 modal that captures next key/mouse press |
| `src/mouse_hook.rs` | low-level WH_MOUSE_LL hook for Mouse3/4/5 PTT |
| `src/text_input.rs` | Win32 modal for free-form config field editing |
| `src/config.rs` | TOML serde + ProjectDirs paths + migrations |
| `src/autostart.rs` | HKCU Run-key toggle |
| `src/singleton.rs` | named-mutex single-instance lock |

### Why blocking reqwest, not async?

The whole app is single-threaded around the tao event loop with worker
threads for the long-running tasks. Blocking IO inside a thread is
simpler than dragging tokio in for one HTTP call per dictation.

### Why `cpal::Stream` is awkward

`cpal::Stream` is `!Send`. The recording thread builds the WAV bytes
locally and hands the bytes (which *are* `Send`) to a network worker.
Don't try to pass the Stream itself across thread boundaries.

### Why two Win32 modals (capture + text input) instead of native dialogs?

`tray-icon` and `tao` don't ship modal dialog primitives, and the
Win32 modals only need ~150 lines each. Pulling in a UI framework
(egui, slint, fltk) for two popups would dwarf the rest of the binary.

---

## Development

### Repo layout

```
vibe-dictate/
├── Cargo.toml          # crate manifest
├── Dockerfile.build    # rust + cargo-xwin builder image
├── README.md           # this file
├── CLAUDE.md           # guidance for Claude Code sessions
├── assets/             # icon resources
├── build.rs            # winres for embedded icon
├── src/                # see Architecture
└── target/             # cargo output (gitignored)
```

The Docker compose for the cross-compile lives one level up at
`../docker-compose-vibedictate-build.yml`.

### Common workflows

**Rebuild after editing source**:

```bash
# from parent VibeVoice repo root
taskkill //IM vibe-dictate.exe //F  # Windows; release the exe lock
docker compose -f docker-compose-vibedictate-build.yml run --rm vibedictate-build
```

**Tail the log while testing**:

```powershell
Get-Content "$env:LOCALAPPDATA\chestercs\vibe-dictate\cache\vibe-dictate.log" -Wait -Tail 30
```

**Reset config to defaults**:

```bash
rm "$APPDATA\chestercs\vibe-dictate\config\config.toml"
# next launch will recreate from defaults
```

**Bump dependencies**:

```bash
cargo update --dry-run    # see what would change
cargo update
# rebuild + smoke-test
```

### Testing

There's no automated test suite — vibe-dictate is a thin glue layer over
Win32 + Gradio, both of which are awkward to mock. Smoke-test by
recording a few utterances in different output modes and target
windows. The architecture intentionally keeps the side-effecting code
(audio capture, HTTP, Win32 injection) thin so manual tests are quick.

### License

MIT. Upstream VibeVoice is also MIT.
