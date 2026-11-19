//! Several PCMs at once: what a game actually does.
//!
//! A wine process does not open one stream. Winealsa opens a PCM per mmdevapi client,
//! so a game with music, effects and voice holds three or four of these at the same
//! time, each with its own period, its own burst and sometimes its own rate. Every
//! claim the single-stream tests make has to survive that — and a few of them can
//! only be tested with a second stream in the process: that positions stay each
//! stream's own, that one device disappearing is not every stream's problem, and that
//! cost grows with the number of streams rather than with their interaction.
//!
//! These run against scripted devices, so "performance" here means work per frame of
//! audio — the conversion, the block staging and the position arithmetic — plus the
//! property that streams do not wait on each other. What it cannot see is a spin on a
//! thread the scripted device does not have; `crate::wake` and the poll callbacks in
//! the ALSA plugin are where that is handled.

mod harness;

use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use gnaudio::backend::mock::{MockConfig, MockDevice};
use gnaudio::*;
use harness::{RampClient, Record, Slab};

/// One stream's scripted device, its engine, and what its client produced.
struct Scripted {
    engine: Engine,
    device: MockDevice,
    record: Arc<Mutex<Record>>,
    /// The client's period, which is what a device is asked to consume at a time here.
    block: u32,
}

impl Scripted {
    fn open(device: MockConfig, config: StreamConfig) -> Scripted {
        let devices: Arc<Mutex<Vec<MockDevice>>> = Arc::new(Mutex::new(Vec::new()));
        let factory: BackendFactory = {
            let devices = devices.clone();
            Arc::new(move || {
                let (scripted, backend) = MockDevice::new(device);
                devices.lock().unwrap().push(scripted);
                Ok(Box::new(backend) as Box<dyn Backend>)
            })
        };
        let record = Arc::new(Mutex::new(Record::default()));
        let dropped = Arc::new(AtomicBool::new(false));
        let client = RampClient::new(record.clone(), dropped.clone());
        let engine = Engine::open_with_factory(Box::new(client), config, factory)
            .expect("a scripted device always opens");
        let device = devices.lock().unwrap().last().unwrap().clone();
        // `dropped` is the client's own liveness flag; nothing here asserts on it, and
        // the client is dropped by the engine when the engine is.
        drop(dropped);
        Scripted {
            engine,
            device,
            record,
            block: config.block_frames,
        }
    }

    /// Lets `periods` of this stream's own period elapse, keeping what the device got.
    fn play(&self, periods: usize) -> Vec<f32> {
        let mut out = Vec::with_capacity(periods * self.block as usize * 2);
        for _ in 0..periods {
            match self.device.deliver_frames(self.block) {
                Some(period) => out.extend_from_slice(&period),
                None => break,
            }
        }
        out
    }

    /// Frames the client has produced in total: the sum of every block it was
    /// handed. Not `Record::next_frame`, which is the ramp's value at the *start* of
    /// the last block it wrote.
    fn produced(&self) -> u64 {
        let record = self.record.lock().unwrap();
        record.frames.iter().map(|frames| u64::from(*frames)).sum()
    }

    fn positions(&self) -> Vec<u64> {
        self.record.lock().unwrap().positions.clone()
    }
}

/// A device's geometry, as the four numbers that decide everything above it.
#[derive(Clone, Copy)]
struct Geometry {
    burst: u32,
    device_buffer: u32,
    block: u32,
}

impl Geometry {
    fn device(self) -> MockConfig {
        MockConfig {
            burst_frames: self.burst,
            device_buffer_frames: self.device_buffer,
            ..MockConfig::default()
        }
    }

    fn config(self) -> StreamConfig {
        StreamConfig {
            block_frames: self.block,
            ..StreamConfig::default()
        }
    }
}

/// Four streams with the shapes this hardware actually produces: the Nova's 192-frame
/// burst, the Odin's 96, the mock's 128, and one client asking for a period several
/// bursts long.
const FOUR: [Geometry; 4] = [
    Geometry {
        burst: 96,
        device_buffer: 192,
        block: 480,
    },
    Geometry {
        burst: 192,
        device_buffer: 384,
        block: 480,
    },
    Geometry {
        burst: 128,
        device_buffer: 256,
        block: 256,
    },
    Geometry {
        burst: 96,
        device_buffer: 192,
        block: 1024,
    },
];

