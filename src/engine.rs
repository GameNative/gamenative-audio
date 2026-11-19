//! The engine: the surface a client programs against, on top of a [`Backend`].
//!
//! What that surface is, concretely:
//!
//! * **The client owns its block size.** [`StreamConfig::block_frames`] is counted
//!   in device bursts, and the device's own period is invisible: one call to
//!   [`Client::render`] per block, spread over however many device periods that
//!   block covers.
//! * **The device's consumption is the clock.** [`Client::render`] runs once per
//!   block, on whichever thread the backend spends waiting for the device — the
//!   HAL's, when it asks for a period through a callback, or the backend's own
//!   writer thread, when a blocking write is what paces it.
//! * **Positions are block-aligned**, zero at the first block after a `start`, and
//!   one block per callback after that. They do not advance while paused, and they
//!   survive a reopen, so a client's timeline is never silently rebased.
//! * **Latency is reported, not promised.** [`Engine::granted`] says what the
//!   device actually gave — including whether persistent-exclusive sharing
//!   survived the request — and [`Engine::latency_frames`] recomputes the honest
//!   figure from the device's live numbers every time it is asked.
//! * **Pause is a pause.** It does not reopen the device, and it does not let the
//!   device replay audio it was still holding.
//!
//! Threading, in two planes:
//!
//! * **Device plane** — [`Client::render`], on the backend's audio thread, via a
//!   [`Sink`] the backend owns. Nothing on this path locks, allocates or blocks.
//! * **Control plane** — every `Engine` method: one mutex, held only by the app's
//!   own threads.
//!
//! Counters live in atomics written by the device plane and read by the control
//! plane, so a client can poll [`Engine::stats`] while the audio thread is
//! mid-block.
//!
//! The client itself is owned by the engine ([`ClientSlot`]), not by a stream, for
//! one reason: a device can disappear and be reopened underneath a running client,
//! and the client must survive that. It is reached only through the device that is
//! currently calling it, and only ever by one thread at a time.

use std::cell::UnsafeCell;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use crate::backend::{Backend, Sink};
use crate::types::{Error, Events, Granted, RenderContext, Stats, StreamConfig, Timestamp};

/// Frames one block may cover, as a bound on a device answering with a tiny burst.
const MAX_BLOCK_BURSTS: u32 = 64;

/// The device's period may not exceed this, or its answer is not believed.
const MAX_BURST_FRAMES: u32 = 8192;

/// The one thing a client implements.
pub trait Client: Send {
    /// Fill one block of audio.
    ///
    /// `out` is channel-major — `ctx.frames` frames of channel 0, then
    /// `ctx.frames` of channel 1, and so on — so `split_at_mut` is how a stereo
    /// client gets its two planes.
    /// It is zero-filled before this is called, so a client with nothing to say can
    /// return immediately.
    ///
    /// Returns the number of frames it actually filled, from the start of each plane.
    /// Everything beyond that is treated as audio the client never produced: it still
    /// reaches the device, because a device wants whole blocks, but it must not count
    /// towards the position. Counting it was a real defect — a short pull inflated the
    /// position permanently, the position eventually ran past the frames the caller had
    /// written, and the ALSA plugin's `avail` then read back larger than its own buffer.
    ///
    /// This runs on the *render* thread, which this crate owns and which is not the
    /// device's real-time thread — the device asks for a period, this thread produces
    /// it, and the device's thread waits with a deadline. A client may therefore block
    /// here (the ALSA plugin waits, briefly, for the caller to finish a period it has
    /// already begun), which it could not do if this ran on the device's own callback.
    /// Stalling for longer than one device period is still counted as a dropout.
    fn render(&mut self, ctx: &RenderContext, out: &mut [f32]) -> u32;
}

/// The client, in a stable slot that outlives any one device.
struct ClientSlot(UnsafeCell<Box<dyn Client>>);

// SAFETY: the box is never moved out of the slot, and `render` is entered by one
// device thread at a time: a device is closed before another is opened (see
// `Engine::reopen`), and `Backend::close` is what stops a device thread calling in.
unsafe impl Send for ClientSlot {}
unsafe impl Sync for ClientSlot {}

