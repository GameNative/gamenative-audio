//! The C ABI, for callers that are not Rust: `include/gnaudio.h` is its
//! declaration.
//!
//! The shape is a handle and a set of calls on it, with the client's render
//! callback crossing the boundary as a plain C function. Two things are the C
//! caller's responsibility, and both are stated in the header: the callback runs on
//! the device's real-time thread, and the `user` pointer is only ever touched from
//! there.
//!
//! The Rust side of the bridge is deliberately thin — one adapter implementing
//! [`Client`] — so that all of the semantics stay in one place, tested once.

use core::ffi::{c_char, c_int, c_void};

use crate::engine::{Client, Engine};
use crate::types::{Error, Granted, RenderContext, StreamConfig};

/// `GNAUDIO_RESULT_OK`.
pub const RESULT_OK: c_int = 0;
pub const RESULT_UNAVAILABLE: c_int = 1;
pub const RESULT_UNSUPPORTED: c_int = 2;
pub const RESULT_DISCONNECTED: c_int = 3;
pub const RESULT_INVALID_STATE: c_int = 4;
pub const RESULT_NO_MEMORY: c_int = 5;
pub const RESULT_DEVICE: c_int = 6;
pub const RESULT_NO_BACKEND: c_int = 7;
pub const RESULT_INVALID_ARGUMENT: c_int = 8;

fn result_of(error: Error) -> c_int {
    match error {
        Error::Unavailable => RESULT_UNAVAILABLE,
        Error::Unsupported => RESULT_UNSUPPORTED,
        Error::Disconnected => RESULT_DISCONNECTED,
        Error::InvalidState => RESULT_INVALID_STATE,
        Error::NoMemory => RESULT_NO_MEMORY,
        Error::Device(_) => RESULT_DEVICE,
        Error::UnavailableBackend => RESULT_NO_BACKEND,
    }
}

/// `gnaudio_config`, as declared in the header.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct CConfig {
    /// Device id, or `GNAUDIO_DEFAULT_DEVICE` (0) for the default.
    pub device_id: i32,
    pub channels: i32,
    /// 0 for the device's own rate.
    pub sample_rate: i32,
    pub block_frames: i32,
    /// 0 to let the device choose its own ring size.
    pub device_buffer_frames: i32,
    /// `GNAUDIO_SHARING_EXCLUSIVE` (0) or `GNAUDIO_SHARING_SHARED` (1).
    pub sharing: i32,
    /// `GNAUDIO_PERFORMANCE_LOW_LATENCY` (0) or `GNAUDIO_PERFORMANCE_POWER_SAVING` (1).
    pub performance: i32,
}

/// `gnaudio_render_info`, as declared in the header.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct CRenderInfo {
    pub position: u64,
    pub time_ns: u64,
    pub frames: u32,
    pub channels: u32,
    pub sample_rate: i32,
    pub _reserved: i32,
}

/// `gnaudio_granted`, as declared in the header.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct CGranted {
    pub device_id: i32,
    pub sample_rate: i32,
    pub device_channels: i32,
    pub client_channels: i32,
    pub frames_per_burst: i32,
    pub device_buffer_frames: i32,
    pub block_frames: i32,
    pub sharing: i32,
    pub performance: i32,
    pub format: i32,
    pub output_latency_frames: i32,
}

/// `gnaudio_stats`, as declared in the header.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct CStats {
    pub callbacks: u64,
    pub frames: u64,
    pub device_xruns: u32,
    pub underruns: u64,
    pub reopens: u64,
}

pub type RenderFn = extern "C" fn(user: *mut c_void, info: *const CRenderInfo, out: *mut f32);

struct CBridge {
    render: RenderFn,
    user: *mut c_void,
}

// SAFETY: the C side documented that the callback is safe to call from the
// device's real-time thread, and that `user` is only touched from there.
unsafe impl Send for CBridge {}

