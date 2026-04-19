//! OpenAI-compatible STT client.
//!
//! Talks to any server that speaks `POST /v1/audio/transcriptions` (our
//! VibeVoice FastAPI shim, OpenAI's Whisper endpoint, vLLM, etc.). One
//! multipart request per dictation; no SSE, no polling. TLS works with
//! either system roots or an extra PEM bundle configured by the user.

use std::time::Duration;

use anyhow::{anyhow, Context, Result};
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

    pub fn transcribe(&self, wav: Vec<u8>, language_hint: &str) -> Result<String> {
        log::info!(
            "STT: posting WAV ({} bytes) to {}",
            wav.len(),
            self.base_url
        );

        let part = multipart::Part::bytes(wav)
            .file_name("recording.wav")
            .mime_str("audio/wav")?;
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

        let resp = req.send().context("transcribe send")?;
        if !resp.status().is_success() {
            let s = resp.status();
            let body = resp.text().unwrap_or_default();
            return Err(anyhow!("transcribe failed: HTTP {} — {}", s, body));
        }
        let parsed: TranscriptionResponse = resp.json().context("transcribe parse json")?;
        log::info!("STT: transcribed {} chars", parsed.text.len());
        Ok(parsed.text)
    }
}
