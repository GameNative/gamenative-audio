//! The shape of the API: what a client asks for, what the device granted, and
//! what it can ask about while playing.
//!
//! The vocabulary in this crate is its own (`gnaudio`), and so is the model: a
//! client-owned block size, a callback that is the device's clock, block-aligned
//! sample positions, honest latency figures, and a pause that does not reopen the
//! device.

#[cfg(target_os = "android")]
use core::ffi::c_int;

/// How the device is asked to be shared.
///
/// `Exclusive` is the one that matters: it is Android's direct path (MMAP/NOIRQ),
/// where the client's buffer is shared with the ALSA driver and the AudioFlinger
/// mixer is bypassed. It is a *request*: a device or a HAL that cannot do it
/// silently falls back, which is why [`Granted::sharing`] reports what actually
/// happened instead of what was asked for.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Sharing {
    Exclusive,
    Shared,
}

#[cfg(target_os = "android")]
impl Sharing {
    pub(crate) const fn to_aaudio(self) -> c_int {
        match self {
            // AAUDIO_SHARING_MODE_EXCLUSIVE / SHARED.
            Sharing::Exclusive => 0,
            Sharing::Shared => 1,
        }
    }

    pub(crate) const fn from_aaudio(value: c_int) -> Sharing {
        if value == 0 {
            Sharing::Exclusive
        } else {
            Sharing::Shared
        }
    }
}

/// How hard the device is asked to try for a small, fast buffer.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Performance {
    /// `LOW_LATENCY`: the fast track, and the only setting that can reach the
    /// exclusive path.
    LowLatency,
    /// `POWER_SAVING`: fewer wakeups, larger buffers, more delay.
    PowerSaving,
}

#[cfg(target_os = "android")]
impl Performance {
    pub(crate) const fn to_aaudio(self) -> c_int {
        match self {
            // AAUDIO_PERFORMANCE_MODE_LOW_LATENCY / POWER_SAVING.
            Performance::LowLatency => 12,
            Performance::PowerSaving => 11,
        }
    }

    pub(crate) const fn from_aaudio(value: c_int) -> Performance {
        if value == 12 {
            Performance::LowLatency
        } else {
            Performance::PowerSaving
        }
    }
}

/// Sample format the device is carrying. The engine drives the device in whatever
/// it runs, converting once, at that boundary. The numbering is AAudio's, so the C
/// ABI can publish it unchanged and a C caller's number means the same thing here.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Format {
    /// `AAUDIO_FORMAT_PCM_I16`.
    Int16 = 1,
    /// `AAUDIO_FORMAT_PCM_FLOAT`.
    Float32 = 2,
    /// `AAUDIO_FORMAT_PCM_I24_PACKED`.
    Int24 = 3,
    /// `AAUDIO_FORMAT_PCM_I32`.
    Int32 = 4,
}

#[cfg(target_os = "android")]
impl Format {
    pub(crate) const fn to_aaudio(self) -> c_int {
        self as c_int
    }

    /// The format a device answers with, or `None` for one this engine cannot
    /// carry: `AAUDIO_FORMAT_INVALID` (-1) and `UNSPECIFIED` (0) both mean the
    /// device is not answering usefully.
    pub(crate) const fn from_aaudio(value: c_int) -> Option<Format> {
        match value {
            1 => Some(Format::Int16),
            2 => Some(Format::Float32),
            3 => Some(Format::Int24),
            4 => Some(Format::Int32),
            _ => None,
        }
    }
}

/// What the client asks for.
#[derive(Clone, Copy, Debug)]
pub struct StreamConfig {
    /// Output device, or `None` for the default (which follows the system's
    /// routing, including a USB interface).
    pub device_id: Option<i32>,
    /// Channels to drive. A device with more channels carries the rest as
    /// silence; a device with fewer gets the client's channels downmixed rather
    /// than a failure — a route's channel count is its own business.
    pub channels: u16,
    /// Sample rate, or `None` for the device's own rate.
    ///
    /// This is the client's contract, not the device's. The device answers with
    /// whatever its route runs at, and the engine converts between the two: a device
    /// that only offers 44.1 kHz still gets audio at the right pitch rather than a
    /// refusal, because the route is not something either side gets to choose.
    pub sample_rate: Option<i32>,
    /// Frames the client is handed per callback — its block size. One device burst
    /// is the granularity: the value is
    /// rounded up to a whole number of bursts, so a client asking for 256 frames
    /// on a 128-frame device gets 256 and a callback every other burst.
    pub block_frames: u32,
    /// Sample format to ask the route for, or `None` for float. A route answers with what
    /// it can take and the engine converts, so this is a request about the route, not about
    /// what the client writes.
    ///
    /// Worth being able to ask because the format reaches the HAL, and not every Android
    /// audio route takes every format well: an ordinary app sends 16-bit, and a route that
    /// is clean for it is the reference point this has to match.
    pub format: Option<Format>,
    /// Room to ask the route for before the stream opens, or `None` to take what it
    /// offers. This is the one buffer property that cannot be changed afterwards:
    /// `setBufferSizeInFrames` may only shrink within it.
    ///
    /// Left `None` everywhere except an experiment, because the route's own capacity is
    /// the answer on every route measured so far.
    pub device_buffer_capacity_frames: Option<u32>,
    /// Frames the device's own ring should hold, or `None` to let AAudio choose.
    /// This is the buffer between the client's deadline and the hardware, and the
    /// only real latency/robustness trade here.
    pub device_buffer_frames: Option<u32>,
    pub sharing: Sharing,
    pub performance: Performance,
}

