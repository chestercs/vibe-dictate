//! OpenAI-compatible STT client.
//!
//! Talks to any server that speaks `POST /v1/audio/transcriptions` (our
//! VibeVoice FastAPI shim, OpenAI's Whisper endpoint, vLLM, etc.). One
//! multipart request per dictation; no SSE, no polling. TLS works with
//! either system roots or an extra PEM bundle configured by the user.

use std::error::Error as StdError;
use std::fmt;
use std::time::Duration;

use anyhow::{Context, Result};
use reqwest::blocking::{multipart, Client};
use serde::Deserialize;

use crate::config::ServerConfig;

pub struct SttClient {
    base_url: String,
    api_key: String,
    model: String,
    client: Client,
}

#[derive(Debug, Deserialize)]
struct TranscriptionResponse {
    text: String,
}

/// Typed transcription failure so the tray can map to a short, user-facing
/// summary (bad URL, bad token, model name, TLS...) instead of dumping a
/// stack-ish error message. Carries the full detail string for logging.
#[derive(Debug, Clone)]
pub enum TranscribeError {
    /// Host unreachable / DNS failed / connection refused / timeout.
    Connect(String),
    /// TLS handshake / certificate error (self-signed CA without extra_ca_cert, etc.).
    Tls(String),
    /// HTTP 401 / 403 — bearer token missing or wrong.
    Auth(String),
    /// HTTP 404 — base URL wrong, or server doesn't expose /v1/audio/transcriptions.
    Endpoint(String),
    /// Remaining HTTP error statuses (5xx, 4xx that aren't 401/403/404).
    Server(String),
    /// Anything else: decode failure, unexpected JSON shape, etc.
    Other(String),
}

impl TranscribeError {
    /// Short user-facing summary for the tray tooltip / balloon. Kept under
    /// ~60 chars so Windows doesn't truncate it in the tooltip line.
    pub fn short_summary(&self) -> String {
        match self {
            Self::Connect(_) => "Cannot reach STT server — check URL / network".to_string(),
            Self::Tls(_) => "TLS / certificate error — check extra_ca_cert".to_string(),
            Self::Auth(_) => "Authentication failed — check API key".to_string(),
            Self::Endpoint(_) => "Endpoint not found — check URL / model name".to_string(),
            Self::Server(m) => format!("Server error: {}", truncate(m, 48)),
            Self::Other(m) => truncate(m, 60),
        }
    }

    /// Whether this error means "network link is down" — drives the gray
    /// Disconnected tray state. Auth / endpoint / server errors still mean
    /// the backend is reachable; only Connect + Tls flip us offline.
    pub fn is_connection_issue(&self) -> bool {
        matches!(self, Self::Connect(_) | Self::Tls(_))
    }
}

impl fmt::Display for TranscribeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Connect(m)
            | Self::Tls(m)
            | Self::Auth(m)
            | Self::Endpoint(m)
            | Self::Server(m)
            | Self::Other(m) => write!(f, "{}", m),
        }
    }
}

impl std::error::Error for TranscribeError {}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let head: String = s.chars().take(max.saturating_sub(1)).collect();
        format!("{}…", head)
    }
}

/// Classify a reqwest error. We walk the source chain looking for the
/// hyper / rustls layer that named the failure — reqwest's top-level
/// Display is usually just "error sending request for url (...)".
fn classify_reqwest_err(e: reqwest::Error) -> TranscribeError {
    let msg = format!("{:#}", e);
    if e.is_timeout() {
        return TranscribeError::Connect(format!("timeout: {}", msg));
    }
    if e.is_connect() {
        return TranscribeError::Connect(msg);
    }
    // Crawl the source chain for rustls / TLS hints.
    let mut src: Option<&dyn StdError> = e.source();
    while let Some(inner) = src {
        let text = inner.to_string();
        let lower = text.to_ascii_lowercase();
        if lower.contains("certificate")
            || lower.contains("tls")
            || lower.contains("handshake")
            || lower.contains("webpki")
            || lower.contains("unknownca")
            || lower.contains("self-signed")
            || lower.contains("self signed")
        {
            return TranscribeError::Tls(msg);
        }
        if lower.contains("dns")
            || lower.contains("connection refused")
            || lower.contains("connectex")
            || lower.contains("no such host")
            || lower.contains("os error")
        {
            return TranscribeError::Connect(msg);
        }
        src = inner.source();
    }
    TranscribeError::Other(msg)
}

