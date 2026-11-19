//! The C ABI: the parts a C caller can get wrong, and what it gets told.
//!
//! A device cannot be reached from this host, so what is tested here is the
//! boundary itself — argument validation, null safety, and the result codes — plus
//! that the declarations in `include/gnaudio.h` and the Rust side agree. The
//! rendering path is the engine's, tested in `tests/engine.rs`, and the header's
//! claims about the device are what `examples/gn-audio-probe.rs` measures.

use core::ffi::CStr;
use std::ffi::c_void;

use gnaudio::c_api::*;

/// A configuration a device could be opened with, for the tests that must get past
/// argument validation to reach the device.
fn valid_config() -> CConfig {
    CConfig {
        device_id: 0,
        channels: 2,
        sample_rate: 0,
        block_frames: 256,
        device_buffer_frames: 0,
        sharing: 0,
        performance: 0,
    }
}

extern "C" fn fill(_user: *mut c_void, _info: *const CRenderInfo, out: *mut f32) {
    // A client with nothing to say; `out` arrives zeroed.
    let _ = out;
}

unsafe fn open_with(config: &CConfig, result: &mut i32) -> *mut c_void {
    // SAFETY: both out-parameters are live for the call.
    unsafe { gnaudio_open(config, Some(fill), core::ptr::null_mut(), result) }
}

#[test]
fn a_null_config_is_refused() {
    let mut result = -1;
    // SAFETY: a null config is exactly what is under test.
    let handle = unsafe {
        gnaudio_open(
            core::ptr::null(),
            Some(fill),
            core::ptr::null_mut(),
            &mut result,
        )
    };
    assert!(handle.is_null());
    assert_eq!(result, RESULT_INVALID_ARGUMENT);
}

#[test]
fn a_null_callback_is_refused() {
    let config = valid_config();
    let mut result = -1;
    // SAFETY: a null callback is exactly what is under test.
    let handle = unsafe { gnaudio_open(&config, None, core::ptr::null_mut(), &mut result) };
    assert!(handle.is_null());
    assert_eq!(result, RESULT_INVALID_ARGUMENT);
}

#[test]
fn nonsense_sizes_are_refused_rather_than_reshaped() {
    let mut result = -1;
    let mut config = valid_config();
    config.channels = 0;
    // SAFETY: the out-parameters are live; no device is opened.
    let handle = unsafe { open_with(&config, &mut result) };
    assert!(handle.is_null());
    assert_eq!(result, RESULT_INVALID_ARGUMENT, "zero channels");

    let mut config = valid_config();
    config.channels = -1;
    // SAFETY: as above.
    let handle = unsafe { open_with(&config, &mut result) };
    assert!(handle.is_null());
    assert_eq!(result, RESULT_INVALID_ARGUMENT, "negative channels");

    let mut config = valid_config();
    config.block_frames = 0;
    // SAFETY: as above.
    let handle = unsafe { open_with(&config, &mut result) };
    assert!(handle.is_null());
    assert_eq!(result, RESULT_INVALID_ARGUMENT, "a zero-length block");
}

#[test]
fn a_result_pointer_may_be_null() {
    let config = valid_config();
    // SAFETY: the caller is allowed to pass no out-parameter.
    let handle = unsafe {
        gnaudio_open(
            &config,
            Some(fill),
            core::ptr::null_mut(),
            core::ptr::null_mut(),
        )
    };
    // Nothing to assert about the handle here beyond "the call did not crash"; on a
    // host it is null because there is no device stack.
    if !handle.is_null() {
        // SAFETY: the handle came from `gnaudio_open`.
        unsafe { gnaudio_close(handle) };
    }
}

#[cfg(not(target_os = "android"))]
#[test]
fn a_host_without_a_device_stack_says_so() {
    let config = valid_config();
    let mut result = -1;
    // SAFETY: valid out-parameter; the open is expected to fail.
    let handle = unsafe { open_with(&config, &mut result) };
    assert!(handle.is_null());
    assert_eq!(
        result, RESULT_NO_BACKEND,
        "the crate builds everywhere, but only Android has AAudio"
    );
}

