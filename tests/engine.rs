//! The engine's semantics, driven by a scripted device: block sizing, positions,
//! pause and resume, disconnects, dropouts, and what a client is told.

mod harness;

use std::sync::Arc;
use std::time::Duration;

use gnaudio::backend::mock::MockConfig;
use gnaudio::*;
use harness::*;

/// A reference device: 48 kHz stereo float, a 128-frame burst, a 256-frame ring,
/// and 21 ms below us.
fn device() -> MockConfig {
    MockConfig::default()
}

fn config(block_frames: u32) -> StreamConfig {
    StreamConfig {
        block_frames,
        ..StreamConfig::default()
    }
}

#[test]
fn the_default_is_the_platform_default_and_it_is_reported() {
    // Shared unless asked otherwise: it is what AAudio does when nothing is requested,
    // and what the PulseAudio sink this was measured against uses. Anything shorter is
    // a request, and the granted mode is what gets reported.
    let fixture = Fixture::open(device(), config(256)).unwrap();
    assert_eq!(fixture.device.requested().unwrap().sharing, Sharing::Shared);
    let granted = fixture.engine.granted();
    assert_eq!(granted.sharing, Sharing::Shared);
    assert_eq!(granted.performance, Performance::LowLatency);
    assert_eq!(granted.sample_rate, 48_000);
    assert_eq!(granted.frames_per_burst, 128);
    assert_eq!(granted.device_channels, 2);
    assert_eq!(granted.client_channels, 2);
    assert_eq!(granted.device_buffer_frames, 256);
    assert_eq!(granted.format, Format::Float32);
}

#[test]
fn an_exclusive_request_that_is_granted_is_reported_as_exclusive() {
    let mut wanted = config(256);
    wanted.sharing = Sharing::Exclusive;
    let fixture = Fixture::open(device(), wanted).unwrap();
    assert_eq!(
        fixture.device.requested().unwrap().sharing,
        Sharing::Exclusive
    );
    assert_eq!(fixture.engine.granted().sharing, Sharing::Exclusive);
}

#[test]
fn a_refused_exclusive_request_is_reported_as_shared() {
    // A device may answer `Shared` without refusing the request, so only reading
    // the granted mode back tells the truth.
    let mut device_config = device();
    device_config.exclusive_supported = false;
    let mut wanted = config(256);
    wanted.sharing = Sharing::Exclusive;
    let fixture = Fixture::open(device_config, wanted).unwrap();
    assert_eq!(
        fixture.device.requested().unwrap().sharing,
        Sharing::Exclusive
    );
    assert_eq!(
        fixture.engine.granted().sharing,
        Sharing::Shared,
        "an exclusive request that the device cannot honour must not be reported as exclusive"
    );
}

#[test]
fn the_block_is_the_clients_own_period_floored_at_one_burst() {
    // Not a whole number of bursts. Rounding up makes the engine demand more audio per
    // block than the client produces at a time, and the difference is silence spliced
    // into the stream — which is exactly what a 192-frame burst did to a 480-frame period
    // (576), while a 96-frame burst left the same period alone (480) and worked.
    for (requested, expected) in [(128, 128), (200, 200), (256, 256), (300, 300), (1024, 1024)] {
        let fixture = Fixture::open(device(), config(requested)).unwrap();
        assert_eq!(
            fixture.engine.granted().block_frames,
            expected,
            "{requested} frames on a 128 frame device"
        );
        fixture.engine.close();
    }

    // A period shorter than the device's burst is raised to it: a device cannot be woken
    // more often than its own burst.
    let mut wide = device();
    wide.burst_frames = 192;
    let fixture = Fixture::open(wide, config(480)).unwrap();
    assert_eq!(
        fixture.engine.granted().block_frames,
        480,
        "480 on a 192 frame burst stays 480 rather than becoming 576"
    );
    fixture.engine.close();

    let mut wide = device();
    wide.burst_frames = 192;
    let fixture = Fixture::open(wide, config(100)).unwrap();
    assert_eq!(fixture.engine.granted().block_frames, 192);
    fixture.engine.close();
}