impl ClientSlot {
    fn new(client: Box<dyn Client>) -> Self {
        Self(UnsafeCell::new(client))
    }

    /// # Safety
    /// The caller must be the device's real-time thread, and no other device may
    /// be running against this slot.
    unsafe fn render(&self, ctx: &RenderContext, out: &mut [f32]) -> u32 {
        // SAFETY: see the type-level note: exclusive by construction.
        let client = unsafe { &mut *self.0.get() };
        client.render(ctx, out)
    }
}

/// Counters the device plane writes and the control plane reads.
#[derive(Default)]
struct Shared {
    /// Position of the block the device is playing, for `sample_position`.
    position: AtomicU64,
    /// Where the next block starts. This is the timeline's resume point: a device
    /// that starts calls [`Sink::reset`], which starts the next block here rather
    /// than at zero, so a reopen continues the client's timeline instead of
    /// silently rebasing it.
    resume_position: AtomicU64,
    time_ns: AtomicU64,
    callbacks: AtomicU64,
    frames: AtomicU64,
    underruns: AtomicU64,
    reopens: AtomicU64,
    events: AtomicU32,
    running: AtomicBool,
}

impl Shared {
    fn add_event(&self, event: Events) {
        self.events.fetch_or(event.bits(), Ordering::AcqRel);
    }
}

struct Ctl {
    backend: Option<Box<dyn Backend>>,
    config: StreamConfig,
    granted: Option<Granted>,
    /// Where a replacement device comes from: at open, and again at every reopen.
    factory: crate::BackendFactory,
}

/// A running audio device, and the client being fed by it.
pub struct Engine {
    ctl: Mutex<Ctl>,
    shared: Arc<Shared>,
    client: Arc<ClientSlot>,
}

impl Engine {
    /// Opens the device and prepares to play through `client`, using the platform's
    /// device stack. The device is opened, not started: nothing is consumed until
    /// [`Engine::start`].
    pub fn open(client: Box<dyn Client>, config: StreamConfig) -> Result<Engine, Error> {
        Self::open_with_factory(client, config, crate::default_factory())
    }

    /// Opens a device from a factory the caller supplies. Useful for embedding a
    /// different device stack, and how the test suite drives the whole engine
    /// without a device.
    pub fn open_with_factory(
        client: Box<dyn Client>,
        config: StreamConfig,
        factory: crate::BackendFactory,
    ) -> Result<Engine, Error> {
        if config.channels == 0 || config.block_frames == 0 {
            return Err(Error::Unsupported);
        }
        let shared = Arc::new(Shared::default());
        let slot = Arc::new(ClientSlot::new(client));

        let mut backend = factory()?;
        let granted = backend.open(
            &config,
            Box::new(EngineWorker::new(slot.clone(), shared.clone())),
        )?;
        log_open(&granted, &config);

        Ok(Engine {
            ctl: Mutex::new(Ctl {
                backend: Some(backend),
                config,
                granted: Some(granted),
                factory,
            }),
            shared,
            client: slot,
        })
    }

    /// What the device granted at open.
    pub fn granted(&self) -> Granted {
        self.ctl.lock().unwrap().granted.unwrap_or_default()
    }

    pub fn config(&self) -> StreamConfig {
        self.ctl.lock().unwrap().config
    }

    /// Begins playback, resetting the sample position to zero: this starts a new
    /// timeline, which is what the client's first block is relative to.
    pub fn start(&self) -> Result<(), Error> {
        let mut ctl = self.ctl.lock().unwrap();
        let Some(backend) = ctl.backend.as_mut() else {
            return Err(Error::InvalidState);
        };
        self.shared.position.store(0, Ordering::Relaxed);
        self.shared.resume_position.store(0, Ordering::Relaxed);
        self.shared.events.store(0, Ordering::Relaxed);
        backend.start()?;
        self.shared.running.store(true, Ordering::Release);
        crate::trace!("start: position reset to zero, device running");
        Ok(())
    }

