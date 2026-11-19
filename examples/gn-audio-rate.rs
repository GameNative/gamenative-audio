//! Measures the two rates that matter separately: what this crate hands the device, and what
//! the device says it has played.
//!
//! "The audio sounds slow" has two very different causes, and they are indistinguishable
//! from a single number. If frames are delivered at the stream's rate and played slower,
//! the route is discarding audio and nothing in the pipeline above can fix it. If frames
//! are *delivered* slower than the rate, this crate is not keeping up and the fault is
//! here. So: play a tone, watch both counters against one wall clock, and print each rate.
//!
//!     cargo run --release --example gn-audio-rate
//!
//! Exits non-zero when either rate is more than two per cent off, so it can gate a device
//! pass.

use std::time::{Duration, Instant};

use gnaudio::*;

struct Tone {
    phase: f64,
    frequency: f64,
    amplitude: f64,
}

impl Client for Tone {
    fn render(&mut self, ctx: &RenderContext, out: &mut [f32]) -> u32 {
        let frames = ctx.frames as usize;
        let channels = ctx.channels.max(1) as usize;
        let rate = ctx.sample_rate.max(1) as f64;
        let step = self.frequency / rate;
        for frame in 0..frames {
            let sample = (self.phase * std::f64::consts::TAU).sin() as f32 * self.amplitude as f32;
            for channel in 0..channels {
                out[frame * channels + channel] = sample;
            }
            self.phase += step;
            if self.phase >= 1.0 {
                self.phase -= 1.0;
            }
        }
        ctx.frames
    }
}

fn main() {
    let seconds: u64 = std::env::var("GNAUDIO_RATE_SECONDS")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(12);

    // Silent by default when asked to be, so a rate can be measured over a game that is
    // already playing without putting a tone on top of it. The device consumes frames
    // either way; only the content differs.
    let amplitude = std::env::var("GNAUDIO_RATE_AMPLITUDE")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(0.2);
    let client = Tone {
        phase: 0.0,
        frequency: 440.0,
        amplitude,
    };
    // `GNAUDIO_RATE_DEVICE_BUFFER` caps the device buffer, and
    // `GNAUDIO_RATE_CAPACITY` asks the route for room before it opens — which is the only
    // way to test a buffer deeper than the route offers by default.
    let device_buffer = std::env::var("GNAUDIO_RATE_DEVICE_BUFFER")
        .ok()
        .and_then(|value| value.parse::<u32>().ok());
    let capacity = std::env::var("GNAUDIO_RATE_CAPACITY")
        .ok()
        .and_then(|value| value.parse::<u32>().ok());
    // `GNAUDIO_RATE_PERF=power` asks for the power-saving path, which on Android is a
    // different kind of track: `LowLatency` reaches an MMAP stream where the HAL supports
    // one, and MMAP over a wireless route is not the same animal as MMAP on the speaker.
    let performance = match std::env::var("GNAUDIO_RATE_PERF").as_deref() {
        Ok("power") | Ok("powersaving") => Performance::PowerSaving,
        _ => Performance::LowLatency,
    };
    // `GNAUDIO_RATE_HZ` asks the route for a rate. The A2DP codec's own rate is not always
    // what the platform reports, and a mismatch shows up as audio that is both fast and
    // starved, so being able to name a rate is how that gets tested.
    let rate = std::env::var("GNAUDIO_RATE_HZ")
        .ok()
        .and_then(|value| value.parse::<i32>().ok());
    // `GNAUDIO_RATE_FORMAT=i16` asks the route for 16-bit, which is what an ordinary app
    // sends. If a route is clean for an ordinary app and broken for this one, the format
    // is the first thing to stop assuming.
    let format = match std::env::var("GNAUDIO_RATE_FORMAT").as_deref() {
        Ok("i16") | Ok("int16") => Some(Format::Int16),
        Ok("f32") | Ok("float") => Some(Format::Float32),
        _ => None,
    };
    let config = StreamConfig {
        sample_rate: rate,
        format,
        block_frames: 480,
        device_buffer_frames: device_buffer,
        device_buffer_capacity_frames: capacity,
        performance,
        ..StreamConfig::default()
    };

    let engine = match Engine::open(Box::new(client), config) {
        Ok(engine) => engine,
        Err(error) => {
            eprintln!("the device refused a stream: {error:?}");
            std::process::exit(2);
        }
    };
    let granted = engine.granted();
    println!(
        "device {} at {} Hz (client {} Hz), {} ch, burst {}, device buffer {} ({} ms), block {} \
         [capacity asked {:?}, {:?}, format {:?}]",
        granted.device_id,
        granted.sample_rate,
        granted.client_sample_rate,
        granted.device_channels,
        granted.frames_per_burst,
        granted.device_buffer_frames,
        granted.device_buffer_frames as u64 * 1000 / granted.sample_rate.max(1) as u64,
        granted.block_frames,
        capacity,
        granted.performance,
        granted.format
    );

    if let Err(error) = engine.start() {
        eprintln!("the device refused to start: {error:?}");
        std::process::exit(2);
    }

    // The granularity the route consumes in. Sampled densely and reported as the deltas
    // between *distinct* positions: a device that jumps 3 072 frames at a time and one that
    // steps 96 frames at a time need different buffers even when both report the same burst
    // and the same rate, and only this shows it.
    let steps = sample_steps(&engine, Duration::from_secs(6));

    // Let the pipeline fill before the first mark: a rate measured across the initial
    // prefill would count frames that were queued rather than played.
    std::thread::sleep(Duration::from_millis(500));
    let first = mark(&engine);
    let started = Instant::now();
    std::thread::sleep(Duration::from_secs(seconds));
    let elapsed_ns = started.elapsed().as_nanos() as u64;
    let second = mark(&engine);
    let stats = engine.stats();
    let _ = engine.pause();

    let wall_ns = elapsed_ns.max(1);
    let claimed = granted.sample_rate.max(1) as u64;

    if let Some((count, min, median, max)) = steps {
        println!(
            "consumption: {count} position changes in 6 s, step min {min} median {median} max \
             {max} frames ({} ms at the median)",
            median as f64 * 1000.0 / granted.sample_rate.max(1) as f64
        );
    }

    let Some(played) = placed(&first, &second) else {
        eprintln!("the device published no play position at all: nothing can be measured");
        std::process::exit(2);
    };
    let delivered = second.delivered.saturating_sub(first.delivered);
    let rate_of = |frames: u64, span_ns: u64| frames.saturating_mul(1_000_000_000) / span_ns.max(1);
    let delivered_rate = rate_of(delivered, wall_ns);
    let played_rate = rate_of(played, wall_ns);

    // The device's own two marks carry the time each position was true, so their ratio is
    // the play rate without any sampling offset in it. A device that publishes a stale
    // position reports a slow rate against our clock and the honest one against its own —
    // which is the difference between a route that discards audio and an instrument that
    // misreads it.
    let device_ns = second.at_ns.saturating_sub(first.at_ns);
    let played_in_its_own_domain = rate_of(played, device_ns);

    println!(
        "\nover {:.2} s of wall clock ({} s of the device's own): delivered {} frames \
         ({} /s), played {} frames ({} /s, {} /s by its own timestamps), claim {} /s",
        wall_ns as f64 / 1e9,
        device_ns as f64 / 1e9,
        delivered,
        delivered_rate,
        played,
        played_rate,
        played_in_its_own_domain,
        claimed
    );
    // Judged on the device's own clock when it published one: that is the number with no
    // measurement error in it, and the wall-clock figure only matters as a cross-check.
    let judged = if device_ns > 0 {
        played_in_its_own_domain
    } else {
        played_rate
    };
    let per_mille = |rate: u64| (rate as i64 - claimed as i64) * 1000 / claimed as i64;
    println!(
        "delivered {:+.0} per mille, played {:+.0} per mille (device xruns {}, our underruns {}, \
         {} callbacks)",
        per_mille(delivered_rate),
        per_mille(judged),
        stats.device_xruns,
        stats.underruns,
        stats.callbacks
    );

    // Which side is short decides the fix, so say it rather than leaving it to a reader.
    let tolerance = claimed / 50; // two per cent
    let delivered_off = delivered_rate.abs_diff(claimed) > tolerance;
    let played_off = judged.abs_diff(claimed) > tolerance;
    if delivered_off {
        println!(
            "\nthis crate is not delivering at the stream's rate: the fault is here, in the \
             render path"
        );
    } else if played_off {
        println!(
            "\nthe stream is fed at its rate and plays slower: the route is discarding audio, \
             and no buffer size in this crate changes that"
        );
    } else {
        println!("\nboth sides agree with the stream's rate");
    }
    if delivered_off || played_off {
        std::process::exit(1);
    }
}