impl Default for StreamConfig {
    fn default() -> Self {
        Self {
            device_id: None,
            channels: 2,
            sample_rate: None,
            block_frames: 512,
            format: None,
            device_buffer_capacity_frames: None,
            device_buffer_frames: None,
            // AAudio's own default when a client asks for nothing, and the mode the
            // PulseAudio sink this was measured against uses. Exclusive is a request
            // that shortens the path and narrows what can be recovered from.
            sharing: Sharing::Shared,
            performance: Performance::LowLatency,
        }
    }
}

/// What the device actually granted — read back from the device, never assumed
/// from what was requested.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Granted {
    pub device_id: i32,
    /// The rate the device plays at — its own answer, not the client's request.
    pub sample_rate: i32,
    /// The rate the client writes at. When it differs from [`Self::sample_rate`] the
    /// engine converts, so a route answering with its native rate is not a repitch.
    pub client_sample_rate: i32,
    pub device_channels: u16,
    pub client_channels: u16,
    /// One device burst: the period the HAL wakes on, and the granularity of
    /// [`StreamConfig::block_frames`].
    pub frames_per_burst: u32,
    /// Frames the device's ring holds.
    pub device_buffer_frames: u32,
    /// Frames the client is handed per callback (a whole number of bursts).
    pub block_frames: u32,
    /// What the device granted, not what was asked for: `Shared` here means an
    /// `Exclusive` request was refused.
    pub sharing: Sharing,
    pub performance: Performance,
    pub format: Format,
    /// Frames between a sample being handed to the device and it being audible,
    /// as the device itself accounts for it.
    pub output_latency_frames: u32,
}

impl Default for Granted {
    fn default() -> Self {
        Self {
            device_id: 0,
            sample_rate: 0,
            client_sample_rate: 0,
            device_channels: 0,
            client_channels: 0,
            frames_per_burst: 0,
            device_buffer_frames: 0,
            block_frames: 0,
            sharing: Sharing::Shared,
            performance: Performance::PowerSaving,
            format: Format::Float32,
            output_latency_frames: 0,
        }
    }
}

/// What went wrong.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Error {
    /// No such device, or it is gone.
    Unavailable,
    /// The device refused the sample rate, channel count or buffer size asked
    /// for. A refusal is never papered over with a resample.
    Unsupported,
    /// The device is gone; [`Engine::reopen`](crate::Engine::reopen) is how to come back.
    Disconnected,
    /// The engine is in the wrong state for this call.
    InvalidState,
    /// The device could not allocate what was asked for.
    NoMemory,
    /// A device-side failure with no better name.
    Device(i32),
    /// No AAudio on this build (a host build, or pre-API-26).
    UnavailableBackend,
}

impl Error {
    /// The AAudio result this error came from, if any.
    pub const fn code(self) -> Option<i32> {
        match self {
            Error::Device(code) => Some(code),
            Error::Unavailable => Some(-889),
            Error::Unsupported => Some(-898),
            Error::Disconnected => Some(-899),
            Error::InvalidState => Some(-895),
            Error::NoMemory => Some(-887),
            Error::UnavailableBackend => None,
        }
    }
}

/// Counters the client can read at any time, from any thread.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Stats {
    /// Client callbacks served since the stream started.
    pub callbacks: u64,
    /// Frames handed to the device since the stream started.
    pub frames: u64,
    /// Sequence breaks the device reported — its own xrun counter.
    pub device_xruns: u32,
    /// Periods the engine could not fill in time (its own dropouts, counted
    /// independently of the device's).
    pub underruns: u64,
    /// How many times the stream has been reopened.
    pub reopens: u64,
}

/// What the client is told about the block it is filling.
#[derive(Clone, Copy, Debug)]
pub struct RenderContext {
    /// Block-aligned sample position of this block: zero at the first callback
    /// after a start, one block per callback after that. It does not advance
    /// while the stream is paused, and it survives a reopen.
    pub position: u64,
    /// Monotonic nanoseconds at the moment the device asked for this block.
    pub time_ns: u64,
    /// Frames to fill — always [`Granted::block_frames`].
    pub frames: u32,
    /// Channels in the slab, in device order.
    pub channels: u16,
    pub sample_rate: i32,
}

/// A hardware timestamp: a sample position and the time it was played.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Timestamp {
    pub frame_position: i64,
    pub time_ns: i64,
}

/// Pending events, drained by `Engine::take_events`.
///
/// These are the things a client cannot see from `render` because they do not
/// happen on the audio thread, and two of them mean "your timeline broke, act on
/// it".
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Events(u32);

impl Events {
    /// The device went away. Nothing is playing; reopen when one is available.
    pub const DEVICE_LOST: Events = Events(1 << 0);
    /// Playback continued, but the clock is not continuous — a reopen, or a device
    /// swap underneath. Drop any timeline built on sample positions.
    pub const RESYNC: Events = Events(1 << 1);
    /// A period could not be filled in time, so audio was dropped. The stream is
    /// still alive; a larger block or a larger device buffer is the fix.
    pub const UNDERRUN: Events = Events(1 << 2);

    pub const fn empty() -> Self {
        Events(0)
    }

    pub const fn bits(self) -> u32 {
        self.0
    }

    pub const fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }

    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }

    pub(crate) const fn from_bits(bits: u32) -> Self {
        Events(bits)
    }
}

impl core::ops::BitOr for Events {
    type Output = Events;

    fn bitor(self, other: Events) -> Events {
        Events(self.0 | other.0)
    }
}

impl core::ops::BitOrAssign for Events {
    fn bitor_assign(&mut self, other: Events) {
        self.0 |= other.0;
    }
}
