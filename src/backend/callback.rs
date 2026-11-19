//! The device: AAudio, driven by its **data callback**, the way PulseAudio's AAudio
//! sink drives it.
//!
//! `module-aaudio-sink.c` is the reference this file is modelled on, because it is the
//! implementation that is known to play on this hardware. Three properties of it are
//! load-bearing, and this file has all three:
//!
//! 1. **The device pulls.** A data callback is registered; the device decides when it
//!    needs audio and how much. Nothing here writes to the stream on a thread of its
//!    own, so there is no second clock competing with the app and no possibility of the
//!    writer and the device disagreeing about who is ahead.
//! 2. **The device buffer is tiny.** Two bursts: 192 frames (4 ms) on a 96-frame-burst
//!    device, 384 (8 ms) on a 192-frame one. The slack lives *above* the device, in the
//!    pipeline that feeds it, exactly as
//!    PulseAudio keeps its queue in its own memblocks rather than in AAudio. A large
//!    AAudio buffer is not free space: it is audio the client has already handed over
//!    and cannot take back, and a sink underneath libasound charges it against the
//!    buffer size the client was promised.
//! 3. **The callback does not render.** It hands the request to another thread and
//!    waits for it, which is what `pa_asyncmsgq_send` does. Rendering on AAudio's
//!    real-time thread means the audio pipeline's own lateness — a lock, a resampler, a
//!    ring that is briefly empty — lands inside the device's deadline.
//!
//! The one place this departs from the reference: PulseAudio renders *into* the
//! callback's buffer, while this file renders into a staging buffer and copies. The
//! reference can do that because `pa_asyncmsgq_send` cannot time out; a hand-off that
//! gives up (which a device deadline needs, because the alternative is a stalled
//! callback) would leave the render thread writing into a buffer AAudio had taken back.
//! Staging makes an abandoned hand-off harmless, at the cost of one memcpy per period.
//!
//! # Periods are sized by the device, not by the client
//!
//! The device decides how much it asks for, and on this hardware that is larger than the
//! client's block — 1 152 frames and up, from AudioFlinger. So the staging buffer is
//! sized from `AAudioStream_getBufferCapacityInFrames` rather than from the client's block,
//! `setFramesPerDataCallback` is not called at all, and a period larger than the pipeline
//! holds is clamped (supplied in part, silence for the remainder) rather than abandoned.
//! The distinction is invisible from above: a stream handed silence every period still
//! reports the right rate, an active track and zero underruns.

use core::cell::UnsafeCell;
use core::ffi::{c_int, c_void};
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicPtr, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use super::{Backend, Sink};
use crate::engine::{block_frames_for, plausible_burst};
use crate::supply::{bytes_per_sample, device_buffer_frames, grown_buffer_frames, supply_plan};
use crate::types::{Error, Events, Format, Granted, Sharing, StreamConfig, Timestamp};

// ---------------------------------------------------------------------------
// AAudio, as declared by the NDK
// ---------------------------------------------------------------------------

type AaudioStreamBuilder = c_void;
type AaudioStream = c_void;

const AAUDIO_OK: c_int = 0;
const AAUDIO_UNSPECIFIED: c_int = 0;

/// `AAUDIO_DIRECTION_OUTPUT`.
const AAUDIO_DIRECTION_OUTPUT: c_int = 0;

/// `aaudio_data_callback_result_t`.
const CALLBACK_CONTINUE: c_int = 0;
const CALLBACK_STOP: c_int = 1;

/// `AAUDIO_ERROR_DISCONNECTED`.
const AAUDIO_ERROR_DISCONNECTED: c_int = -899;
/// `AAUDIO_USAGE_GAME` from `aaudio_usage_t`, used by [`set_game_usage`].
const AAUDIO_USAGE_GAME: c_int = 14;

/// How long the render thread waits for a period before checking the stream's state.
///
/// Two of this device's own periods, so a healthy stream never reaches it — the wait ends
/// on the request — and a dead one is noticed within tens of milliseconds rather than
/// never.
const STATE_POLL_NS: i64 = 40_000_000;

/// Android's `CLOCK_MONOTONIC`.
const CLOCK_MONOTONIC: c_int = 1;

/// How long the device's thread waits for one period to be produced.
///
/// This is a deadline, not a policy: a callback that never returns is a stalled device,
/// and a period of silence is the lesser failure. Twenty milliseconds is two of this
/// device's own periods at 1 152 frames, so a render thread that is merely busy still
/// makes it, and one that is wedged gives up rather than taking the stream with it.
const RENDER_TIMEOUT_NS: i64 = 20_000_000;

type DataCallback = extern "C" fn(
    stream: *mut AaudioStream,
    user_data: *mut c_void,
    audio_data: *mut c_void,
    num_frames: c_int,
) -> c_int;

type ErrorCallback = extern "C" fn(stream: *mut AaudioStream, user_data: *mut c_void, error: c_int);

// `dlopen`/`dlsym`, for the one builder call that does not exist on every API level this
// crate supports. See `set_game_usage`.
#[link(name = "dl")]
extern "C" {
    fn dlopen(filename: *const std::os::raw::c_char, flag: c_int) -> *mut c_void;
    fn dlsym(handle: *mut c_void, symbol: *const std::os::raw::c_char) -> *mut c_void;
}

#[link(name = "aaudio")]
extern "C" {
    fn AAudio_createStreamBuilder(builder: *mut *mut AaudioStreamBuilder) -> c_int;
    fn AAudioStreamBuilder_setDeviceId(builder: *mut AaudioStreamBuilder, device_id: c_int);
    fn AAudioStreamBuilder_setDirection(builder: *mut AaudioStreamBuilder, direction: c_int);
    fn AAudioStreamBuilder_setSharingMode(builder: *mut AaudioStreamBuilder, mode: c_int);
    fn AAudioStreamBuilder_setPerformanceMode(builder: *mut AaudioStreamBuilder, mode: c_int);
    fn AAudioStreamBuilder_setFormat(builder: *mut AaudioStreamBuilder, format: c_int);
    fn AAudioStreamBuilder_setSampleRate(builder: *mut AaudioStreamBuilder, sample_rate: c_int);
    fn AAudioStreamBuilder_setChannelCount(builder: *mut AaudioStreamBuilder, channels: c_int);
    fn AAudioStreamBuilder_setBufferCapacityInFrames(
        builder: *mut AaudioStreamBuilder,
        frames: c_int,
    );
    fn AAudioStreamBuilder_setDataCallback(
        builder: *mut AaudioStreamBuilder,
        callback: Option<DataCallback>,
        user_data: *mut c_void,
    );
    fn AAudioStreamBuilder_setErrorCallback(
        builder: *mut AaudioStreamBuilder,
        callback: Option<ErrorCallback>,
        user_data: *mut c_void,
    );
    fn AAudioStreamBuilder_openStream(
        builder: *mut AaudioStreamBuilder,
        stream: *mut *mut AaudioStream,
        input_stream: *mut *mut AaudioStream,
    ) -> c_int;
    fn AAudioStreamBuilder_delete(builder: *mut AaudioStreamBuilder) -> c_int;

    fn AAudioStream_requestStart(stream: *mut AaudioStream) -> c_int;
    fn AAudioStream_getState(stream: *mut AaudioStream) -> c_int;
    fn AAudioStream_requestPause(stream: *mut AaudioStream) -> c_int;
    fn AAudioStream_requestFlush(stream: *mut AaudioStream) -> c_int;
    fn AAudioStream_requestStop(stream: *mut AaudioStream) -> c_int;
    fn AAudioStream_close(stream: *mut AaudioStream) -> c_int;
    fn AAudioStream_getSampleRate(stream: *mut AaudioStream) -> c_int;
    fn AAudioStream_getChannelCount(stream: *mut AaudioStream) -> c_int;
    fn AAudioStream_getFormat(stream: *mut AaudioStream) -> c_int;
    fn AAudioStream_getFramesPerBurst(stream: *mut AaudioStream) -> c_int;
    fn AAudioStream_getBufferSizeInFrames(stream: *mut AaudioStream) -> c_int;
    fn AAudioStream_getBufferCapacityInFrames(stream: *mut AaudioStream) -> c_int;
    fn AAudioStream_getPerformanceMode(stream: *mut AaudioStream) -> c_int;
    fn AAudioStream_getSharingMode(stream: *mut AaudioStream) -> c_int;
    fn AAudioStream_getDeviceId(stream: *mut AaudioStream) -> c_int;
    fn AAudioStream_getXRunCount(stream: *mut AaudioStream) -> c_int;
    fn AAudioStream_getTimestamp(
        stream: *mut AaudioStream,
        clock_id: c_int,
        frame_position: *mut i64,
        time_nanoseconds: *mut i64,
    ) -> c_int;
    fn AAudioStream_setBufferSizeInFrames(stream: *mut AaudioStream, frames: c_int) -> c_int;
}