#[test]
fn one_block_spans_several_device_periods() {
    let fixture = Fixture::open(device(), config(512)).unwrap();
    fixture.engine.start().unwrap();

    // One period of 128 frames: the client is asked once, and the device is handed
    // the first quarter of the block it produced.
    let first = fixture.device.deliver().unwrap();
    assert_eq!(first.len(), 128 * 2);
    assert_eq!(renders(&fixture), 1, "a 512 frame block is one render");
    let record = fixture.record();
    assert_eq!(record.frames, vec![512]);
    assert_eq!(record.positions, vec![0]);
    drop(record);
    for (index, sample) in first.iter().enumerate() {
        let frame = index / 2;
        let channel = index % 2;
        assert_eq!(
            *sample,
            frame as f32 + channel as f32 * 1000.0,
            "period sample {index} of the first block"
        );
    }

    // The rest of the block, period by period, with no further render.
    let second = fixture.device.deliver().unwrap();
    assert_eq!(renders(&fixture), 1);
    for (index, sample) in second.iter().enumerate() {
        let frame = 128 + index / 2;
        let channel = index % 2;
        assert_eq!(*sample, frame as f32 + channel as f32 * 1000.0);
    }

    // Third and fourth periods: still the same block.
    fixture.device.deliver().unwrap();
    fixture.device.deliver().unwrap();
    assert_eq!(renders(&fixture), 1, "four periods are one 512 frame block");

    // And once the block is spent, the next period asks for another.
    fixture.device.deliver().unwrap();
    assert_eq!(renders(&fixture), 2);
    let record = fixture.record();
    assert_eq!(
        record.positions,
        vec![0, 512],
        "positions are block aligned"
    );
    assert_eq!(record.frames, vec![512, 512]);
}

#[test]
fn a_block_smaller_than_the_device_period_is_raised_to_one_burst() {
    // Audio cannot be handed over sooner than the device runs, so a block below one
    // burst is raised to it and reported, rather than promised as smaller.
    let mut device_config = device();
    device_config.burst_frames = 512;
    let fixture = Fixture::open(device_config, config(128)).unwrap();
    assert_eq!(fixture.engine.granted().block_frames, 512);
    fixture.engine.start().unwrap();
    let period = fixture.device.deliver().unwrap();
    assert_eq!(period.len(), 512 * 2);
    assert_eq!(renders(&fixture), 1);
    assert_eq!(
        fixture.record().frames,
        vec![512],
        "told, not quietly assumed"
    );
}

#[test]
fn a_stream_is_continuous_across_every_block_boundary() {
    // The device hands over audio in pieces that need not match the client's block,
    // so the engine must carry a half-consumed block across periods. A 384 frame
    // block on a 128 frame device is three periods: the second period must start
    // exactly where the first left off.
    let fixture = Fixture::open(device(), config(384)).unwrap();
    fixture.engine.start().unwrap();

    let mut expected_frame = 0u64;
    for period in 0..9 {
        let samples = fixture.device.deliver().unwrap();
        assert_eq!(samples.len(), 128 * 2);
        for (index, sample) in samples.iter().enumerate() {
            let frame = expected_frame + (index / 2) as u64;
            let channel = index % 2;
            assert_eq!(
                *sample,
                frame as f32 + channel as f32 * 1000.0,
                "period {period}, frame {frame}, channel {channel}"
            );
        }
        expected_frame += 128;
    }
    assert_eq!(
        renders(&fixture),
        3,
        "nine periods are three 384 frame blocks"
    );
    assert_eq!(fixture.record().positions, vec![0, 384, 768]);
}

#[test]
fn position_is_block_aligned_and_start_begins_a_new_timeline() {
    let fixture = Fixture::open(device(), config(256)).unwrap();
    fixture.engine.start().unwrap();
    fixture.device.deliver_all(8);
    assert_eq!(fixture.record().positions, vec![0, 256, 512, 768]);
    assert_eq!(fixture.engine.sample_position().0, 768);

    // Stopping and starting is a new timeline; the position goes back to zero and
    // the client's first block is its first again.
    fixture.engine.stop().unwrap();
    fixture.engine.start().unwrap();
    fixture.device.deliver_all(4);
    let record = fixture.record();
    assert_eq!(&record.positions[4..], &[0, 256]);
    assert_eq!(fixture.engine.sample_position().0, 256);
}

