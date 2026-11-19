// A game-like writer, to see whether the plugin starves where a tight loop does not.
//
// `tone` writes as fast as the device accepts, which keeps the ring permanently full.
// No ALSA client does that: winealsa asks for a 10 ms period and writes one period at
// a time from a timer, so the ring hovers near empty and a late wakeup is a dropout.
// That difference is the whole question behind "why does this one game crackle", so it
// has to be measured with a writer that behaves like the game's.
//
//   pace <rate> <period_frames> <periods> <seconds> [jitter_ms]
//
// Writes one period every `period_frames/rate` seconds, with an optional random extra
// delay of up to `jitter_ms`, and prints what libasound reported after each write. The
// plugin's own starvation counter is what turns this into an answer; this program only
// provides the timing.

use core::ffi::{c_char, c_int, c_long, c_void};

const PLAYBACK: c_int = 0;
const S16_LE: c_int = 2;
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

/// Sleeps until an absolute monotonic deadline.
///
/// `thread::sleep` takes a *relative* interval and is imprecise under load: measured on
/// a device it came back tens of milliseconds late on most wakeups, which is enough to
/// starve any buffer and would make this harness blame the PCM for its own timing.
/// `clock_nanosleep` with `TIMER_ABSTIME` schedules against the clock itself, so being
/// late once does not shift every later wakeup.
fn sleep_until_ns(deadline_ns: u64) {
    #[cfg(target_pointer_width = "64")]
    #[repr(C)]
    struct Timespec {
        tv_sec: i64,
        tv_nsec: i64,
    }

    extern "C" {
        fn clock_nanosleep(
            clock_id: c_int,
            flags: c_int,
            request: *const Timespec,
            remain: *mut Timespec,
        ) -> c_int;
    }
    /// `CLOCK_MONOTONIC`, and `TIMER_ABSTIME`.
    const CLOCK_MONOTONIC: c_int = 1;
    const TIMER_ABSTIME: c_int = 1;

    let request = Timespec {
        tv_sec: (deadline_ns / 1_000_000_000) as i64,
        tv_nsec: (deadline_ns % 1_000_000_000) as i64,
    };
    // SAFETY: `request` is a live local of the layout the call expects; a null remainder
    // is allowed.
    unsafe {
        clock_nanosleep(CLOCK_MONOTONIC, TIMER_ABSTIME, &request, core::ptr::null_mut());
    }
}

