use std::io::Cursor;
use std::sync::{Arc, Mutex};

use anyhow::{anyhow, Context, Result};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{SampleFormat, StreamConfig};

use crate::config::AudioConfig;

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

        let mut buf = Cursor::new(Vec::<u8>::with_capacity(mono.len() * 2 + 44));
        let spec = hound::WavSpec {
            channels: 1,
            sample_rate: self.sample_rate,
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