fn classify_http_status(status: reqwest::StatusCode, body: &str) -> TranscribeError {
    let detail = format!("HTTP {} — {}", status, truncate(body, 160));
    match status.as_u16() {
        401 | 403 => TranscribeError::Auth(detail),
        404 => TranscribeError::Endpoint(detail),
        _ => TranscribeError::Server(detail),
    }
}

impl SttClient {
    pub fn new(cfg: &ServerConfig) -> Result<Self> {
        let mut builder = Client::builder().timeout(Duration::from_secs(300));
        let ca_path = cfg.extra_ca_cert.trim();
        if !ca_path.is_empty() {
            // Remote deployments typically use self-signed or internal CAs.
            // We read the PEM once per client; reqwest accepts both single
            // certs and concatenated bundles.
            let pem = std::fs::read(ca_path)
                .with_context(|| format!("read extra_ca_cert from '{}'", ca_path))?;
            let cert = reqwest::Certificate::from_pem(&pem)
                .with_context(|| format!("parse extra_ca_cert PEM at '{}'", ca_path))?;
            builder = builder.add_root_certificate(cert);
            log::info!("STT client: loaded extra CA from '{}'", ca_path);
        }
        let client = builder.build().context("build reqwest client")?;
        Ok(Self {
            base_url: cfg.base_url.trim_end_matches('/').to_string(),
            api_key: cfg.api_key.clone(),
            model: cfg.model.clone(),
            client,
        })
    }

    pub fn transcribe(
        &self,
        wav: Vec<u8>,
        language_hint: &str,
    ) -> std::result::Result<String, TranscribeError> {
        log::info!(
            "STT: posting WAV ({} bytes) to {}",
            wav.len(),
            self.base_url
        );

        let part = multipart::Part::bytes(wav)
            .file_name("recording.wav")
            .mime_str("audio/wav")
            .map_err(|e| TranscribeError::Other(format!("multipart part: {e:#}")))?;
        let mut form = multipart::Form::new()
            .part("file", part)
            .text("model", self.model.clone())
            .text("response_format", "json");

        let lang = language_hint.trim();
        if !lang.is_empty() {
            form = form.text("language", lang.to_string());
        }

        let url = format!("{}/v1/audio/transcriptions", self.base_url);
        let mut req = self.client.post(&url).multipart(form);
        if !self.api_key.is_empty() {
            req = req.bearer_auth(&self.api_key);
        }

        let resp = req.send().map_err(classify_reqwest_err)?;
        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().unwrap_or_default();
            return Err(classify_http_status(status, &body));
        }
        let parsed: TranscriptionResponse = resp
            .json()
            .map_err(|e| TranscribeError::Other(format!("parse json: {e:#}")))?;
        log::info!("STT: transcribed {} chars", parsed.text.len());
        Ok(parsed.text)
    }

    /// Cheap reachability probe for the heartbeat thread. Hits `/v1/models`
    /// (every OpenAI-compat server exposes it) with a short timeout and the
    /// configured bearer. Success = connection OK; classified error otherwise.
    pub fn health_check(&self) -> std::result::Result<(), TranscribeError> {
        let url = format!("{}/v1/models", self.base_url);
        let mut req = self.client.get(&url).timeout(Duration::from_secs(5));
        if !self.api_key.is_empty() {
            req = req.bearer_auth(&self.api_key);
        }
        let resp = req.send().map_err(classify_reqwest_err)?;
        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().unwrap_or_default();
            return Err(classify_http_status(status, &body));
        }
        Ok(())
    }
}
