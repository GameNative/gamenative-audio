//! The C boundary's layout, checked against the header that declares it.
//!
//! Two ways this drifts silently, both caught here: a struct whose Rust and C
//! layouts differ (a C caller then reads garbage), and a constant whose two values
//! differ (a caller then asks for the wrong thing). The header is parsed rather
//! than trusted, so editing one side alone fails the build.

use std::mem::{offset_of, size_of};

use gnaudio::c_api::*;
use gnaudio::types::{Format, Performance, Sharing};

const HEADER: &str = include_str!("../include/gnaudio.h");

/// The value of `#define <name> <value>` in the header, where the value is an
/// integer or `(1 << n)`.
fn define(name: &str) -> i64 {
    for line in HEADER.lines() {
        let line = line.trim();
        let Some(rest) = line.strip_prefix("#define ") else {
            continue;
        };
        let mut parts = rest.split_whitespace();
        if parts.next() != Some(name) {
            continue;
        }
        let value = parts.collect::<Vec<_>>().join(" ");
        let value = value.trim();
        if let Some(shift) = value.strip_prefix("(1 << ") {
            let shift = shift.trim_end_matches(')').trim();
            return 1i64 << shift.parse::<u32>().unwrap();
        }
        return value
            .parse::<i64>()
            .unwrap_or_else(|_| panic!("{name} in the header is not an integer: {value}"));
    }
    panic!("{name} is not defined in include/gnaudio.h");
}

#[test]
fn result_codes_agree_with_the_header() {
    for (name, code) in [
        ("GNAUDIO_RESULT_OK", RESULT_OK),
        ("GNAUDIO_RESULT_UNAVAILABLE", RESULT_UNAVAILABLE),
        ("GNAUDIO_RESULT_UNSUPPORTED", RESULT_UNSUPPORTED),
        ("GNAUDIO_RESULT_DISCONNECTED", RESULT_DISCONNECTED),
        ("GNAUDIO_RESULT_INVALID_STATE", RESULT_INVALID_STATE),
        ("GNAUDIO_RESULT_NO_MEMORY", RESULT_NO_MEMORY),
        ("GNAUDIO_RESULT_DEVICE", RESULT_DEVICE),
        ("GNAUDIO_RESULT_NO_BACKEND", RESULT_NO_BACKEND),
        ("GNAUDIO_RESULT_INVALID_ARGUMENT", RESULT_INVALID_ARGUMENT),
    ] {
        assert_eq!(define(name), code as i64, "{name}");
    }
}

#[test]
fn modes_agree_with_the_header() {
    // A caller in another language has nothing but these numbers.
    assert_eq!(define("GNAUDIO_DEFAULT_DEVICE"), 0);
    assert_eq!(
        define("GNAUDIO_SHARING_EXCLUSIVE"),
        Sharing::Exclusive as i64
    );
    assert_eq!(define("GNAUDIO_SHARING_SHARED"), Sharing::Shared as i64);
    assert_eq!(
        define("GNAUDIO_PERFORMANCE_LOW_LATENCY"),
        Performance::LowLatency as i64
    );
    assert_eq!(
        define("GNAUDIO_PERFORMANCE_POWER_SAVING"),
        Performance::PowerSaving as i64
    );
    assert_eq!(define("GNAUDIO_FORMAT_FLOAT32"), Format::Float32 as i64);
}

#[test]
fn event_bits_agree_with_the_header() {
    use gnaudio::types::Events;
    for (name, event) in [
        ("GNAUDIO_EVENT_DEVICE_LOST", Events::DEVICE_LOST),
        ("GNAUDIO_EVENT_RESYNC", Events::RESYNC),
        ("GNAUDIO_EVENT_UNDERRUN", Events::UNDERRUN),
    ] {
        assert_eq!(define(name), event.bits() as i64, "{name}");
    }
}