#[test]
fn pause_freezes_the_position_and_resume_leaves_no_gap() {
    let fixture = Fixture::open(device(), config(256)).unwrap();
    fixture.engine.start().unwrap();
    fixture.device.deliver_all(4);
    let before = fixture.engine.sample_position().0;
    let renders_before = renders(&fixture);

    fixture.engine.pause().unwrap();
    assert!(!fixture.engine.is_running());
    assert!(
        fixture.device.deliver().is_none(),
        "a paused device must hand out no periods"
    );
    assert_eq!(renders(&fixture), renders_before, "and ask for no blocks");
    assert_eq!(
        fixture.engine.sample_position().0,
        before,
        "the position must not advance while paused"
    );

    fixture.engine.resume().unwrap();
    assert!(fixture.engine.is_running());
    fixture.device.deliver_all(4);
    let record = fixture.record();
    assert_eq!(
        record.positions,
        vec![0, 256, 512, 768],
        "the timeline continues where it was frozen, with nothing skipped"
    );
    assert_eq!(
        record.frames.iter().sum::<u32>(),
        4 * 256,
        "and no audio is lost or repeated across the pause"
    );
}

#[test]
fn a_device_with_spare_channels_gets_silence_on_them() {
    let mut device_config = device();
    device_config.device_channels = 4;
    let mut stream = config(128);
    stream.channels = 2;
    let fixture = Fixture::open(device_config, stream).unwrap();
    assert_eq!(fixture.engine.granted().client_channels, 2);
    assert_eq!(fixture.engine.granted().device_channels, 4);
    fixture.engine.start().unwrap();

    let period = fixture.device.deliver().unwrap();
    assert_eq!(period.len(), 128 * 4);
    for frame in 0..128 {
        let base = frame * 4;
        assert_eq!(period[base], frame as f32, "channel 0 carries the client");
        assert_eq!(period[base + 1], frame as f32 + 1000.0, "and channel 1");
        assert_eq!(
            period[base + 2],
            0.0,
            "channel 2 is the device's own, silent"
        );
        assert_eq!(period[base + 3], 0.0, "channel 3 too");
    }
}

#[test]
fn latency_is_recomputed_from_the_devices_live_numbers() {
    let fixture = Fixture::open(device(), config(256)).unwrap();
    // 1008 frames below us + 256 queued + the larger of block and burst.
    fixture.device.set_queued_frames(256);
    assert_eq!(fixture.engine.latency_frames(), 1008 + 256 + 256);

    // A device that has drained its ring is closer to the speaker.
    fixture.device.set_queued_frames(0);
    assert_eq!(fixture.engine.latency_frames(), 1008 + 256);
}

#[test]
fn a_disconnect_is_reported_and_reopen_recovers() {
    let fixture = Fixture::open(device(), config(256)).unwrap();
    fixture.engine.start().unwrap();
    fixture.device.deliver_all(4);
    let before = fixture.engine.sample_position().0;

    fixture.device.disconnect();
    let events = fixture.engine.take_events();
    assert!(
        events.contains(Events::DEVICE_LOST),
        "the loss must be visible"
    );
    assert!(!fixture.engine.is_running());
    assert!(fixture.device.deliver().is_none());

    let granted = fixture.engine.reopen().unwrap();
    assert_eq!(fixture.devices_opened(), 2, "a second device was opened");
    let events = fixture.engine.take_events();
    assert!(
        events.contains(Events::RESYNC),
        "the clock is not continuous across a reopen, and the client must be told"
    );
    assert!(
        fixture.engine.is_running(),
        "playback resumed on the new device"
    );
    assert_eq!(granted.block_frames, 256);

    // The client keeps going from where it was, on the new device.
    let device = fixture.current_device();
    device.deliver_all(2);
    let record = fixture.record();
    assert_eq!(
        record.positions,
        vec![0, 256, 512],
        "the position continues across the reopen: the same client, the same timeline"
    );
    assert_eq!(fixture.engine.stats().reopens, 1);
    assert_eq!(
        before, 256,
        "two blocks were played before the device went away"
    );
}