/// Maps an AAudio result to the engine's error, keeping the code.
fn error_from(result: c_int) -> Error {
    match result {
        AAUDIO_ERROR_DISCONNECTED => Error::Disconnected,
        // AAUDIO_ERROR_ILLEGAL_ARGUMENT / UNIMPLEMENTED / INVALID_STATE.
        -898 | -890 | -895 => Error::Unsupported,
        _ => Error::Device(result),
    }
}

// ---------------------------------------------------------------------------
// The hand-off between the device's thread and ours
// ---------------------------------------------------------------------------

/// One request from the device's thread to the render thread.
///
/// `request` and `done` are counters rather than a queue because there is never more
/// than one period in flight: the device asks, waits, and takes the answer. A counter
/// that the render thread echoes back is enough to tell "my period is ready" from "an
/// older one is", and it needs no allocation on either side.
#[derive(Default)]
struct Handoff {
    /// Bumped by the device's thread for every period it wants.
    request: u64,
    /// Bumped by the render thread when it has filled the staging buffer for `request`.
    done: u64,
    /// Frames the current request asks for.
    frames: u32,
    /// Set when the render thread should leave: the stream is being closed.
    quit: bool,
}

/// State shared between AAudio's callback thread and ours.
///
/// The staging buffer is in an [`UnsafeCell`] because its ownership alternates: the
/// render thread writes it, the device's thread reads it, and the [`Handoff`] mutex is
/// what orders those two — the device's thread publishes a request before the render
/// thread touches it, and only reads after `done` echoes that request back. A lock
/// around the buffer itself would serialise the copy against a fill that is already
/// finished.
struct Shared {
    handoff: Mutex<Handoff>,
    cv: Condvar,
    /// Interleaved float, `capacity × device_channels`. Written by the render thread,
    /// read by the device's thread.
    staging: UnsafeCell<Vec<f32>>,
    /// Frames the staging buffer holds, which is what bounds one period.
    staging_frames: AtomicU32,
    device_channels: AtomicU32,
    /// The device's sample format, as `aaudio_format_t` numbers it.
    format: AtomicI32,
    sample_rate: AtomicI32,
    /// Set by the error path and by `close`, so the callback stops asking for audio a
    /// moment before the stream goes away.
    stopping: AtomicBool,
    /// The device is being rebuilt and nothing may be written to it.
    fault: AtomicI32,
    events: AtomicI32,
    last_error: AtomicI32,
    /// Periods the device asked for that this backend could not supply in time.
    short: AtomicU64,
    /// The same, in periods and in frames of silence inserted, plus what the most recent
    /// clamp asked for and held — the four numbers that turn "it sounds chopped" into a
    /// figure. Reported from the render thread; the audio thread only sets them.
    shorts: AtomicU64,
    /// Periods the render thread missed outright, which cost the whole period in silence
    /// rather than the tail of one, and the frames those periods asked for.
    timeouts: AtomicU64,
    long_asks: AtomicU64,
    silenced: AtomicU64,
    short_asked: AtomicU64,
    short_held: AtomicU64,
    short_pending: AtomicBool,
    short_reported_at: AtomicU64,
    /// Frames presented before the current stream was opened: a rebuild continues the
    /// clock instead of restarting it.
    presented_base: AtomicU64,
    /// The device's burst size, which changes with the route.
    burst: AtomicU32,
    /// Frames the device's ring holds on this route, for turning into a duration.
    buffer_frames: AtomicU32,
    /// The most the buffer may grow to: what the caller's budget leaves, and the route's own
    /// capacity. Set when the stream is described, because the growth rule runs per period.
    grow_ceiling: AtomicU32,
    /// When the device last asked for a period. A stream that stops asking while claiming
    /// to play is not playing, and this is the only evidence of it.
    last_request_ns: AtomicU64,
    /// The interval between the last two callbacks. The freeze detector is calibrated
    /// against it rather than against a constant, because the cadence is the route's.
    cadence_ns: AtomicU64,
    /// The last `AAudioStream_getXRunCount` seen, so "the device lost audio" is noticed
    /// once per loss rather than once per period.
    last_xruns: AtomicU32,
    /// The device's presented count and time at the last probe, so the rate the device is
    /// actually consuming audio can be measured against the rate it claims.
    ///
    /// This exists because "the audio sounds slow" has exactly one numeric meaning —
    /// fewer frames played per wall second than the stream's sample rate — and nothing
    /// else in this crate measures it. An ALSA client's position is a number this crate
    /// chooses, so it cannot answer the question.
    probe_at_ns: AtomicU64,
    probe_frames: AtomicU64,
    /// How long the render thread may take to answer one period.
    ///
    /// At least [`RENDER_TIMEOUT_NS`], and at least one device buffer's duration. The
    /// reference sink's callback waits for its render with no deadline at all, because the
    /// sink's own thread is the one doing the work and a late period is better than a
    /// silent one. A deadline is still needed here — a wedged render thread must not hold
    /// the device's thread for ever — but a flat 20 ms is shorter than one buffer on some
    /// routes (A2DP opens at 16 ms) and would insert a whole period of silence for a
    /// response that was merely late.
    render_timeout_ns: AtomicU64,
    /// When to pretend the device was disconnected, and whether that has been done.
    ///
    /// A diagnostic, not a feature: verifying the rebuild path needs a device to unplug,
    /// and an unexercised path is not one to trust. `GNAUDIO_FAULT_AFTER_MS=<n>` makes the
    /// render thread report a disconnect `n` milliseconds after the stream starts, which
    /// drives the same code an unplugged headset does — fault, backoff, replacement,
    /// resumed clock — with no hardware involved. One shot per stream.
    inject_at_ns: AtomicU64,
    injected: AtomicBool,
}

// SAFETY: every field is an atomic, a mutex, or the staging buffer whose access is
// ordered by the hand-off. The struct is reachable from two threads by design.
unsafe impl Send for Shared {}
unsafe impl Sync for Shared {}

impl Shared {
    fn lock(&self) -> std::sync::MutexGuard<'_, Handoff> {
        self.handoff
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// How long one period's render may take: the flat floor, or one device buffer's
    /// duration when the route is deeper than that.
    fn render_timeout_ns(&self) -> u64 {
        self.render_timeout_ns.load(Ordering::Acquire)
    }

    fn channels(&self) -> usize {
        self.device_channels.load(Ordering::Acquire).max(1) as usize
    }

    /// The device's format, as this file's own enum. Defaults to float, which is what
    /// the stream is asked for and what every device measured so far grants.
    fn format(&self) -> Format {
        Format::from_aaudio(self.format.load(Ordering::Acquire)).unwrap_or(Format::Float32)
    }

    /// Records that the device is unusable and wakes the render thread.
    ///
    /// The wakeup is the point. A stream that has gone delivers no periods, so the render
    /// thread — which otherwise waits for one — would never learn that anything happened,
    /// and the rebuild this crate is capable of would never run. Every path that discovers
    /// a dead device goes through here so there is exactly one copy of the ordering rule:
    /// predicate set under the mutex, then notified.
    fn note_fault(&self, code: i32) {
        let handoff = self.lock();
        self.fault.store(code, Ordering::Release);
        self.stopping.store(true, Ordering::Release);
        self.events
            .fetch_or(Events::DEVICE_LOST.bits() as i32, Ordering::Relaxed);
        drop(handoff);
        self.cv.notify_all();
    }

    /// Replaces the staging buffer. Only legal while no callback can be running, which
    /// is what `stopping` guarantees at an open and at a rebuild.
    fn resize_staging(&self, frames: u32) {
        let samples = frames as usize * self.channels();
        // SAFETY: no callback is running: every caller sets `stopping` first and only
        // clears it after this returns.
        let buffer = unsafe { &mut *self.staging.get() };
        buffer.clear();
        buffer.resize(samples, 0.0);
        self.staging_frames.store(frames, Ordering::Release);
    }
}