#[test]
fn the_config_struct_is_the_one_the_header_declares() {
    assert_eq!(size_of::<CConfig>(), 7 * size_of::<i32>());
    assert_eq!(offset_of!(CConfig, device_id), 0);
    assert_eq!(offset_of!(CConfig, channels), 4);
    assert_eq!(offset_of!(CConfig, sample_rate), 8);
    assert_eq!(offset_of!(CConfig, block_frames), 12);
    assert_eq!(offset_of!(CConfig, device_buffer_frames), 16);
    assert_eq!(offset_of!(CConfig, sharing), 20);
    assert_eq!(offset_of!(CConfig, performance), 24);
}

#[test]
fn the_render_info_struct_is_the_one_the_header_declares() {
    // What the callback is handed must be readable by a C client field by field.
    assert_eq!(offset_of!(CRenderInfo, position), 0);
    assert_eq!(offset_of!(CRenderInfo, time_ns), 8);
    assert_eq!(offset_of!(CRenderInfo, frames), 16);
    assert_eq!(offset_of!(CRenderInfo, channels), 20);
    assert_eq!(offset_of!(CRenderInfo, sample_rate), 24);
    assert_eq!(offset_of!(CRenderInfo, _reserved), 28);
    assert_eq!(size_of::<CRenderInfo>(), 32, "aligned to its u64 fields");
}

#[test]
fn the_granted_struct_is_the_one_the_header_declares() {
    assert_eq!(size_of::<CGranted>(), 11 * size_of::<i32>());
    assert_eq!(offset_of!(CGranted, device_id), 0);
    assert_eq!(offset_of!(CGranted, sample_rate), 4);
    assert_eq!(offset_of!(CGranted, device_channels), 8);
    assert_eq!(offset_of!(CGranted, client_channels), 12);
    assert_eq!(offset_of!(CGranted, frames_per_burst), 16);
    assert_eq!(offset_of!(CGranted, device_buffer_frames), 20);
    assert_eq!(offset_of!(CGranted, block_frames), 24);
    assert_eq!(offset_of!(CGranted, sharing), 28);
    assert_eq!(offset_of!(CGranted, performance), 32);
    assert_eq!(offset_of!(CGranted, format), 36);
    assert_eq!(offset_of!(CGranted, output_latency_frames), 40);
}

#[test]
fn the_stats_struct_is_the_one_the_header_declares() {
    // The one struct with a hole in it: a u32 between two u64s.
    assert_eq!(offset_of!(CStats, callbacks), 0);
    assert_eq!(offset_of!(CStats, frames), 8);
    assert_eq!(offset_of!(CStats, device_xruns), 16);
    assert_eq!(offset_of!(CStats, underruns), 24);
    assert_eq!(offset_of!(CStats, reopens), 32);
    assert_eq!(size_of::<CStats>(), 40);
}

#[test]
fn every_exported_symbol_the_header_declares_exists() {
    // Taking the address of each is what links them: a header promising a function
    // the library does not export fails here rather than at a caller's link.
    let exported: &[(&str, *const ())] = &[
        ("gnaudio_open", gnaudio_open as *const ()),
        ("gnaudio_start", gnaudio_start as *const ()),
        ("gnaudio_pause", gnaudio_pause as *const ()),
        ("gnaudio_resume", gnaudio_resume as *const ()),
        ("gnaudio_stop", gnaudio_stop as *const ()),
        ("gnaudio_reopen", gnaudio_reopen as *const ()),
        ("gnaudio_close", gnaudio_close as *const ()),
        ("gnaudio_granted", gnaudio_granted as *const ()),
        ("gnaudio_stats", gnaudio_stats as *const ()),
        (
            "gnaudio_latency_frames",
            gnaudio_latency_frames as *const (),
        ),
        (
            "gnaudio_sample_position",
            gnaudio_sample_position as *const (),
        ),
        ("gnaudio_timestamp", gnaudio_timestamp as *const ()),
        ("gnaudio_take_events", gnaudio_take_events as *const ()),
        ("gnaudio_set_logging", gnaudio_set_logging as *const ()),
        ("gnaudio_result_text", gnaudio_result_text as *const ()),
    ];
    for (name, address) in exported {
        assert!(!address.is_null(), "{name}");
        assert!(
            HEADER.contains(name),
            "{name} is exported but not declared in include/gnaudio.h"
        );
    }
}
