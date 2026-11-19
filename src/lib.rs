//! A native Android audio engine for direct, low-latency output, on AAudio.
//!
//! It exists to give an app a device it drives directly through its own callback,
//! at a block size *it* chooses, with a clock it can trust and a latency figure
//! that is measured rather than claimed. On Android that means AAudio — including
//! the exclusive path where the client's buffer is shared with the driver and the
//! AudioFlinger mixer is out of the way — and it means being honest about the
//! cases where the device says no.
//!
//! ```no_run
//! use gnaudio::{Client, Engine, RenderContext, StreamConfig};
//!
//! struct Tone {
//!     phase: f32,
//! }
//!
//! impl Client for Tone {
//!     fn render(&mut self, ctx: &RenderContext, out: &mut [f32]) -> u32 {
//!         let frames = ctx.frames as usize;
//!         let (left, right) = out.split_at_mut(frames);
//!         let step = 440.0 / ctx.sample_rate as f32;
//!         for frame in 0..frames {
//!             let sample = (self.phase * core::f32::consts::TAU).sin() * 0.2;
//!             self.phase = (self.phase + step) % 1.0;
//!             left[frame] = sample;
//!             right[frame] = sample;
//!         }
//!         ctx.frames
//!     }
//! }
//!
//! let engine = Engine::open(
//!     Box::new(Tone { phase: 0.0 }),
//!     StreamConfig { block_frames: 256, ..StreamConfig::default() },
//! )?;
//! engine.start()?;
//! # Ok::<(), gnaudio::Error>(())
//! ```
//!
//! # The shape of a client
//!
//! * [`Engine::open`] asks the device for a configuration and reports what it
//!   *granted* — including whether exclusive sharing survived.
//! * [`Engine::start`] begins a new timeline at position zero;
//!   [`Engine::pause`] and [`Engine::resume`] stop and continue without reopening
//!   the device; [`Engine::stop`] is a stop with the position left readable.
//! * [`Client::render`] is called once per block, on a thread the backend owns — the
//!   device's own when it drives us with a callback, the backend's writer thread when
//!   it lets the device pull through its data callback, and produces each period on a
//!   thread of its own.
//! * [`Engine::take_events`] is how a client learns that the device went away;
//!   [`Engine::reopen`] is how it comes back.
//!
//! # What this is not
//!
//! * **Not a Wine or guest-side driver.** It is an Android library, and it has no
//!   Windows-side artifact at all. A Windows host reaches it through `alsa-plugin/`,
//!   which is an ordinary ALSA PCM that libasound loads in-process.
//! * **Not a promise of desktop-class latency.** Android's floor is the HAL's, and a
//!   shared-mode stream also pays for the AudioFlinger mixer. Exclusive sharing is
//!   the one lever that removes the mixer; whether a device grants it is the
//!   device's answer, and [`Engine::granted`] reports it rather than quietly
//!   pretending.
//! * **Not output-only by accident.** Input is simply not implemented yet: an
//!   engine that plays is the whole of what was asked for first.

pub mod backend;
mod backend_factory;
pub mod c_api;
mod clock;
#[cfg_attr(not(target_os = "android"), allow(dead_code))]
mod reconnect;
mod resample;
mod supply;
// The only host-side consumer is the test suite; the backend that uses it is
// Android-only, so a host build would otherwise warn that it is dead.
#[cfg_attr(not(target_os = "android"), allow(dead_code))]
mod engine;
mod trace;
pub mod types;

pub use engine::{Client, Engine};
pub use trace::{logging, set_logging};
pub use types::{
    Error, Events, Format, Granted, Performance, RenderContext, Sharing, Stats, StreamConfig,
    Timestamp,
};

/// A backend the caller supplies, for tests and for embedding a different device
/// stack.
pub use backend::{Backend, Sink};

/// A factory the engine uses whenever it needs a device: at open, and again at
/// every reopen. Having one is what makes a device swap testable, and what lets a
/// caller substitute a device stack of its own.
pub type BackendFactory =
    std::sync::Arc<dyn Fn() -> Result<Box<dyn Backend>, types::Error> + Send + Sync>;

/// The device stack this build offers.
pub fn default_factory() -> BackendFactory {
    std::sync::Arc::new(backend_factory::open_backend)
}