impl Default for Shared {
    fn default() -> Self {
        Self {
            handoff: Mutex::new(Handoff::default()),
            cv: Condvar::new(),
            staging: UnsafeCell::new(Vec::new()),
            staging_frames: AtomicU32::new(0),
            device_channels: AtomicU32::new(0),
            format: AtomicI32::new(1),
            sample_rate: AtomicI32::new(48_000),
            stopping: AtomicBool::new(true),
            fault: AtomicI32::new(0),
            events: AtomicI32::new(0),
            last_error: AtomicI32::new(0),
            short: AtomicU64::new(0),
            shorts: AtomicU64::new(0),
            timeouts: AtomicU64::new(0),
            long_asks: AtomicU64::new(0),
            silenced: AtomicU64::new(0),
            short_asked: AtomicU64::new(0),
            short_held: AtomicU64::new(0),
            short_pending: AtomicBool::new(false),
            short_reported_at: AtomicU64::new(0),
            presented_base: AtomicU64::new(0),
            burst: AtomicU32::new(0),
            buffer_frames: AtomicU32::new(0),
            grow_ceiling: AtomicU32::new(0),
            cadence_ns: AtomicU64::new(0),
            last_request_ns: AtomicU64::new(0),
            last_xruns: AtomicU32::new(0),
            render_timeout_ns: AtomicU64::new(RENDER_TIMEOUT_NS as u64),
            probe_at_ns: AtomicU64::new(0),
            probe_frames: AtomicU64::new(0),
            inject_at_ns: AtomicU64::new(0),
            injected: AtomicBool::new(false),
        }
    }
}

/// Writes float samples into the device's buffer in the format it asked for.
fn write_in_format(dst: *mut u8, samples: &[f32], format: Format) {
    match format {
        Format::Float32 => {
            // SAFETY: the device guarantees room for `samples.len()` floats.
            unsafe {
                core::ptr::copy_nonoverlapping(samples.as_ptr(), dst as *mut f32, samples.len())
            };
        }
        Format::Int16 => {
            for (index, sample) in samples.iter().enumerate() {
                let value = (sample.clamp(-1.0, 1.0) * 32_767.0) as i16;
                // SAFETY: as above, in i16 units.
                unsafe { core::ptr::write_unaligned((dst as *mut i16).add(index), value) };
            }
        }
        Format::Int32 | Format::Int24 => {
            for (index, sample) in samples.iter().enumerate() {
                let value = (sample.clamp(-1.0, 1.0) * 2_147_483_647.0) as i32;
                // SAFETY: as above, in i32 units.
                unsafe { core::ptr::write_unaligned((dst as *mut i32).add(index), value) };
            }
        }
    }
}

/// Zeroes a period's worth of the device's buffer, in whichever format it asked for.
fn write_silence(dst: *mut u8, samples: usize, format: Format) {
    match format {
        Format::Float32 => {
            // SAFETY: the device guarantees room for `samples` floats.
            unsafe { core::ptr::write_bytes(dst, 0, samples * 4) };
        }
        Format::Int16 => {
            // SAFETY: as above, in i16 units.
            unsafe { core::ptr::write_bytes(dst, 0, samples * 2) };
        }
        Format::Int32 | Format::Int24 => {
            // SAFETY: as above, in i32 units.
            unsafe { core::ptr::write_bytes(dst, 0, samples * 4) };
        }
    }
}

/// The device's real-time thread, once per period.
///
/// It publishes a request, waits for the render thread to answer it, and copies the
/// answer into the buffer AAudio handed it. The wait is bounded: a device deadline is
/// not negotiable, and silence now beats a stalled callback. Every give-up is counted,
/// because a period the pipeline could not supply is a dropout the app cannot see.
extern "C" fn data_callback(
    _stream: *mut AaudioStream,
    user_data: *mut c_void,
    audio_data: *mut c_void,
    num_frames: c_int,
) -> c_int {
    if user_data.is_null() {
        return CALLBACK_STOP;
    }
    // SAFETY: `user_data` is the `Arc<Shared>` this backend keeps alive for as long as
    // the stream exists, and AAudio calls this from exactly one thread at a time.
    let shared = unsafe { &*(user_data as *const Shared) };
    if audio_data.is_null() || num_frames <= 0 {
        return CALLBACK_CONTINUE;
    }
    if shared.stopping.load(Ordering::Acquire) {
        // Silence, not `CALLBACK_STOP`: returning STOP asks AAudio to *stop the stream*,
        // and this flag is also set for the instant between `requestStart` and the first
        // period, when the stream must keep running. Only `close` wants it gone, and by
        // then the stream is closed rather than asked to stop.
        write_silence(
            audio_data as *mut u8,
            num_frames as usize * shared.channels(),
            shared.format(),
        );
        return CALLBACK_CONTINUE;
    }

    let channels = shared.channels();
    let asked = num_frames as usize;
    // The device decides how much to ask for and it may ask for more than the pipeline
    // was sized for. Supplying what fits and silence for the rest is a dropout; bailing
    // out of the period altogether is a stream that never makes a sound. Clamping is the
    // lesser failure and the only one that degrades in proportion.
    let capacity = shared.staging_frames.load(Ordering::Acquire) as usize;
    let (supplied, missing) = supply_plan(asked, capacity);
    let frames = supplied;
    if missing > 0 {
        // The device asked for more than the pipeline can hold, so most of this period is
        // silence this engine inserted. That is a dropout the listener hears as broken or
        // chopped audio while every counter above it reads healthy — the device plays what
        // it was given, at the right rate, and nothing else notices. Counted and reported
        // once a second from here; nothing may log on this thread.
        shared.silenced.fetch_add(missing as u64, Ordering::Relaxed);
        shared.shorts.fetch_add(1, Ordering::Relaxed);
        note_short_period(shared, asked, capacity);
    }

    let epoch = {
        let mut handoff = shared.lock();
        handoff.frames = frames as u32;
        handoff.request = handoff.request.wrapping_add(1);
        let epoch = handoff.request;
        drop(handoff);
        shared.cv.notify_all();
        epoch
    };

    let ready = {
        let deadline = std::time::Instant::now() + Duration::from_nanos(shared.render_timeout_ns());
        let mut handoff = shared.lock();
        loop {
            if handoff.done == epoch || handoff.quit {
                break handoff.done == epoch;
            }
            let now = std::time::Instant::now();
            if now >= deadline {
                break false;
            }
            let (next, timeout) = shared
                .cv
                .wait_timeout(handoff, deadline.saturating_duration_since(now))
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            handoff = next;
            if timeout.timed_out() && handoff.done != epoch {
                break false;
            }
        }
    };

    let format = shared.format();
    let target = audio_data as *mut u8;
    if ready && supplied > 0 {
        // SAFETY: the render thread has finished with the buffer — `done` echoes this
        // request, and the mutex released it — so this is a plain read of memory nobody
        // else is touching. `channels` came with the same handshake.
        let staging = unsafe { &*shared.staging.get() };
        write_in_format(target, &staging[..supplied * channels], format);
    } else {
        // The render thread did not produce this period in time, so the whole period is
        // silence. On a route that asks for a long period — A2DP asked for 4 800 frames,
        // a hundred milliseconds — one of these is a very audible gap, and neither the
        // device's xrun counter nor the engine's own late-block count sees it. Recorded
        // here, reported from the render thread.
        shared.short.fetch_add(1, Ordering::Relaxed);
        shared.timeouts.fetch_add(1, Ordering::Relaxed);
        shared.long_asks.fetch_add(asked as u64, Ordering::Relaxed);
        note_short_period(shared, asked, 0);
        shared
            .events
            .fetch_or(Events::UNDERRUN.bits() as i32, Ordering::Relaxed);
        write_silence(target, asked * channels, format);
    }
    if ready && supplied < asked {
        // The clamp bit: the device wanted more than the pipeline holds. Same failure as
        // a timeout from the listener's side, so it is counted the same way.
        shared.short.fetch_add(1, Ordering::Relaxed);
        shared
            .events
            .fetch_or(Events::UNDERRUN.bits() as i32, Ordering::Relaxed);
        write_silence(
            target.wrapping_add(supplied * channels * bytes_per_sample(format)),
            (asked - supplied) * channels,
            format,
        );
    }
    CALLBACK_CONTINUE
}

/// The device's error callback. Runs on whichever thread AAudio chooses, so it only
/// records: rebuilding a stream from here would close the stream that is calling in.
extern "C" fn error_callback(_stream: *mut AaudioStream, user_data: *mut c_void, error: c_int) {
    if user_data.is_null() {
        return;
    }
    // SAFETY: as above; only atomics are touched.
    let shared = unsafe { &*(user_data as *const Shared) };
    shared.last_error.store(error, Ordering::Relaxed);
    if !crate::reconnect::is_reconnectable(error) {
        return;
    }
    shared.note_fault(error);
}

// ---------------------------------------------------------------------------
// The backend
// ---------------------------------------------------------------------------

pub struct AaudioBackend {
    /// The stream, in a slot the render thread can replace on rebuild.
    stream: Arc<AtomicPtr<AaudioStream>>,
    shared: Arc<Shared>,
    /// The thread that renders. It owns the sink, so joining it is what makes dropping
    /// the sink safe.
    render: Option<std::thread::JoinHandle<()>>,
    granted: Granted,
    config: StreamConfig,
    /// Where each rebuild puts its new `Granted`, so the backend can report the shape
    /// the device actually settled on.
    latest: Arc<Mutex<Granted>>,
}

