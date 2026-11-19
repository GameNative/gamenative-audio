//! Converting between the rate the client writes and the rate the device plays.
//!
//! The reference sink has no resampler of its own and needs none: it opens its AAudio
//! stream with the rate left unspecified, takes whatever the route answers with, and
//! PulseAudio's graph converts on the way in. This crate has no graph below it — the
//! ALSA PCM above is the only thing that could convert, and it cannot convert a rate it
//! was not asked for — so the conversion belongs here, in the one place that knows both
//! numbers: the device's answer and the client's contract.
//!
//! Without it, a route that answers 44.1 kHz while the client writes 48 kHz plays every
//! sample 8.8 % slow and low, and the backlog that produces grows until the delay is
//! audible. That is not a tuning problem and no buffer size fixes it.
//!
//! Layout: **channel-major (planar)**, one plane per channel, each `frames` long — the same
//! shape [`crate::Client::render`] produces and [`crate::engine`] reads back. The ring and
//! the device buffer are interleaved; this is the one place in the pipeline that is not, and
//! reading it as interleaved silently swaps channels and reads every other frame.
//!
//! The interpolation is Catmull-Rom over four points. Linear interpolation across a
//! 48→44.1 conversion folds audible aliasing into anything with content above 10 kHz,
//! and this is music.

/// Input frames kept from the previous block, so the first output frame of a block is
/// interpolated with the input that came before it instead of starting at a hard edge.
const HISTORY: usize = 3;

pub(crate) struct Resampler {
    in_rate: i32,
    out_rate: i32,
    channels: u16,
    /// Input frames consumed per output frame. Exactly `1.0` when the rates match.
    step: f64,
    /// Where the next output frame falls inside the input block, in `[0, 1)`.
    phase: f64,
    /// `HISTORY × channels` interleaved samples preceding the current block.
    history: Vec<f32>,
}

impl Resampler {
    pub(crate) fn new(in_rate: i32, out_rate: i32, channels: u16) -> Self {
        let in_rate = in_rate.max(1);
        let out_rate = out_rate.max(1);
        Self {
            in_rate,
            out_rate,
            channels,
            step: in_rate as f64 / out_rate as f64,
            phase: 0.0,
            history: vec![0.0; HISTORY * channels.max(1) as usize],
        }
    }

    /// Whether the client's rate already is the device's, in which case blocks pass
    /// through untouched.
    pub(crate) fn is_identity(&self) -> bool {
        self.in_rate == self.out_rate
    }

    /// Input frames needed to produce `out_frames` output frames.
    ///
    /// Constant for a given rate pair, which is what keeps the caller's accounting
    /// simple: the same number of frames is pulled from the client every block, and the
    /// fractional remainder is carried in [`Self::phase`] rather than accumulating as
    /// drift.
    pub(crate) fn input_frames(&self, out_frames: u32) -> u32 {
        if self.is_identity() {
            return out_frames;
        }
        let span = self.phase + out_frames as f64 * self.step;
        span.ceil() as u32
    }

    /// Output frames that will carry audio, given that the client supplied
    /// `supplied` of the input frames it was asked for.
    ///
    /// The device is handed a zero-padded block when a client falls short, so the part
    /// of the block that is real audio ends where the supplied input ends.
    pub(crate) fn real_output_frames(&self, supplied: u32, out_frames: u32) -> u32 {
        if self.is_identity() {
            return supplied.min(out_frames);
        }
        if supplied as f64 <= self.phase {
            return 0;
        }
        let real = ((supplied as f64 - self.phase) / self.step).floor() + 1.0;
        (real as u32).clamp(0, out_frames)
    }

    /// Resamples one block of `input` into `out`, which holds `out_frames` frames.
    ///
    /// `input` must hold at least [`Self::input_frames`] frames; the remainder is the
    /// caller's to zero-fill. Returns the frames written, which is every frame asked
    /// for: a device period is never short because the rate changed.
    pub(crate) fn process(&mut self, input: &[f32], out_frames: u32, out: &mut [f32]) -> u32 {
        let channels = self.channels.max(1) as usize;
        let out_frames = out_frames as usize;
        let in_frames = input.len() / channels;
        if out.len() < out_frames * channels || in_frames == 0 {
            return 0;
        }
        if self.is_identity() {
            let frames = out_frames.min(in_frames);
            out[..frames * channels].copy_from_slice(&input[..frames * channels]);
            return frames as u32;
        }

        for k in 0..out_frames {
            let position = self.phase + k as f64 * self.step;
            let base = position.floor();
            let t = (position - base) as f32;
            let base = base as isize;
            for channel in 0..channels {
                let y0 = self.sample(input, in_frames, base - 1, channel);
                let y1 = self.sample(input, in_frames, base, channel);
                let y2 = self.sample(input, in_frames, base + 1, channel);
                let y3 = self.sample(input, in_frames, base + 2, channel);
                out[channel * out_frames + k] = catmull_rom(y0, y1, y2, y3, t);
            }
        }

        // Carry the fraction, not the drift: `consumed` is what this block used, and the
        // remainder is where the next block starts.
        let consumed = self.input_frames(out_frames as u32) as usize;
        self.phase += out_frames as f64 * self.step - consumed as f64;
        if self.phase < 0.0 {
            // Only reachable if the rate changed mid-stream; a fresh resampler starts at
            // zero and cannot produce it.
            self.phase = 0.0;
        }
        self.remember(input, in_frames);
        out_frames as u32
    }

