//! The test fixture: a client that writes a checkable ramp, and an engine wired to
//! a scripted device.
//!
//! Everything a test asserts on goes through this file, and nothing here has a
//! clock of its own: the device's periods happen when a test asks for them, so
//! there is nothing to sleep on and nothing to blame on timing.

#![allow(dead_code)]

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use gnaudio::backend::mock::{MockConfig, MockDevice};
use gnaudio::{Backend, BackendFactory, Client, Engine, Error, RenderContext, StreamConfig};

/// What a test can inspect after the fact.
#[derive(Default, Debug)]
pub struct Record {
    /// Every block position the client was handed, in order.
    pub positions: Vec<u64>,
    /// Every block size the client was handed, in order.
    pub frames: Vec<u32>,
    /// Every channel count the client was handed, in order.
    pub channels: Vec<u16>,
    /// The value the client's ramp has reached.
    pub next_frame: u64,
    /// Renders that took longer than their block's deadline, when a test asks the
    /// client to be slow on purpose.
    pub slow_blocks: u32,
}

impl Record {
    /// The channel-major slab the client would have produced, rebuilt from the
    /// ramp, so a test can compare it against what the device received.
    pub fn expected_slab(&self, channels: usize) -> Vec<f32> {
        let mut out = Vec::new();
        for (index, frames) in self.frames.iter().enumerate() {
            let start = self.positions[index];
            for channel in 0..channels {
                for frame in 0..*frames as u64 {
                    out.push((start + frame) as f32 + channel as f32 * 1000.0);
                }
            }
        }
        out
    }
}

/// A client that writes, per channel, the absolute frame index plus a per-channel
/// offset — so a test can tell order, continuity, duplication and channel mixing
/// apart from a single assertion.
pub struct RampClient {
    pub record: Arc<Mutex<Record>>,
    /// Sleep this long, once, on the given render index — used to force the
    /// engine's dropout accounting.
    pub sleep_on_render: Option<(u64, std::time::Duration)>,
    renders: u64,
    next_frame: u64,
    dropped: Arc<AtomicBool>,
}

impl RampClient {
    pub fn new(record: Arc<Mutex<Record>>, dropped: Arc<AtomicBool>) -> Self {
        Self {
            record,
            sleep_on_render: None,
            renders: 0,
            next_frame: 0,
            dropped,
        }
    }
}

impl Drop for RampClient {
    fn drop(&mut self) {
        self.dropped.store(true, Ordering::Relaxed);
    }
}

impl Client for RampClient {
    fn render(&mut self, ctx: &RenderContext, out: &mut [f32]) -> u32 {
        if let Some((index, duration)) = self.sleep_on_render {
            if self.renders == index {
                std::thread::sleep(duration);
            }
        }
        self.renders += 1;
        let frames = ctx.frames as usize;
        let channels = ctx.channels as usize;
        for channel in 0..channels {
            for frame in 0..frames {
                out[channel * frames + frame] =
                    (self.next_frame + frame as u64) as f32 + channel as f32 * 1000.0;
            }
        }
        let mut record = self.record.lock().unwrap();
        record.positions.push(ctx.position);
        record.frames.push(ctx.frames);
        record.channels.push(ctx.channels);
        record.next_frame = self.next_frame;
        self.next_frame += ctx.frames as u64;
        ctx.frames
    }
}

/// One scripted device, its engine, and the record of what the client saw.
pub struct Fixture {
    pub engine: Engine,
    pub device: MockDevice,
    pub record: Arc<Mutex<Record>>,
    pub client_dropped: Arc<AtomicBool>,
    pub devices: Arc<Mutex<Vec<MockDevice>>>,
    pub config: StreamConfig,
}

impl Fixture {
    /// Opens an engine over a scripted device, with a client writing a ramp.
    pub fn open(device_config: MockConfig, config: StreamConfig) -> Result<Fixture, Error> {
        Self::open_with(device_config, config, None)
    }

    /// As [`Fixture::open`], with a client that sleeps on one render.
    pub fn open_with(
        device_config: MockConfig,
        config: StreamConfig,
        sleep_on_render: Option<(u64, std::time::Duration)>,
    ) -> Result<Fixture, Error> {
        let devices: Arc<Mutex<Vec<MockDevice>>> = Arc::new(Mutex::new(Vec::new()));
        let factory: BackendFactory = {
            let devices = devices.clone();
            Arc::new(move || {
                let (device, backend) = MockDevice::new(device_config);
                devices.lock().unwrap().push(device);
                Ok(Box::new(backend) as Box<dyn Backend>)
            })
        };
        let record = Arc::new(Mutex::new(Record::default()));
        let client_dropped = Arc::new(AtomicBool::new(false));
        let mut client = RampClient::new(record.clone(), client_dropped.clone());
        client.sleep_on_render = sleep_on_render;

        let engine = Engine::open_with_factory(Box::new(client), config, factory)?;
        let device = devices.lock().unwrap().last().unwrap().clone();
        Ok(Fixture {
            engine,
            device,
            record,
            client_dropped,
            devices,
            config,
        })
    }

    /// The device the engine is on now, after any reopen.
    pub fn current_device(&self) -> MockDevice {
        self.devices
            .lock()
            .unwrap()
            .last()
            .expect("a device was opened")
            .clone()
    }

    /// As [`Fixture::open`], with a client of the caller's own choosing.
    pub fn open_with_client(
        device_config: MockConfig,
        config: StreamConfig,
        client: Box<dyn gnaudio::Client>,
    ) -> Result<Fixture, Error> {
        let devices: Arc<Mutex<Vec<MockDevice>>> = Arc::new(Mutex::new(Vec::new()));
        let factory: BackendFactory = {
            let devices = devices.clone();
            Arc::new(move || {
                let (device, backend) = MockDevice::new(device_config);
                devices.lock().unwrap().push(device);
                Ok(Box::new(backend) as Box<dyn Backend>)
            })
        };
        let engine = Engine::open_with_factory(client, config, factory)?;
        let device = devices.lock().unwrap().last().unwrap().clone();
        Ok(Fixture {
            engine,
            device,
            record: Arc::new(Mutex::new(Record::default())),
            client_dropped: Arc::new(AtomicBool::new(false)),
            devices,
            config,
        })
    }

    pub fn devices_opened(&self) -> usize {
        self.devices.lock().unwrap().len()
    }

    pub fn record(&self) -> std::sync::MutexGuard<'_, Record> {
        self.record.lock().unwrap()
    }
}

/// A parsed view of what the device was handed: channel-major, so it can be
/// compared against what the client produced.
pub struct Slab {
    pub frames: usize,
    pub channels: usize,
    pub data: Vec<f32>,
}

impl Slab {
    /// De-interleaves the device's view of the stream into channel planes.
    pub fn from_interleaved(interleaved: &[f32], channels: usize) -> Slab {
        let frames = interleaved.len() / channels.max(1);
        let mut data = vec![0.0; interleaved.len()];
        for frame in 0..frames {
            for channel in 0..channels {
                data[channel * frames + frame] = interleaved[frame * channels + channel];
            }
        }
        Slab {
            frames,
            channels,
            data,
        }
    }

    pub fn channel(&self, channel: usize) -> &[f32] {
        &self.data[channel * self.frames..(channel + 1) * self.frames]
    }
}

/// Counts how many times the client was asked to render.
pub fn renders(fixture: &Fixture) -> usize {
    fixture.record().positions.len()
}

/// The longest run of frames the client produced, for continuity checks.
pub fn produced_frames(fixture: &Fixture) -> u64 {
    fixture.record().next_frame
}