#[test]
fn stats_count_what_the_device_did() {
    let fixture = Fixture::open(device(), config(256)).unwrap();
    fixture.device.set_device_xruns(3);
    fixture.engine.start().unwrap();
    fixture.device.deliver_all(6);

    let stats = fixture.engine.stats();
    assert_eq!(stats.callbacks, 3, "six periods are three 256 frame blocks");
    assert_eq!(
        stats.frames,
        6 * 128,
        "and the device was handed every frame"
    );
    assert_eq!(
        stats.device_xruns, 3,
        "the device's own count is passed through"
    );
    assert_eq!(stats.underruns, 0);
    assert_eq!(stats.reopens, 0);
}

#[test]
fn a_client_that_misses_its_deadline_is_counted() {
    // A 128 frame period at 48 kHz is 2.7 ms; a client that takes 50 ms is late.
    let fixture =
        Fixture::open_with(device(), config(128), Some((0, Duration::from_millis(50)))).unwrap();
    fixture.engine.start().unwrap();
    fixture.device.deliver_all(2);

    let stats = fixture.engine.stats();
    assert_eq!(stats.underruns, 1, "one late block, counted");
    let events = fixture.engine.take_events();
    assert!(
        events.contains(Events::UNDERRUN),
        "and reported to the client, which cannot see it from `render`"
    );
}

#[test]
fn close_stops_the_device_and_releases_the_client() {
    let Fixture {
        engine,
        device,
        client_dropped,
        ..
    } = Fixture::open(device(), config(256)).unwrap();
    engine.start().unwrap();
    device.deliver_all(2);
    assert!(!client_dropped.load(std::sync::atomic::Ordering::Relaxed));

    engine.close();
    assert_eq!(device.close_calls(), 1, "the device is closed exactly once");
    assert!(
        client_dropped.load(std::sync::atomic::Ordering::Relaxed),
        "and the client is released only after the device can no longer call it"
    );
}

#[test]
fn asks_the_device_for_what_the_client_wants() {
    let mut stream = config(384);
    stream.channels = 2;
    stream.sample_rate = Some(48_000);
    stream.device_buffer_frames = Some(512);
    stream.performance = Performance::PowerSaving;
    let fixture = Fixture::open(device(), stream).unwrap();

    let requested = fixture.device.requested().unwrap();
    assert_eq!(requested.block_frames, 384);
    assert_eq!(requested.channels, 2);
    assert_eq!(requested.sample_rate, Some(48_000));
    assert_eq!(requested.device_buffer_frames, Some(512));
    assert_eq!(requested.performance, Performance::PowerSaving);
    assert_eq!(
        fixture.engine.granted().performance,
        Performance::PowerSaving
    );
}

#[test]
fn a_route_answers_with_its_own_geometry_and_the_engine_works_with_it() {
    // Neither of these is a refusal. A device only ever runs at one rate and one channel
    // count, and both are the route's rather than the client's: each is the answer the
    // engine converts from.
    let mut stream = config(256);
    stream.sample_rate = Some(44_100);
    let fixture = Fixture::open(device(), stream).expect("the device answers with 48 kHz");
    assert_eq!(fixture.engine.granted().sample_rate, 48_000);
    assert_eq!(fixture.engine.granted().client_sample_rate, 44_100);

    let mut stream = config(256);
    stream.channels = 4;
    let fixture = Fixture::open(device(), stream).expect("four planes fold into two");
    assert_eq!(fixture.engine.granted().client_channels, 4);
    assert_eq!(fixture.engine.granted().device_channels, 2);

    // A device that will not open at all reports why.
    let mut device_config = device();
    device_config.refuse_open = Some(Error::Disconnected);
    let error = Fixture::open(device_config, config(256)).err().unwrap();
    assert_eq!(error, Error::Disconnected);
}

