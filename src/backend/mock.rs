//! A scripted device: the same contract as `AaudioBackend`, with the clock under
//! the test's control instead of a HAL's.
//!
//! Period events happen only when a test asks for one, on the test's own thread,
//! so callback cadence, block boundaries, positions, pauses and underruns are all
//! deterministic — no sleeps, no races, no third-party timing to blame. Every
//! semantic claim the engine makes is tested through this file.

use std::sync::{Arc, Mutex};

use super::{Backend, Sink};
use crate::engine::{block_frames_for, plausible_burst};
use crate::types::{Error, Events, Format, Granted, Sharing, StreamConfig, Timestamp};

#[derive(Clone, Copy, Debug)]
pub struct MockConfig {
    pub device_id: i32,
    pub device_channels: u16,
    pub sample_rate: i32,
    pub burst_frames: u32,
    /// Frames the device's own ring holds.
    pub device_buffer_frames: u32,
    /// Frames the device and its HAL add below us.
    pub stream_latency_frames: u32,
    /// Whether an `Exclusive` request is granted or silently downgraded. A device
    /// can answer `Shared` without ever refusing the request, which is why the
    /// granted mode is read back rather than assumed.
    pub exclusive_supported: bool,
    /// Refuse this at open, as a device with an unsupported rate would.
    pub refuse_open: Option<Error>,
    /// Fail the start, as a device whose route vanished would.
    pub refuse_start: Option<Error>,
}

impl Default for MockConfig {
    fn default() -> Self {
        // A reference Android device: 48 kHz float stereo, a 128-frame burst, and
        // 21 ms of device and mixer latency below us.
        Self {
            device_id: 7,
            device_channels: 2,
            sample_rate: 48_000,
            burst_frames: 128,
            device_buffer_frames: 256,
            stream_latency_frames: 1008,
            exclusive_supported: true,
            refuse_open: None,
            refuse_start: None,
        }
    }
}

#[derive(Default)]
struct State {
    sink: Option<Box<dyn Sink>>,
    requested: Option<StreamConfig>,
    opened: bool,
    running: bool,
    paused: bool,
    closed: bool,

    /// Where the device says it is, for `timestamp`.
    frame_position: i64,
    time_ns: i64,
    xruns: u32,
    queued_frames: u32,
    disconnected: bool,
    disconnect_reported: bool,

    open_calls: u32,
    start_calls: u32,
    pause_calls: u32,
    resume_calls: u32,
    stop_calls: u32,
    close_calls: u32,
}

/// The test's end of a scripted device.
#[derive(Clone)]
pub struct MockDevice {
    config: MockConfig,
    state: Arc<Mutex<State>>,
}

impl MockDevice {
    pub fn new(config: MockConfig) -> (MockDevice, MockBackend) {
        let state = Arc::new(Mutex::new(State {
            time_ns: 1_000_000,
            ..State::default()
        }));
        (
            MockDevice {
                config,
                state: state.clone(),
            },
            MockBackend { config, state },
        )
    }

    /// Lets one device period elapse: the device asks the sink to fill a burst and
    /// hands back what it produced. `None` when the device is not consuming, which
    /// is how a test asserts that nothing was asked for.
    pub fn deliver(&self) -> Option<Vec<f32>> {
        self.deliver_frames(self.config.burst_frames)
    }

    /// A period of a given size, for the devices that do not use their own burst.
    pub fn deliver_frames(&self, frames: u32) -> Option<Vec<f32>> {
        let mut state = self.state.lock().unwrap();
        if !state.running || state.paused || state.closed {
            return None;
        }
        let channels = self.config.device_channels as usize;
        let mut interleaved = vec![0.0f32; frames as usize * channels];
        state.sink.as_mut()?.fill(&mut interleaved, frames);
        state.frame_position += frames as i64;
        state.time_ns += (frames as i64 * 1_000_000_000) / self.config.sample_rate as i64;
        Some(interleaved)
    }

    /// Lets `count` periods elapse and concatenates what the device got.
    pub fn deliver_all(&self, count: usize) -> Vec<f32> {
        let mut out = Vec::new();
        for _ in 0..count {
            match self.deliver() {
                Some(period) => out.extend_from_slice(&period),
                None => break,
            }
        }
        out
    }

    /// The device went away: no more periods, and the engine is expected to say so.
    pub fn disconnect(&self) {
        let mut state = self.state.lock().unwrap();
        state.running = false;
        state.closed = true;
        state.disconnected = true;
        // The sink goes with the device, exactly as a closed stream does.
        state.sink = None;
    }

    /// The device counted a sequence break on its own.
    pub fn set_device_xruns(&self, count: u32) {
        self.state.lock().unwrap().xruns = count;
    }

    /// Frames the device still holds, for the latency figure.
    pub fn set_queued_frames(&self, frames: u32) {
        self.state.lock().unwrap().queued_frames = frames;
    }

    /// Where the device says it is playing, for `Engine::timestamp`.
    pub fn set_timestamp(&self, frame_position: i64, time_ns: i64) {
        let mut state = self.state.lock().unwrap();
        state.frame_position = frame_position;
        state.time_ns = time_ns;
    }

    /// What the engine asked the device for.
    pub fn requested(&self) -> Option<StreamConfig> {
        self.state.lock().unwrap().requested
    }

    pub fn open_calls(&self) -> u32 {
        self.state.lock().unwrap().open_calls
    }