impl Client for CBridge {
    fn render(&mut self, ctx: &RenderContext, out: &mut [f32]) -> u32 {
        let info = CRenderInfo {
            position: ctx.position,
            time_ns: ctx.time_ns,
            frames: ctx.frames,
            channels: ctx.channels as u32,
            sample_rate: ctx.sample_rate,
            _reserved: 0,
        };
        (self.render)(self.user, &info, out.as_mut_ptr());
        // The C ABI's render callback returns nothing, so a C client cannot say how much
        // of the block it filled. Assume all of it: that is what a C client that does not
        // know about the shortfall has always meant, and it keeps the ABI unchanged.
        ctx.frames
    }
}

/// Restores a handle made by [`gnaudio_open`].
///
/// # Safety
/// `engine` must be a pointer from [`gnaudio_open`] that has not been closed.
unsafe fn as_engine<'a>(engine: *mut c_void) -> Option<&'a Engine> {
    if engine.is_null() {
        None
    } else {
        // SAFETY: the caller guarantees it came from `gnaudio_open` and is live.
        Some(unsafe { &*(engine as *const Engine) })
    }
}

/// Opens a device and prepares to render through `render`. Returns null on
/// failure, with the reason in `result` when it is not null.
///
/// # Safety
/// `config` must point at a valid `gnaudio_config`, and `render` must be a valid
/// function pointer.
#[no_mangle]
pub unsafe extern "C" fn gnaudio_open(
    config: *const CConfig,
    render: Option<RenderFn>,
    user: *mut c_void,
    result: *mut c_int,
) -> *mut c_void {
    let set = |code: c_int| {
        if !result.is_null() {
            // SAFETY: the caller passed an out-parameter it owns.
            unsafe { *result = code };
        }
    };
    if config.is_null() || render.is_none() {
        set(RESULT_INVALID_ARGUMENT);
        return core::ptr::null_mut();
    }
    // SAFETY: null-checked above.
    let config = unsafe { *config };
    // A C caller gets told what is wrong rather than having a nonsense value
    // quietly reshaped into something plausible.
    if config.channels < 1 || config.block_frames < 1 || config.channels > u16::MAX as i32 {
        set(RESULT_INVALID_ARGUMENT);
        return core::ptr::null_mut();
    }
    let stream_config = StreamConfig {
        device_id: match config.device_id {
            0 => None,
            id => Some(id),
        },
        channels: config.channels as u16,
        sample_rate: match config.sample_rate {
            0 => None,
            rate => Some(rate),
        },
        block_frames: config.block_frames as u32,
        // The C surface has no field for this, and does not need one: a caller that wants
        // room asks AAudio for it through the route's own default.
        format: None,
        device_buffer_capacity_frames: None,
        device_buffer_frames: match config.device_buffer_frames {
            0 => None,
            frames => Some(frames as u32),
        },
        sharing: if config.sharing == 0 {
            crate::types::Sharing::Exclusive
        } else {
            crate::types::Sharing::Shared
        },
        performance: if config.performance == 0 {
            crate::types::Performance::LowLatency
        } else {
            crate::types::Performance::PowerSaving
        },
    };

    let client = CBridge {
        // SAFETY: checked above.
        render: unsafe { render.unwrap_unchecked() },
        user,
    };
    match Engine::open(Box::new(client), stream_config) {
        Ok(engine) => {
            set(RESULT_OK);
            Box::into_raw(Box::new(engine)) as *mut c_void
        }
        Err(error) => {
            set(result_of(error));
            core::ptr::null_mut()
        }
    }
}

/// # Safety
/// `engine` must be a live handle from [`gnaudio_open`].
#[no_mangle]
pub unsafe extern "C" fn gnaudio_start(engine: *mut c_void) -> c_int {
    // SAFETY: the caller guarantees a live handle.
    match unsafe { as_engine(engine) } {
        Some(engine) => engine.start().err().map_or(RESULT_OK, result_of),
        None => RESULT_INVALID_STATE,
    }
}