    /// Stops consuming without releasing the device. The position is frozen, and
    /// anything the device was still holding is discarded, so a resume cannot
    /// replay stale audio.
    pub fn pause(&self) -> Result<(), Error> {
        let mut ctl = self.ctl.lock().unwrap();
        let Some(backend) = ctl.backend.as_mut() else {
            return Err(Error::InvalidState);
        };
        backend.pause()?;
        self.shared.running.store(false, Ordering::Release);
        crate::trace!(
            "pause: position {} frozen, {} underruns so far",
            self.shared.position.load(Ordering::Relaxed),
            self.shared.underruns.load(Ordering::Relaxed)
        );
        Ok(())
    }

    /// Continues from a pause without touching the position: the client's next
    /// block follows the last one it rendered.
    pub fn resume(&self) -> Result<(), Error> {
        let mut ctl = self.ctl.lock().unwrap();
        let Some(backend) = ctl.backend.as_mut() else {
            return Err(Error::InvalidState);
        };
        backend.resume()?;
        self.shared.running.store(true, Ordering::Release);
        crate::trace!(
            "resume: continuing at position {}",
            self.shared.position.load(Ordering::Relaxed)
        );
        Ok(())
    }

    /// Stops the device. The position stays readable, and the next
    /// [`Engine::start`] begins a new timeline from zero.
    pub fn stop(&self) -> Result<(), Error> {
        let mut ctl = self.ctl.lock().unwrap();
        let Some(backend) = ctl.backend.as_mut() else {
            return Err(Error::InvalidState);
        };
        backend.stop()?;
        self.shared.running.store(false, Ordering::Release);
        crate::trace!(
            "stop: {} callbacks served",
            self.shared.callbacks.load(Ordering::Relaxed)
        );
        Ok(())
    }

    /// Whether the device is currently being consumed.
    pub fn is_running(&self) -> bool {
        self.shared.running.load(Ordering::Acquire)
    }

    /// Whether audio is reaching a device right now.
    ///
    /// `is_running` says the engine has been started; this says the device is
    /// actually taking audio. They differ while a backend rebuilds its device after
    /// Android moves output — headphones unplugged, a Bluetooth headset leaving — and
    /// a caller that must keep its own clock, as an ALSA PCM does, uses this to decide
    /// between handing audio over and dropping it.
    pub fn is_delivering(&self) -> bool {
        // Deliberately *not* gated on `running`. Before `start` nothing is playing yet,
        // but the device is there and a caller must keep handing it audio — a backend
        // that is not "running" because the app has not called `start` yet is a
        // different state from one that is rebuilding its device, and treating them
        // alike deadlocks the start: the app's writes are dropped, so the stream never
        // fills, so it never starts, so everything stays dropped.
        self.ctl
            .lock()
            .unwrap()
            .backend
            .as_ref()
            .map(|backend| backend.is_delivering())
            .unwrap_or(false)
    }

    /// Block-aligned position of the block the device is playing, and the
    /// monotonic time it was handed out.
    pub fn sample_position(&self) -> (u64, u64) {
        (
            self.shared.position.load(Ordering::Relaxed),
            self.shared.time_ns.load(Ordering::Relaxed),
        )
    }

    /// Frames between a sample being handed to the device and it being audible:
    /// what the device still holds, plus its own turning, plus the block being
    /// filled. Recomputed from the device's live figures rather than promised once
    /// at open.
    pub fn latency_frames(&self) -> u32 {
        let mut ctl = self.ctl.lock().unwrap();
        let Some(granted) = ctl.granted else {
            return 0;
        };
        let queued = ctl
            .backend
            .as_mut()
            .map(|backend| backend.queued_frames())
            .unwrap_or(0);
        granted
            .output_latency_frames
            .saturating_add(queued)
            .saturating_add(granted.block_frames.max(granted.frames_per_burst))
    }

    /// Frames the device still holds that have not been played. Part of the honest
    /// latency figure, and what a caller draining a queue waits on.
    pub fn queued_frames(&self) -> u32 {
        let mut ctl = self.ctl.lock().unwrap();
        ctl.backend
            .as_mut()
            .map(|backend| backend.queued_frames())
            .unwrap_or(0)
    }

    /// The device's own timestamp, when it can produce one. Worth preferring over
    /// [`Engine::sample_position`] for drift-free sync: it is the device's view of
    /// the hardware clock rather than the callback cadence.
    pub fn timestamp(&self) -> Option<Timestamp> {
        self.ctl.lock().unwrap().backend.as_mut()?.timestamp()
    }