fn monotonic_ns() -> u64 {
    #[cfg(target_pointer_width = "64")]
    #[repr(C)]
    struct Timespec {
        tv_sec: i64,
        tv_nsec: i64,
    }
    extern "C" {
        fn clock_gettime(clock_id: c_int, tp: *mut Timespec) -> c_int;
    }
    let mut ts = Timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: a live local, of the expected layout.
    unsafe { clock_gettime(1, &mut ts) };
    (ts.tv_sec as u64) * 1_000_000_000 + ts.tv_nsec as u64
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

/// A cheap deterministic "random" so runs are comparable: xorshift, seeded per run.
struct Jitter(u64);

impl Jitter {
    fn next_ms(&mut self, max: u64) -> u64 {
        if max == 0 {
            return 0;
        }
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0 % (max + 1)
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 5 {
        eprintln!("usage: pace <rate> <period_frames> <periods> <seconds> [jitter_ms]");
        std::process::exit(2);
    }
    let rate: u32 = args[1].parse().expect("rate");
    let period_frames: u32 = args[2].parse().expect("period frames");
    let periods: u32 = args[3].parse().expect("periods");
    let seconds: u64 = args[4].parse().expect("seconds");
    let jitter_ms: u64 = args.get(5).and_then(|v| v.parse().ok()).unwrap_or(0);
    // Silence before the first sound, so a capture that budgets from the open can be told
    // from one that budgets from the first sound. A real client spends its opening seconds
    // on a loading screen, which is exactly what this reproduces.
    let silent_ms: u64 = args.get(6).and_then(|v| v.parse().ok()).unwrap_or(0);

    let latency_us = (period_frames as u64 * periods as u64 * 1_000_000 / rate as u64) as u32;

    let mut pcm: *mut Pcm = core::ptr::null_mut();
    // SAFETY: every call below takes the handle this function opened.
    unsafe {
        let result = snd_pcm_open(&mut pcm, c"default".as_ptr(), PLAYBACK, 0);
        if result < 0 {
            eprintln!("pace: snd_pcm_open: {}", describe(result));
            std::process::exit(1);
        }
        let result = snd_pcm_set_params(pcm, S16_LE, RW_INTERLEAVED, 2, rate, 0, latency_us);
        if result < 0 {
            eprintln!("pace: snd_pcm_set_params: {}", describe(result));
            snd_pcm_close(pcm);
            std::process::exit(1);
        }
        println!(
            "pace: opened {rate} Hz, period {period_frames} frames, {periods} periods \
             ({latency_us} us), jitter up to {jitter_ms} ms"
        );

        // A 440 Hz tone, so a dropout is audible as a rasp rather than as nothing.
        let mut block = vec![0i16; period_frames as usize * 2];
        let mut phase = 0.0f32;
        let step = 440.0f32 / rate as f32;
        for frame in 0..period_frames as usize {
            let sample = (phase * core::f32::consts::TAU).sin() * 0.2;
            phase = (phase + step) % 1.0;
            let value = (sample * 32_767.0) as i16;
            block[frame * 2] = value;
            block[frame * 2 + 1] = value;
        }

        let period_ns = period_frames as u64 * 1_000_000_000 / rate as u64;
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(seconds);
        let mut jitter = Jitter(0x9E3779B97F4A7C15);
        let mut written = 0u64;
        let mut wakes = 0u64;
        let began = std::time::Instant::now();
        // How late this thread's own wakeups are. A ring that starves is only a plugin
        // fault if the writer was on time; without this the two are indistinguishable.
        let mut late_total_ms = 0u64;
        let mut late_worst_ms = 0u64;
        let mut late_over_20 = 0u64;
        let start_ns = monotonic_ns();
        let mut next_wake = start_ns;
        let mut last_wake = next_wake;

        let silent_until = start_ns + silent_ms * 1_000_000;
        let tone = block.clone();
        let mut silent = silent_ms > 0;
        while std::time::Instant::now() < deadline {
            // Restore the tone once the silent period is over: filling the block with
            // zeros is destructive, and the first version of this test wrote silence for
            // the whole run and then blamed the plugin for capturing no sound.
            if silent && monotonic_ns() >= silent_until {
                silent = false;
                block.copy_from_slice(&tone);
            }
            if silent {
                block.fill(0);
            }
            let asked = monotonic_ns();
            let result = snd_pcm_writei(pcm, block.as_ptr() as *const c_void, period_frames as c_long);
            // Which half of the period went missing: the write waiting for room in the
            // ring, or this thread not being scheduled for its sleep. Only the first is
            // the plugin's doing.
            let write_ms = monotonic_ns().saturating_sub(asked) / 1_000_000;
            if result < 0 {
                eprintln!("pace: snd_pcm_writei: {}", describe(result as c_int));
                break;
            }
            written += result as u64;
            wakes += 1;

            // One period of sleep, plus the jitter a busy game thread would add.
            let extra = jitter.next_ms(jitter_ms) * 1_000_000;
            let wanted = period_ns + extra;
            next_wake += wanted;
            sleep_until_ns(next_wake);

            // Lateness is the interval this wakeup actually took against the one it
            // asked for — not the distance from a deadline in the past. Measured the
            // second way, one late wakeup made every later one look late and the total
            // exceeded the elapsed time, which is arithmetic nonsense and hid the real
            // figure.
            let now = monotonic_ns();
            let sleep_ms = now.saturating_sub(asked) / 1_000_000 - write_ms;
            let interval = now.saturating_sub(last_wake);
            last_wake = now;
            let late_ms = interval.saturating_sub(wanted) / 1_000_000;
            late_total_ms += late_ms;
            late_worst_ms = late_worst_ms.max(late_ms);
            if late_ms > 20 {
                late_over_20 += 1;
                println!(
                    "pace: late {late_ms} ms at {:.3} s — writei {write_ms} ms, sleep {sleep_ms} ms",
                    (now - start_ns) as f64 / 1e9
                );
            }
            // Late: keep the absolute schedule and write the periods that were missed.
            //
            // Resetting the schedule to `now` instead throws the deficit away for good, so
            // the harness permanently under-supplies — measured, 46 680 frames/s where the
            // device needs 48 000 — and the plugin's ring then starves by exactly that
            // shortfall. That produced seventeen "dropouts" that were entirely the harness.
            // A real client writes whatever the device has room for on each tick and so
            // recovers; this does the same, bounded to the ring's own depth.
            while next_wake + period_ns < now {
                let result =
                    snd_pcm_writei(pcm, block.as_ptr() as *const c_void, period_frames as c_long);
                if result < 0 {
                    break;
                }
                written += result as u64;
                wakes += 1;
                next_wake += period_ns;
            }

        }

        let elapsed = began.elapsed().as_secs_f64();
        println!(
            "pace: wrote {written} frames in {elapsed:.2}s ({:.0} frames/s), {wakes} wakeups",
            written as f64 / elapsed.max(0.001)
        );
        println!(
            "pace: writer lateness: {late_total_ms} ms total, {late_worst_ms} ms worst, \
             {late_over_20} wakeups over 20 ms late"
        );
        snd_pcm_drain(pcm);
        snd_pcm_close(pcm);
    }
}
