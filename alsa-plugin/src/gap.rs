//! Looking for seams in what the app hands us.
//!
//! Every counter in this plugin watches the drain side: how deep the ring is, whether
//! the device reported an xrun. A stream can crackle with all of them healthy, because
//! the damage can be on the *push* side — audio that was spliced rather than dropped.
//!
//! There are two shapes of that, and both are visible in the samples themselves:
//!
//! * A **gap**: a run of exact zeros between two non-zero frames, in audio that was
//!   otherwise continuous. Nothing in a music or sound-effect stream produces that;
//!   a caller padding a buffer it could not fill does.
//! * A **step**: two adjacent samples that differ by most of full scale. At 48 kHz a
//!   real transient spans several samples, so a step that large is a splice between
//!   two pieces of audio that never met.
//!
//! Both are counted, never logged per event: a summary at close, and one line the first
//! time the count is big enough to be the thing being investigated.

use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};

/// The shortest run of silence treated as a gap, in frames: 2 ms at 48 kHz.
///
/// Below this a zero run is ordinary audio — a percussive tail, a fade — and above it
/// nothing legitimate sits between two loud frames.
const MIN_GAP_FRAMES: u32 = 96;

/// A step this large between adjacent samples is a splice, not a transient.
const CLICK_STEP: f32 = 0.75;

/// Below this a sample counts as silence.
const SILENCE: f32 = 1.0e-6;

/// Counts seams in one stream. Fed the app's frames as they are handed over.
pub(crate) struct Seams {
    gaps: u32,
    /// Frames of silence in those gaps, so the summary can say how long they were: a
    /// hole one period long is a caller padding a buffer, and a hole several seconds
    /// long is the music between two tracks.
    gap_frames: u32,
    clicks: u32,
    /// Frames of the run currently being measured, if we are inside one.
    run: u32,
    /// The last non-silent sample of the first channel.
    prev: f32,
}

impl Seams {
    pub(crate) fn new() -> Self {
        Self {
            gaps: 0,
            gap_frames: 0,
            clicks: 0,
            run: 0,
            prev: 0.0,
        }
    }

    /// Looks at `frames` frames of interleaved audio.
    pub(crate) fn observe(&mut self, samples: &[f32], channels: usize) {
        if channels == 0 {
            return;
        }
        for frame in samples.chunks_exact(channels) {
            let silent = frame.iter().all(|sample| sample.abs() < SILENCE);
            if silent {
                if self.run > 0 || self.prev.abs() >= SILENCE {
                    self.run = self.run.saturating_add(1);
                }
                continue;
            }
            if self.run > 0 {
                // A run only counts if audio continues on the far side of it. Silence
                // that runs to the end of the stream is the app finishing, not a hole.
                if self.run >= MIN_GAP_FRAMES {
                    self.gaps = self.gaps.saturating_add(1);
                    self.gap_frames = self.gap_frames.saturating_add(self.run);
                }
                self.run = 0;
            }
            let sample = frame[0];
            if self.prev.abs() >= SILENCE && (sample - self.prev).abs() > CLICK_STEP {
                self.clicks = self.clicks.saturating_add(1);
            }
            self.prev = sample;
        }
    }
}

/// Whether the first seam worth reporting has already been logged, so a session
/// produces one line rather than a stream of them.
static ANNOUNCED: AtomicBool = AtomicBool::new(false);
/// How many seams to see before saying anything. One splice can be an app starting a
/// sound; a run of them is the crackle.
const ANNOUNCE_AFTER: u32 = 8;