    /// Frames the device has presented since the engine opened, or `None` when it
    /// cannot say. Monotone across a rebuild: the frames an old device presented are
    /// carried into the new one's count rather than starting again at zero.
    pub fn played_frames(&self) -> Option<u64> {
        self.ctl.lock().unwrap().backend.as_ref()?.played_frames()
    }

    pub fn stats(&self) -> Stats {
        let mut ctl = self.ctl.lock().unwrap();
        let device_xruns = ctl
            .backend
            .as_mut()
            .map(|backend| backend.device_xruns())
            .unwrap_or(0);
        Stats {
            callbacks: self.shared.callbacks.load(Ordering::Relaxed),
            frames: self.shared.frames.load(Ordering::Relaxed),
            device_xruns,
            underruns: self.shared.underruns.load(Ordering::Relaxed),
            reopens: self.shared.reopens.load(Ordering::Relaxed),
        }
    }

    /// Drains pending [`Events`]. Safe from any thread, and the only way a client
    /// learns that a device went away underneath it.
    pub fn take_events(&self) -> Events {
        let mut bits = self.shared.events.swap(0, Ordering::AcqRel);
        // A device knows about its own disappearance before anyone polls for it.
        if let Some(backend) = self.ctl.lock().unwrap().backend.as_mut() {
            bits |= backend.take_events().bits();
        }
        if bits & Events::DEVICE_LOST.bits() != 0 {
            // A device that is gone cannot be started again; say so rather than
            // letting a later `start` fail obscurely.
            self.shared.running.store(false, Ordering::Release);
        }
        Events::from_bits(bits)
    }

    /// Reopens the device after it disappeared, keeping the client and the
    /// position. `RESYNC` is raised, because the clock is not continuous across a
    /// reopen even when the position is.
    ///
    /// The old stream is closed before the new one is built, and closing it is what
    /// stops its thread calling into the client — that ordering is what makes
    /// handing the same client to a new device sound.
    pub fn reopen(&self) -> Result<Granted, Error> {
        let mut ctl = self.ctl.lock().unwrap();
        let config = ctl.config;
        if let Some(mut backend) = ctl.backend.take() {
            backend.close();
            self.shared.backend_closed();
        }
        // The new device's `start` picks the timeline up from `resume_position`,
        // which this deliberately leaves alone.
        let position = self.shared.resume_position.load(Ordering::Relaxed);

        let mut backend = (ctl.factory)()?;
        let worker = EngineWorker::new(self.client.clone(), self.shared.clone());
        let granted = backend.open(&config, Box::new(worker))?;
        if ctl.granted.is_some() {
            backend.start()?;
            self.shared.running.store(true, Ordering::Release);
        }
        log_open(&granted, &config);

        ctl.granted = Some(granted);
        ctl.backend = Some(backend);
        self.shared.reopens.fetch_add(1, Ordering::Relaxed);
        self.shared.add_event(Events::RESYNC);
        crate::trace!("reopen: position kept at {position}");
        Ok(granted)
    }

    /// Releases the device and the client.
    ///
    /// The device is closed *before* the client is dropped: `Backend::close` is
    /// what guarantees no real-time thread will touch the client again.
    pub fn close(self) {
        let mut ctl = self.ctl.lock().unwrap();
        if let Some(mut backend) = ctl.backend.take() {
            backend.close();
        }
        self.shared.running.store(false, Ordering::Release);
        crate::trace!(
            "close: {} callbacks served",
            self.shared.callbacks.load(Ordering::Relaxed)
        );
    }
}

impl Shared {
    /// Called by a backend when it has stopped calling the sink, so a dropped tail
    /// is not reported as an underrun.
    fn backend_closed(&self) {
        self.running.store(false, Ordering::Release);
    }
}

