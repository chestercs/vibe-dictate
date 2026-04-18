use std::fs;
use std::path::PathBuf;

use anyhow::{Context, Result};
use directories_next::ProjectDirs;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub gradio: GradioConfig,
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
pub struct GradioConfig {
    pub url: String,
    pub function: String,
    pub api_token: String,
}

impl Default for GradioConfig {
    fn default() -> Self {
        Self {
            url: "http://localhost:7860".to_string(),
            function: "transcribe_audio".to_string(),
            api_token: String::new(),
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
            context_info: String::new(),
            max_new_tokens: 16384,
            language_hint: "hu".to_string(),
        }
    }
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
            binding: "RightAlt+Space".to_string(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OutputConfig {
    pub mode: OutputMode,
    pub trailing_space: bool,
}

impl Default for OutputConfig {
    fn default() -> Self {
        Self {
            mode: OutputMode::Clipboard,
            trailing_space: true,
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
            gradio: GradioConfig::default(),
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
        let cfg: Config = toml::from_str(&text)
            .with_context(|| format!("Parse {}", path.display()))?;
        log::info!("Loaded config from {}", path.display());
        Ok(cfg)
    }

    pub fn save(&self) -> Result<()> {
        let path = Self::config_path()?;
        let text = toml::to_string_pretty(self)?;
        fs::write(&path, text).with_context(|| format!("Write {}", path.display()))?;
        Ok(())
    }
}