#[test]
fn a_start_that_the_device_refuses_is_reported() {
    let mut device_config = device();
    device_config.refuse_start = Some(Error::Disconnected);
    let fixture = Fixture::open(device_config, config(256)).unwrap();
    assert_eq!(fixture.engine.start().err(), Some(Error::Disconnected));
    assert!(!fixture.engine.is_running(), "and nothing is consumed");
    assert!(
        fixture.device.deliver().is_none(),
        "in particular, no period may reach the client"
    );
}

#[test]
fn a_device_that_never_answers_the_default_request_still_works() {
    // The smallest device worth supporting: a 64 frame burst, one channel.
    let device_config = MockConfig {
        burst_frames: 64,
        device_channels: 1,
        device_buffer_frames: 128,
        ..MockConfig::default()
    };
    let mut stream = config(64);
    stream.channels = 1;
    let fixture = Fixture::open(device_config, stream).unwrap();
    assert_eq!(fixture.engine.granted().block_frames, 64);
    fixture.engine.start().unwrap();
    let period = fixture.device.deliver().unwrap();
    assert_eq!(period.len(), 64);
    assert_eq!(period[0], 0.0);
    assert_eq!(period[63], 63.0);
}

#[test]
fn the_client_is_only_ever_asked_from_the_device() {
    // A regression guard for the threading contract: every render happens because a
    // period was demanded, so the count can never run ahead of what the device was
    // given.
    let fixture = Fixture::open(device(), config(128)).unwrap();
    fixture.engine.start().unwrap();
    let mut delivered = 0u32;
    for _ in 0..16 {
        if fixture.device.deliver().is_some() {
            delivered += 1;
        }
    }
    assert_eq!(
        renders(&fixture) as u32,
        delivered,
        "one render per period, because the block is one period here"
    );
}

/// The engine must keep working when a caller polls it from another thread while
/// the device is running — the reason its counters are atomics.
#[test]
fn queries_are_safe_from_another_thread_while_running() {
    let fixture = Fixture::open(device(), config(256)).unwrap();
    fixture.engine.start().unwrap();
    let engine = Arc::new(fixture.engine);
    let poller = {
        let engine = engine.clone();
        std::thread::spawn(move || {
            for _ in 0..100 {
                let _ = engine.stats();
                let _ = engine.sample_position();
                let _ = engine.latency_frames();
                let _ = engine.take_events();
            }
        })
    };
    fixture.device.deliver_all(32);
    poller.join().unwrap();
    assert_eq!(engine.stats().callbacks, 16, "32 periods, 256 frame blocks");
}

/// A client that only ever fills the first half of the block it is asked for.
///
/// That is what an ALSA plugin does when its ring runs dry: the device still gets a whole
/// block, because a device wants whole blocks, and the tail is silence the engine inserted.
struct HalfSupply {
    renders: Arc<std::sync::atomic::AtomicU32>,
    supplied: Arc<std::sync::atomic::AtomicU64>,
}

impl Client for HalfSupply {
    fn render(&mut self, ctx: &RenderContext, out: &mut [f32]) -> u32 {
        let frames = ctx.frames as usize;
        let channels = ctx.channels.max(1) as usize;
        let half = frames / 2;
        for channel in 0..channels {
            for frame in 0..half {
                out[channel * frames + frame] = 0.5;
            }
        }
        self.renders
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        self.supplied
            .fetch_add(half as u64, std::sync::atomic::Ordering::Relaxed);
        half as u32
    }
}

