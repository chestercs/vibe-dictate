use std::fs;
use std::path::PathBuf;

use anyhow::{Context, Result};
use directories_next::ProjectDirs;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    /// `[server]` in TOML. Older configs used `[gradio]` with fields
    /// `url` / `api_token`; the serde aliases on ServerConfig let those
    /// load transparently and get rewritten on the next save.
    #[serde(default, alias = "gradio")]
    pub server: ServerConfig,
    #[serde(default)]
    pub stt: SttConfig,
    #[serde(default)]
    pub audio: AudioConfig,
    #[serde(default)]
    pub hotkey: HotkeyConfig,
    #[serde(default)]
    pub output: OutputConfig,
    #[serde(default)]
    pub startup: StartupConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerConfig {
    /// Base URL of the OpenAI-compatible STT server, e.g.
    /// `http://localhost:8080` or `https://stt.example.com`. No trailing
    /// slash. The client appends `/v1/audio/transcriptions`.
    #[serde(alias = "url")]
    pub base_url: String,
    /// Bearer token for the server. Leave empty for localhost or when the
    /// server has auth disabled.
    #[serde(alias = "api_token")]
    pub api_key: String,
    /// Model identifier sent in the multipart `model` field. Servers that
    /// host a single model (our VibeVoice shim, most self-hosted vLLM
    /// deployments) ignore this. For OpenAI Whisper, set to `whisper-1`.
    #[serde(default = "default_stt_model")]
    pub model: String,
    /// Absolute path to a PEM-encoded CA certificate (or bundle) that
    /// reqwest should trust in addition to the system roots. Leave empty
    /// for localhost / public-CA deployments; set this when pointing the
    /// client at a remote endpoint behind a self-signed cert or a
    /// private CA (e.g. Tailscale funnel, internal proxy).
    #[serde(default)]
    pub extra_ca_cert: String,
}

fn default_stt_model() -> String {
    "microsoft/VibeVoice-ASR-HF".to_string()
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            base_url: "http://localhost:8080".to_string(),
            api_key: String::new(),
            model: default_stt_model(),
            extra_ca_cert: String::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SttConfig {
    pub context_info: String,
    pub max_new_tokens: u32,
    pub language_hint: String,
}

impl Default for SttConfig {
    fn default() -> Self {
        Self {
            context_info: default_context_info(),
            max_new_tokens: 8192,
            language_hint: "Hungarian".to_string(),
        }
    }
}

/// Map short ISO 639-1 codes (or obvious two-letter variants) to the full
/// English language names the ASR model's prompt handling responds to
/// best. Returns None if the hint already looks like a full name.
fn iso_to_language_name(hint: &str) -> Option<&'static str> {
    match hint.trim().to_ascii_lowercase().as_str() {
        "hu" => Some("Hungarian"),
        "en" => Some("English"),
        "de" => Some("German"),
        "fr" => Some("French"),
        "es" => Some("Spanish"),
        "it" => Some("Italian"),
        "pt" => Some("Portuguese"),
        "pl" => Some("Polish"),
        "nl" => Some("Dutch"),
        "ja" => Some("Japanese"),
        "ko" => Some("Korean"),
        "zh" => Some("Chinese"),
        _ => None,
    }
}