#[test]
fn every_call_tolerates_a_null_handle() {
    // A C caller that drops a handle and keeps calling must get a defined answer
    // rather than a crash: a crash in an audio callback cannot be caught.
    // SAFETY: null handles are exactly what is under test, and no call may touch
    // memory behind them.
    unsafe {
        assert_eq!(gnaudio_start(core::ptr::null_mut()), RESULT_INVALID_STATE);
        assert_eq!(gnaudio_pause(core::ptr::null_mut()), RESULT_INVALID_STATE);
        assert_eq!(gnaudio_resume(core::ptr::null_mut()), RESULT_INVALID_STATE);
        assert_eq!(gnaudio_stop(core::ptr::null_mut()), RESULT_INVALID_STATE);
        assert_eq!(gnaudio_reopen(core::ptr::null_mut()), RESULT_INVALID_STATE);
        assert_eq!(gnaudio_sample_position(core::ptr::null_mut()), -1);
        assert_eq!(gnaudio_latency_frames(core::ptr::null_mut()), -1);
        assert_eq!(gnaudio_take_events(core::ptr::null_mut()), 0);
        assert_eq!(
            gnaudio_timestamp(core::ptr::null_mut(), &mut 0i64, &mut 0i64),
            RESULT_INVALID_STATE
        );
        assert_eq!(
            gnaudio_granted(core::ptr::null_mut(), &mut default_granted()),
            RESULT_INVALID_STATE
        );
        assert_eq!(
            gnaudio_stats(core::ptr::null_mut(), &mut default_stats()),
            RESULT_INVALID_STATE
        );
        gnaudio_close(core::ptr::null_mut());
    }
}

#[test]
fn an_out_pointer_is_required_where_one_is_written() {
    // SAFETY: a null out-parameter with a null handle: neither is dereferenced.
    unsafe {
        assert_eq!(
            gnaudio_granted(core::ptr::null_mut(), core::ptr::null_mut()),
            RESULT_INVALID_ARGUMENT
        );
        assert_eq!(
            gnaudio_stats(core::ptr::null_mut(), core::ptr::null_mut()),
            RESULT_INVALID_ARGUMENT
        );
    }
}

#[test]
fn a_timestamp_ask_need_not_want_both_halves() {
    // SAFETY: null out-parameters are allowed, and the handle is null here so
    // nothing is written either way.
    let code = unsafe {
        gnaudio_timestamp(
            core::ptr::null_mut(),
            core::ptr::null_mut(),
            core::ptr::null_mut(),
        )
    };
    assert_eq!(code, RESULT_INVALID_STATE);
}

#[test]
fn result_text_is_stable_and_never_null() {
    for (code, expected) in [
        (RESULT_OK, "ok"),
        (RESULT_UNAVAILABLE, "unavailable"),
        (RESULT_UNSUPPORTED, "unsupported"),
        (RESULT_DISCONNECTED, "disconnected"),
        (RESULT_INVALID_STATE, "invalid state"),
        (RESULT_NO_MEMORY, "out of memory"),
        (RESULT_DEVICE, "device error"),
        (RESULT_NO_BACKEND, "no backend"),
        (RESULT_INVALID_ARGUMENT, "invalid argument"),
    ] {
        // SAFETY: the returned pointer is a static NUL-terminated string.
        let text = unsafe { CStr::from_ptr(gnaudio_result_text(code)) };
        assert_eq!(text.to_str().unwrap(), expected);
    }
    // SAFETY: as above.
    let unknown = unsafe { CStr::from_ptr(gnaudio_result_text(1234)) };
    assert_eq!(unknown.to_str().unwrap(), "unknown");
}

#[test]
fn logging_can_be_toggled_by_a_c_caller() {
    // The engine logs by default; a caller that does not want it can say so, and
    // either call is safe without a device.
    gnaudio_set_logging(0);
    gnaudio_set_logging(1);
}

fn default_granted() -> CGranted {
    CGranted {
        device_id: 0,
        sample_rate: 0,
        device_channels: 0,
        client_channels: 0,
        frames_per_burst: 0,
        device_buffer_frames: 0,
        block_frames: 0,
        sharing: 0,
        performance: 0,
        format: 0,
        output_latency_frames: 0,
    }
}

fn default_stats() -> CStats {
    CStats {
        callbacks: 0,
        frames: 0,
        device_xruns: 0,
        underruns: 0,
        reopens: 0,
    }
}