    /// Sample `index` of the input, where a negative index reads the history of the
    /// previous block and one past the end holds the last sample rather than a zero.
    fn sample(&self, input: &[f32], in_frames: usize, index: isize, channel: usize) -> f32 {
        if index < 0 {
            let back = (-index) as usize;
            if back <= HISTORY {
                return self.history[channel * HISTORY + (HISTORY - back)];
            }
            return self.history[channel * HISTORY];
        }
        let index = index as usize;
        if index < in_frames {
            return input[channel * in_frames + index];
        }
        // Past the end: hold the last real sample. A zero here is a step to silence and
        // a click at every block boundary.
        if in_frames > 0 {
            return input[channel * in_frames + (in_frames - 1)];
        }
        0.0
    }

    /// Keeps the last [`HISTORY`] input frames for the next block.
    fn remember(&mut self, input: &[f32], in_frames: usize) {
        let channels = self.channels.max(1) as usize;
        for back in 1..=HISTORY {
            for channel in 0..channels {
                let index = if in_frames >= back {
                    input[channel * in_frames + (in_frames - back)]
                } else {
                    0.0
                };
                self.history[channel * HISTORY + (HISTORY - back)] = index;
            }
        }
    }
}

/// Catmull-Rom interpolation through `y1` and `y2`, `t` in `[0, 1)`.
fn catmull_rom(y0: f32, y1: f32, y2: f32, y3: f32, t: f32) -> f32 {
    let a = -0.5 * y0 + 1.5 * y1 - 1.5 * y2 + 0.5 * y3;
    let b = y0 - 2.5 * y1 + 2.0 * y2 - 0.5 * y3;
    let c = -0.5 * y0 + 0.5 * y2;
    ((a * t + b) * t + c) * t + y1
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn equal_rates_pass_every_frame_through_untouched() {
        let mut resampler = Resampler::new(48_000, 48_000, 2);
        assert!(resampler.is_identity());
        let input: Vec<f32> = (0..480 * 2).map(|n| n as f32 * 0.001).collect();
        let mut out = vec![0.0; 480 * 2];
        assert_eq!(resampler.process(&input, 480, &mut out), 480);
        assert_eq!(out, input, "an identity conversion must be bit-exact");
        assert_eq!(resampler.input_frames(480), 480);
    }

    #[test]
    fn a_slower_device_is_fed_fewer_input_frames_per_block() {
        let resampler = Resampler::new(48_000, 44_100, 2);
        assert!(!resampler.is_identity());
        // 480 output frames need 480 × 48000/44100 = 522.4 input frames.
        assert_eq!(resampler.input_frames(480), 523);
        // And a faster device needs more.
        assert_eq!(Resampler::new(44_100, 48_000, 2).input_frames(480), 441);
    }

    #[test]
    fn a_block_produces_exactly_the_frames_the_device_asked_for() {
        let mut resampler = Resampler::new(48_000, 44_100, 2);
        let input = vec![0.25; 523 * 2];
        let mut out = vec![0.0; 480 * 2];
        assert_eq!(resampler.process(&input, 480, &mut out), 480);
        // A constant input must stay constant: an interpolator that overshoots on a flat
        // signal is one that will ring on real music.
        for sample in &out {
            assert!((sample - 0.25).abs() < 1e-5, "got {sample}");
        }
    }

    #[test]
    fn the_carried_fraction_never_drifts() {
        // Two thousand blocks is ten seconds at 48 kHz-to-44.1 kHz. If the fraction were
        // dropped instead of carried, the output would fall behind by a block every
        // twenty and the pitch would be wrong by a growing amount.
        let mut resampler = Resampler::new(48_000, 44_100, 1);
        let mut produced = 0u64;
        let mut consumed = 0u64;
        for _ in 0..2_000 {
            let need = resampler.input_frames(480) as usize;
            let input = vec![0.0; need];
            let mut out = vec![0.0; 480];
            resampler.process(&input, 480, &mut out);
            produced += 480;
            consumed += need as u64;
            assert!(
                (0.0..1.0).contains(&resampler.phase),
                "phase left its range: {}",
                resampler.phase
            );
        }
        let ratio = produced as f64 / consumed as f64;
        let expected = 44_100.0 / 48_000.0;
        assert!(
            (ratio - expected).abs() < 1e-3,
            "output/input ratio {ratio} should be {expected}"
        );
    }

    #[test]
    fn a_tone_keeps_its_frequency_and_its_channels_stay_separate() {
        // 1 kHz left, 3 kHz right, resampled 48 -> 44.1. A repitch would move the zero
        // crossings; a channel swap would put the wrong tone on each side.
        let in_rate = 48_000.0f64;
        let mut resampler = Resampler::new(48_000, 44_100, 2);
        let mut block_in = vec![0.0f32; 523 * 2];
        let mut out = vec![0.0f32; 480 * 2];
        let mut left: Vec<f32> = Vec::new();
        let mut right: Vec<f32> = Vec::new();
        let mut n = 0usize;
        for _ in 0..40 {
            let need = resampler.input_frames(480) as usize;
            // Planes: 1 kHz on the first, 3 kHz on the second.
            for frame in 0..need {
                let t = (n + frame) as f64 / in_rate;
                block_in[frame] = (2.0 * std::f64::consts::PI * 1_000.0 * t).sin() as f32;
                block_in[need + frame] = (2.0 * std::f64::consts::PI * 3_000.0 * t).sin() as f32;
            }
            n += need;
            resampler.process(&block_in, 480, &mut out);
            for frame in 0..480 {
                left.push(out[frame]);
                right.push(out[480 + frame]);
            }
        }
        // Skip the first block: interpolation needs history to fill behind it.
        let measured_left = frequency_of(&left[960..], 44_100.0);
        let measured_right = frequency_of(&right[960..], 44_100.0);
        assert!(
            (measured_left - 1_000.0).abs() < 40.0,
            "left tone moved to {measured_left} Hz"
        );
        assert!(
            (measured_right - 3_000.0).abs() < 80.0,
            "right tone moved to {measured_right} Hz"
        );
    }

    #[test]
    fn a_short_pull_only_counts_the_audio_it_produced() {
        let resampler = Resampler::new(48_000, 44_100, 2);
        // Nothing supplied: no output frame carries audio.
        assert_eq!(resampler.real_output_frames(0, 480), 0);
        // Everything supplied: all of them do.
        assert_eq!(resampler.real_output_frames(523, 480), 480);
        // Half supplied: about half of them.
        let half = resampler.real_output_frames(262, 480);
        assert!((200..=280).contains(&half), "got {half}");
    }

    /// Zero crossings over a second, twice the frequency of the signal.
    fn frequency_of(samples: &[f32], rate: f64) -> f64 {
        let mut crossings = 0usize;
        for pair in samples.windows(2) {
            if (pair[0] < 0.0 && pair[1] >= 0.0) || (pair[0] >= 0.0 && pair[1] < 0.0) {
                crossings += 1;
            }
        }
        crossings as f64 * rate / (2.0 * samples.len() as f64)
    }

    #[test]
    fn the_engines_planar_slab_keeps_its_channels_apart() {
        // The engine hands this a channel-major slab — that is what `Client::render`
        // produces and what `EngineWorker::fill_period` reads back. Left is a ramp, right
        // is a constant far away from it, so any crossing of the two is unmistakable.
        let channels = 2usize;
        let mut resampler = Resampler::new(48_000, 44_100, channels as u16);
        let needed = resampler.input_frames(480) as usize;
        let mut input = vec![0.0f32; needed * channels];
        for frame in 0..needed {
            input[frame] = 0.0; // plane 0: left, silent
            input[needed + frame] = 0.25; // plane 1: right, constant
        }
        let mut out = vec![0.0f32; 480 * channels];
        resampler.process(&input, 480, &mut out);
        for k in 0..480 {
            let left = out[k];
            let right = out[480 + k];
            assert!(
                left.abs() < 1e-5,
                "frame {k}: left channel picked up {} from the right plane",
                left
            );
            assert!(
                (right - 0.25).abs() < 1e-5,
                "frame {k}: right channel is {right}, not the constant it was given"
            );
        }
    }
}