/// Default prompt we feed to VibeVoice ASR when context_info isn't filled
/// in explicitly. Without a language anchor the model guesses wildly on
/// short utterances and on sentences that mix Hungarian + English terms;
/// this prompt tells it to stay in Hungarian as the default language but
/// leave code-mixed English words intact (brand names, tech jargon, etc.).
fn default_context_info() -> String {
    "The speaker uses Hungarian as the primary language, and may mix in English technical terms, proper nouns, brand names, and abbreviations. Transcribe verbatim — keep each word in its original language, do not translate.".to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AudioConfig {
    pub mic_device: String,
    pub sample_rate: u32,
}

impl Default for AudioConfig {
    fn default() -> Self {
        Self {
            mic_device: String::new(),
            sample_rate: 16000,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HotkeyConfig {
    pub binding: String,
}

impl Default for HotkeyConfig {
    fn default() -> Self {
        Self {
            binding: "F8".to_string(),
        }
    }
}

pub const HOTKEY_OPTIONS: &[&str] =
    &["F7", "F8", "F9", "F10", "F11", "F12", "Pause", "ScrollLock"];

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OutputConfig {
    pub mode: OutputMode,
    pub trailing_space: bool,
    /// If true, inject a Return keypress after the transcription. Useful for
    /// chat clients / terminals where the user wants the message sent
    /// immediately. Default false because most editors don't want a stray
    /// newline appended to dictated text.
    #[serde(default)]
    pub send_enter: bool,
    /// Milliseconds to sleep between successive characters in SendInput
    /// mode. Too fast and Electron/Chromium apps (Discord, Slack, VS Code),
    /// Notepad, and some terminals silently drop characters; too slow and
    /// dictation feels sluggish. 20 ms ≈ 50 chars/sec is the safe default
    /// that works even on slower CPUs; drop to 5-10 ms on fast machines
    /// into well-behaved editors if you want faster injection.
    #[serde(default = "default_send_key_delay_ms")]
    pub send_key_delay_ms: u64,
    /// Milliseconds to hold each key "down" before releasing (down→up gap
    /// per character). Usually 0 is fine; bump to 5-10 ms for a handful of
    /// legacy apps that filter out keypresses with zero duration.
    #[serde(default)]
    pub send_key_down_delay_ms: u64,
}

fn default_send_key_delay_ms() -> u64 { 20 }

impl Default for OutputConfig {
    fn default() -> Self {
        Self {
            mode: OutputMode::Clipboard,
            trailing_space: true,
            send_enter: false,
            send_key_delay_ms: default_send_key_delay_ms(),
            send_key_down_delay_ms: 0,
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum OutputMode {
    Clipboard,
    Sendinput,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StartupConfig {
    pub autostart: bool,
    pub start_minimized: bool,
}

impl Default for StartupConfig {
    fn default() -> Self {
        Self {
            autostart: false,
            start_minimized: true,
        }
    }
}

impl Default for Config {
    fn default() -> Self {
        Self {
            server: ServerConfig::default(),
            stt: SttConfig::default(),
            audio: AudioConfig::default(),
            hotkey: HotkeyConfig::default(),
            output: OutputConfig::default(),
            startup: StartupConfig::default(),
        }
    }
}

impl Config {
    pub fn config_path() -> Result<PathBuf> {
        let dirs = ProjectDirs::from("com", "chestercs", "vibe-dictate")
            .context("Could not resolve APPDATA directory")?;
        let dir = dirs.config_dir();
        fs::create_dir_all(dir).context("Failed to create config dir")?;
        Ok(dir.join("config.toml"))
    }

    pub fn log_path() -> Result<PathBuf> {
        let dirs = ProjectDirs::from("com", "chestercs", "vibe-dictate")
            .context("Could not resolve APPDATA directory")?;
        let dir = dirs.cache_dir();
        fs::create_dir_all(dir).context("Failed to create cache dir")?;
        Ok(dir.join("vibe-dictate.log"))
    }

    pub fn load_or_default() -> Result<Self> {
        let path = Self::config_path()?;
        if !path.exists() {
            let cfg = Config::default();
            cfg.save()?;
            log::info!("Created default config at {}", path.display());
            return Ok(cfg);
        }
        let text = fs::read_to_string(&path)
            .with_context(|| format!("Read {}", path.display()))?;
        let mut cfg: Config = toml::from_str(&text)
            .with_context(|| format!("Parse {}", path.display()))?;
        log::info!("Loaded config from {}", path.display());
        if cfg.migrate_in_place() {
            cfg.save()?;
            log::info!("Migrated stale config values to defaults");
        }
        Ok(cfg)
    }

    fn migrate_in_place(&mut self) -> bool {
        let mut changed = false;
        // Alt-based hotkeys conflict with Windows app menus and AltGr (RightAlt = Ctrl+Alt
        // on Hungarian layouts) tends to leave Alt stuck. Force-migrate to F8 default.
        let lower = self.hotkey.binding.to_ascii_lowercase();
        let has_alt = lower.split('+').any(|t| {
            matches!(
                t.trim(),
                "alt" | "rightalt" | "altgr" | "leftalt"
            )
        });
        if has_alt {
            log::warn!(
                "Migrating Alt-based hotkey '{}' to 'F8' (Alt conflicts with app menus)",
                self.hotkey.binding
            );
            self.hotkey.binding = "F8".to_string();
            changed = true;
        }

        // Upgrade 2-letter ISO hints to full English language names — ASR
        // prompt quality is measurably better with "Hungarian" than "hu".
        if let Some(expanded) = iso_to_language_name(&self.stt.language_hint) {
            log::info!(
                "Migrating language_hint '{}' → '{}'",
                self.stt.language_hint,
                expanded
            );
            self.stt.language_hint = expanded.to_string();
            changed = true;
        }

        // If context_info is empty, seed a sensible default prompt that
        // anchors on the user's primary language and allows mixed terms.
        if self.stt.context_info.trim().is_empty() {
            log::info!("Seeding default context_info prompt");
            self.stt.context_info = default_context_info();
            changed = true;
        }

        // v0.1 → v0.2 transport migration: Gradio (7860) → OpenAI-compat
        // (8080). Only auto-migrate the exact legacy default URL; if the
        // user pointed at a remote/custom URL we leave it — they'll know
        // whether it's OpenAI-compat or not.
        if self.server.base_url.trim_end_matches('/') == "http://localhost:7860" {
            log::info!("Migrating server.base_url 7860 → 8080 (OpenAI-compat default)");
            self.server.base_url = "http://localhost:8080".to_string();
            changed = true;
        }
        changed
    }

    pub fn save(&self) -> Result<()> {
        let path = Self::config_path()?;
        let text = toml::to_string_pretty(self)?;
        fs::write(&path, text).with_context(|| format!("Write {}", path.display()))?;
        Ok(())
    }
}