// SAFETY: the stream and the shared state are process-local handles; the thread handle
// is `Send` by construction.
unsafe impl Send for AaudioBackend {}

impl AaudioBackend {
    pub fn new() -> Self {
        Self {
            stream: Arc::new(AtomicPtr::new(core::ptr::null_mut())),
            shared: Arc::new(Shared::default()),
            render: None,
            granted: Granted::default(),
            config: StreamConfig::default(),
            latest: Arc::new(Mutex::new(Granted::default())),
        }
    }

    fn current(&self) -> *mut AaudioStream {
        self.stream.load(Ordering::Acquire)
    }
}

impl Default for AaudioBackend {
    fn default() -> Self {
        Self::new()
    }
}

/// Opens a stream, registers both callbacks against `shared`, and applies the buffer
/// size the reference sink uses: two bursts, clamped to what the device allows.
fn build_stream(
    config: &StreamConfig,
    shared: &Arc<Shared>,
) -> Result<(*mut AaudioStream, Granted), Error> {
    let mut builder: *mut AaudioStreamBuilder = core::ptr::null_mut();
    // SAFETY: `builder` is a valid out-parameter.
    let result = unsafe { AAudio_createStreamBuilder(&mut builder) };
    if result != AAUDIO_OK || builder.is_null() {
        return Err(error_from(result));
    }
    let user = Arc::as_ptr(shared) as *mut c_void;

    // SAFETY: every call below takes the builder this function just created, and the
    // shared state outlives the stream because the backend holds an `Arc` to it.
    unsafe {
        AAudioStreamBuilder_setDirection(builder, AAUDIO_DIRECTION_OUTPUT);
        AAudioStreamBuilder_setDeviceId(builder, config.device_id.unwrap_or(AAUDIO_UNSPECIFIED));
        AAudioStreamBuilder_setSharingMode(builder, config.sharing.to_aaudio());
        AAudioStreamBuilder_setPerformanceMode(builder, config.performance.to_aaudio());
        // The reference sink's usage line, from inside its low-latency branch — the branch
        // GameNative runs, because `PulseAudioComponent` passes `low_latency=true`:
        //
        //   if (AAudioStreamBuilder_setUsage && android_get_device_api_level() >= 28)
        //       AAudioStreamBuilder_setUsage(u->builder, AAUDIO_USAGE_GAME);
        //
        // This crate had never asked for it, so every stream opened as the platform default
        // `AAUDIO_USAGE_MEDIA`. Usage is what the platform routes on, prioritises on and
        // ducks on; a game's audio declared as generic media is the one property the
        // working sink has that this one did not.
        set_game_usage(builder);
        AAudioStreamBuilder_setFormat(
            builder,
            config.format.unwrap_or(Format::Float32).to_aaudio(),
        );
        // Rate and channel count are left unspecified, as the reference sink leaves them.
        // A route answers with its own rate and its own channel count, and that answer is
        // the only true statement about it: pinning the client's numbers here does not make
        // the route run at them, it only means the crate never finds out what the route
        // actually is. The engine converts between the two, so nothing above needs to know
        // either.
        AAudioStreamBuilder_setSampleRate(builder, AAUDIO_UNSPECIFIED);
        AAudioStreamBuilder_setChannelCount(builder, AAUDIO_UNSPECIFIED);
        // Capacity is asked for only when the caller asked for a device buffer, and then
        // only as that buffer's size. Capacity is the room the stream has to work in, and
        // `setBufferSizeInFrames` can never exceed it — so a caller that wants a deep
        // buffer has to say so before the stream opens and cannot fix it afterwards.
        //
        // Not a line the reference has. It exists for a route it cannot reach: over A2DP
        // this HAL consumes in ~4 800-frame steps while opening with a capacity of 1 536,
        // so every buffer size AAudio would accept is smaller than one step and the stream
        // plays something other than what was written.
        if let Some(frames) = config.device_buffer_capacity_frames {
            AAudioStreamBuilder_setBufferCapacityInFrames(builder, frames.max(1) as c_int);
        }
        //
        // `setFramesPerDataCallback` is not called: pinning it to the client's block makes
        // every larger device period take the silent early return described above.
        AAudioStreamBuilder_setDataCallback(builder, Some(data_callback), user);
        AAudioStreamBuilder_setErrorCallback(builder, Some(error_callback), user);
    }

    let mut stream: *mut AaudioStream = core::ptr::null_mut();
    // SAFETY: a null input-stream pointer is how an output-only stream is asked for.
    let result =
        unsafe { AAudioStreamBuilder_openStream(builder, &mut stream, core::ptr::null_mut()) };
    // SAFETY: the builder is ours and is not needed again.
    unsafe { AAudioStreamBuilder_delete(builder) };
    if result != AAUDIO_OK || stream.is_null() {
        return Err(error_from(result));
    }

    let burst = unsafe { AAudioStream_getFramesPerBurst(stream) }.max(1) as u32;
    let capacity =
        (unsafe { AAudioStream_getBufferCapacityInFrames(stream) }.max(0) as u32).max(burst);
    // What the route chose for itself, and what this backend uses.
    //
    // The reference sink does not size the buffer in its default configuration: `pa__init`
    // sets `low_latency = false`, and every sizing decision in the file — the
    // `frames_per_burst * 2` and the growth on xruns alike — sits inside `if (u->low_latency)`.
    // With the flag off it logs "using default buffer size" and never calls
    // `AAudioStream_setBufferSizeInFrames` at all.
    //
    // That matters because the choice AAudio makes is already per-route: this device
    // reports a 96-frame burst on both its speaker and over A2DP, yet opens with 192 frames
    // on one and 768 on the other. Overriding both with one number derived from a burst
    // throws that away, and the route that lost it is the one with an encoder and a radio
    // link in the path.
    let route_buffer =
        (unsafe { AAudioStream_getBufferSizeInFrames(stream) }.max(0) as u32).max(burst);
    // Two bursts, clamped to the route's capacity — the reference sink's low-latency branch,
    // which is the branch GameNative runs. The caller's budget is a ceiling on top of that,
    // because the pipeline above holds its backlog out of the same buffer size the app was
    // promised; an experiment that asks for capacity asks for a deep buffer outright.
    let mut wanted = match config.device_buffer_capacity_frames {
        Some(_) => capacity,
        None => device_buffer_frames(burst, capacity),
    };
    if let Some(ceiling) = config.device_buffer_frames {
        wanted = wanted.min(ceiling.max(burst));
    }
    // SAFETY: the stream is open and nothing is running against it.
    let applied = unsafe { AAudioStream_setBufferSizeInFrames(stream, wanted as i32) };
    let buffer = if applied > 0 { applied as u32 } else { wanted };
    let usage = match granted_usage(stream) {
        Some(AAUDIO_USAGE_GAME) => "GAME",
        Some(_) => "not GAME",
        None => "unreported",
    };
    crate::trace!(
        "callback: buffer {} frames ({} ms): burst {}, capacity {}, route opened at {}, usage {}",
        buffer,
        buffer as u64 * 1000 / sample_rate_of(stream),
        burst,
        capacity,
        route_buffer,
        usage
    );

    let granted = match read_granted(stream, config, buffer as u32) {
        Ok(granted) => granted,
        Err(error) => {
            // SAFETY: the stream was opened above and nothing else holds it.
            unsafe { AAudioStream_close(stream) };
            return Err(error);
        }
    };
    Ok((stream, granted))
}

/// Reads back what the device settled on. Nothing here trusts a request.
fn read_granted(
    stream: *mut AaudioStream,
    config: &StreamConfig,
    buffer: u32,
) -> Result<Granted, Error> {
    // SAFETY: the stream is open, so every getter is valid.
    let (sample_rate, channels, burst, format) = unsafe {
        (
            AAudioStream_getSampleRate(stream),
            AAudioStream_getChannelCount(stream),
            AAudioStream_getFramesPerBurst(stream),
            AAudioStream_getFormat(stream),
        )
    };
    let burst = plausible_burst(burst).ok_or(Error::Unsupported)?;
    let format = Format::from_aaudio(format).ok_or(Error::Unsupported)?;
    if sample_rate <= 0 || channels <= 0 {
        return Err(Error::Unsupported);
    }
    // A device with more channels than the client asked for carries the extras as
    // silence; one with fewer gets the client's planes folded down, because a route's
    // channel count is the route's business and the client's contract is about its own
    // slab.
    Ok(Granted {
        device_id: unsafe { AAudioStream_getDeviceId(stream) },
        sample_rate,
        // The client's own rate, which the engine resamples to when the device answers
        // with something else. Asking for no rate at all makes the device's answer the
        // only rate there is, and then these two agree.
        client_sample_rate: config.sample_rate.unwrap_or(sample_rate),
        device_channels: channels as u16,
        client_channels: config.channels,
        frames_per_burst: burst,
        device_buffer_frames: buffer,
        block_frames: block_frames_for(config.block_frames, burst),
        sharing: Sharing::from_aaudio(unsafe { AAudioStream_getSharingMode(stream) }),
        performance: crate::types::Performance::from_aaudio(unsafe {
            AAudioStream_getPerformanceMode(stream)
        }),
        format,
        // What the device itself accounts for below us: one burst, which is the
        // granularity it wakes on. Whatever is still queued is added live.
        output_latency_frames: burst,
    })
}