fn log_open(granted: &Granted, config: &StreamConfig) {
    crate::trace!(
        "open: device {} at {} Hz, {} device ch / {} client ch, burst {}, device buffer {}, block \
         {} frames, sharing {:?}, performance {:?}, format {:?}",
        granted.device_id,
        granted.sample_rate,
        granted.device_channels,
        granted.client_channels,
        granted.frames_per_burst,
        granted.device_buffer_frames,
        granted.block_frames,
        granted.sharing,
        granted.performance,
        granted.format,
    );
    if granted.sharing != config.sharing {
        crate::trace!(
            "sharing {:?} was asked for and {:?} was granted: on Android that is the device's \
             answer, not a failure of the request",
            config.sharing,
            granted.sharing
        );
    }
    if granted.client_sample_rate != granted.sample_rate {
        // Said plainly, because the alternative reading of these two numbers is the one
        // that cost a night: the client's rate is not the device's, and every sample is
        // converted between them rather than played at the wrong speed.
        crate::trace!(
            "the client writes {} Hz and the device plays {} Hz: converting",
            granted.client_sample_rate,
            granted.sample_rate
        );
    }
}

/// The real-time side: the device's thread lands here, and this is what turns the
/// device's periods into the client's blocks.
struct EngineWorker {
    client: Arc<ClientSlot>,
    shared: Arc<Shared>,
    /// Channel-major block: `block_frames` frames per channel, `client_channels`
    /// planes.
    staging: Vec<f32>,
    /// A block's worth of the client's own frames, at the client's rate, before
    /// conversion. Equal in size to `staging` when the rates agree.
    input: Vec<f32>,
    block_frames: u32,
    client_channels: u16,
    device_channels: u16,
    /// The rate the device plays at, which is what its timeline is counted in.
    sample_rate: i32,
    /// The rate the client writes at. Equal to `sample_rate` unless the route answered
    /// with something else, in which case this block is converted on the way out.
    client_rate: i32,
    resampler: crate::resample::Resampler,
    /// The device has fewer channels than the client, so the client's planes are folded
    /// down into it. Set from the device's answer; a route that matches never takes this
    /// path.
    downmix: bool,
    /// Frames of the current block already handed to the device.
    staged_from: u32,
    /// Frames of the current block still to hand over.
    staged_left: u32,
    /// How long the device can play without being fed again, in nanoseconds — which is
    /// the deadline a client's block has to be produced within.
    ///
    /// The device buffer's duration, not one burst. A burst is how often the device wakes,
    /// not how much slack there is: producing a 480-frame block in 5.2 ms against a 4 ms
    /// buffer is a real dropout, but the same 5.2 ms against a 20 ms buffer is not, and
    /// measuring against a burst reported every block as a dropout on a route whose audio
    /// was arriving perfectly. Bursts also vary by route — the same device reports 96
    /// frames on its speaker and on Bluetooth, while the buffers wanted differ by an order
    /// of magnitude.
    period_ns: u64,
    /// Last time an underrun was reported, so a struggling stream produces one
    /// event per second rather than thousands.
    last_underrun_ns: u64,
    /// Block-aligned position of the next block to be handed out.
    next_position: u64,
    /// Frames of the current block the client actually filled. Anything past this in
    /// the block is silence this engine inserted, not audio the client produced.
    supplied: u32,
}

impl EngineWorker {
    fn new(client: Arc<ClientSlot>, shared: Arc<Shared>) -> Self {
        Self {
            client,
            shared,
            staging: Vec::new(),
            input: Vec::new(),
            block_frames: 0,
            client_channels: 0,
            device_channels: 0,
            sample_rate: 48_000,
            client_rate: 48_000,
            resampler: crate::resample::Resampler::new(48_000, 48_000, 1),
            downmix: false,
            staged_from: 0,
            staged_left: 0,
            period_ns: 0,
            last_underrun_ns: 0,
            next_position: 0,
            supplied: 0,
        }
    }

