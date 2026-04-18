use std::io::{BufRead, BufReader};
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use reqwest::blocking::{multipart, Client};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::config::GradioConfig;

pub struct GradioClient {
    base_url: String,
    function: String,
    token: String,
    client: Client,
}

#[derive(Debug, Deserialize)]
struct CallResponse {
    event_id: String,
}

impl GradioClient {
    pub fn new(cfg: &GradioConfig) -> Result<Self> {
        let client = Client::builder()
            .timeout(Duration::from_secs(300))
            .build()?;
        Ok(Self {
            base_url: cfg.url.trim_end_matches('/').to_string(),
            function: cfg.function.clone(),
            token: cfg.api_token.clone(),
            client,
        })
    }

    pub fn transcribe(
        &self,
        wav: Vec<u8>,
        context_info: &str,
        max_new_tokens: u32,
        language_hint: &str,
    ) -> Result<String> {
        let file_path = self.upload(wav)?;
        log::debug!("Uploaded file path: {}", file_path);

        let event_id = self.call(&file_path, context_info, max_new_tokens, language_hint)?;
        log::debug!("Event id: {}", event_id);

        let text = self.poll_result(&event_id)?;
        Ok(text)
    }

    fn upload(&self, wav: Vec<u8>) -> Result<String> {
        let part = multipart::Part::bytes(wav)
            .file_name("recording.wav")
            .mime_str("audio/wav")?;
        let form = multipart::Form::new().part("files", part);

        let url = format!("{}/gradio_api/upload", self.base_url);
        let mut req = self.client.post(&url).multipart(form);
        if !self.token.is_empty() {
            req = req.bearer_auth(&self.token);
        }
        let resp = req.send().context("upload send")?;
        if !resp.status().is_success() {
            return Err(anyhow!("upload failed: HTTP {}", resp.status()));
        }
        let paths: Vec<String> = resp.json().context("upload parse json")?;
        paths
            .into_iter()
            .next()
            .ok_or_else(|| anyhow!("upload returned no paths"))
    }

    fn call(
        &self,
        file_path: &str,
        context_info: &str,
        max_new_tokens: u32,
        language_hint: &str,
    ) -> Result<String> {
        let url = format!("{}/gradio_api/call/{}", self.base_url, self.function);
        let body = json!({
            "data": [
                {
                    "path": file_path,
                    "url": null,
                    "orig_name": "recording.wav",
                    "mime_type": "audio/wav",
                    "meta": { "_type": "gradio.FileData" }
                },
                context_info,
                max_new_tokens,
                language_hint
            ]
        });

        let mut req = self.client.post(&url).json(&body);
        if !self.token.is_empty() {
            req = req.bearer_auth(&self.token);
        }
        let resp = req.send().context("call send")?;
        if !resp.status().is_success() {
            let s = resp.status();
            let body = resp.text().unwrap_or_default();
            return Err(anyhow!("call failed: HTTP {} — {}", s, body));
        }
        let parsed: CallResponse = resp.json().context("call parse json")?;
        Ok(parsed.event_id)
    }

    fn poll_result(&self, event_id: &str) -> Result<String> {
        let url = format!(
            "{}/gradio_api/call/{}/{}",
            self.base_url, self.function, event_id
        );
        let mut req = self.client.get(&url);
        if !self.token.is_empty() {
            req = req.bearer_auth(&self.token);
        }
        let resp = req.send().context("poll send")?;
        if !resp.status().is_success() {
            return Err(anyhow!("poll failed: HTTP {}", resp.status()));
        }

        // Parse SSE: lines like "event: complete" and "data: <json>"
        let reader = BufReader::new(resp);
        let mut last_event: Option<String> = None;
        let mut data_buf = String::new();
        for line in reader.lines() {
            let line = line.context("sse read line")?;
            if line.is_empty() {
                // dispatch event
                if let Some(ev) = last_event.as_deref() {
                    match ev {
                        "complete" => {
                            let parsed: Value = serde_json::from_str(&data_buf)
                                .with_context(|| format!("parse sse data '{}'", data_buf))?;
                            return extract_text(&parsed);
                        }
                        "error" => {
                            return Err(anyhow!("gradio error event: {}", data_buf));
                        }
                        _ => {}
                    }
                }
                last_event = None;
                data_buf.clear();
            } else if let Some(rest) = line.strip_prefix("event:") {
                last_event = Some(rest.trim().to_string());
            } else if let Some(rest) = line.strip_prefix("data:") {
                if !data_buf.is_empty() {
                    data_buf.push('\n');
                }
                data_buf.push_str(rest.trim_start());
            }
        }
        Err(anyhow!("SSE stream ended without complete event"))
    }
}

fn extract_text(v: &Value) -> Result<String> {
    // Gradio "complete" data is typically a JSON array of outputs.
    // transcribe_audio returns a single Textbox string.
    match v {
        Value::Array(arr) => {
            let first = arr
                .first()
                .ok_or_else(|| anyhow!("gradio complete array empty"))?;
            match first {
                Value::String(s) => Ok(s.clone()),
                other => Ok(other.to_string()),
            }
        }
        Value::String(s) => Ok(s.clone()),
        other => Ok(other.to_string()),
    }
}
