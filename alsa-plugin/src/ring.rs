//! The buffer between the app's writes and the device's clock.
//!
//! One producer, one consumer, no lock on either side. The producer is whatever
//! thread the app writes PCM on — under libasound that is the app's own audio
//! thread, holding libasound's mutex, which is why nothing here may block or take a
//! lock the engine could also want. The consumer is the engine's writer thread.
//!
//! Layout: interleaved `f32` at the device's channel count, because that is the
//! order the app writes and the order the device wants; the one conversion, from
//! the app's sample format to float, happens on the producer side, in
//! [`crate::pcm`].
//!
//! Why a ring at all rather than writing straight through: the app's write must
//! return in the time a real sound card would take, not in the time an AAudio write
//! takes. The ring is what a sound card's buffer is, and the writer thread is what
//! the card's engine is.
//!
//! Ordering: `head` is published with `Release` after the samples are written and
//! read with `Acquire` before they are read; `tail` likewise. The two regions —
//! `[tail, head)` for the consumer and `[head, tail + capacity)` for the producer —
//! never overlap, which is what makes the raw sample access sound without a lock.

use core::cell::UnsafeCell;
use core::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

/// Starvation bookkeeping: how often the ring had less than the engine asked for.
///
/// This is the one measurement that distinguishes "the plugin crackles" from "the
/// device crackles": when the ring is empty the engine plays zeros, and a listener
/// hears that as crackle or dropouts with no error anywhere — libasound is satisfied,
/// because the read position keeps advancing. Counted on the audio thread (two atomic
/// adds, no formatting, no locks) and reported from the write path.
static STARVATIONS: AtomicU64 = AtomicU64::new(0);
static STARVED_FRAMES: AtomicU64 = AtomicU64::new(0);
static LOW_WATER_FRAMES: AtomicU64 = AtomicU64::new(u64::MAX);
/// When the first and last starvation happened, in nanoseconds on the same clock the
/// plugin uses elsewhere. A total says how much silence there was; when it happened says
/// whether it is a startup transient or a steady defect, and the two have nothing in
/// common — the first is the device filling its buffer, the second is the writer failing.
static FIRST_STARVATION_NS: AtomicU64 = AtomicU64::new(0);
static LAST_STARVATION_NS: AtomicU64 = AtomicU64::new(0);
static OPENED_NS: AtomicU64 = AtomicU64::new(0);

/// A monotonic clock in nanoseconds, for placing events relative to each other. Only the
/// differences matter, so the epoch is irrelevant.
fn clock_ns() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0)
}

/// Records that the ring came up `frames` short of what was asked for.
pub(crate) fn note_starvation(frames: u64) {
    STARVATIONS.fetch_add(1, Ordering::Relaxed);
    STARVED_FRAMES.fetch_add(frames, Ordering::Relaxed);
    let now = clock_ns();
    let _ = FIRST_STARVATION_NS.compare_exchange(0, now, Ordering::Relaxed, Ordering::Relaxed);
    LAST_STARVATION_NS.store(now, Ordering::Relaxed);
}

/// Marks the moment a stream began, so a starvation can be placed relative to it.
pub(crate) fn note_opened() {
    OPENED_NS.store(clock_ns(), Ordering::Relaxed);
    FIRST_STARVATION_NS.store(0, Ordering::Relaxed);
    LAST_STARVATION_NS.store(0, Ordering::Relaxed);
}

/// How long after the stream began the first and last starvation happened, in
/// milliseconds, or `None` if the ring never starved.
pub(crate) fn starvation_span_ms() -> Option<(u64, u64)> {
    let opened = OPENED_NS.load(Ordering::Relaxed);
    let first = FIRST_STARVATION_NS.load(Ordering::Relaxed);
    let last = LAST_STARVATION_NS.load(Ordering::Relaxed);
    if opened == 0 || first == 0 {
        return None;
    }
    Some((
        first.saturating_sub(opened) / 1_000_000,
        last.saturating_sub(opened) / 1_000_000,
    ))
}

/// Records the frames left in the ring after a pull, keeping the low-water mark so a
/// report can say how close it came to running dry even when it never did.
pub(crate) fn note_pull_level(frames: usize) {
    let frames = frames as u64;
    let mut low = LOW_WATER_FRAMES.load(Ordering::Relaxed);
    while frames < low {
        match LOW_WATER_FRAMES.compare_exchange_weak(
            low,
            frames,
            Ordering::Relaxed,
            Ordering::Relaxed,
        ) {
            Ok(_) => break,
            Err(current) => low = current,
        }
    }
}