    pub fn start_calls(&self) -> u32 {
        self.state.lock().unwrap().start_calls
    }

    pub fn pause_calls(&self) -> u32 {
        self.state.lock().unwrap().pause_calls
    }

    pub fn resume_calls(&self) -> u32 {
        self.state.lock().unwrap().resume_calls
    }

    pub fn stop_calls(&self) -> u32 {
        self.state.lock().unwrap().stop_calls
    }

    pub fn close_calls(&self) -> u32 {
        self.state.lock().unwrap().close_calls
    }

    /// Distinct open calls that received a sink: more than one means the engine
    /// reopened rather than reusing the device.
    pub fn open_count(&self) -> u32 {
        self.open_calls()
    }
}

pub struct MockBackend {
    config: MockConfig,
    state: Arc<Mutex<State>>,
}

impl Backend for MockBackend {
    fn open(&mut self, config: &StreamConfig, mut sink: Box<dyn Sink>) -> Result<Granted, Error> {
        let mut state = self.state.lock().unwrap();
        state.open_calls += 1;
        state.requested = Some(*config);
        if let Some(error) = self.config.refuse_open {
            return Err(error);
        }
        // A rate the client asks for is not something this device can be made to run at,
        // and it does not refuse the request either: it answers with the rate its own
        // route runs at and lets the engine convert. Nor is a channel count it cannot
        // offer a failure — a device with fewer channels gets the client's folded down.

        let burst = plausible_burst(self.config.burst_frames as i32).ok_or(Error::Unsupported)?;
        let granted = Granted {
            device_id: self.config.device_id,
            sample_rate: self.config.sample_rate,
            client_sample_rate: config.sample_rate.unwrap_or(self.config.sample_rate),
            device_channels: self.config.device_channels,
            client_channels: config.channels,
            frames_per_burst: burst,
            device_buffer_frames: self.config.device_buffer_frames,
            block_frames: block_frames_for(config.block_frames, burst),
            sharing: if config.sharing == Sharing::Exclusive && self.config.exclusive_supported {
                Sharing::Exclusive
            } else {
                // A device that cannot do it answers `Shared` rather than erroring.
                Sharing::Shared
            },
            performance: config.performance,
            format: Format::Float32,
            output_latency_frames: self.config.stream_latency_frames,
        };
        // The device is the first thing that knows the burst, so it is the thing
        // that sizes the client's block.
        sink.configure(&granted);
        // The device keeps the sink: it is what it calls when it needs audio, and
        // dropping it here would silently disconnect the client.
        state.sink = Some(sink);
        state.opened = true;
        state.paused = false;
        state.closed = false;
        state.queued_frames = self.config.device_buffer_frames;
        Ok(granted)
    }

    fn start(&mut self) -> Result<(), Error> {
        let mut state = self.state.lock().unwrap();
        state.start_calls += 1;
        if let Some(error) = self.config.refuse_start {
            return Err(error);
        }
        if !state.opened {
            return Err(Error::InvalidState);
        }
        // Starting a stream is a new timeline: the sink drops any half-consumed
        // block so the first block the client renders after this is its first.
        if let Some(sink) = state.sink.as_mut() {
            sink.reset();
        }
        state.running = true;
        state.paused = false;
        Ok(())
    }

    fn pause(&mut self) -> Result<(), Error> {
        let mut state = self.state.lock().unwrap();
        state.pause_calls += 1;
        if !state.running && !state.paused {
            return Err(Error::InvalidState);
        }
        // The device discards what it was holding, so a resume cannot replay it.
        state.paused = true;
        state.running = false;
        state.queued_frames = 0;
        Ok(())
    }

    fn resume(&mut self) -> Result<(), Error> {
        let mut state = self.state.lock().unwrap();
        state.resume_calls += 1;
        if !state.paused {
            return Err(Error::InvalidState);
        }
        state.paused = false;
        state.running = true;
        Ok(())
    }

    fn stop(&mut self) -> Result<(), Error> {
        let mut state = self.state.lock().unwrap();
        state.stop_calls += 1;
        if !state.opened {
            return Err(Error::InvalidState);
        }
        state.running = false;
        state.paused = false;
        state.queued_frames = 0;
        Ok(())
    }

    fn close(&mut self) {
        let mut state = self.state.lock().unwrap();
        state.close_calls += 1;
        state.running = false;
        state.paused = false;
        state.opened = false;
        state.closed = true;
        // Dropping the sink here is the contract that lets the engine drop the
        // client: no device thread can be inside it after this.
        state.sink = None;
    }

    fn device_xruns(&mut self) -> u32 {
        self.state.lock().unwrap().xruns
    }

    fn timestamp(&mut self) -> Option<Timestamp> {
        let state = self.state.lock().unwrap();
        if state.disconnected {
            return None;
        }
        Some(Timestamp {
            frame_position: state.frame_position,
            time_ns: state.time_ns,
        })
    }

    fn queued_frames(&mut self) -> u32 {
        self.state.lock().unwrap().queued_frames
    }

    fn take_events(&mut self) -> Events {
        let mut state = self.state.lock().unwrap();
        if state.disconnected && !state.disconnect_reported {
            state.disconnect_reported = true;
            return Events::DEVICE_LOST;
        }
        Events::empty()
    }

    fn sample_rate(&self) -> i32 {
        self.config.sample_rate
    }
}