fn open_all(geometries: &[Geometry]) -> Vec<Scripted> {
    geometries
        .iter()
        .map(|g| Scripted::open(g.device(), g.config()))
        .collect()
}

#[test]
fn four_pcms_in_one_process_each_get_every_frame_they_produce() {
    let streams = open_all(&FOUR);
    for stream in &streams {
        stream.engine.start().unwrap();
    }
    // Round-robin, so the four timelines advance in the same wall-clock order a sound
    // card's would: stream A's period, then B's, then C's.
    for _ in 0..200 {
        for stream in &streams {
            stream.device.deliver_frames(stream.block);
        }
    }
    for (index, stream) in streams.iter().enumerate() {
        let produced = stream.produced();
        let expected = 200 * stream.block as u64;
        assert_eq!(
            produced, expected,
            "stream {index} produced {produced} frames, expected {expected}"
        );
        // Every block was whole, so what the client produced is what the device was
        // handed — nothing was dropped for one stream because another was busy.
        assert_eq!(
            stream.engine.stats().frames,
            produced,
            "stream {index} delivered {} of {produced} produced frames",
            stream.engine.stats().frames
        );
        assert_eq!(stream.engine.stats().underruns, 0, "stream {index} starved");
        assert_eq!(
            stream.engine.stats().callbacks,
            200,
            "stream {index} cadence"
        );
    }
}

#[test]
fn each_stream_keeps_its_own_timeline() {
    let streams = open_all(&FOUR);
    for stream in &streams {
        stream.engine.start().unwrap();
    }
    for _ in 0..50 {
        for stream in &streams {
            stream.device.deliver_frames(stream.block);
        }
    }
    for (index, stream) in streams.iter().enumerate() {
        let positions = stream.positions();
        assert_eq!(positions[0], 0, "stream {index} starts its own timeline");
        for pair in positions.windows(2) {
            assert_eq!(
                pair[1] - pair[0],
                stream.block as u64,
                "stream {index} advanced by {} frames, not its own block",
                pair[1] - pair[0]
            );
        }
        assert_eq!(
            *positions.last().unwrap(),
            (positions.len() as u64 - 1) * stream.block as u64,
            "stream {index} position is its own block count"
        );
        // And the engine's own report agrees with the client's, stream by stream.
        let (position, _) = stream.engine.sample_position();
        assert_eq!(position, *positions.last().unwrap(), "stream {index}");
    }
}

#[test]
fn what_a_device_receives_is_that_streams_own_audio() {
    // The one cross-talk a shared pipeline could produce: two clients' audio meeting in
    // one buffer. Each client writes its absolute frame index in channel 0 and that index
    // plus 1000 in channel 1, so a wrong stream in the buffer is arithmetic, not a guess.
    let streams = open_all(&FOUR);
    for stream in &streams {
        stream.engine.start().unwrap();
    }
    let mut received: Vec<Vec<f32>> = vec![Vec::new(); streams.len()];
    for _ in 0..20 {
        for (index, stream) in streams.iter().enumerate() {
            received[index].extend_from_slice(&stream.device.deliver_frames(stream.block).unwrap());
        }
    }
    for (index, stream) in streams.iter().enumerate() {
        let slab = Slab::from_interleaved(&received[index], 2);
        // The ramp counts from zero and never repeats, so each stream's device buffer
        // has to hold its own consecutive frames — channel 0 the frame index, channel
        // 1 that index plus a thousand. Anything else is another stream's audio, a
        // dropped block, or a duplicated one.
        for (channel, offset) in [(0usize, 0.0f32), (1, 1000.0)] {
            for (frame, sample) in slab.channel(channel).iter().enumerate() {
                assert_eq!(
                    *sample,
                    frame as f32 + offset,
                    "stream {index} channel {channel} frame {frame} is not that stream's audio"
                );
            }
        }
        assert!(stream.produced() >= slab.frames as u64);
    }
}