/// Takes the counters and restarts the window. `None` when nothing has happened since
/// the last call.
pub(crate) fn take_starvation_report() -> Option<(u64, u64, u64)> {
    let count = STARVATIONS.swap(0, Ordering::Relaxed);
    let frames = STARVED_FRAMES.swap(0, Ordering::Relaxed);
    let low = LOW_WATER_FRAMES.swap(u64::MAX, Ordering::Relaxed);
    if count == 0 && low == u64::MAX {
        return None;
    }
    Some((count, frames, if low == u64::MAX { 0 } else { low }))
}

/// An interleaved `f32` ring, sized in frames.
pub(crate) struct Ring {
    samples: UnsafeCell<Vec<f32>>,
    capacity: usize,
    channels: usize,
    /// Sample index the producer will write next. Never wraps.
    head: AtomicUsize,
    /// Sample index the consumer will read next. Never wraps.
    tail: AtomicUsize,
    /// Frames the consumer has taken, for diagnostics only.
    pulled: AtomicUsize,
}

// SAFETY: the two halves of the buffer are disjoint by construction (see the
// module note), the indices are atomic, and each side is only ever driven by one
// thread — libasound calls `transfer` under its own mutex, which serialises every
// producer for a given PCM.
unsafe impl Send for Ring {}
unsafe impl Sync for Ring {}

impl Ring {
    /// A ring holding `frames` frames of `channels` samples each.
    pub(crate) fn new(frames: usize, channels: usize) -> Self {
        let channels = channels.max(1);
        let capacity = frames.max(1) * channels;
        Self {
            samples: UnsafeCell::new(vec![0.0; capacity]),
            capacity,
            channels,
            head: AtomicUsize::new(0),
            tail: AtomicUsize::new(0),
            pulled: AtomicUsize::new(0),
        }
    }

    /// Frames the ring holds in total. Diagnostics.
    pub(crate) fn capacity_frames(&self) -> usize {
        self.capacity / self.channels
    }

    fn used_samples(&self) -> usize {
        self.head
            .load(Ordering::Acquire)
            .wrapping_sub(self.tail.load(Ordering::Acquire))
    }

    /// Frames waiting to be played.
    pub(crate) fn available_frames(&self) -> usize {
        self.used_samples() / self.channels
    }

    /// Frames that would fit.
    pub(crate) fn free_frames(&self) -> usize {
        (self.capacity - self.used_samples()) / self.channels
    }

    /// Empties the ring. Only valid while neither side is in a call.
    pub(crate) fn clear(&self) {
        self.tail
            .store(self.head.load(Ordering::Acquire), Ordering::Release);
    }

    /// Copies as much of `src` as fits, in whole frames. Returns the frames taken,
    /// which is 0 when the ring is full and `src.len() / channels` when it is not.
    pub(crate) fn push(&self, src: &[f32]) -> usize {
        let channels = self.channels;
        let frames = (src.len() / channels).min(self.free_frames());
        if frames == 0 {
            return 0;
        }
        let samples = frames * channels;
        let head = self.head.load(Ordering::Relaxed);
        // SAFETY: the region written here is `[head, head + samples)`, which is
        // inside the producer's half — the consumer only ever reads up to `head`.
        let buffer = unsafe { &mut *self.samples.get() };
        let start = head % self.capacity;
        let first = samples.min(self.capacity - start);
        buffer[start..start + first].copy_from_slice(&src[..first]);
        if first < samples {
            buffer[..samples - first].copy_from_slice(&src[first..samples]);
        }
        // The samples are in place before the index that publishes them.
        self.head.store(head + samples, Ordering::Release);
        frames
    }

