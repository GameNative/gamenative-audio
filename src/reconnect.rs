//! What to do when the audio device changes underneath a running stream.
//!
//! Android moves audio between devices constantly: headphones in and out, a Bluetooth
//! headset connecting, a USB DAC removed, a call taking the route. Three things can
//! happen to a stream when it does, and only one of them needs this module:
//!
//! * **Re-routed.** The stream stays open and AAudio sends it to the new device. No
//!   error, no gap beyond the HAL's own switch. Nothing to do — this is the common
//!   case, and it is why a naive implementation can look like it handles switching.
//! * **Disconnected.** The device the stream was routed to is gone. Every call on the
//!   stream now fails with `AAUDIO_ERROR_DISCONNECTED`, and AAudio's own header says
//!   the stream "cannot be used after the device is disconnected" — it must be closed
//!   and rebuilt. Silence forever otherwise, because a stream that has been
//!   disconnected never comes back.
//! * **Invalidated.** The stream was invalidated by the server, or a call timed out
//!   because the HAL had already given up on it. Same answer as disconnected: rebuild.
//!
//! This module is the policy — which results mean "rebuild", how long to wait between
//! attempts, and whether a rebuilt stream is the same stream. The work itself lives in
//! the backend, because only it owns a stream to rebuild.
//!
//! The policy is deliberately the same set of errors the libasound sink module treats
//! as reconnectable, so the two paths behave alike on the same device.

use core::time::Duration;

/// `AAUDIO_ERROR_DISCONNECTED`: the device went away.
pub(crate) const DISCONNECTED: i32 = -899;
/// `AAUDIO_ERROR_INVALID_STATE`: the stream is no longer usable.
pub(crate) const INVALID_STATE: i32 = -895;
/// `AAUDIO_ERROR_INVALID_HANDLE`: the server rejected the stream handle.
pub(crate) const INVALID_HANDLE: i32 = -892;
/// `AAUDIO_ERROR_TIMEOUT`: the HAL had already given up on the stream.
pub(crate) const TIMEOUT: i32 = -885;

/// Whether a result means the stream should be rebuilt rather than the caller told
/// the device is gone for good.
///
/// `DISCONNECTED` is the one that matters — it is what an unplugged headset or a
/// departed Bluetooth device produces — and the other three are the ways a stream can
/// be left in a state where every subsequent call fails for the same reason. Anything
/// else (bad arguments, out of memory, unimplemented) is not cured by asking again.
pub(crate) fn is_reconnectable(result: i32) -> bool {
    matches!(
        result,
        DISCONNECTED | INVALID_STATE | INVALID_HANDLE | TIMEOUT
    )
}

/// `aaudio_stream_state_t` values, counted from the header's own order.
const STATE_STARTING: i32 = 3;
const STATE_STARTED: i32 = 4;

/// Whether a stream in this state will ask for audio.
///
/// Only `STARTED` — and `STARTING`, which is the instant between `requestStart` and the
/// first period. Everything else is a stream that will not call back, and a stream that
/// will not call back has to be replaced: `PAUSED` and `STOPPED` because nothing is
/// driving them, `DISCONNECTED` because the header says such a stream can never be used
/// again, and `CLOSING`/`CLOSED` because it is already on its way out.
///
/// This is the check the reference sink makes on every render. Doing it only from the
/// error callback is not enough: some HALs end a stream when the route goes without
/// calling back at all, and then the state is the only evidence there is.
pub(crate) fn state_is_playing(state: i32) -> bool {
    matches!(state, STATE_STARTED | STATE_STARTING)
}

/// Whether a fault code means "the device is gone" rather than a caller's mistake.
///
/// Shared with the state watchdog, which records `DISCONNECTED` when the state says the
/// stream is unusable but nothing called back — the condition is the same one, so it is
/// spelled with the same code and travels the same path.
pub(crate) const STATE_LOST: i32 = DISCONNECTED;

/// Whether the render thread should go back to sleep instead of producing a period.
///
/// Four things stop it waiting, and the second and third are why this is a function with
/// tests rather than a `while` condition: a device that has gone delivers no periods, so a
/// wait keyed only on "a period is due" parks for ever and the rebuild never runs. The
/// timeout is the backstop for the same reason — it is what makes the state checkable when
/// no callback and no error arrives.
pub(crate) fn should_keep_waiting(
    period_due: bool,
    quit: bool,
    fault: i32,
    timed_out: bool,
) -> bool {
    period_due && !quit && fault == 0 && !timed_out
}