/// # Safety
/// `engine` must be a live handle from [`gnaudio_open`].
#[no_mangle]
pub unsafe extern "C" fn gnaudio_pause(engine: *mut c_void) -> c_int {
    // SAFETY: the caller guarantees a live handle.
    match unsafe { as_engine(engine) } {
        Some(engine) => engine.pause().err().map_or(RESULT_OK, result_of),
        None => RESULT_INVALID_STATE,
    }
}

/// # Safety
/// `engine` must be a live handle from [`gnaudio_open`].
#[no_mangle]
pub unsafe extern "C" fn gnaudio_resume(engine: *mut c_void) -> c_int {
    // SAFETY: the caller guarantees a live handle.
    match unsafe { as_engine(engine) } {
        Some(engine) => engine.resume().err().map_or(RESULT_OK, result_of),
        None => RESULT_INVALID_STATE,
    }
}

/// # Safety
/// `engine` must be a live handle from [`gnaudio_open`].
#[no_mangle]
pub unsafe extern "C" fn gnaudio_stop(engine: *mut c_void) -> c_int {
    // SAFETY: the caller guarantees a live handle.
    match unsafe { as_engine(engine) } {
        Some(engine) => engine.stop().err().map_or(RESULT_OK, result_of),
        None => RESULT_INVALID_STATE,
    }
}

/// Rebuilds the device after it disappeared. `GNAUDIO_ERROR_DISCONNECTED` means it
/// is not back yet.
///
/// # Safety
/// `engine` must be a live handle from [`gnaudio_open`].
#[no_mangle]
pub unsafe extern "C" fn gnaudio_reopen(engine: *mut c_void) -> c_int {
    // SAFETY: the caller guarantees a live handle.
    match unsafe { as_engine(engine) } {
        Some(engine) => engine.reopen().err().map_or(RESULT_OK, result_of),
        None => RESULT_INVALID_STATE,
    }
}

/// # Safety
/// `engine` must be a live handle from [`gnaudio_open`], and `out` a valid
/// `gnaudio_granted`.
#[no_mangle]
pub unsafe extern "C" fn gnaudio_granted(engine: *mut c_void, out: *mut CGranted) -> c_int {
    if out.is_null() {
        return RESULT_INVALID_ARGUMENT;
    }
    // SAFETY: the caller guarantees live pointers.
    let Some(engine) = (unsafe { as_engine(engine) }) else {
        return RESULT_INVALID_STATE;
    };
    let granted: Granted = engine.granted();
    // SAFETY: null-checked above.
    unsafe {
        *out = CGranted {
            device_id: granted.device_id,
            sample_rate: granted.sample_rate,
            device_channels: granted.device_channels as i32,
            client_channels: granted.client_channels as i32,
            frames_per_burst: granted.frames_per_burst as i32,
            device_buffer_frames: granted.device_buffer_frames as i32,
            block_frames: granted.block_frames as i32,
            sharing: match granted.sharing {
                crate::types::Sharing::Exclusive => 0,
                crate::types::Sharing::Shared => 1,
            },
            performance: match granted.performance {
                crate::types::Performance::LowLatency => 0,
                crate::types::Performance::PowerSaving => 1,
            },
            format: granted.format as i32,
            output_latency_frames: granted.output_latency_frames as i32,
        }
    };
    RESULT_OK
}

/// # Safety
/// `engine` must be a live handle from [`gnaudio_open`], and `out` a valid
/// `gnaudio_stats`.
#[no_mangle]
pub unsafe extern "C" fn gnaudio_stats(engine: *mut c_void, out: *mut CStats) -> c_int {
    if out.is_null() {
        return RESULT_INVALID_ARGUMENT;
    }
    // SAFETY: the caller guarantees live pointers.
    let Some(engine) = (unsafe { as_engine(engine) }) else {
        return RESULT_INVALID_STATE;
    };
    let stats = engine.stats();
    // SAFETY: null-checked above.
    unsafe {
        *out = CStats {
            callbacks: stats.callbacks,
            frames: stats.frames,
            device_xruns: stats.device_xruns,
            underruns: stats.underruns,
            reopens: stats.reopens,
        }
    };
    RESULT_OK
}

