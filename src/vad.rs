//! Energy-based voice-activity detector.
//!
//! Takes raw i16 mono PCM frames (any length — we re-slice into exact 20 ms
//! analysis frames internally), runs RMS against an adaptive noise floor,
//! and emits `SpeechEnd` events carrying the buffered utterance samples
//! ready for WAV-encoding + upload.
//!
//! Deliberately dumb: no WebRTC VAD, no neural net, no extra crates. Close-
//! mic dictation has a generous SNR, so a dynamic threshold against a
//! floor-clamped noise EMA is enough and keeps the binary tiny.

use std::collections::VecDeque;

use crate::config::VadConfig;

/// Length of one analysis frame in milliseconds. 20 ms is the classic
/// compromise — short enough that speech onset isn't clipped, long enough
/// that the per-frame RMS is stable against single-sample spikes.
const FRAME_MS: u32 = 20;

/// EMA smoothing factor for the noise floor while we're in silence.
/// `new = α * current + (1-α) * old` — ~1.5 s time constant at 20 ms frames.
const NOISE_ALPHA: f32 = 0.015;

/// How many of the most-recent silence frames to prepend to an utterance
/// once it opens. Without this, `start_frames` worth of speech (60 ms at the
/// default) gets chopped off the front of every utterance.
const PRE_ROLL_FRAMES: usize = 6;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    Idle,
    InSpeech,
}

pub enum VadEvent {
    /// Nothing interesting this frame (still silence, or still speaking).
    Nothing,
    /// First frame of a new utterance. Useful for tray feedback; the
    /// samples keep accumulating internally until `SpeechEnd`.
    SpeechStart,
    /// Utterance ended (either by `end_frames` silence or by `max_seconds`
    /// cap). The buffer carries pre-roll + entire speech body as mono i16.
    SpeechEnd { samples: Vec<i16> },
}

pub struct EnergyVad {
    cfg: VadConfig,
    sample_rate: u32,
    frame_samples: usize,

    /// Leftover samples that didn't fill a full 20 ms frame on the previous
    /// `push` call. Prepended to the next batch before re-slicing.
    pending: Vec<i16>,

    noise_floor: f32,

    state: State,
    consec_speech: u32,
    consec_silence: u32,

    /// Ring of recent silence frames kept around so we can prepend a small
    /// audio cushion to the utterance once it opens.
    pre_roll: VecDeque<Vec<i16>>,

    /// Live utterance buffer; grows while `state == InSpeech`.
    utterance: Vec<i16>,
    /// Hard cap in samples — `max_seconds * sample_rate`.
    max_samples: usize,
}

impl EnergyVad {
    pub fn new(cfg: VadConfig, sample_rate: u32) -> Self {
        let frame_samples = ((sample_rate * FRAME_MS) / 1000) as usize;
        let max_samples = (cfg.max_seconds as usize) * (sample_rate as usize);
        Self {
            noise_floor: cfg.noise_floor_min.max(1.0),
            cfg,
            sample_rate,
            frame_samples,
            pending: Vec::new(),
            state: State::Idle,
            consec_speech: 0,
            consec_silence: 0,
            pre_roll: VecDeque::with_capacity(PRE_ROLL_FRAMES + 1),
            utterance: Vec::new(),
            max_samples,
        }
    }

    pub fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    /// Frame length in samples for this VAD's sample rate. Used by the
    /// audio worker to pace its channel reads.
    pub fn frame_samples(&self) -> usize {
        self.frame_samples
    }

    /// Feed a chunk of mono i16 samples. Returns every event that fired
    /// within this chunk — typically 0 or 1, but a chunk long enough to
    /// span silence→speech→silence can yield Start+End in one call.
    pub fn push(&mut self, samples: &[i16]) -> Vec<VadEvent> {
        let mut events = Vec::new();
        // Fast path: merge the leftover bytes from last call with the new
        // batch and re-slice into full 20 ms frames. Anything left over is
        // parked in `pending` for next time.
        self.pending.extend_from_slice(samples);
        let full_frames = self.pending.len() / self.frame_samples;
        if full_frames == 0 {
            return events;
        }
        let consumed = full_frames * self.frame_samples;
        // Clone-out the frames we're going to analyse, then drop them from
        // the pending buffer. Cheaper than doing the drain + slice dance.
        let frames: Vec<i16> = self.pending.drain(..consumed).collect();
        for frame in frames.chunks(self.frame_samples) {
            self.handle_frame(frame, &mut events);
        }
        events
    }

