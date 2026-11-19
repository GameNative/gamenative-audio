// An ALSA tone, played through whatever `pcm.!default` points at.
//
// This is a test fixture, not part of the plugin: it opens the default PCM the way a
// game would (simple mode, so libasound does the hw_params dance), plays 3 seconds of
// 440 Hz, drains and closes. Its purpose is to separate two questions that are easy
// to confuse on a device —
//
//   * does the plugin *load and play* (this program), and
//   * does a Wine game reach it (a game run).
//
// If this is silent while the player plays, the fault is at or below libasound; if
// this plays and the game is silent, the fault is in Wine's alsa driver or its
// configuration, and looking at AAudio would be a waste of a pass.
//
// Built by `alsa-plugin/test.sh`, which links it against the container's own
// `libasound.so.2` (a bionic AArch64 library) so the call conventions match exactly.
// The ALSA surface below is hand-declared for the same reason the engine declares
// AAudio: the numbers are the contract, and they are checkable against `pcm.h`.

use core::ffi::{c_char, c_int, c_long, c_void};

/// `snd_pcm_stream_t`: `PLAYBACK` is 0.
const PLAYBACK: c_int = 0;
/// `snd_pcm_format_t`: `S16_LE = 2`, `FLOAT_LE = 14`.
const S16_LE: c_int = 2;
/// `snd_pcm_access_t`: `RW_INTERLEAVED = 3`.
const RW_INTERLEAVED: c_int = 3;

type Pcm = c_void;

#[link(name = "asound")]
extern "C" {
    fn snd_pcm_open(pcm: *mut *mut Pcm, name: *const c_char, stream: c_int, mode: c_int) -> c_int;
    fn snd_pcm_set_params(
        pcm: *mut Pcm,
        format: c_int,
        access: c_int,
        channels: u32,
        rate: u32,
        soft_resample: c_int,
        latency_us: u32,
    ) -> c_int;
    fn snd_pcm_writei(pcm: *mut Pcm, buffer: *const c_void, size: c_long) -> c_long;
    fn snd_pcm_drain(pcm: *mut Pcm) -> c_int;
    fn snd_pcm_close(pcm: *mut Pcm) -> c_int;
    fn snd_strerror(err: c_int) -> *const c_char;
}

fn describe(err: c_int) -> String {
    // SAFETY: `snd_strerror` returns a static string for any int.
    let text = unsafe { snd_strerror(err) };
    if text.is_null() {
        return format!("error {err}");
    }
    // SAFETY: the string is NUL-terminated and static.
    unsafe { std::ffi::CStr::from_ptr(text) }
        .to_string_lossy()
        .into_owned()
}

fn main() {
    let mut pcm: *mut Pcm = core::ptr::null_mut();
    // SAFETY: every call below takes the handle this function opened.
    unsafe {
        let result = snd_pcm_open(&mut pcm, c"default".as_ptr(), PLAYBACK, 0);
        if result < 0 {
            eprintln!("tone: snd_pcm_open: {}", describe(result));
            std::process::exit(1);
        }
        println!("tone: opened \"default\"");

        // Ask for 20 ms of latency: enough that a scheduling hiccup is absorbed and
        // short enough that a listener can tell it is playing.
        let result = snd_pcm_set_params(pcm, S16_LE, RW_INTERLEAVED, 2, 48_000, 0, 20_000);
        if result < 0 {
            eprintln!("tone: snd_pcm_set_params: {}", describe(result));
            snd_pcm_close(pcm);
            std::process::exit(1);
        }
        println!("tone: params accepted (48000 Hz, 2 ch, S16_LE, RW_INTERLEAVED)");

        // A quarter of a second of sine at 0.2 full scale, written repeatedly for
        // three seconds. 0.2 is -14 dBFS, the same level the engine's own probe used,
        // so a pass and a failure can be compared directly.
        let rate = 48_000usize;
        let chunk = rate / 4;
        let mut block = vec![0i16; chunk * 2];
        let step = 440.0f32 / rate as f32;
        for frame in 0..chunk {
            let sample = (frame as f32 * step * core::f32::consts::TAU).sin() * 0.2;
            let value = (sample * 32_767.0) as i16;
            block[frame * 2] = value;
            block[frame * 2 + 1] = value;
        }

        // Rounds are the argument, so a device pass can run one second instead of
        // three when it is only asking whether the timing is right.
        let rounds: usize = std::env::args()
            .nth(1)
            .and_then(|value| value.parse().ok())
            .unwrap_or(12);
        let mut written = 0u64;
        let began = std::time::Instant::now();
        let mut calls = 0u64;
        let mut shortest = usize::MAX;
        for round in 0..rounds {
            let mut offset = 0usize;
            while offset < chunk {
                let result =
                    snd_pcm_writei(pcm, block[offset * 2..].as_ptr() as *const c_void, (chunk - offset) as c_long);
                if result < 0 {
                    eprintln!("tone: snd_pcm_writei: {}", describe(result as c_int));
                    snd_pcm_close(pcm);
                    std::process::exit(1);
                }
                calls += 1;
                shortest = shortest.min(result as usize);
                offset += result as usize;
                written += result as u64;
            }
            println!(
                "tone: round {round}: {written} frames in {:.2}s, {} writei calls, shortest {} frames",
                began.elapsed().as_secs_f64(),
                calls,
                if shortest == usize::MAX { 0 } else { shortest }
            );
        }
        println!(
            "tone: wrote {written} frames in {:.2}s ({} writei calls, {:.0} frames/s)",
            began.elapsed().as_secs_f64(),
            calls,
            written as f64 / began.elapsed().as_secs_f64()
        );

        let result = snd_pcm_drain(pcm);
        if result < 0 {
            eprintln!("tone: snd_pcm_drain: {}", describe(result));
        }
        let result = snd_pcm_close(pcm);
        if result < 0 {
            eprintln!("tone: snd_pcm_close: {}", describe(result));
        }
        println!("tone: closed");
    }
}