#[test]
fn silence_inserted_for_a_short_pull_does_not_count_as_delivered() {
    // The position is derived from what the engine says it delivered. Counting a whole
    // block when the client only filled half of it inflates the position permanently,
    // and an inflated position is what once pushed the ALSA plugin's reported `hw_ptr`
    // past the frames the app had written — so `avail` read back larger than its buffer,
    // and the client's own buffer arithmetic went with it.
    let renders = Arc::new(std::sync::atomic::AtomicU32::new(0));
    let supplied = Arc::new(std::sync::atomic::AtomicU64::new(0));
    let client = HalfSupply {
        renders: renders.clone(),
        supplied: supplied.clone(),
    };
    let fixture = Fixture::open_with_client(device(), config(256), Box::new(client)).unwrap();
    fixture.engine.start().unwrap();
    fixture.device.deliver_all(16);

    let renders = renders.load(std::sync::atomic::Ordering::Relaxed) as u64;
    let supplied = supplied.load(std::sync::atomic::Ordering::Relaxed);
    assert!(renders > 0, "the client was asked for audio");
    assert_eq!(
        fixture.engine.stats().frames,
        supplied,
        "only the frames the client produced may count as delivered"
    );
    assert!(
        supplied < renders * 256,
        "half of each block was never produced, so less than whole blocks was delivered"
    );
}

#[test]
fn a_route_whose_own_rate_differs_is_converted_rather_than_repitched() {
    // A device answers with its own rate, not the client's: 44.1 kHz is a real answer on
    // a Bluetooth route. Handing it 48 kHz frames would play everything 8.8 % slow and
    // low, with a backlog that grows the whole time.
    let mut device_config = device();
    device_config.sample_rate = 44_100;
    let mut wanted = config(256);
    wanted.sample_rate = Some(48_000);
    let fixture = Fixture::open(device_config, wanted).unwrap();

    let granted = fixture.engine.granted();
    assert_eq!(granted.sample_rate, 44_100, "the device's answer");
    assert_eq!(
        granted.client_sample_rate, 48_000,
        "the client's contract, kept alongside it"
    );

    fixture.engine.start().unwrap();
    // One device period is one 128-frame burst on this device, whatever the rate.
    let samples = fixture.device.deliver().unwrap();
    assert_eq!(samples.len(), 128 * 2, "a device period is still a period");

    // The client was asked for its own frames at its own rate. The record is read and
    // released rather than held: delivering a period runs the client, and the client
    // takes that same lock.
    let (asked, positions) = {
        let record = fixture.record();
        (record.frames.clone(), record.positions.clone())
    };
    assert_eq!(
        asked,
        vec![279],
        "256 frames at 44.1 kHz are 278.6 frames at 48 kHz, rounded up"
    );
    assert_eq!(
        positions,
        vec![0],
        "positions are counted in the client's frames"
    );

    // ...and the audio arrives at the rate the device plays: a ramp rises by the ratio
    // between the two rates per device frame. A repitch would leave it rising by one.
    let left: Vec<f32> = samples.chunks(2).map(|frame| frame[0]).collect();
    let step = 48_000.0 / 44_100.0;
    for index in 3..left.len() {
        let delta = left[index] - left[index - 1];
        assert!(
            (delta - step as f32).abs() < 0.2,
            "frame {index} advanced by {delta}, expected {step}"
        );
    }

    // Two periods make one 256-frame block, so five periods are three blocks, and each
    // one continues the client's timeline rather than restarting it.
    for _ in 0..4 {
        fixture.device.deliver().unwrap();
    }
    assert_eq!(
        fixture.record().positions,
        vec![0, 279, 558],
        "each block consumes the client's frames, not the device's"
    );
}

#[test]
fn a_mono_route_gets_the_mean_of_the_clients_channels() {
    // Fewer channels than the client drives is a route's own business, not a failure:
    // taking the first plane alone would silently drop half the mix.
    let mut device_config = device();
    device_config.device_channels = 1;
    let fixture = Fixture::open(device_config, config(256)).unwrap();
    assert_eq!(fixture.engine.granted().client_channels, 2);

    fixture.engine.start().unwrap();
    let samples = fixture.device.deliver().unwrap();
    assert_eq!(samples.len(), 128, "one plane per frame");
    // The ramp writes frame + 0 for the left channel and frame + 1000 for the right, so
    // the mean is frame + 500.
    for (frame, sample) in samples.iter().enumerate().take(128) {
        assert!(
            (*sample - (frame as f32 + 500.0)).abs() < 0.001,
            "frame {frame} is {sample}"
        );
    }
}