    /// Pushes `src` in full, waiting for the ring to make room.
    ///
    /// This is the difference between a plugin and a busy-wait. libasound calls
    /// `transfer` with however much the app asked to write, and a short return is
    /// back-pressure that libasound reports straight back to the app — which is
    /// correct only if the app then *waits*. A game writing a 250 ms chunk into a
    /// 20 ms ring would otherwise spin at full CPU and make progress at whatever rate
    /// its retry loop managed, which measured as 1200 frames/s instead of 48000.
    ///
    /// So the producer parks here, exactly as it would on a sound card, and the
    /// consumer's own progress is what releases it. The wait is a sleep rather than a
    /// condvar on purpose: the consumer is the engine's writer thread, whose contract
    /// is no locks on its path, and a 1 ms park is far below the ring's own depth.
    ///
    /// Returns the frames taken. Less than asked only when `deadline` passes, which
    /// means the device has stopped consuming — a state the app should see rather than
    /// wait out forever.
    pub(crate) fn push_blocking(&self, src: &[f32], deadline: std::time::Instant) -> usize {
        let channels = self.channels;
        let wanted = src.len() / channels;
        let mut taken = 0;
        let mut spins = 0u32;
        let mut last_report = std::time::Instant::now();
        let mut last_pulled = self.pulled_frames();
        while taken < wanted {
            let pushed = self.push(&src[taken * channels..]);
            taken += pushed;
            if taken >= wanted {
                break;
            }
            if pushed == 0 {
                if std::time::Instant::now() >= deadline {
                    break;
                }
                spins += 1;
                if last_report.elapsed() >= std::time::Duration::from_secs(1) {
                    let pulled = self.pulled_frames();
                    crate::log::trace!(
                        "push_blocking: waiting, {taken}/{wanted} taken, capacity {} frames, \
                         available {}, consumer took {} frames since the last report ({spins} waits)",
                        self.capacity_frames(),
                        self.available_frames(),
                        pulled - last_pulled
                    );
                    last_report = std::time::Instant::now();
                    last_pulled = pulled;
                    spins = 0;
                }
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
        }
        taken
    }

    /// Reads up to `frames` frames into `out`, which is **channel-major**:
    /// `frames` samples per channel, one plane per channel, as the engine's client
    /// is handed its block. Anything past the end of `out`'s planes is left alone,
    /// and a short read leaves the rest of `out` untouched — the engine zero-fills
    /// its block before calling in, so an underrun is silence rather than history.
    ///
    /// Returns the frames read.
    pub(crate) fn pull_planar(&self, out: &mut [f32], frames: usize, out_channels: usize) -> usize {
        self.pull_planar_at(out, frames, out_channels, 0)
    }

    /// As [`Ring::pull_planar`], writing this pull's frames from `from` onwards.
    ///
    /// The engine's client is asked for a whole block, and a block can arrive in pieces:
    /// the app may have written half of it. Pulling the rest into the same block, after the
    /// frames already taken, is what lets the device thread wait for the remainder instead
    /// of splicing silence over it.
    pub(crate) fn pull_planar_at(
        &self,
        out: &mut [f32],
        frames: usize,
        out_channels: usize,
        from: usize,
    ) -> usize {
        let channels = self.channels;
        let planes = out_channels.min(channels).max(1);
        // One plane's worth of samples in `out`. This is the stride between channels,
        // and it is the buffer's frame count — not the number of frames being read,
        // which may be shorter.
        let stride = out.len() / out_channels.max(1);
        if from >= stride {
            return 0;
        }
        let available = self.available_frames();
        let frames = frames.min(available).min(stride - from);
        if frames == 0 {
            return 0;
        }
        let tail = self.tail.load(Ordering::Relaxed);
        // SAFETY: the region read here is `[tail, tail + frames * channels)`, which
        // ends at or before `head`, so it is inside the consumer's half.
        let buffer = unsafe { &*self.samples.get() };
        for frame in 0..frames {
            let base = (tail + frame * channels) % self.capacity;
            for channel in 0..planes {
                let index = (base + channel) % self.capacity;
                out[channel * stride + from + frame] = buffer[index];
            }
        }
        self.tail.store(tail + frames * channels, Ordering::Release);
        self.pulled.fetch_add(frames, Ordering::Relaxed);
        frames
    }

    /// Frames the consumer has taken since the ring was made. Diagnostics.
    pub(crate) fn pulled_frames(&self) -> usize {
        self.pulled.load(Ordering::Relaxed)
    }
}

#[cfg(test)]
mod tests {
    use super::Ring;

    #[test]
    fn what_goes_in_comes_out_in_order() {
        let ring = Ring::new(16, 2);
        let input: Vec<f32> = (0..8).map(|value| value as f32).collect();
        assert_eq!(ring.push(&input), 4, "eight samples is four stereo frames");
        assert_eq!(ring.available_frames(), 4);

        let mut out = vec![f32::NAN; 4 * 2];
        assert_eq!(ring.pull_planar(&mut out, 4, 2), 4);
        assert_eq!(out[0], 0.0, "left plane, first frame");
        assert_eq!(out[1], 2.0, "left plane, second frame");
        assert_eq!(out[4], 1.0, "right plane, first frame");
        assert_eq!(out[5], 3.0, "right plane, second frame");
        assert_eq!(ring.available_frames(), 0);
    }