/// Frames between a sample being handed to the device and it being audible, right
/// now. Negative when there is no device.
///
/// # Safety
/// `engine` must be a live handle from [`gnaudio_open`].
#[no_mangle]
pub unsafe extern "C" fn gnaudio_latency_frames(engine: *mut c_void) -> i32 {
    // SAFETY: the caller guarantees a live handle.
    match unsafe { as_engine(engine) } {
        Some(engine) => engine.latency_frames() as i32,
        None => -1,
    }
}

/// Drains pending `GNAUDIO_EVENT_*` bits.
///
/// # Safety
/// `engine` must be a live handle from [`gnaudio_open`].
#[no_mangle]
pub unsafe extern "C" fn gnaudio_take_events(engine: *mut c_void) -> c_int {
    // SAFETY: the caller guarantees a live handle.
    match unsafe { as_engine(engine) } {
        Some(engine) => engine.take_events().bits() as c_int,
        None => 0,
    }
}

/// The device's own timestamp, when it can produce one.
///
/// # Safety
/// `engine` must be a live handle from [`gnaudio_open`], and both out-parameters
/// valid or null.
#[no_mangle]
pub unsafe extern "C" fn gnaudio_timestamp(
    engine: *mut c_void,
    frame_position: *mut i64,
    time_ns: *mut i64,
) -> c_int {
    // SAFETY: the caller guarantees a live handle.
    let Some(engine) = (unsafe { as_engine(engine) }) else {
        return RESULT_INVALID_STATE;
    };
    let Some(stamp) = engine.timestamp() else {
        return RESULT_UNAVAILABLE;
    };
    // SAFETY: the caller documented the out-parameters.
    unsafe {
        if !frame_position.is_null() {
            *frame_position = stamp.frame_position;
        }
        if !time_ns.is_null() {
            *time_ns = stamp.time_ns;
        }
    }
    RESULT_OK
}

/// Block-aligned position, or -1 when there is no device.
///
/// # Safety
/// `engine` must be a live handle from [`gnaudio_open`].
#[no_mangle]
pub unsafe extern "C" fn gnaudio_sample_position(engine: *mut c_void) -> i64 {
    // SAFETY: the caller guarantees a live handle.
    match unsafe { as_engine(engine) } {
        Some(engine) => engine.sample_position().0 as i64,
        None => -1,
    }
}

/// Closes the device and frees the handle. Safe on null; do not use the handle
/// afterwards.
///
/// # Safety
/// `engine` must be a handle from [`gnaudio_open`], or null, and not used again.
#[no_mangle]
pub unsafe extern "C" fn gnaudio_close(engine: *mut c_void) {
    if engine.is_null() {
        return;
    }
    // SAFETY: the caller guarantees the handle came from `gnaudio_open` and that
    // it is not used again.
    let engine = unsafe { Box::from_raw(engine as *mut Engine) };
    engine.close();
}

/// Turns the engine's own logcat output on or off.
#[no_mangle]
pub extern "C" fn gnaudio_set_logging(enabled: c_int) {
    crate::trace::set_logging(enabled != 0);
}

/// A stable description of a result code, for logging.
#[no_mangle]
pub extern "C" fn gnaudio_result_text(code: c_int) -> *const c_char {
    let text: &[u8] = match code {
        RESULT_OK => b"ok\0",
        RESULT_UNAVAILABLE => b"unavailable\0",
        RESULT_UNSUPPORTED => b"unsupported\0",
        RESULT_DISCONNECTED => b"disconnected\0",
        RESULT_INVALID_STATE => b"invalid state\0",
        RESULT_NO_MEMORY => b"out of memory\0",
        RESULT_DEVICE => b"device error\0",
        RESULT_NO_BACKEND => b"no backend\0",
        RESULT_INVALID_ARGUMENT => b"invalid argument\0",
        _ => b"unknown\0",
    };
    text.as_ptr() as *const c_char
}