impl Seams {
    /// Drains the counts into `gaps`/`clicks` and, exactly once per process, says so.
    ///
    /// Called on the app's own thread inside a write it was going to do anyway: two
    /// comparisons per sample and, at most once, a line of output. The detector's own
    /// state survives, because a seam can straddle two of the app's writes and the
    /// silence between them is the thing being measured.
    pub(crate) fn note(&mut self, gaps: &AtomicU32, gap_frames: &AtomicU32, clicks: &AtomicU32) {
        let (own_gaps, own_frames, own_clicks) = (self.gaps, self.gap_frames, self.clicks);
        if own_gaps == 0 && own_clicks == 0 {
            return;
        }
        self.gaps = 0;
        self.gap_frames = 0;
        self.clicks = 0;
        gap_frames.fetch_add(own_frames, Ordering::Relaxed);
        let total_gaps = gaps.fetch_add(own_gaps, Ordering::Relaxed) + own_gaps;
        let total_clicks = clicks.fetch_add(own_clicks, Ordering::Relaxed) + own_clicks;
        if total_gaps + total_clicks < ANNOUNCE_AFTER || ANNOUNCED.swap(true, Ordering::AcqRel) {
            return;
        }
        crate::log::trace!(
            "seams: after {total_gaps} gaps and {total_clicks} steps in the audio handed \
             over — something upstream is splicing, not dropping"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Counts what a sequence of writes produced, the way the plugin does.
    fn count(data: &[f32], channels: usize) -> (u32, u32) {
        let mut seams = Seams::new();
        let gaps = AtomicU32::new(0);
        let gap_frames = AtomicU32::new(0);
        let clicks = AtomicU32::new(0);
        seams.observe(data, channels);
        seams.note(&gaps, &gap_frames, &clicks);
        (gaps.load(Ordering::Relaxed), clicks.load(Ordering::Relaxed))
    }

    /// A quiet sine, the way ordinary audio looks to this detector.
    fn sine(frames: usize) -> Vec<f32> {
        (0..frames)
            .flat_map(|i| {
                let value = (i as f32 * 0.05).sin() * 0.3;
                [value, value]
            })
            .collect()
    }

    #[test]
    fn continuous_audio_has_no_seams() {
        assert_eq!(count(&sine(4800), 2), (0, 0));
    }

    #[test]
    fn a_run_of_zeros_between_two_loud_parts_is_a_gap() {
        let mut data = sine(480);
        data.extend(std::iter::repeat_n(0.0f32, 480 * 2));
        data.extend(sine(480));

        assert_eq!(count(&data, 2), (1, 0));
    }

    #[test]
    fn a_short_zero_run_is_ordinary_audio() {
        let mut data = sine(480);
        data.extend(std::iter::repeat_n(0.0f32, 8 * 2));
        data.extend(sine(480));

        assert_eq!(count(&data, 2), (0, 0));
    }

    #[test]
    fn silence_that_runs_to_the_end_is_not_a_gap() {
        let mut data = sine(480);
        data.extend(std::iter::repeat_n(0.0f32, 480 * 2));

        assert_eq!(count(&data, 2), (0, 0));
    }

    #[test]
    fn a_full_scale_jump_between_two_samples_is_a_step() {
        let mut data = vec![0.5f32; 2];
        data.extend([-0.5f32, -0.5]);

        assert_eq!(count(&data, 2), (0, 1));
    }

    #[test]
    fn a_gap_is_measured_across_calls() {
        // The app writes in periods, so a seam can straddle two of them — which is
        // exactly when a caller padded a buffer it could not fill.
        let mut seams = Seams::new();
        let gaps = AtomicU32::new(0);
        let gap_frames = AtomicU32::new(0);
        let clicks = AtomicU32::new(0);
        seams.observe(&sine(480), 2);
        seams.observe(&vec![0.0f32; 240 * 2], 2);
        seams.observe(&vec![0.0f32; 240 * 2], 2);
        seams.note(&gaps, &gap_frames, &clicks);
        assert_eq!(gaps.load(Ordering::Relaxed), 0, "still inside the run");
        seams.observe(&sine(480), 2);
        seams.note(&gaps, &gap_frames, &clicks);
        assert_eq!(gaps.load(Ordering::Relaxed), 1);
    }
}