    fn handle_frame(&mut self, frame: &[i16], events: &mut Vec<VadEvent>) {
        let rms = rms_i16(frame);
        let threshold = self.noise_floor * self.cfg.speech_ratio;
        let is_speech = rms > threshold;

        match self.state {
            State::Idle => {
                if is_speech {
                    self.consec_speech += 1;
                    self.consec_silence = 0;
                    if self.consec_speech >= self.cfg.start_frames {
                        // Open a new utterance. Seed it with the pre-roll
                        // so the first ~120 ms of speech isn't chopped.
                        self.state = State::InSpeech;
                        self.utterance.clear();
                        for pre in self.pre_roll.drain(..) {
                            self.utterance.extend_from_slice(&pre);
                        }
                        self.utterance.extend_from_slice(frame);
                        self.consec_silence = 0;
                        events.push(VadEvent::SpeechStart);
                        return;
                    }
                    // Also push speech-looking frames into pre-roll in case
                    // start_frames isn't met — otherwise a short burst
                    // leaves the noise floor unchanged but discards audio.
                    self.push_pre_roll(frame);
                } else {
                    self.consec_speech = 0;
                    // Let the noise floor drift only during confirmed silence
                    // (prevents speech itself from lifting the threshold).
                    self.update_noise_floor(rms);
                    self.push_pre_roll(frame);
                }
            }
            State::InSpeech => {
                self.utterance.extend_from_slice(frame);
                if is_speech {
                    self.consec_silence = 0;
                } else {
                    self.consec_silence += 1;
                }
                let too_long = self.utterance.len() >= self.max_samples;
                let ended = self.consec_silence >= self.cfg.end_frames;
                if ended || too_long {
                    let min_samples = ((self.cfg.min_utterance_ms as usize)
                        * (self.sample_rate as usize))
                        / 1000;
                    let trailing_silence =
                        (self.consec_silence as usize) * self.frame_samples;
                    // Strip the trailing silence tail we used as the endpoint
                    // detector — no reason to ship hundreds of ms of room
                    // tone to the ASR.
                    let body_len = self.utterance.len().saturating_sub(trailing_silence);
                    let body = self.utterance[..body_len].to_vec();
                    let reason = if too_long { "max cap" } else { "silence" };
                    log::info!(
                        "VAD: utterance end ({}), {} ms (body), rms_noise={:.1} thr={:.1}",
                        reason,
                        body.len() * 1000 / self.sample_rate as usize,
                        self.noise_floor,
                        threshold,
                    );
                    self.state = State::Idle;
                    self.consec_speech = 0;
                    self.consec_silence = 0;
                    self.utterance.clear();
                    self.pre_roll.clear();
                    if body.len() >= min_samples {
                        events.push(VadEvent::SpeechEnd { samples: body });
                    } else {
                        log::info!(
                            "VAD: utterance below min ({} ms), discarded",
                            body.len() * 1000 / self.sample_rate as usize
                        );
                    }
                }
            }
        }
    }

    fn push_pre_roll(&mut self, frame: &[i16]) {
        if self.pre_roll.len() >= PRE_ROLL_FRAMES {
            self.pre_roll.pop_front();
        }
        self.pre_roll.push_back(frame.to_vec());
    }

    fn update_noise_floor(&mut self, rms: f32) {
        let next = NOISE_ALPHA * rms + (1.0 - NOISE_ALPHA) * self.noise_floor;
        self.noise_floor = next.max(self.cfg.noise_floor_min);
    }
}

fn rms_i16(frame: &[i16]) -> f32 {
    if frame.is_empty() {
        return 0.0;
    }
    let mut acc: f64 = 0.0;
    for &s in frame {
        let v = s as f64;
        acc += v * v;
    }
    (acc / frame.len() as f64).sqrt() as f32
}
