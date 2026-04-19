use std::io::Cursor;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{SampleFormat, StreamConfig};
use crossbeam_channel::{bounded, Receiver, Sender};

use crate::config::{AudioConfig, VadConfig};
use crate::vad::{EnergyVad, VadEvent};

pub struct Recorder {
    samples: Arc<Mutex<Vec<i16>>>,
    stream: cpal::Stream,
    sample_rate: u32,
    channels: u16,
}

impl Recorder {
    pub fn start(cfg: &AudioConfig) -> Result<Self> {
        let host = cpal::default_host();
        let device = pick_input_device(&host, &cfg.mic_device)?;
        log::info!("Input device: {}", device.name().unwrap_or_default());

        let default_cfg = device
            .default_input_config()
            .context("default_input_config")?;
        let sample_format = default_cfg.sample_format();
        let channels = default_cfg.channels();
        let sample_rate = default_cfg.sample_rate().0;

        let stream_cfg: StreamConfig = default_cfg.into();
        let samples = Arc::new(Mutex::new(Vec::<i16>::with_capacity(
            (sample_rate as usize) * 10,
        )));
        let samples_cb = samples.clone();

        let err_cb = |e| log::error!("audio stream error: {e:?}");

        let stream = match sample_format {
            SampleFormat::F32 => device.build_input_stream(
                &stream_cfg,
                move |data: &[f32], _| {
                    let mut guard = samples_cb.lock().unwrap();
                    for &s in data {
                        let v = (s.clamp(-1.0, 1.0) * i16::MAX as f32) as i16;
                        guard.push(v);
                    }
                },
                err_cb,
                None,
            ),
            SampleFormat::I16 => device.build_input_stream(
                &stream_cfg,
                move |data: &[i16], _| {
                    let mut guard = samples_cb.lock().unwrap();
                    guard.extend_from_slice(data);
                },
                err_cb,
                None,
            ),
            SampleFormat::U16 => device.build_input_stream(
                &stream_cfg,
                move |data: &[u16], _| {
                    let mut guard = samples_cb.lock().unwrap();
                    for &s in data {
                        let v = (s as i32 - 32768) as i16;
                        guard.push(v);
                    }
                },
                err_cb,
                None,
            ),
            other => return Err(anyhow!("Unsupported sample format: {:?}", other)),
        }
        .context("build_input_stream")?;

        stream.play().context("stream.play")?;

        Ok(Self {
            samples,
            stream,
            sample_rate,
            channels,
        })
    }

    pub fn stop_and_encode_wav(self) -> Result<Vec<u8>> {
        drop(self.stream);
        let samples = Arc::try_unwrap(self.samples)
            .map_err(|_| anyhow!("samples still referenced"))?
            .into_inner()
            .map_err(|_| anyhow!("samples mutex poisoned"))?;

        let mono = if self.channels > 1 {
            downmix_to_mono(&samples, self.channels as usize)
        } else {
            samples
        };
        encode_mono_wav(mono, self.sample_rate)
    }
}

/// Peak-normalize + hound-encode a mono i16 buffer into a WAV byte stream.
/// Shared by PTT (`Recorder::stop_and_encode_wav`) and voice-activation
/// (`VadSession` worker) so both pipelines hand identical audio to the ASR.
pub fn encode_mono_wav(mut mono: Vec<i16>, sample_rate: u32) -> Result<Vec<u8>> {
    // Peak-normalize quiet-but-valid speech up to ~-0.9 dBFS so VibeVoice-ASR
    // sees a consistent loudness profile. Skipped when peak is already high
    // (don't re-clip) or very low (don't amplify pure background noise into
    // hallucinations).
    normalize_peak_i16(&mut mono);

    let mut buf = Cursor::new(Vec::<u8>::with_capacity(mono.len() * 2 + 44));
    let spec = hound::WavSpec {
        channels: 1,
        sample_rate,
        bits_per_sample: 16,
        sample_format: hound::SampleFormat::Int,
    };
    {
        let mut writer = hound::WavWriter::new(&mut buf, spec)?;
        for s in mono {
            writer.write_sample(s)?;
        }
        writer.finalize()?;
    }
    Ok(buf.into_inner())
}

pub fn list_input_devices() -> Vec<String> {
    let host = cpal::default_host();
    match host.input_devices() {
        Ok(it) => it
            .filter_map(|d| d.name().ok())
            .collect::<Vec<_>>(),
        Err(e) => {
            log::warn!("Failed to enumerate input devices: {e:#}");
            Vec::new()
        }
    }
}