#[test]
fn an_opened_stream_produces_nothing_until_it_starts() {
    // Wine opens PCMs it does not immediately play: format probing, a stream sitting
    // behind a paused menu. An opened stream that renders would be audio nobody asked
    // for, and a frame count that says otherwise.
    let stream = Scripted::open(FOUR[0].device(), FOUR[0].config());
    assert_eq!(
        stream.device.deliver(),
        None,
        "a device that has not started"
    );
    assert_eq!(stream.produced(), 0, "and a client nobody asked");
    stream.engine.start().unwrap();
    assert!(stream.device.deliver().is_some(), "a device that has");
    assert!(stream.produced() > 0);
}

#[test]
fn six_streams_cost_a_small_fraction_of_one_core_per_stream() {
    // Work per frame of audio, which is the only thing a scripted device can measure:
    // the resampler's identity path, the block spread over device periods, the position
    // arithmetic and the diagnostics. A spin on the render thread would not appear here
    // — that thread does not exist in this harness — so this is a cost guard, not proof
    // that nothing spins.
    const STREAMS: usize = 6;
    const SECONDS: u64 = 5;
    let geometries = [FOUR[0], FOUR[1], FOUR[2], FOUR[0], FOUR[1], FOUR[3]];
    let streams = open_all(&geometries);
    for stream in &streams {
        stream.engine.start().unwrap();
    }
    let total_frames = SECONDS * 48_000;
    let started = Instant::now();
    let mut expected = Vec::new();
    for stream in &streams {
        // Whole periods only: a period is the unit a device consumes in, so a block
        // that does not divide the total leaves a partial one at the end.
        let wanted = (total_frames / u64::from(stream.block)) as usize;
        expected.push(wanted as u64 * u64::from(stream.block));
        stream.play(wanted);
    }
    let elapsed = started.elapsed().as_secs_f64();
    let audio_seconds = STREAMS as f64 * SECONDS as f64;
    let share = elapsed / audio_seconds;
    println!(
        "{STREAMS} streams, {audio_seconds:.0} s of audio in {elapsed:.3} s: \
         {:.2} % of one core per stream",
        share * 100.0
    );
    for (index, stream) in streams.iter().enumerate() {
        assert_eq!(
            stream.produced(),
            expected[index],
            "stream {index} did not produce its whole {SECONDS} s"
        );
    }
    assert!(
        share < 0.10,
        "each stream costs {:.1} % of a core for realtime audio",
        share * 100.0
    );
}

#[test]
fn a_slow_stream_does_not_slow_its_neighbours() {
    // One client that takes its time — a game loading a level, a sound being decoded —
    // must cost the other streams nothing. Each stream is driven from its own thread
    // here, which is how a real device would do it.
    const PERIODS: usize = 100;
    let fast = [FOUR[0], FOUR[1], FOUR[2]];
    let slow_geometry = FOUR[3];

    let mut handles = Vec::new();
    for geometry in fast {
        handles.push(std::thread::spawn(move || {
            let stream = Scripted::open(geometry.device(), geometry.config());
            stream.engine.start().unwrap();
            let started = Instant::now();
            stream.play(PERIODS);
            (stream.produced(), started.elapsed())
        }));
    }
    let slow = std::thread::spawn(move || {
        let stream = Scripted::open(slow_geometry.device(), slow_geometry.config());
        stream.engine.start().unwrap();
        for index in 0..PERIODS {
            // Every tenth period takes ten times as long as a period should.
            if index % 10 == 0 {
                std::thread::sleep(Duration::from_millis(5));
            }
            stream.device.deliver_frames(stream.block);
        }
        stream.produced()
    });

    let results: Vec<(u64, Duration)> = handles.into_iter().map(|h| h.join().unwrap()).collect();
    let slow_produced = slow.join().unwrap();
    for (index, (produced, took)) in results.iter().enumerate() {
        assert_eq!(
            *produced,
            (PERIODS as u64) * fast[index].block as u64,
            "stream {index} lost audio while another stream was slow"
        );
        assert!(
            took.as_secs_f64() < 5.0,
            "stream {index} was held up for {took:?} by another stream"
        );
    }
    assert_eq!(
        slow_produced,
        (PERIODS as u64) * slow_geometry.block as u64,
        "the slow stream still gets all of its own audio"
    );
}

