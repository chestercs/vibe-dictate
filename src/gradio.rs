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
        log::info!(
            "Gradio: uploading WAV ({} bytes) to {}",
            wav.len(),
            self.base_url
        );
        let file_path = self.upload(wav)?;
        log::info!("Gradio: uploaded as '{}'", file_path);

        let event_id = self.call(&file_path, context_info, max_new_tokens, language_hint)?;
        log::info!(
            "Gradio: call queued, event_id={}, polling…",
            event_id
        );

        let text = self.poll_result(&event_id)?;
        log::info!("Gradio: poll complete ({} chars)", text.len());
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

        // VibeVoice ASR demo's transcribe_audio takes 10 positional inputs in
        // this exact order — see /gradio_api/info on the running server. The
        // first is the FileData; the next three (path/start/end) are only for
        // long-audio segmentation and stay empty in push-to-talk usage.
        // language_hint isn't a first-class parameter, so we fold it in as a
        // "Preferred language:" prefix and keep the user's context_info as
        // the free-form body — that way both signals reach the model.
        let lang = language_hint.trim();
        let ctx = context_info.trim();
        let effective_context = match (lang.is_empty(), ctx.is_empty()) {
            (true, true) => String::new(),
            (true, false) => ctx.to_string(),
            (false, true) => format!("Preferred language: {}.", lang),
            (false, false) => format!("Preferred language: {}. {}", lang, ctx),
        };

        let body = json!({
            "data": [
                {
                    "path": file_path,
                    "url": null,
                    "orig_name": "recording.wav",
                    "mime_type": "audio/wav",
                    "meta": { "_type": "gradio.FileData" }
                },
                "",                  // audio_path_input
                "",                  // start_time_input
                "",                  // end_time_input
                max_new_tokens,      // max_new_tokens
                0.0,                 // temperature
                1.0,                 // top_p
                false,               // do_sample
                1.0,                 // repetition_penalty
                effective_context    // context_info
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
    // VibeVoice ASR returns two outputs in `complete`: [raw_text, segments_html].
    // The raw_text is a human-formatted block ending in a JSON segments array:
    //   --- ✅ Raw Output ---
    //   📥 Input: ... tokens
    //   📤 Output: ... tokens | ⏱️ Time: 27.04s
    //   ---
    //   assistant
    //   [{"Start":0.0,"End":2.67,"Speaker":0,"Content":"Hello, hello, see ya."}]
    let raw = match v {
        Value::Array(arr) => arr
            .first()
            .ok_or_else(|| anyhow!("gradio complete array empty"))?,
        other => other,
    };
    let raw_str = match raw {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    };

    if let Some(joined) = parse_segments(&raw_str) {
        return Ok(joined);
    }

    log::warn!(
        "Could not extract segment Contents from raw output, returning as-is ({} chars)",
        raw_str.len()
    );
    Ok(raw_str)
}

#[derive(Debug, Deserialize)]
struct Segment {
    #[serde(rename = "Content")]
    content: String,
}

fn parse_segments(raw: &str) -> Option<String> {
    // The segments array always opens with `[{` (array of objects). Naively
    // searching for the last `[` is wrong because Content values themselves
    // can contain bracketed meta-tags like "[Music]". Anchor on `[{` and the
    // matching trailing `}]`.
    let start = raw.find("[{")?;
    let end = raw[start..].rfind("}]")? + start + 2;
    let json_slice = raw[start..end].trim();

    let segs: Vec<Segment> = serde_json::from_str(json_slice).ok()?;
    let joined = segs
        .iter()
        .map(|s| s.content.trim())
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join(" ");
    if joined.is_empty() {
        None
    } else {
        Some(joined)
    }
}