    #[test]
    fn a_push_that_would_overflow_keeps_the_whole_frames() {
        let ring = Ring::new(2, 2);
        let input: Vec<f32> = (0..9).map(|value| value as f32).collect();
        assert_eq!(ring.push(&input), 2, "only two frames fit");
        assert_eq!(ring.free_frames(), 0);
        assert_eq!(ring.push(&input), 0, "a full ring takes nothing");
    }

    #[test]
    fn a_pull_past_the_end_is_short_and_leaves_the_rest_alone() {
        let ring = Ring::new(8, 2);
        assert_eq!(ring.push(&[1.0, 2.0, 3.0, 4.0]), 2, "two stereo frames");
        let mut out = vec![f32::NAN; 4 * 2];
        assert_eq!(ring.pull_planar(&mut out, 4, 2), 2, "two frames were there");
        assert_eq!((out[0], out[1]), (1.0, 3.0), "left plane, two frames");
        assert_eq!((out[4], out[5]), (2.0, 4.0), "right plane, two frames");
        assert!(
            out[2].is_nan() && out[3].is_nan() && out[6].is_nan() && out[7].is_nan(),
            "the frames past the short read are left untouched: {out:?}"
        );
    }

    #[test]
    fn the_indices_wrap_rather_than_growing_the_buffer() {
        let ring = Ring::new(4, 1);
        // Four frames at a time, five pushes: the second half of the last push has
        // to land at the start of the buffer again.
        for round in 0..5 {
            let input: Vec<f32> = (0..4).map(|value| (value + round * 4) as f32).collect();
            assert_eq!(ring.push(&input), 4);
            let mut out = vec![0.0; 4];
            assert_eq!(ring.pull_planar(&mut out, 4, 1), 4);
            assert_eq!(out, input, "round {round} kept its order across the wrap");
        }
    }

    #[test]
    fn a_partial_frame_is_never_accepted() {
        let ring = Ring::new(4, 2);
        assert_eq!(ring.push(&[1.0, 2.0, 3.0]), 1, "three samples is one frame");
        assert_eq!(ring.available_frames(), 1);
    }

    #[test]
    fn more_channels_than_the_block_are_dropped_not_mixed() {
        let ring = Ring::new(4, 2);
        ring.push(&[0.5, 0.25, 0.75, 0.125]);
        let mut out = vec![0.0; 2];
        assert_eq!(ring.pull_planar(&mut out, 2, 1), 2, "one plane asked for");
        assert_eq!(out, vec![0.5, 0.75], "left channel only");
    }

    #[test]
    fn a_blocking_push_waits_for_room_rather_than_dropping_the_rest() {
        use std::sync::Arc;
        use std::time::{Duration, Instant};

        let ring = Arc::new(Ring::new(4, 1));
        let producer = {
            let ring = ring.clone();
            std::thread::spawn(move || {
                // Eight frames into a four-frame ring: half of it has to wait for the
                // consumer, and all eight must survive in order.
                let input: Vec<f32> = (0..8).map(|value| value as f32).collect();
                ring.push_blocking(&input, Instant::now() + Duration::from_secs(2))
            })
        };

        // Drain as a device would, in two halves, so the producer's second push has to
        // wait for the first to be read.
        let mut heard = Vec::new();
        for _ in 0..2 {
            let mut out = vec![0.0f32; 4];
            loop {
                let read = ring.pull_planar(&mut out, 4, 1);
                if read > 0 {
                    heard.extend_from_slice(&out[..read]);
                    break;
                }
                std::thread::sleep(Duration::from_millis(1));
            }
            std::thread::sleep(Duration::from_millis(5));
        }

        assert_eq!(producer.join().unwrap(), 8, "every frame was accepted");
        assert_eq!(heard, (0..8).map(|value| value as f32).collect::<Vec<_>>());
    }

    #[test]
    fn a_blocking_push_gives_up_when_the_device_stops_taking_audio() {
        use std::time::{Duration, Instant};

        let ring = Ring::new(4, 1);
        // A full ring whose consumer never runs: the push must return what it has
        // rather than hang the app's audio thread forever.
        assert_eq!(ring.push(&[1.0; 4]), 4);
        let started = Instant::now();
        let taken = ring.push_blocking(&[2.0; 4], started + Duration::from_millis(20));
        assert_eq!(taken, 0, "there was never room");
        assert!(
            started.elapsed() >= Duration::from_millis(15),
            "and it did wait"
        );
    }

    #[test]
    fn clearing_drops_what_was_queued() {
        let ring = Ring::new(8, 2);
        ring.push(&[1.0; 8]);
        assert_eq!(ring.available_frames(), 4);
        ring.clear();
        assert_eq!(ring.available_frames(), 0);
        assert_eq!(ring.free_frames(), 8);
    }
}
