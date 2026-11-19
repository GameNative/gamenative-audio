//! Device probe: what this device actually grants, and what it costs.
//!
//! Build it for the phone, push it, run it from a shell — its output goes to the
//! shell, and its `trace!` lines also land in logcat under `gnaudio`:
//!
//! ```sh
//! scripts/build-android.sh
//! adb push dist/arm64-v8a/gn-audio-probe /data/local/tmp/
//! adb shell /data/local/tmp/gn-audio-probe
//! ```
//!
//! For each candidate block size it prints what the device answered — sharing,
//! performance mode, burst, ring size, format — and then, after a second of real
//! playback, the figures that matter: the dropouts the device counted, the
//! dropouts the client saw from its own deadline, the engine's latency estimate,
//! and how far behind the device's own play position is. That last column is the
//! number to compare exclusive against shared, because it is the only honest
//! evidence that the mixer was actually bypassed.

use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use gnaudio::{Client, Engine, Error, Performance, RenderContext, Sharing, StreamConfig};

/// Plays a sine and measures its own deadline against it.
struct Tone {
    phase: f32,
    frequency: f32,
    longest_block_ns: Arc<AtomicU64>,
    late_blocks: Arc<AtomicU32>,
}

impl Client for Tone {
    fn render(&mut self, ctx: &RenderContext, out: &mut [f32]) -> u32 {
        let started = Instant::now();
        let frames = ctx.frames as usize;
        let channels = ctx.channels.max(1) as usize;
        let step = self.frequency / ctx.sample_rate.max(1) as f32;
        for frame in 0..frames {
            let sample = (self.phase * core::f32::consts::TAU).sin() * 0.1;
            self.phase = (self.phase + step) % 1.0;
            for channel in 0..channels {
                out[channel * frames + frame] = sample;
            }
        }
        let elapsed = started.elapsed().as_nanos() as u64;
        self.longest_block_ns.fetch_max(elapsed, Ordering::Relaxed);
        let period_ns = ctx.frames as u64 * 1_000_000_000 / ctx.sample_rate.max(1) as u64;
        if elapsed > period_ns {
            self.late_blocks.fetch_add(1, Ordering::Relaxed);
        }
        ctx.frames
    }
}

fn main() {
    println!("gnaudio device probe");
    println!(
        "{:>6} {:>9} {:>6} {:>6} {:>7} {:>7} {:>11} {:>8} {:>8}",
        "block", "sharing", "burst", "ring", "cb", "xruns", "late", "lat ms", "lag ms"
    );

    let mut refused = 0;
    for sharing in [Sharing::Exclusive, Sharing::Shared] {
        for block_frames in [128u32, 256, 512, 1024] {
            match probe(block_frames, sharing) {
                Ok(row) => println!("{row}"),
                Err(error) => {
                    refused += 1;
                    println!("{block_frames:>6} {sharing:>9?}  refused: {error:?}");
                }
            }
        }
    }

    if refused == 8 {
        println!("\nevery configuration was refused: this device grants no output stream at all");
        std::process::exit(1);
    }
    println!(
        "\nlat ms is the engine's own estimate; lag ms is (frames handed over - the device's play \
         position), which is the part exclusive sharing is supposed to remove."
    );
}

fn probe(block_frames: u32, sharing: Sharing) -> Result<String, Error> {
    let longest_block_ns = Arc::new(AtomicU64::new(0));
    let late_blocks = Arc::new(AtomicU32::new(0));
    let client = Tone {
        phase: 0.0,
        frequency: 440.0,
        longest_block_ns: longest_block_ns.clone(),
        late_blocks: late_blocks.clone(),
    };
    let config = StreamConfig {
        block_frames,
        sharing,
        performance: Performance::LowLatency,
        ..StreamConfig::default()
    };

    let engine = Engine::open(Box::new(client), config)?;
    let granted = engine.granted();
    engine.start()?;

    // A second of real playback, then read back what actually happened.
    std::thread::sleep(Duration::from_millis(1000));
    let stats = engine.stats();
    let latency_frames = engine.latency_frames();
    let timestamp = engine.timestamp();
    let _events = engine.take_events();
    let _ = engine.pause();

    let rate = granted.sample_rate.max(1) as f64;
    let to_ms = |frames: f64| frames * 1000.0 / rate;
    let lag_ms = match timestamp {
        Some(stamp) => format!(
            "{:.2}",
            to_ms(stats.frames as i64 as f64 - stamp.frame_position as f64)
        ),
        None => "n/a".to_string(),
    };
    let late = late_blocks.load(Ordering::Relaxed);
    let longest_ms = to_ms(longest_block_ns.load(Ordering::Relaxed) as f64 / 1_000_000.0);

    Ok(format!(
        "{:>6} {:>9} {:>6} {:>6} {:>7} {:>7} {:>11} {:>8} {:>8}",
        granted.block_frames,
        format!("{:?}", granted.sharing),
        granted.frames_per_burst,
        granted.device_buffer_frames,
        stats.callbacks,
        stats.device_xruns,
        format!("{late} ({longest_ms:.1})"),
        format!("{:.2}", to_ms(latency_frames as f64)),
        lag_ms,
    ))
}