fn pick_input_device(
    host: &cpal::Host,
    name: &str,
) -> Result<cpal::Device> {
    if name.is_empty() {
        return host
            .default_input_device()
            .ok_or_else(|| anyhow!("No default input device"));
    }
    for d in host.input_devices()? {
        if d.name().map(|n| n == name).unwrap_or(false) {
            return Ok(d);
        }
    }
    log::warn!(
        "Input device '{}' not found, falling back to default",
        name
    );
    host.default_input_device()
        .ok_or_else(|| anyhow!("No default input device"))
}

/// Scale i16 PCM samples so the peak sits near -0.9 dBFS (~90% of i16
/// full-scale). Noisy silence (peak < ~5% FS) is left untouched — boosting
/// a microphone's self-noise to 90% just gives the ASR model a chance to
/// hallucinate speech into pure hiss. Audio that's already loud (peak ≥
/// target) also passes through unchanged.
fn normalize_peak_i16(samples: &mut [i16]) {
    // 90% of i16::MAX, leaves headroom for rounding artifacts.
    const TARGET: i32 = (i16::MAX as i32) * 9 / 10;
    // 5% of i16::MAX (~-26 dBFS peak). Below this we assume silence.
    const FLOOR: i32 = (i16::MAX as i32) / 20;

    let peak = samples
        .iter()
        .map(|&s| (s as i32).unsigned_abs() as i32)
        .max()
        .unwrap_or(0);
    if peak < FLOOR {
        log::info!("Normalize: peak {} below silence floor, skipping", peak);
        return;
    }
    if peak >= TARGET {
        return;
    }
    let scale = TARGET as f32 / peak as f32;
    log::info!("Normalize: peak {} → target {} (×{:.2})", peak, TARGET, scale);
    for s in samples.iter_mut() {
        let scaled = (*s as f32 * scale).round() as i32;
        *s = scaled.clamp(i16::MIN as i32, i16::MAX as i32) as i16;
    }
}

fn downmix_to_mono(samples: &[i16], channels: usize) -> Vec<i16> {
    let frames = samples.len() / channels;
    let mut out = Vec::with_capacity(frames);
    for f in 0..frames {
        let mut sum: i32 = 0;
        for c in 0..channels {
            sum += samples[f * channels + c] as i32;
        }
        out.push((sum / channels as i32) as i16);
    }
    out
}

/// Event surfaced to `main.rs` by the VAD worker thread. The worker keeps
/// the `cpal::Stream` alive (it's `!Send`, so we can't hand it off) and
/// emits either status pings (Opened / Closed) or a fully-encoded WAV
/// ready for `openai::SttClient::transcribe`.
#[derive(Debug)]
pub enum VadSessionEvent {
    /// The capture stream started successfully. Useful for the tray to
    /// flip to "listening" state, and for the main loop to know the mic
    /// is actually hot.
    Opened { sample_rate: u32 },
    /// Endpoint detected — here's the encoded WAV. Hand straight to the
    /// existing `send_and_inject` path.
    Utterance { wav: Vec<u8>, duration_ms: u64 },
    /// Brief "speech started" indicator, for tray green-flash while VAD
    /// is running. Not required for correctness.
    SpeechStart,
    /// Capture or device init failed. Carries a short diagnostic; the
    /// worker thread exits after emitting this.
    Error(String),
    /// Normal shutdown acknowledgement.
    Closed,
}

/// Handle to the VAD capture worker. Drop or call `stop()` to tear it
/// down; the worker's cpal::Stream is freed as the thread returns.
pub struct VadSession {
    shutdown: Arc<AtomicBool>,
    thread: Option<thread::JoinHandle<()>>,
    pub rx: Receiver<VadSessionEvent>,
}

impl VadSession {
    pub fn start(audio_cfg: AudioConfig, vad_cfg: VadConfig) -> Self {
        let shutdown = Arc::new(AtomicBool::new(false));
        let shutdown_thread = shutdown.clone();
        // Small capacity — we just need to buffer ~1 second of events.
        // Utterance WAVs are the only heavy payload and they drop straight
        // into the receiver.
        let (evt_tx, evt_rx) = bounded::<VadSessionEvent>(16);
        let evt_tx_run = evt_tx.clone();
        let thread = thread::spawn(move || {
            if let Err(e) = vad_worker(audio_cfg, vad_cfg, shutdown_thread, evt_tx_run.clone()) {
                let _ = evt_tx_run.send(VadSessionEvent::Error(format!("{:#}", e)));
            }
            let _ = evt_tx_run.send(VadSessionEvent::Closed);
        });
        Self {
            shutdown,
            thread: Some(thread),
            rx: evt_rx,
        }
    }