/// What the render thread needs to rebuild the device it is feeding: everything the
/// open needed, and the slots to publish the result into.
struct RenderRequest {
    config: StreamConfig,
    shared: Arc<Shared>,
    stream: Arc<AtomicPtr<AaudioStream>>,
    latest: Arc<Mutex<Granted>>,
}

/// The render thread: waits for the device to ask, fills the staging buffer, answers.
///
/// Owning the sink here rather than on AAudio's thread is the point of the whole design.
/// This thread is an ordinary one — it may block, it may take a lock, it may wait on a
/// ring that is briefly empty — and none of that lands inside the device's deadline.
fn render_loop(request: RenderRequest, mut sink: Box<dyn Sink>) {
    let shared = request.shared.clone();
    let mut attempts = 0u32;
    loop {
        // Three ways out of this wait, and the third is what makes a device switch work:
        // a period to fill, a quit, or a timeout that lets the stream be inspected. Waiting
        // for a period alone deadlocks on a disconnected stream, because a disconnected
        // stream never asks for one.
        let (epoch, frames, quit, due) = {
            let mut handoff = shared.lock();
            let mut timed_out = false;
            while crate::reconnect::should_keep_waiting(
                handoff.request == handoff.done,
                handoff.quit,
                shared.fault.load(Ordering::Acquire),
                timed_out,
            ) {
                let (next, timeout) = shared
                    .cv
                    .wait_timeout(handoff, Duration::from_nanos(STATE_POLL_NS as u64))
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                handoff = next;
                timed_out = timeout.timed_out();
            }
            // Whether the device is actually waiting for this period. A timeout leaves the
            // loop with nothing outstanding, and the difference matters: see below.
            let due = handoff.request != handoff.done;
            (handoff.request, handoff.frames, handoff.quit, due)
        };
        if quit {
            return;
        }
        // The watchdog, which is the part the reference sink does on every render: ask the
        // stream what it thinks it is doing. A device switch can leave a stream stopped,
        // paused or disconnected *without* an error callback — some HALs simply finish the
        // stream when the route goes — and then nothing asks for audio and nothing reports
        // a fault. Polling here is what turns that into a rebuild.
        // Only while this crate believes audio should be flowing. A pause leaves the
        // stream `PAUSED` on purpose, and replacing that would turn the engine's own
        // pause into a disconnect.
        if shared.fault.load(Ordering::Acquire) == 0 && !shared.stopping.load(Ordering::Acquire) {
            // Two ways a stream stops being real. It can say so — not `STARTED`/`STARTING`
            // — or it can go on claiming to play while never asking for audio again. The
            // second is what a suspended container leaves behind: AudioFlinger's track
            // reads `Active: no` with its frame count frozen while AAudio still reports a
            // started stream.
            let silent = shared.last_request_ns.load(Ordering::Acquire);
            let stale = silent != 0
                && crate::clock::now_ns().saturating_sub(silent)
                    > crate::reconnect::silence_timeout_ns(
                        shared.buffer_frames.load(Ordering::Acquire),
                        shared.sample_rate.load(Ordering::Acquire),
                    );
            if stale || !stream_is_healthy(&request) {
                shared.note_fault(crate::reconnect::STATE_LOST);
            }
        }
        if shared.fault.load(Ordering::Acquire) != 0 {
            // A disconnected stream never recovers on its own, and closing it from the
            // callback that reported the fault is not allowed. This thread is where the
            // rebuild belongs, so the answer to a fault is to do nothing with this
            // period and let the rebuild below publish a working stream.
            attempts = recreate_stream(&request, &mut sink, attempts);
            if shared.lock().quit {
                return;
            }
            let mut handoff = shared.lock();
            handoff.done = epoch;
            drop(handoff);
            shared.cv.notify_all();
            continue;
        }
        attempts = 0;
        if !due {
            // Nothing is waiting for this period, so rendering one would be a period nobody
            // plays: it pulls the ring, counts frames as delivered, and advances a position
            // the device never consumed — the app's audio consumed at the poll cadence and
            // reported as heard. The reference cannot reach this state at all: its
            // `sink_process_render` is only ever entered from the data callback.
            continue;
        }
        shared
            .last_request_ns
            .store(crate::clock::now_ns(), Ordering::Release);
        // The injected fault, if one was asked for and is due. Doing it here rather than on
        // a timer thread keeps it on the one thread that can safely observe the stream.
        // The time is checked *before* the one-shot latch is taken. Reversing these two
        // arms the injector and then disarms it on the first period, so it never fires.
        let armed = shared.inject_at_ns.load(Ordering::Acquire);
        if armed != 0
            && crate::clock::now_ns() >= armed
            && !shared.injected.swap(true, Ordering::AcqRel)
        {
            crate::trace!("callback: injecting a disconnect, as GNAUDIO_FAULT_AFTER_MS asked");
            shared.note_fault(AAUDIO_ERROR_DISCONNECTED);
            continue;
        }
        let channels = shared.channels();
        let samples = frames as usize * channels;
        {
            // SAFETY: the device's thread published this request and is waiting for
            // `done` to echo it, so nobody else is reading or writing the buffer.
            let staging = unsafe { &mut *shared.staging.get() };
            if staging.len() >= samples {
                let period = &mut staging[..samples];
                period.fill(0.0);
                sink.fill(period, frames);
            }
        }
        // The reference sink's adaptive top-up and its consumption measurement, both once
        // per period on the thread that owns the stream.
        grow_on_xruns(&request);
        probe_consumption(&request);
        report_short_periods(&shared);
        let mut handoff = shared.lock();
        handoff.done = epoch;
        drop(handoff);
        shared.cv.notify_all();
    }
}

/// Closes the stream this backend is on. The reference sink's `recreate_aaudio_stream`,
/// first half: stop, close, and hold no pointer to a stream that is gone.
///
/// `requestStop` before `close` is not tidiness — it is what ends the stream's claim on
/// the route and returns its buffers, so the replacement does not open onto a HAL still
/// tidying up the last one.
fn retire_stream(request: &RenderRequest) {
    let shared = &request.shared;
    let old = request.stream.swap(core::ptr::null_mut(), Ordering::AcqRel);
    if old.is_null() {
        return;
    }
    // Everything the old stream presented continues the clock rather than restarting it,
    // so a route change's gap reads as a gap and not as a position going backwards. Read
    // before closing: the position is only queryable while the stream is open.
    let mut frame_position: i64 = 0;
    let mut time_ns: i64 = 0;
    // SAFETY: the stream is open and this thread is the only one closing it.
    let read = unsafe {
        AAudioStream_getTimestamp(old, CLOCK_MONOTONIC, &mut frame_position, &mut time_ns)
    };
    if read == AAUDIO_OK && frame_position > 0 {
        let base = shared.presented_base.load(Ordering::Acquire);
        shared.presented_base.store(
            base.saturating_add(frame_position as u64),
            Ordering::Release,
        );
    }
    // SAFETY: the stream is open and this thread is the only one using it.
    unsafe {
        AAudioStream_requestStop(old);
        AAudioStream_close(old);
    }
}