    /// Sizes the block to the device's answer and allocates the staging buffer
    /// once. Called before the device runs; never on the real-time path.
    fn configure_worker(&mut self, granted: &Granted) {
        // The client's own channel count, not the smaller of the two: the staging buffer
        // holds what the client produced, and the downmix to a mono device happens on the
        // way out of it.
        self.client_channels = granted.client_channels.max(1);
        self.device_channels = granted.device_channels.max(1);
        self.sample_rate = granted.sample_rate.max(1);
        self.client_rate = granted.client_sample_rate.max(1);
        self.block_frames = granted.block_frames.max(1);
        self.resampler = crate::resample::Resampler::new(
            self.client_rate,
            self.sample_rate,
            self.client_channels,
        );
        self.downmix = self.device_channels < self.client_channels;
        if self.client_rate != self.sample_rate {
            // Not a failure and not a repitch: the route answered with its own rate and
            // this is where the difference is absorbed.
            crate::trace!(
                "engine: converting {} Hz client audio to a {} Hz device",
                self.client_rate,
                self.sample_rate
            );
        }
        let frames = self.block_frames as usize * self.client_channels as usize;
        if self.staging.len() != frames {
            self.staging.clear();
            self.staging.resize(frames, 0.0);
        }
        let wanted =
            self.resampler.input_frames(self.block_frames) as usize * self.client_channels as usize;
        if self.input.len() != wanted {
            self.input.clear();
            self.input.resize(wanted, 0.0);
        }
        self.staged_from = 0;
        self.staged_left = 0;
        self.supplied = 0;
        // At least one block: a buffer smaller than the block being produced cannot cover
        // it, and that is a genuine shortage rather than a rounding of the measurement.
        let slack = granted
            .device_buffer_frames
            .max(granted.block_frames)
            .max(granted.frames_per_burst);
        self.period_ns = frames_for_ns(slack, granted.sample_rate);
    }

    /// Starts a timeline: the next block the client renders follows the last one it
    /// rendered before the device stopped, or is its first after a fresh start.
    fn reset_timeline(&mut self) {
        self.staged_from = 0;
        self.staged_left = 0;
        self.supplied = 0;
        self.next_position = self.shared.resume_position.load(Ordering::Relaxed);
    }

    /// Hands the device `frames` frames of interleaved audio, asking the client for
    /// a block whenever the staging buffer runs dry.
    fn fill_period(&mut self, interleaved: &mut [f32], frames: u32) -> u32 {
        let device_channels = self.device_channels as usize;
        let stride = self.block_frames as usize;
        let total = frames as usize;
        let mut written = 0usize;
        let mut supplied = 0u32;

        while written < total {
            if self.staged_left == 0 {
                self.render_block();
            }
            let take = (total - written).min(self.staged_left as usize);
            for frame in 0..take {
                // Only the head of a block came from the client; the tail is silence
                // this engine inserted to make a whole block, and counting it would
                // move the position forward by audio nobody produced.
                if (self.staged_from + frame as u32) < self.supplied {
                    supplied += 1;
                }
                for channel in 0..device_channels {
                    let sample = if self.downmix {
                        // More client planes than the device has: each missing output
                        // folds into the one below it, so quad becomes stereo and
                        // anything becomes mono. Taking plane 0 alone would silently drop
                        // half the mix, which is exactly the kind of quiet wrongness a
                        // route should not be able to introduce.
                        let mut sum = 0.0;
                        let mut count = 0.0;
                        let mut plane = channel;
                        while plane < self.client_channels as usize {
                            sum += self.staging[plane * stride + self.staged_from as usize + frame];
                            count += 1.0;
                            plane += device_channels;
                        }
                        sum / count
                    } else if (channel as u16) < self.client_channels {
                        self.staging[channel * stride + self.staged_from as usize + frame]
                    } else {
                        // A device with more channels than the client drives gets
                        // silence on the rest, rather than whatever was there.
                        0.0
                    };
                    interleaved[(written + frame) * device_channels + channel] = sample;
                }
            }
            written += take;
            self.staged_from += take as u32;
            self.staged_left -= take as u32;
        }
        supplied
    }

