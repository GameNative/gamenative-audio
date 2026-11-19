//! The device side: what the engine needs from an audio device, and what a
//! device needs from the engine.
//!
//! The direction of control is the opposite of a pull engine's: [`Sink::fill`] is
//! called by whoever is waiting on the device, and that wait *is* the clock.
//! Everything a client sees — block size, block-aligned positions, latencies — is
//! derived from it.
//!
//! Two shapes satisfy this, and both are in `backend/`: a device that asks for a
//! period through a data callback (`aaudio`), where the HAL's thread is the clock;
//! and a device driven by its own data callback (`callback`), where
//! the write is. Which one a build uses is a deployment decision, not a contract — the
//! engine above them does not know the difference.
//!
//! Ownership is deliberate: a [`Backend`] takes ownership of its [`Sink`], and the
//! sink reaches the client through the engine's slot rather than owning it — the
//! client has to outlive any one device, because a device can be rebuilt underneath
//! it. A real-time device thread therefore reaches the client through the device
//! that called it: no lock, no shared mutable state, and one device closed before
//! another is opened. `Backend::close` is the only thing that stops the sink being
//! called, and it only runs once the device has stopped calling `fill`.

use crate::types::{Error, Events, Granted, StreamConfig, Timestamp};

#[cfg(target_os = "android")]
pub mod callback;
pub mod mock;

/// The engine, seen from the device thread.
///
/// `fill` runs on an audio thread — the HAL's, or one this crate owns — and must not
/// allocate, must not block, and must not take a lock a control path could hold. Both
/// implementations in this crate are audited for that, which is why the writer thread
/// never touches the sink from anywhere but its own loop.
pub trait Sink: Send {
    /// Sizes the client's block to what the device granted. Called once per open,
    /// from the opening thread, before the device starts — never on the real-time
    /// path, which is what makes the allocation inside it legitimate.
    fn configure(&mut self, granted: &Granted);

    /// Tells the sink that a device is starting, so it drops any half-consumed
    /// block and takes the next block from the timeline's resume point: zero after
    /// a fresh start, and where the client left off when a device is rebuilt.
    /// Called before the device runs.
    fn reset(&mut self);

    /// Fill `interleaved`: `frames` frames of `device_channels` samples, in the
    /// device's format. `frames` is the device's period, which is not necessarily
    /// the client's block size — spreading one block over the periods it covers is
    /// this side's job, not the client's.
    fn fill(&mut self, interleaved: &mut [f32], frames: u32);
}

/// An audio device the engine can drive.
pub trait Backend: Send {
    /// Opens the device and hands it the sink. Returns what was *granted*, read
    /// back from the device, which may differ from what was asked for.
    fn open(&mut self, config: &StreamConfig, sink: Box<dyn Sink>) -> Result<Granted, Error>;

    /// Begins consuming. Between this and the first `fill` the device may hold a
    /// period of its own silence, which is why the engine counts what the device
    /// does with its own counters rather than pretending there is no ramp.
    fn start(&mut self) -> Result<(), Error>;

    /// Stops consuming without releasing the device. No period may reach the sink
    /// after this returns, and any audio the device still holds is discarded so it
    /// cannot play on resume.
    fn pause(&mut self) -> Result<(), Error>;

    /// Continues from a pause.
    fn resume(&mut self) -> Result<(), Error>;

    /// Stops the device. Weaker than [`Backend::pause`] only in intent — the
    /// device is idle until the next `start`.
    fn stop(&mut self) -> Result<(), Error>;

    /// Releases the device. After this returns, `fill` will never be called again,
    /// which is what makes it safe for the engine to drop the sink.
    fn close(&mut self);

    /// Sequence breaks the device itself counted since it was opened.
    fn device_xruns(&mut self) -> u32;

    /// The device's own view of where playback is, when it can produce one.
    fn timestamp(&mut self) -> Option<Timestamp>;

    /// Frames the device has presented since it opened, from the device's own clock,
    /// carried across a rebuild rather than restarted by one.
    ///
    /// `None` when the backend cannot tell, which is not the same as zero. An ALSA
    /// client's read position has to be this and not the frames handed over: the
    /// difference between the two is the whole device buffer, and a position that
    /// leads the truth by more than the client's own buffer cannot be represented in
    /// libasound's arithmetic.
    fn played_frames(&self) -> Option<u64> {
        None
    }

    /// Frames the device still holds that have not been played.
    fn queued_frames(&mut self) -> u32;

    /// Events only the device can know about — a disconnect it saw before anyone
    /// polled. Drained by the engine and merged with its own.
    fn take_events(&mut self) -> Events;

    /// The device's current sample rate, for recomputing latency after a change.
    fn sample_rate(&self) -> i32;

    /// Whether audio is currently reaching a device.
    ///
    /// True for every backend that has one open. A backend that rebuilds its device
    /// underneath — which is what happens when Android moves output to headphones and
    /// back — reports false for the interval where nothing is playing, so a caller
    /// that must keep its own clock can drop rather than block on a sink nobody is
    /// draining.
    fn is_delivering(&self) -> bool {
        true
    }
}