/// Retires the current stream and opens a replacement on whatever the system is routed to
/// now — the reference sink's `recreate_aaudio_stream`, whole.
///
/// The order matters and is the reference's: **stop, close, forget, create, start.** The
/// replacement is created immediately, with no delay in front of it; only a *failure*
/// waits before the next attempt. A backoff in front of the first attempt would put that
/// much silence on every route change for nothing.
///
/// Returns the attempt count so a device that stays gone costs a poll rather than a spin.
fn recreate_stream(request: &RenderRequest, sink: &mut Box<dyn Sink>, mut attempts: u32) -> u32 {
    let shared = &request.shared;
    loop {
        // A close during a rebuild must not wait for a device that may never come back:
        // `close` sets `quit` and joins this thread, so giving up here is what makes a
        // permanent disconnect a clean shutdown instead of a hang.
        if shared.lock().quit {
            return attempts;
        }
        retire_stream(request);

        // Unpinned, deliberately: the device that was there is gone, and the point of a
        // recreate is to reach whatever is routed now. A pinned id would ask for the
        // device that just left and fail until it came back — correct behaviour only for
        // a caller that pinned one on purpose.
        let mut config = request.config;
        config.device_id = None;
        match build_stream(&config, shared) {
            Ok((stream, granted)) => {
                // Nothing is refused here. A route answers with its own rate, its own
                // channel count and its own burst, and the engine is told all three so it
                // can convert. Requiring a replacement to match the device that just left
                // sends the search back for it, and a route that genuinely runs at another
                // rate could never satisfy that.
                shared
                    .device_channels
                    .store(granted.device_channels as u32, Ordering::Release);
                shared
                    .format
                    .store(granted.format.to_aaudio(), Ordering::Release);
                shared
                    .sample_rate
                    .store(granted.sample_rate, Ordering::Release);
                shared
                    .burst
                    .store(granted.frames_per_burst, Ordering::Release);
                shared
                    .buffer_frames
                    .store(granted.device_buffer_frames, Ordering::Release);
                shared.grow_ceiling.store(
                    config
                        .device_buffer_frames
                        .unwrap_or(granted.device_buffer_frames)
                        .max(granted.device_buffer_frames),
                    Ordering::Release,
                );
                shared.cadence_ns.store(0, Ordering::Release);
                shared
                    .last_request_ns
                    .store(crate::clock::now_ns(), Ordering::Release);
                shared.resize_staging(capacity_frames(stream, granted.frames_per_burst));
                shared.render_timeout_ns.store(
                    render_deadline_ns(
                        granted.device_buffer_frames,
                        granted.sample_rate,
                        granted.block_frames,
                    ),
                    Ordering::Release,
                );
                // The device's own xrun count restarts with the stream, so the growth rule
                // must not compare a new stream's count against the old one's.
                shared.last_xruns.store(0, Ordering::Release);
                shared.probe_at_ns.store(0, Ordering::Release);
                shared.probe_frames.store(0, Ordering::Release);
                // A replacement can settle on a different burst or block shape, and the
                // sink is what stages whole client blocks across device periods. It has to
                // learn the new one before this stream can ask it for audio.
                sink.configure(&granted);
                sink.reset();
                shared.fault.store(0, Ordering::Release);
                shared.stopping.store(false, Ordering::Release);
                if let Ok(mut latest) = request.latest.lock() {
                    *latest = granted;
                }
                request.stream.store(stream, Ordering::Release);
                // SAFETY: the stream is open and nothing has started it yet.
                unsafe { AAudioStream_requestStart(stream) };
                shared
                    .events
                    .fetch_or(Events::RESYNC.bits() as i32, Ordering::Relaxed);
                // The buffer is printed because it is the number a route switch is judged
                // by: without it, a recreate that kept the previous route's buffer reads
                // identically in the log to one that got the size it needed.
                crate::trace!(
                    "callback: recreated on device {} at {} Hz, {} ch, burst {}, device \
                     buffer {} ({} ms), block {} (client {} Hz, {} ch) (attempt {attempts})",
                    granted.device_id,
                    granted.sample_rate,
                    granted.device_channels,
                    granted.frames_per_burst,
                    granted.device_buffer_frames,
                    granted.device_buffer_frames as u64 * 1000 / granted.sample_rate.max(1) as u64,
                    granted.block_frames,
                    granted.client_sample_rate,
                    granted.client_channels
                );
                return 0;
            }
            Err(error) => {
                crate::trace!("callback: recreate refused: {error:?}");
                attempts = attempts.saturating_add(1);
                std::thread::sleep(crate::reconnect::backoff(attempts));
            }
        }
    }
}

/// Records that this period was clamped, and lets the render thread say so once a second.
///
/// The sampling is done here because this is the audio thread; the formatting and the
/// write happen in [`report_short_periods`], which runs on the thread that owns the stream.
fn note_short_period(shared: &Shared, asked: usize, held: usize) {
    shared.short_asked.store(asked as u64, Ordering::Release);
    shared.short_held.store(held as u64, Ordering::Release);
    shared.short_pending.store(true, Ordering::Release);
}

/// Says, once a second, that the device asked for more audio than the pipeline could hold.
fn report_short_periods(shared: &Shared) {
    if !shared.short_pending.swap(false, Ordering::AcqRel) {
        return;
    }
    let now = crate::clock::now_ns();
    let last = shared.short_reported_at.load(Ordering::Acquire);
    if now.saturating_sub(last) < 1_000_000_000 {
        shared.short_pending.store(true, Ordering::Release);
        return;
    }
    shared.short_reported_at.store(now, Ordering::Release);
    crate::trace!(
        "callback: silence inserted — {} periods missed their deadline ({}) and {} frames \
         clamped, {} frames in all; the device asked for {} and the pipeline held {}",
        shared.timeouts.load(Ordering::Acquire),
        shared.long_asks.load(Ordering::Acquire),
        shared.shorts.load(Ordering::Acquire),
        shared.silenced.load(Ordering::Acquire),
        shared.short_asked.load(Ordering::Acquire),
        shared.short_held.load(Ordering::Acquire)
    );
}

/// Grows the device buffer by a burst when the device reports it lost audio — the reference's
/// rule from `sink_process_render`: on any change in `AAudioStream_getXRunCount`, raise the
/// buffer by one burst, bounded by what the pipeline above allows and by the route's capacity.
///
/// This is how a route that genuinely needs more than two bursts reaches the size it wants
/// without anybody guessing one. It can miss — `getXRunCount` has been seen reporting zero
/// through hundreds of milliseconds of silence on this class of hardware — so the two-burst
/// floor is what makes a route usable and this is the top-up on top of it.
fn grow_on_xruns(request: &RenderRequest) {
    let shared = &request.shared;
    let stream = request.stream.load(Ordering::Acquire);
    if stream.is_null() {
        return;
    }
    // SAFETY: the stream is open; every getter here reads the stream's own state.
    let xruns = unsafe { AAudioStream_getXRunCount(stream) }.max(0) as u32;
    let previous = shared.last_xruns.load(Ordering::Acquire);
    if xruns <= previous {
        return;
    }
    shared.last_xruns.store(xruns, Ordering::Release);
    let current = unsafe { AAudioStream_getBufferSizeInFrames(stream) }.max(0) as u32;
    let capacity = unsafe { AAudioStream_getBufferCapacityInFrames(stream) }.max(0) as u32;
    let wanted = grown_buffer_frames(
        current.max(1),
        shared.burst.load(Ordering::Acquire),
        shared.grow_ceiling.load(Ordering::Acquire),
        capacity,
    );
    if wanted > current {
        // SAFETY: the stream is open.
        let applied = unsafe { AAudioStream_setBufferSizeInFrames(stream, wanted as i32) };
        crate::trace!(
            "callback: device lost audio ({} xruns), buffer {} -> {} frames",
            xruns,
            current,
            if applied > 0 { applied as u32 } else { wanted }
        );
    }
}

/// Reports the rate the device is really consuming audio, once every two seconds.
///
/// The one measurement that distinguishes the two ways audio can "sound slow": a device
/// playing fewer frames per second than its own sample rate (the route cannot keep up, and
/// the pipeline above it will back up), versus a device consuming correctly while
/// something else is wrong. Nothing else here can answer it, because an ALSA client's
/// position is a number this crate chooses rather than measures.
fn probe_consumption(request: &RenderRequest) {
    let shared = &request.shared;
    let stream = request.stream.load(Ordering::Acquire);
    if stream.is_null() {
        return;
    }
    let mut frame_position: i64 = 0;
    let mut time_ns: i64 = 0;
    // SAFETY: the stream is open; this is a read of its own presentation clock.
    let read = unsafe {
        AAudioStream_getTimestamp(stream, CLOCK_MONOTONIC, &mut frame_position, &mut time_ns)
    };
    if read != AAUDIO_OK || frame_position < 0 || time_ns <= 0 {
        return;
    }
    let last_at = shared.probe_at_ns.load(Ordering::Acquire);
    let last_frames = shared.probe_frames.load(Ordering::Acquire);
    // The pair the device published, not a wall-clock sample of it: `getTimestamp` reports
    // the position *and the time it was true*, and dividing a position sampled twice by our
    // own clock counts the device's reporting lag as if it were playback, which reads a
    // healthy 48 kHz stream as 46 kHz — 4 % slow — and invents a rate mismatch.
    if last_at == 0 {
        shared
            .probe_at_ns
            .store(time_ns.max(0) as u64, Ordering::Release);
        shared
            .probe_frames
            .store(frame_position as u64, Ordering::Release);
        return;
    }
    let elapsed = (time_ns as u64).saturating_sub(last_at);
    if elapsed < 2_000_000_000 {
        return;
    }
    shared.probe_at_ns.store(time_ns as u64, Ordering::Release);
    shared
        .probe_frames
        .store(frame_position as u64, Ordering::Release);
    if (frame_position as u64) <= last_frames {
        return;
    }
    let frames = frame_position as u64 - last_frames;
    let played_per_second = frames.saturating_mul(1_000_000_000) / elapsed.max(1);
    let claimed = shared.sample_rate.load(Ordering::Acquire).max(1) as u64;
    // Only when it is wrong, so a healthy stream stays quiet: a route playing at the rate
    // it claims needs no line, and a line per two seconds would be noise in every log.
    let deviation = played_per_second.abs_diff(claimed) * 1_000 / claimed;
    if deviation >= 20 {
        crate::trace!(
            "callback: device is playing {played_per_second} frames/s, not {claimed} \
             ({deviation} per mille {})",
            if played_per_second < claimed {
                "slow"
            } else {
                "fast"
            }
        );
    }
}