    /// Asks the client for one whole block, on the device's thread, and converts it to
    /// the device's rate.
    fn render_block(&mut self) {
        let frames = self.block_frames;
        // How much of the client's audio a device period consumes. This is the client's
        // number, so it is what its position advances by and what the ring above is
        // credited for — using the device's block here would make the position a count of
        // the wrong kind of frame.
        let needed = self.resampler.input_frames(frames);
        self.input.fill(0.0);
        let started = crate::clock::now_ns();
        let ctx = RenderContext {
            position: self.next_position,
            time_ns: crate::clock::now_ns(),
            // The client is asked for its own frames at its own rate: what it hands back
            // is audio, not a count to be reinterpreted downstream.
            frames: needed,
            channels: self.client_channels,
            sample_rate: self.client_rate,
        };
        self.shared
            .position
            .store(self.next_position, Ordering::Relaxed);
        self.shared.time_ns.store(ctx.time_ns, Ordering::Relaxed);
        // SAFETY: this is the device's real-time thread, and no other device is
        // running against the slot — a device is closed before another is opened.
        let pulled = unsafe { self.client.render(&ctx, &mut self.input) }.min(needed);
        // The part of the converted block that carries audio rather than the silence a
        // short pull left behind.
        self.supplied = self.resampler.real_output_frames(pulled, frames);
        self.resampler
            .process(&self.input, frames, &mut self.staging);
        let elapsed = crate::clock::now_ns().saturating_sub(started);
        // A block that took longer than a device period to produce cannot have
        // reached the device in time: that is a dropout, and the only way a client
        // finds out without the device's xrun counter.
        if self.period_ns > 0 && elapsed > self.period_ns {
            self.shared.underruns.fetch_add(1, Ordering::Relaxed);
            let now = crate::clock::now_ns();
            if now.saturating_sub(self.last_underrun_ns) >= 1_000_000_000 {
                self.last_underrun_ns = now;
                self.shared.add_event(Events::UNDERRUN);
                crate::trace!(
                    "underrun: a {frames} frame block took {elapsed} ns to produce, against \
                     {period} ns of device buffer",
                    period = self.period_ns
                );
            }
        }
        self.next_position = self.next_position.wrapping_add(needed as u64);
        self.shared
            .resume_position
            .store(self.next_position, Ordering::Relaxed);
        self.staged_from = 0;
        self.staged_left = frames;
        self.shared.callbacks.fetch_add(1, Ordering::Relaxed);
    }
}

impl Sink for EngineWorker {
    fn configure(&mut self, granted: &Granted) {
        self.configure_worker(granted);
    }

    fn reset(&mut self) {
        self.reset_timeline();
    }

    fn fill(&mut self, interleaved: &mut [f32], frames: u32) {
        if self.device_channels == 0 || frames == 0 {
            return;
        }
        let supplied = self.fill_period(interleaved, frames);
        self.shared
            .frames
            .fetch_add(supplied as u64, Ordering::Relaxed);
    }
}

/// How long `frames` frames last at `sample_rate`, in nanoseconds.
fn frames_for_ns(frames: u32, sample_rate: i32) -> u64 {
    if sample_rate <= 0 {
        return 0;
    }
    (frames as u64 * 1_000_000_000) / sample_rate as u64
}

/// The block size a device's answer implies: the client's own period, floored at one burst.
///
/// The floor is required — a device cannot be woken more often than its burst, so a client
/// asking for less than one gets one. The ceiling is the clamp against a nonsensical request.
///
/// What is deliberately *not* here is rounding the period up to a whole number of bursts.
/// That looks tidy and is wrong, because the device's burst and the client's period are two
/// unrelated numbers, and rounding makes the engine ask the client for more audio than the
/// client produces at a time. Measured on two devices with the same 480-frame period: a
/// 96-frame burst divides it evenly (480 stays 480, and everything works), while a 192-frame
/// burst rounds it to 576 — so every block asked for 576 frames from a supplier that hands
/// over 480, and the difference became silence spliced into the stream. The reference sink
/// has no such step: `pa_sink_render_into_full` renders exactly the `numFrames` the device
/// asked for, and PulseAudio's graph supplies them in full. A block is already allowed to
/// span device periods here — that is what `staged_from`/`staged_left` are for — so nothing
/// needs the block to be a whole number of bursts.
pub(crate) fn block_frames_for(requested: u32, burst: u32) -> u32 {
    let burst = burst.max(1);
    requested.clamp(1, burst * MAX_BLOCK_BURSTS).max(burst)
}

/// Whether a device's reported period is believable.
pub(crate) fn plausible_burst(burst: i32) -> Option<u32> {
    if burst >= 1 && (burst as u32) <= MAX_BURST_FRAMES {
        Some(burst as u32)
    } else {
        None
    }
}