    pub fn stop(mut self) {
        self.shutdown.store(true, Ordering::SeqCst);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

impl Drop for VadSession {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::SeqCst);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

fn vad_worker(
    audio_cfg: AudioConfig,
    vad_cfg: VadConfig,
    shutdown: Arc<AtomicBool>,
    evt_tx: Sender<VadSessionEvent>,
) -> Result<()> {
    let host = cpal::default_host();
    let device = pick_input_device(&host, &audio_cfg.mic_device)?;
    log::info!("VAD input device: {}", device.name().unwrap_or_default());

    let default_cfg = device
        .default_input_config()
        .context("default_input_config")?;
    let sample_format = default_cfg.sample_format();
    let channels = default_cfg.channels();
    let sample_rate = default_cfg.sample_rate().0;
    let stream_cfg: StreamConfig = default_cfg.into();

    // cpal delivers audio on its own callback thread. We don't run the VAD
    // there — the callback must stay short — so we pipe raw (interleaved)
    // samples through a bounded channel to the worker body below.
    let (sample_tx, sample_rx) = bounded::<Vec<i16>>(32);
    let err_cb = |e| log::error!("VAD audio stream error: {e:?}");

    let stream = match sample_format {
        SampleFormat::F32 => device.build_input_stream(
            &stream_cfg,
            {
                let sample_tx = sample_tx.clone();
                move |data: &[f32], _| {
                    let mut chunk = Vec::with_capacity(data.len());
                    for &s in data {
                        let v = (s.clamp(-1.0, 1.0) * i16::MAX as f32) as i16;
                        chunk.push(v);
                    }
                    let _ = sample_tx.try_send(chunk);
                }
            },
            err_cb,
            None,
        ),
        SampleFormat::I16 => device.build_input_stream(
            &stream_cfg,
            {
                let sample_tx = sample_tx.clone();
                move |data: &[i16], _| {
                    let _ = sample_tx.try_send(data.to_vec());
                }
            },
            err_cb,
            None,
        ),
        SampleFormat::U16 => device.build_input_stream(
            &stream_cfg,
            {
                let sample_tx = sample_tx.clone();
                move |data: &[u16], _| {
                    let mut chunk = Vec::with_capacity(data.len());
                    for &s in data {
                        let v = (s as i32 - 32768) as i16;
                        chunk.push(v);
                    }
                    let _ = sample_tx.try_send(chunk);
                }
            },
            err_cb,
            None,
        ),
        other => return Err(anyhow!("Unsupported sample format: {:?}", other)),
    }
    .context("build_input_stream (vad)")?;
    stream.play().context("stream.play (vad)")?;
    drop(sample_tx); // The only remaining sender is inside the stream callback.

    let _ = evt_tx.send(VadSessionEvent::Opened { sample_rate });

    let mut vad = EnergyVad::new(vad_cfg, sample_rate);
    let ch = channels as usize;
    while !shutdown.load(Ordering::SeqCst) {
        let interleaved = match sample_rx.recv_timeout(Duration::from_millis(100)) {
            Ok(v) => v,
            Err(crossbeam_channel::RecvTimeoutError::Timeout) => continue,
            Err(crossbeam_channel::RecvTimeoutError::Disconnected) => {
                log::warn!("VAD sample channel closed unexpectedly");
                break;
            }
        };
        let mono: Vec<i16> = if ch > 1 {
            downmix_to_mono(&interleaved, ch)
        } else {
            interleaved
        };
        for event in vad.push(&mono) {
            match event {
                VadEvent::Nothing => {}
                VadEvent::SpeechStart => {
                    let _ = evt_tx.try_send(VadSessionEvent::SpeechStart);
                }
                VadEvent::SpeechEnd { samples } => {
                    let duration_ms = (samples.len() as u64 * 1000) / sample_rate as u64;
                    match encode_mono_wav(samples, sample_rate) {
                        Ok(wav) => {
                            let _ = evt_tx.send(VadSessionEvent::Utterance {
                                wav,
                                duration_ms,
                            });
                        }
                        Err(e) => {
                            log::error!("VAD wav encode failed: {e:#}");
                            let _ = evt_tx.send(VadSessionEvent::Error(format!(
                                "encode wav: {e:#}"
                            )));
                        }
                    }
                }
            }
        }
    }
    drop(stream);
    Ok(())
}