/// Asks the platform to treat this stream as a game's: `AAUDIO_USAGE_GAME`.
///
/// Resolved through `dlsym` rather than linked. `AAudioStreamBuilder_setUsage` is
/// `__INTRODUCED_IN(28)` and this crate's floor is API 26, so a direct reference would be a
/// strong undefined symbol; the module is linked `-z now`, which resolves every undefined
/// symbol at load, so on an API 26/27 device the whole plugin would fail to load and the
/// container would be silent rather than merely defaulted. The reference sink gets the same
/// effect from a weak declaration (and documents the same hazard in its own comment); Rust
/// cannot declare weak symbols, so the lookup is explicit and a missing symbol is skipped.
///
/// A missing symbol is not an error: opening with the platform's default usage is the
/// documented behaviour when the call is unavailable.
fn set_game_usage(builder: *mut AaudioStreamBuilder) {
    type SetUsage = unsafe extern "C" fn(*mut AaudioStreamBuilder, c_int);
    // The library is already loaded in every process that can call this, so the handle is
    // only ever used to ask for one symbol.
    // SAFETY: a NUL-terminated string literal and a plain flag.
    let handle = unsafe { dlopen(c"libaaudio.so".as_ptr(), 0x00002) };
    if handle.is_null() {
        return;
    }
    // SAFETY: as above, and `dlsym` returns either null or a symbol of the type asked for.
    let symbol = unsafe { dlsym(handle, c"AAudioStreamBuilder_setUsage".as_ptr()) };
    if symbol.is_null() {
        return;
    }
    // SAFETY: the symbol exists on this platform and has this signature — it is the
    // declaration from `AAudio.h`, which is the definition libaaudio was built from.
    let set_usage: SetUsage = unsafe { core::mem::transmute(symbol) };
    // SAFETY: the builder is live at this point in `build_stream`.
    unsafe { set_usage(builder, AAUDIO_USAGE_GAME) };
}

/// The usage the stream is actually running with, or `None` when the platform cannot say.
///
/// Reported rather than assumed: `setUsage` is a request, and a platform that ignored it
/// would leave every symptom attributed to the wrong cause. The same rule as the thread
/// priority — say whether it was granted instead of believing the request.
fn granted_usage(stream: *mut AaudioStream) -> Option<c_int> {
    type GetUsage = unsafe extern "C" fn(*mut AaudioStream) -> c_int;
    // SAFETY: a NUL-terminated string literal and a plain flag.
    let handle = unsafe { dlopen(c"libaaudio.so".as_ptr(), 0x00002) };
    if handle.is_null() {
        return None;
    }
    // SAFETY: as above; either null or a `getUsage` of that signature.
    let symbol = unsafe { dlsym(handle, c"AAudioStream_getUsage".as_ptr()) };
    if symbol.is_null() {
        return None;
    }
    // SAFETY: the declaration from `AAudio.h`, and the stream is open here.
    let get_usage: GetUsage = unsafe { core::mem::transmute(symbol) };
    // SAFETY: the stream is open.
    Some(unsafe { get_usage(stream) })
}

/// The stream's sample rate, or 48 kHz when it will not say — used only for log lines.
fn sample_rate_of(stream: *mut AaudioStream) -> u64 {
    // SAFETY: the stream is open wherever this is called.
    let rate = unsafe { AAudioStream_getSampleRate(stream) };
    if rate > 0 {
        rate as u64
    } else {
        48_000
    }
}

/// Whether the stream currently claims to be playing.
///
/// `STARTED` only, plus `STARTING`, which is the instant between `requestStart` and the
/// first period. Everything else — paused, stopped, flushing, closing, disconnected — is a
/// stream that will not ask for audio, and a stream that will not ask for audio has to be
/// replaced. A null pointer is not unhealthy: it means a recreate is already under way.
fn stream_is_healthy(request: &RenderRequest) -> bool {
    let stream = request.stream.load(Ordering::Acquire);
    if stream.is_null() {
        return true;
    }
    // SAFETY: the stream is open; reading its state is what the reference sink does from
    // its render path every period.
    let state = unsafe { AAudioStream_getState(stream) };
    crate::reconnect::state_is_playing(state)
}

/// The buffer capacity of a stream, which is the most a single callback can ask for.

/// The bound on a period's render, as a number the caller's geometry justifies.
///
/// The reference has none: `pa_asyncmsgq_send` waits until the sink thread has rendered, and
/// `pa_sink_render_into_full` waits until the graph has the audio. A deadline shorter than a
/// legitimate render is therefore not a safety net — it is a defect, and it was one here: a
/// flat 20 ms fired against a client that legitimately waits for its own next block, and the
/// whole period became silence on a stream whose every counter read healthy.
///
/// So the bound is derived from how long a render can legitimately take, not chosen: the
/// client waits for its own period, and the plugin gives up on a starved ring after its own
/// timeout, so twice a generous ceiling on both is where this sits. It is a valve against a
/// wedged thread, and nothing else.
fn render_deadline_ns(device_buffer_frames: u32, sample_rate: i32, block_frames: u32) -> u64 {
    let block_ns = if sample_rate > 0 {
        (block_frames as u64).saturating_mul(1_000_000_000) / sample_rate as u64
    } else {
        0
    };
    let buffer_ns = if sample_rate > 0 {
        (device_buffer_frames as u64).saturating_mul(1_000_000_000) / sample_rate as u64
    } else {
        0
    };
    // 250 ms: twice the longest a starved ring waits (80 ms) plus one block and one buffer,
    // and comfortably above anything a healthy render takes.
    block_ns
        .saturating_add(buffer_ns)
        .saturating_mul(2)
        .max(RENDER_TIMEOUT_NS as u64)
        .max(250_000_000)
}

fn capacity_frames(stream: *mut AaudioStream, burst: u32) -> u32 {
    let capacity = unsafe { AAudioStream_getBufferCapacityInFrames(stream) }.max(0) as u32;
    capacity.max(burst).max(1)
}