/// How long to wait before rebuild attempt `attempt` (0-based).
///
/// Doubling from 50 ms and capped at one second. A departed device may stay gone for
/// a while — someone unplugs headphones, walks away with a headset, answers a call —
/// so this never gives up; it only stops hammering the HAL while nothing is there.
/// The cap matters as much as the first step: an unbounded retry loop against a HAL
/// that keeps refusing is a battery drain and a log flood, not resilience.
pub(crate) fn backoff(attempt: u32) -> Duration {
    const FIRST_MS: u64 = 50;
    const MAX_MS: u64 = 1000;
    // 50, 100, 200, 400, 800, then the cap. Shifted rather than multiplied so the
    // sequence is exact and cannot overflow.
    let shift = attempt.min(5);
    Duration::from_millis((FIRST_MS << shift).min(MAX_MS))
}

/// How long a stream may go without asking for audio before it is not playing.
///
/// Four device buffers, floored at 100 ms. Long enough that no ordinary scheduling delay
/// reaches it, short enough that a route which has silently stopped is replaced within a
/// tenth of a second.
///
/// This exists because a stream can stop being real without saying so: a suspended
/// container leaves AudioFlinger's track reading `Active: no` with its frame count frozen
/// while AAudio still reports `STARTED`, so the state check alone clears it and nothing else
/// in the pipeline notices. The reference sink needs no such rule because its render path is
/// only reachable from the data callback; a design with its own render thread has to ask the
/// question separately.
pub(crate) fn silence_timeout_ns(device_buffer_frames: u32, sample_rate: i32) -> u64 {
    const FLOOR_NS: u64 = 100_000_000;
    if sample_rate <= 0 || device_buffer_frames == 0 {
        return FLOOR_NS;
    }
    let buffer_ns =
        (device_buffer_frames as u64).saturating_mul(1_000_000_000) / sample_rate as u64;
    buffer_ns.saturating_mul(4).max(FLOOR_NS)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_four_recoverable_results_are_exactly_the_ones_that_mean_rebuild() {
        for code in [DISCONNECTED, INVALID_STATE, INVALID_HANDLE, TIMEOUT] {
            assert!(is_reconnectable(code), "{code} must be rebuildable");
        }
        // Everything else is a caller's mistake or a resource problem: asking again
        // would just fail again, so those surface to the client instead.
        for code in [-898, -896, -890, -889, -887, -884, -1, 0, 1] {
            assert!(!is_reconnectable(code), "{code} must not trigger a rebuild");
        }
    }

    #[test]
    fn the_retry_interval_doubles_and_then_stops_at_one_second() {
        let mills: Vec<u64> = (0..8).map(|n| backoff(n).as_millis() as u64).collect();
        assert_eq!(
            mills,
            vec![50, 100, 200, 400, 800, 1000, 1000, 1000],
            "a device that stays gone must not be hammered"
        );
        // Monotone: a shorter wait after a longer one would be a burst, not a backoff.
        for pair in mills.windows(2) {
            assert!(pair[1] >= pair[0]);
        }
    }

    #[test]
    fn a_stream_only_counts_as_playing_when_it_will_ask_for_audio() {
        assert!(state_is_playing(STATE_STARTED));
        assert!(
            state_is_playing(STATE_STARTING),
            "starting is not yet a fault"
        );
        // Everything below will never call back, and each one has been seen on a switch.
        for dead in [0, 1, 2, 5, 6, 7, 8, 9, 10, 11, 12, 13] {
            assert!(!state_is_playing(dead), "state {dead} must be replaced");
        }
    }

    #[test]
    fn a_dead_device_never_parks_the_render_thread_for_ever() {
        // Waiting only for "a period is due" on a stream that will never ask for one
        // parks this thread for ever: the fault is never read, the rebuild never happens,
        // and the result is silence with every counter healthy.
        assert!(!should_keep_waiting(false, false, STATE_LOST, false));
        // A fault is a reason to wake even with no period due.
        assert!(!should_keep_waiting(true, false, STATE_LOST, false));
        // Quit likewise.
        assert!(!should_keep_waiting(true, true, 0, false));
        // A timeout is what makes the state checkable when nothing calls back at all.
        assert!(!should_keep_waiting(true, false, 0, true));
        // A healthy stream waits for its period and nothing else.
        assert!(should_keep_waiting(true, false, 0, false));
    }

    #[test]
    fn a_stream_that_stops_asking_is_declared_dead() {
        // The Nova's geometry: a 384-frame buffer at 48 kHz is 8 ms, so four of them is
        // 32 ms and the floor raises it to 100 ms.
        assert_eq!(silence_timeout_ns(384, 48_000), 100_000_000);
        // A deep buffer on a slow route scales with its own duration instead.
        assert_eq!(silence_timeout_ns(9_600, 48_000), 800_000_000);
        // A device that will not say: the floor applies and is never zero.
        assert_eq!(silence_timeout_ns(0, 48_000), 100_000_000);
        assert_eq!(silence_timeout_ns(384, 0), 100_000_000);
    }
}
