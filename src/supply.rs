//! How much of a device period this crate can supply, and in what units.
//!
//! Both functions are here rather than beside the device code for one reason: the device
//! module is Android-only, and the arithmetic in it is exactly what went wrong once. A
//! callback that is asked for more frames than it has must supply what it has and silence
//! the rest — never abandon the period, because an abandoned period is a stream that
//! reports the right rate, an active track and zero underruns while making no sound at
//! all. That is not a hypothetical: it is what made this crate ship a blocking-write
//! backend for a while on the false conclusion that AAudio's callback path did not work
//! on this hardware.
//!
//! So the decision is a pure function, in a file every platform compiles, with tests that
//! run on any host.

/// How much of a requested period can be supplied from what is held.
///
/// Returns `(frames to fill from the pipeline, frames of silence after them)`. The second
/// number is zero whenever the pipeline can cover the whole period.
///
/// * A device that asks for more than is held gets what is held, then silence. That is a
///   dropout, and it is counted as one, but it is a dropout in proportion rather than a
///   silent stream.
/// * A device that asks for nothing gets nothing, and owes nothing.
// Only the Android device backend calls these, so on a host build they are reachable
// from the tests below and nowhere else. That is the intent, not an oversight.
#[cfg_attr(not(target_os = "android"), allow(dead_code))]
pub fn supply_plan(asked: usize, held: usize) -> (usize, usize) {
    let supplied = asked.min(held);
    (supplied, asked - supplied)
}

/// How much buffer the device should hold, in the reference sink's low-latency mode.
///
/// `frames_per_burst × 2`, clamped to what the route can allocate — the expression from
/// `pa_create_aaudio_stream`:
///
/// ```c
/// int32_t bufferSize = u->frames_per_burst * 2;
/// if (bufferSize > bufferCapacity) bufferSize = bufferCapacity;
/// AAudioStream_setBufferSizeInFrames(u->stream, bufferSize);
/// ```
///
/// Two things had to be established before this could be trusted. The branch is gated on
/// `u->low_latency`, which `pa__init` defaults to false but GameNative's
/// `PulseAudioComponent` enables, so the sink that demonstrably survives a Bluetooth switch
/// is running *this* expression — verified in its own log on this device: `AAudio buffer size
/// set to 384 frames (burst: 192, capacity: 1536)`. And a burst is the route's own unit: it
/// is how often the HAL wakes for that route, so two of them is a duration the route defined
/// rather than one imposed on it.
#[cfg_attr(not(target_os = "android"), allow(dead_code))]
pub fn device_buffer_frames(burst: u32, capacity: u32) -> u32 {
    let burst = burst.max(1);
    burst.saturating_mul(2).min(capacity.max(burst))
}

/// How much larger the device buffer should become after the device lost audio.
///
/// One burst at a time, never past what the route can allocate and never past the ceiling
/// the pipeline above this device imposes. This is `sink_process_render`'s rule — it raises
/// the buffer by `frames_per_burst` each time `AAudioStream_getXRunCount` moves — and it is
/// what makes a route that needs more than two bursts get there without anyone guessing a
/// number. Returns `current` unchanged when there is nowhere to grow.
#[cfg_attr(not(target_os = "android"), allow(dead_code))]
pub fn grown_buffer_frames(current: u32, burst: u32, ceiling: u32, capacity: u32) -> u32 {
    let limit = ceiling.min(capacity);
    if current >= limit {
        return current;
    }
    current.saturating_add(burst.max(1)).min(limit)
}

/// Bytes one sample occupies, in a format numbered as `aaudio_format_t` numbers them.
///
/// `I24` is reported as four because that is what this crate writes for it: `AAudio`'s
/// packed 24-bit form is not something the engine asks for, and pretending to support it
/// by writing three bytes would be worse than the one number being generous.
#[cfg_attr(not(target_os = "android"), allow(dead_code))]
pub fn bytes_per_sample(format: crate::types::Format) -> usize {
    use crate::types::Format;
    match format {
        Format::Float32 | Format::Int32 | Format::Int24 => 4,
        Format::Int16 => 2,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::Format;

    #[test]
    fn a_period_larger_than_the_pipeline_is_clamped_not_abandoned() {
        // The exact shapes measured on this hardware: device periods of 1 152 and up
        // against a 480 frame buffer.
        assert_eq!(supply_plan(1_152, 480), (480, 672));
        assert_eq!(supply_plan(3_840, 1_152), (1_152, 2_688));
    }

    #[test]
    fn a_period_the_pipeline_can_cover_leaves_no_silence() {
        assert_eq!(supply_plan(96, 3_840), (96, 0));
        assert_eq!(supply_plan(3_840, 3_840), (3_840, 0));
    }

    #[test]
    fn nothing_asked_is_nothing_supplied() {
        assert_eq!(supply_plan(0, 480), (0, 0));
        assert_eq!(supply_plan(480, 0), (0, 480));
    }

    #[test]
    fn a_sample_is_the_size_the_device_asked_for() {
        assert_eq!(bytes_per_sample(Format::Float32), 4);
        assert_eq!(bytes_per_sample(Format::Int32), 4);
        assert_eq!(bytes_per_sample(Format::Int16), 2);
    }

    #[test]
    fn the_device_buffer_is_two_bursts_as_the_reference_sets_it() {
        // The figure the reference sink sets on this device, from its own log: burst 192,
        // buffer 384, capacity 1 536.
        assert_eq!(device_buffer_frames(192, 1_536), 384);
        // The other device: burst 96, capacity 1 536.
        assert_eq!(device_buffer_frames(96, 1_536), 192);
        // Never above what the route can allocate, never below one burst, never zero.
        assert_eq!(device_buffer_frames(192, 200), 200);
        assert_eq!(device_buffer_frames(192, 0), 192);
        assert!(device_buffer_frames(0, 0) >= 1);
    }

    #[test]
    fn a_buffer_that_lost_audio_grows_one_burst_at_a_time_within_its_limits() {
        // 384 on this route, capacity 1 536, and the pipeline above lets it go to 960.
        assert_eq!(grown_buffer_frames(384, 192, 960, 1_536), 576);
        assert_eq!(grown_buffer_frames(576, 192, 960, 1_536), 768);
        assert_eq!(grown_buffer_frames(768, 192, 960, 1_536), 960);
        // At the ceiling already: unchanged, and the same at the route's capacity.
        assert_eq!(grown_buffer_frames(960, 192, 960, 1_536), 960);
        assert_eq!(grown_buffer_frames(1_536, 192, 9_999, 1_536), 1_536);
        // A ceiling below the current size never shrinks the buffer.
        assert_eq!(grown_buffer_frames(500, 192, 100, 1_536), 500);
    }
}