impl Backend for AaudioBackend {
    fn open(&mut self, config: &StreamConfig, mut sink: Box<dyn Sink>) -> Result<Granted, Error> {
        if !self.current().is_null() {
            return Err(Error::InvalidState);
        }
        // Nothing may call in until the stream is fully described.
        self.shared.stopping.store(true, Ordering::Release);

        let (stream, granted) = build_stream(config, &self.shared)?;
        self.shared
            .device_channels
            .store(granted.device_channels as u32, Ordering::Release);
        self.shared
            .format
            .store(granted.format.to_aaudio(), Ordering::Release);
        self.shared
            .sample_rate
            .store(granted.sample_rate, Ordering::Release);
        self.shared
            .resize_staging(capacity_frames(stream, granted.frames_per_burst));
        self.shared.render_timeout_ns.store(
            render_deadline_ns(
                granted.device_buffer_frames,
                granted.sample_rate,
                granted.block_frames,
            ),
            Ordering::Release,
        );
        self.shared.fault.store(0, Ordering::Release);
        self.shared.injected.store(false, Ordering::Release);
        self.shared
            .burst
            .store(granted.frames_per_burst, Ordering::Release);
        self.shared
            .buffer_frames
            .store(granted.device_buffer_frames, Ordering::Release);
        self.shared.grow_ceiling.store(
            config
                .device_buffer_frames
                .unwrap_or(granted.device_buffer_frames)
                .max(granted.device_buffer_frames),
            Ordering::Release,
        );
        self.shared.cadence_ns.store(0, Ordering::Release);
        self.shared
            .last_request_ns
            .store(crate::clock::now_ns(), Ordering::Release);

        self.shared.last_xruns.store(0, Ordering::Release);
        self.shared.probe_at_ns.store(0, Ordering::Release);
        self.shared.probe_frames.store(0, Ordering::Release);
        self.shared.inject_at_ns.store(
            std::env::var("GNAUDIO_FAULT_AFTER_MS")
                .ok()
                .and_then(|value| value.parse::<u64>().ok())
                .map(|ms| crate::clock::now_ns().saturating_add(ms.saturating_mul(1_000_000)))
                .unwrap_or(0),
            Ordering::Release,
        );

        // The sink learns its block size before anything can ask it for one.
        sink.configure(&granted);
        sink.reset();

        self.stream.store(stream, Ordering::Release);
        self.granted = granted;
        self.config = *config;
        if let Ok(mut latest) = self.latest.lock() {
            *latest = granted;
        }

        let request = RenderRequest {
            config: *config,
            shared: self.shared.clone(),
            stream: self.stream.clone(),
            latest: self.latest.clone(),
        };
        let shared = self.shared.clone();
        // SAFETY: a plain thread with no captured references; the sink moves in.
        let render = std::thread::Builder::new()
            .name("gnaudio-render".to_string())
            .spawn(move || {
                raise_priority();
                render_loop(request, sink);
                shared.stopping.store(true, Ordering::Release);
            })
            .map_err(|_| Error::NoMemory)?;
        self.render = Some(render);

        crate::trace!(
            "callback: device {} at {} Hz, {} ch, burst {}, device buffer {} ({} ms), block {}",
            granted.device_id,
            granted.sample_rate,
            granted.device_channels,
            granted.frames_per_burst,
            granted.device_buffer_frames,
            granted.device_buffer_frames as u64 * 1000 / granted.sample_rate.max(1) as u64,
            granted.block_frames
        );
        Ok(granted)
    }

    fn start(&mut self) -> Result<(), Error> {
        let stream = self.current();
        if stream.is_null() {
            // A rebuild may be in flight, and a start during one is simply early: the
            // render thread starts the replacement itself. Not an error.
            return Ok(());
        }
        // SAFETY: the stream is open and the render thread only fills on request.
        let result = unsafe { AAudioStream_requestStart(stream) };
        if result != AAUDIO_OK {
            if crate::reconnect::is_reconnectable(result) {
                // The reference sink does exactly this: a start that cannot reach the
                // device schedules a recreate and reports success, because being routed
                // elsewhere is a thing that happens, not a failure of the caller's.
                self.shared.note_fault(result);
                return Ok(());
            }
            return Err(error_from(result));
        }
        // Only now may the device ask for audio: a callback that arrives before the
        // render thread is waiting would otherwise spend its deadline for nothing.
        self.shared.stopping.store(false, Ordering::Release);
        Ok(())
    }

    fn pause(&mut self) -> Result<(), Error> {
        let stream = self.current();
        if stream.is_null() {
            return Err(Error::InvalidState);
        }
        self.shared.stopping.store(true, Ordering::Release);
        // SAFETY: the stream is open.
        let result = unsafe { AAudioStream_requestPause(stream) };
        if result != AAUDIO_OK {
            return Err(error_from(result));
        }
        // Discard whatever the device was still holding, so a resume cannot replay audio
        // from before the pause. Valid once the stream is paused.
        unsafe { AAudioStream_requestFlush(stream) };
        Ok(())
    }

    fn resume(&mut self) -> Result<(), Error> {
        let stream = self.current();
        if stream.is_null() {
            return Ok(());
        }
        // SAFETY: the stream is open.
        let result = unsafe { AAudioStream_requestStart(stream) };
        if result != AAUDIO_OK {
            if crate::reconnect::is_reconnectable(result) {
                // The route may have changed while this was paused, which leaves the old
                // stream unusable. Rebuild rather than fail a resume.
                self.shared.note_fault(result);
                return Ok(());
            }
            return Err(error_from(result));
        }
        self.shared.stopping.store(false, Ordering::Release);
        Ok(())
    }

    fn stop(&mut self) -> Result<(), Error> {
        let stream = self.current();
        if stream.is_null() {
            return Err(Error::InvalidState);
        }
        self.shared.stopping.store(true, Ordering::Release);
        // SAFETY: the stream is open.
        let result = unsafe { AAudioStream_requestStop(stream) };
        if result != AAUDIO_OK {
            return Err(error_from(result));
        }
        Ok(())
    }

    fn close(&mut self) {
        self.shared.stopping.store(true, Ordering::Release);
        {
            let mut handoff = self.shared.lock();
            handoff.quit = true;
            drop(handoff);
            self.shared.cv.notify_all();
        }
        // Joining is what makes dropping the sink safe: the render thread owns it.
        if let Some(render) = self.render.take() {
            let _ = render.join();
        }
        let stream = self.stream.swap(core::ptr::null_mut(), Ordering::AcqRel);
        if !stream.is_null() {
            // SAFETY: the callback has returned `CALLBACK_STOP` since `stopping` was set,
            // and the render thread has left.
            unsafe { AAudioStream_close(stream) };
        }
    }

    fn device_xruns(&mut self) -> u32 {
        let stream = self.current();
        if stream.is_null() {
            return 0;
        }
        // SAFETY: the stream is open.
        let count = unsafe { AAudioStream_getXRunCount(stream) };
        count.max(0) as u32
    }

    fn timestamp(&mut self) -> Option<Timestamp> {
        let stream = self.current();
        if stream.is_null() {
            return None;
        }
        let mut frame_position: i64 = 0;
        let mut time_ns: i64 = 0;
        // SAFETY: both are live out-parameters.
        let result = unsafe {
            AAudioStream_getTimestamp(stream, CLOCK_MONOTONIC, &mut frame_position, &mut time_ns)
        };
        if result != AAUDIO_OK {
            return None;
        }
        Some(Timestamp {
            frame_position,
            time_ns,
        })
    }

    fn played_frames(&self) -> Option<u64> {
        let stream = self.current();
        let base = self.shared.presented_base.load(Ordering::Acquire);
        if stream.is_null() {
            return Some(base);
        }
        let mut frame_position: i64 = 0;
        let mut time_ns: i64 = 0;
        // SAFETY: both are live out-parameters.
        let result = unsafe {
            AAudioStream_getTimestamp(stream, CLOCK_MONOTONIC, &mut frame_position, &mut time_ns)
        };
        if result != AAUDIO_OK || frame_position < 0 {
            // A callback stream has no "frames written" counter of ours to fall back
            // on — the device filled those buffers, not this code — so the base is the
            // honest answer rather than a guess at progress.
            return Some(base);
        }
        Some(base.saturating_add(frame_position as u64))
    }

    fn queued_frames(&mut self) -> u32 {
        let stream = self.current();
        if stream.is_null() {
            return 0;
        }
        // SAFETY: the stream is open.
        let buffer = unsafe { AAudioStream_getBufferSizeInFrames(stream) };
        buffer.max(0) as u32
    }

    fn take_events(&mut self) -> Events {
        let bits = self.shared.events.swap(0, Ordering::AcqRel);
        Events::from_bits(bits as u32)
    }

    fn sample_rate(&self) -> i32 {
        self.granted.sample_rate
    }

    fn is_delivering(&self) -> bool {
        !self.current().is_null() && self.shared.fault.load(Ordering::Acquire) == 0
    }
}

impl Drop for AaudioBackend {
    fn drop(&mut self) {
        self.close();
    }
}

/// Asks the kernel to schedule the render thread as audio work.
///
/// `THREAD_PRIORITY_AUDIO` is -16 on Android, and an app may set it on its own thread.
/// Failure is not fatal and not unusual — a device policy can refuse it — so the stream
/// carries on: the priority reduces the size of the problem, and the pipeline's own
/// slack is what bounds it.
fn raise_priority() {
    extern "C" {
        fn setpriority(which: c_int, who: u32, prio: c_int) -> c_int;
    }
    /// `PRIO_PROCESS`.
    const PRIO_PROCESS: c_int = 0;
    /// Android's `THREAD_PRIORITY_AUDIO`.
    const THREAD_PRIORITY_AUDIO: c_int = -16;

    // SAFETY: `who` of 0 means the calling thread, and no pointer is involved.
    let result = unsafe { setpriority(PRIO_PROCESS, 0, THREAD_PRIORITY_AUDIO) };
    if result == 0 {
        crate::trace!("callback: render thread is at audio priority");
    } else {
        crate::trace!("callback: render thread could not take audio priority ({result})");
    }
}