/// Samples the device's play position every two milliseconds and reports the size of its
/// steps: how many frames it takes at a time, which is the buffer a route has to be given.
fn sample_steps(engine: &Engine, window: Duration) -> Option<(usize, u64, u64, u64)> {
    let deadline = Instant::now() + window;
    let mut last: Option<u64> = None;
    let mut steps: Vec<u64> = Vec::new();
    while Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(2));
        // A device answers `getTimestamp` only once it is properly running, and returns
        // nothing at all for the first few milliseconds. Skipping those is the difference
        // between measuring the route and reporting that it could not be measured.
        let Some(stamp) = engine.timestamp() else {
            continue;
        };
        let position = stamp.frame_position.max(0) as u64;
        match last {
            // Only a *change* is a step. Sampling faster than the device moves repeats the
            // same position, and counting those as zero would report a step of nothing.
            Some(previous) if position > previous => {
                steps.push(position - previous);
                last = Some(position);
            }
            None => last = Some(position),
            _ => {}
        }
    }
    if steps.is_empty() {
        return None;
    }
    steps.sort_unstable();
    let median = steps[steps.len() / 2];
    Some((steps.len(), steps[0], median, *steps.last().unwrap()))
}

struct Mark {
    delivered: u64,
    position: u64,
    /// The position's own time, in the device's clock domain.
    at_ns: u64,
}

fn mark(engine: &Engine) -> Mark {
    let stats = engine.stats();
    match engine.timestamp() {
        Some(stamp) => Mark {
            delivered: stats.frames,
            position: stamp.frame_position.max(0) as u64,
            at_ns: stamp.time_ns.max(0) as u64,
        },
        None => Mark {
            delivered: stats.frames,
            position: 0,
            at_ns: 0,
        },
    }
}

/// Frames played between two marks, or `None` when the device never published a position.
///
/// Two positions a second apart are always comparable; a position that did not move at all
/// is a device that never answered, which is a different failure from a slow one.
fn placed(first: &Mark, second: &Mark) -> Option<u64> {
    if first.position == 0 && second.position == 0 {
        return None;
    }
    Some(second.position.saturating_sub(first.position))
}