#[test]
fn one_stream_losing_its_device_leaves_the_others_playing() {
    let streams = open_all(&FOUR);
    for stream in &streams {
        stream.engine.start().unwrap();
    }
    for _ in 0..10 {
        for stream in &streams {
            stream.device.deliver_frames(stream.block);
        }
    }
    streams[1].device.disconnect();
    for _ in 0..10 {
        for stream in &streams {
            stream.device.deliver_frames(stream.block);
        }
    }
    assert!(
        streams[1]
            .engine
            .take_events()
            .contains(Events::DEVICE_LOST),
        "the stream that lost its device must say so"
    );
    for (index, stream) in streams.iter().enumerate() {
        if index == 1 {
            continue;
        }
        assert!(
            stream.engine.take_events().is_empty(),
            "stream {index} was told about another stream's device"
        );
        assert_eq!(
            stream.engine.stats().frames,
            20 * stream.block as u64,
            "stream {index} kept playing"
        );
        assert!(stream.engine.is_running());
    }
}

#[test]
fn a_stream_at_another_rate_keeps_its_own_conversion() {
    // A 44.1 kHz client beside a 48 kHz one: the device runs at 48, so one stream is
    // converted and one is not, and neither may borrow the other's numbers.
    let converted = Geometry {
        burst: 192,
        device_buffer: 384,
        block: 480,
    };
    let native = Geometry {
        burst: 192,
        device_buffer: 384,
        block: 480,
    };
    let converted_stream = Scripted::open(
        converted.device(),
        StreamConfig {
            sample_rate: Some(44_100),
            ..converted.config()
        },
    );
    let native_stream = Scripted::open(native.device(), native.config());
    converted_stream.engine.start().unwrap();
    native_stream.engine.start().unwrap();
    for _ in 0..50 {
        converted_stream
            .device
            .deliver_frames(converted_stream.block);
        native_stream.device.deliver_frames(native_stream.block);
    }
    let granted = converted_stream.engine.granted();
    assert_eq!(
        granted.sample_rate, 48_000,
        "the device plays what it plays"
    );
    assert_eq!(
        granted.client_sample_rate, 44_100,
        "the client writes its own"
    );
    assert_eq!(native_stream.engine.granted().client_sample_rate, 48_000);

    // The converted client is asked for fewer frames per device block — the same audio,
    // counted at its own rate — and the ratio is 44.1/48, not one and not a guess.
    let converted_frames = converted_stream.produced() as f64;
    let native_frames = native_stream.produced() as f64;
    let ratio = converted_frames / native_frames;
    assert!(
        (ratio - 44_100.0 / 48_000.0).abs() < 0.01,
        "the converted stream asked for {converted_frames} frames against {native_frames}: \
         a ratio of {ratio:.4}"
    );
    assert_eq!(converted_stream.engine.stats().frames, 50 * 480);
    assert_eq!(native_stream.engine.stats().frames, 50 * 480);
}

#[test]
fn every_counter_is_per_stream() {
    // Statistics that lived in process-wide statics would show one stream's callbacks
    // against another's frames, and the first stream to close would reset the second's
    // window. Two streams with different periods make that visible.
    let short = Geometry {
        burst: 96,
        device_buffer: 192,
        block: 240,
    };
    let long = Geometry {
        burst: 96,
        device_buffer: 192,
        block: 1920,
    };
    let a = Scripted::open(short.device(), short.config());
    let b = Scripted::open(long.device(), long.config());
    a.engine.start().unwrap();
    b.engine.start().unwrap();
    for _ in 0..10 {
        a.device.deliver_frames(a.block);
    }
    for _ in 0..3 {
        b.device.deliver_frames(b.block);
    }
    assert_eq!(a.engine.stats().callbacks, 10);
    assert_eq!(b.engine.stats().callbacks, 3);
    assert_eq!(a.engine.stats().frames, 10 * 240);
    assert_eq!(b.engine.stats().frames, 3 * 1920);
    assert_eq!(a.engine.granted().block_frames, 240);
    assert_eq!(b.engine.granted().block_frames, 1920);
}
