//! The PCM object, and the C surface the ALSA shim calls into.
//!
//! What this is: libasound's ioplug contract, implemented over the engine. The app
//! writes PCM with `snd_pcm_writei`; libasound asks us (`transfer`) to take it; we
//! convert it to float, push it into a ring, and return. The engine's writer thread
//! pulls periods out of that ring and hands them to AAudio through its data callback,
//! which is the mechanism measured to be audible on this device.
//!
//! Why every function here is `unsafe extern "C"` and takes a raw pointer: these are
//! called by a C library we do not control, from threads we do not own, with pointer
//! arguments we did not create. Every entry point therefore checks its pointer first
//! and answers with a negative errno on anything it cannot do — returns an error,
//! never `abort`, because an ALSA plugin that aborts takes the game with it.
//!
//! The one structural decision worth naming: **the app's thread must never wait on
//! the device**. `transfer` converts, copies and returns; it hands nothing to AAudio
//! and waits for nothing. That keeps a `writei` costing what it costs against a real
//! sound card, and it is why the ring exists at all.

use core::ffi::{c_int, c_void};
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use gnaudio::{Client, Engine, Performance, RenderContext, Sharing, StreamConfig};

use crate::log::trace;
use crate::ring::Ring;

/// `EINVAL`, `EIO`, `ENODEV`, `EBUSY`: what an ALSA callback returns when it cannot
/// do what it was asked.
const EINVAL: c_int = -22;
const EIO: c_int = -5;
const ENODEV: c_int = -19;
const EBUSY: c_int = -16;

/// `snd_pcm_format_t` values this plugin carries, from `pcm.h`'s own numbering:
/// `S8 = 0`, `U8 = 1`, `S16_LE = 2`, … `S32_LE = 10`, … `FLOAT_LE = 14`.
const FORMAT_S16_LE: c_int = 2;
const FORMAT_S32_LE: c_int = 10;
const FORMAT_FLOAT_LE: c_int = 14;

/// How long `drain` waits for the device to play out what it holds before giving up on
/// it. A drain that never returns would hang whatever called it.
const DRAIN_TIMEOUT_MS: u64 = 750;

/// How long a deferred start waits for its buffer to fill before beginning on whatever
/// it has. Short enough that a one-shot sound is still heard, long enough that a stream
/// writing continuously fills the buffer first.
const PREFILL_TIMEOUT_NS: u64 = 50_000_000;

/// How long after the app's last write an empty ring still counts as starvation. Four
/// device periods: long enough that a late writer is still judged, short enough that a
/// stream being closed is not blamed for draining.
const STARVED_AFTER_WRITE_NS: u64 = 80_000_000;

/// A monotonic clock in nanoseconds, for the prefill deadline.
fn now_ns() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0)
}

/// How long a write may wait for the ring to make room before it returns a short count.
///
/// Short on purpose. This call holds libasound's PCM mutex, and winealsa runs a timer
/// thread that needs the same mutex to read the pointer — so a long wait here stalls
/// Wine's own clock, not just the writer. Returning short hands the delay back to the
/// app, which retries on its own schedule with the lock released, which is what a real
/// sound card's `writei` does.
const WRITE_TIMEOUT_MS: u64 = 50;
/// Frames the app may have outstanding: libasound's `avail` at its ceiling.
///
/// `avail = buffer_size - (appl_ptr - hw_ptr)`, so it is simultaneously the credit the
/// app writes against and the most the ring can ever hold. Two blocks is the smallest
/// value that lets the engine pull a whole block while another one is in flight; the
/// buffer size caps it because libasound's own arithmetic is modulo that.
///
/// This was a tuning knob once, and that is what broke the pipeline twice: at 240 the
/// ring was 240 frames deep while a block is 480, so every block was short by
/// construction, and at 1 920 the reported position ran past the app's write pointer.
/// The right value follows from the block size, so it is computed rather than chosen.
fn credit_frames(period: u32, buffer: u32) -> u32 {
    let period = period.max(1);
    period.saturating_mul(2).min(buffer.max(period)).max(period)
}

/// How long a device buffer lasts, in nanoseconds: the wait a period's render is allowed.
fn device_buffer_ns(frames: u32, rate: i32) -> u64 {
    if rate <= 0 || frames == 0 {
        return 0;
    }
    (u64::from(frames) * 1_000_000_000) / rate as u64
}

/// Frames the ring holds: the whole credit, so no write the app is allowed to make can
/// overflow it, and never more than the buffer libasound was given.
fn ring_frames_for(period: u32, buffer: u32) -> u32 {
    credit_frames(period, buffer).clamp(period.max(1), buffer.max(period))
}


/// How the device stream is shared with the rest of the system.
///
/// Shared, which is what the PulseAudio AAudio sink this was measured against uses —
/// its builder sets `AAUDIO_SHARING_MODE_SHARED` unconditionally, and that path is the
/// one that has been played through by more people than this one has. Exclusive was
/// tried first and is still reachable with `GNAUDIO_SHARING=exclusive`, but on this
/// hardware it grants `MMAP_PLAYBACK`: no mixer thread between this writer and the DMA
/// buffer, which is a shorter path and a far less forgiving one. A stream that drops
/// audio down there has no mixer to hide it and no counter to report it.
fn low_latency() -> bool {
    static MODE: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *MODE.get_or_init(|| {
        matches!(
            std::env::var("GNAUDIO_LOW_LATENCY").as_deref(),
            Ok("1") | Ok("true") | Ok("on") | Ok("yes")
        )
    })
}

fn sharing_mode() -> Sharing {
    static MODE: std::sync::OnceLock<Sharing> = std::sync::OnceLock::new();
    *MODE.get_or_init(|| match std::env::var("GNAUDIO_SHARING").as_deref() {
        Ok("exclusive") => Sharing::Exclusive,
        _ => Sharing::Shared,
    })
}

/// Whether to report what the device has played rather than what it was handed.
///
/// Off by default: `GNAUDIO_POSITION_PLAYED=1` asks for it.
///
/// The difference is the whole device buffer, and it decides how much room winealsa
/// believes it has. libasound's `avail` is `buffer_size - (appl_ptr - hw_ptr)`, and a
/// position that lags playback by the device buffer subtracts that buffer from every
/// answer the app gets about its own space. With a 1920 frame buffer and a 3744 frame
/// device buffer, the subtraction leaves nothing, and winealsa — which pads its lead-in
/// with silence whenever it cannot move the app's audio into ALSA — fills the gap with
/// digital silence. That is a click, and it is invisible to every counter here.
///
/// Reporting what was accepted keeps that subtraction at the size of this plugin's own
/// ring, a single period. The cost is honest but real: `snd_pcm_delay` then understates
/// latency by the device buffer, because the device buffer is ours and not libasound's.
fn played_position() -> bool {
    static WHICH: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *WHICH.get_or_init(|| {
        // Off by default, and it took a measurement to learn why. The device's presented
        // count is honest about *latency* but it counts every frame the device played,
        // including the silence this engine inserts when the ring cannot fill a whole
        // block. Those frames were never written by the app, so a position derived from
        // them runs past the app's own write pointer — and libasound's `avail` then reads
        // back larger than the buffer it was given, which no sound card can produce.
        // Measured with `WINEDEBUG=+alsa`: half of all samples were out of range, by up to
        // 4 800 frames (100 ms).
        //
        // The engine's own delivered count counts only frames a client supplied, so it
        // cannot exceed what the app wrote. `GNAUDIO_POSITION_PLAYED=1` asks for the
        // device's figure anyway, for anyone who needs playback rather than supply.
        std::env::var("GNAUDIO_POSITION_PLAYED")
            .map(|value| value != "0")
            .unwrap_or(false)
    })
}

/// How long the reported position may stand still, while a device is taking audio,
/// before that counts as a stall, as a floor: the real threshold is derived from the
/// device buffer, because standing still for less than one buffer cannot be a stall.
const STALL_AFTER_NS: u64 = 20_000_000;

/// The stall threshold for a device: twice its buffer's duration, never below the floor.
///
/// Twice, not once, because the question being asked is not "is the buffer empty" but
/// "has the device stopped", and one buffer's worth of stillness is exactly what a full
/// buffer looks like at the moment before it starts draining.
fn stall_threshold_ns(device_buffer_frames: u32, sample_rate: i32) -> u64 {
    if sample_rate <= 0 {
        return STALL_AFTER_NS;
    }
    let buffer_ns =
        (device_buffer_frames as u64).saturating_mul(2_000_000_000) / sample_rate as u64;
    buffer_ns.max(STALL_AFTER_NS)
}

/// The engine, seen from the ring: pull a block, leave the rest silent.
///
/// The engine zero-fills its staging block before calling this, so a short pull
/// leaves silence rather than the previous block's audio. Nothing here blocks or
/// allocates: this is the only thing between the app's writes and the speaker.
struct Source {
    ring: Arc<Ring>,
    /// When the app last handed us audio, so an empty ring can be told from a starved
    /// one: silence after the app stopped writing is the app finishing, not a dropout.
    last_write_ns: Arc<AtomicU64>,
    /// How long one period's render may wait for the ring, in nanoseconds.
    ///
    /// The device buffer's duration. That is how long the hardware can play without new
    /// audio, so a shorter wait strands frames the device had room for and a longer one can
    /// only deliver silence late. Half an app period is neither: it is a number about the
    /// supplier rather than the consumer.
    wait_ns: Arc<AtomicU64>,
    /// Whether to count what this pull found. Always.
    ///
    /// These two atomics are the only things that can say the engine spliced silence into
    /// the device stream — a click that leaves no trace in what the app handed over, no
    /// xrun on the device, and no gap in the capture. Gating them behind an environment
    /// variable meant every session that mattered reported "the ring was never measured",
    /// and a counter that is off cannot distinguish a clean stream from a starving one.
    /// Measured cost of leaving them on: two relaxed atomics per block.
    diagnostics: bool,
}

/// How long the device thread may wait for the app to finish writing a block it has
/// already begun.
///
/// The engine asks for one block at a time, and a block is the app's own period — so the
/// two are always racing: the engine's demand and the app's supply land microseconds
/// apart, in whichever order the scheduler chose. Losing that race means a whole block of
/// silence spliced into the stream, which is a click, and it happens every few hundred
/// milliseconds for no reason a listener can act on. The device is holding
/// tens of milliseconds of audio by then, so waiting a couple of milliseconds for the rest
/// of a block costs nothing and turns the race into a non-event.
/// How long to sleep between attempts while waiting, chosen to be far below one period.
const RENDER_POLL_NS: u64 = 200_000;

impl Client for Source {
    fn render(&mut self, ctx: &RenderContext, out: &mut [f32]) -> u32 {
        let wanted = ctx.frames as usize;
        let channels = ctx.channels as usize;
        let mut pulled = self.ring.pull_planar(out, wanted, channels);
        if pulled < wanted {
            // `pa_sink_render_into_full`: render the *whole* period rather than handing the
            // device half of one. The wait is bounded by the device's own period, though,
            // and that bound is load-bearing rather than tidy: an unbounded wait that ends
            // only when the app stops writing costs 78.6 ms on every starved block, because
            // the app's write credit is this plugin's own position and a position that does
            // not advance withholds the credit that would refill the ring. The two deadlock
            // and the device plays a third of realtime.
            let deadline = now_ns().saturating_add(self.wait_ns.load(Ordering::Acquire));
            while pulled < wanted {
                if now_ns() >= deadline {
                    break;
                }
                let since_write =
                    now_ns().saturating_sub(self.last_write_ns.load(Ordering::Acquire));
                if since_write >= STARVED_AFTER_WRITE_NS {
                    break;
                }
                std::thread::sleep(std::time::Duration::from_nanos(RENDER_POLL_NS));
                pulled += self.ring.pull_planar_at(out, wanted, channels, pulled);
            }
        }
        if self.diagnostics {
            // Only a ring that ran dry *while the app was still writing* is a dropout.
            // After the last write the ring drains legitimately, and counting that made
            // every closing stream report hundreds of starved frames for its own tail.
            let since_write = now_ns().saturating_sub(self.last_write_ns.load(Ordering::Acquire));
            if pulled < wanted && since_write < STARVED_AFTER_WRITE_NS {
                // Nothing may be logged from here — this is the device's audio thread —
                // so the fact is counted and reported from elsewhere.
                crate::ring::note_starvation((wanted - pulled) as u64);
            }
            crate::ring::note_pull_level(self.ring.available_frames());
        }
        // What the engine may count as consumed.
        //
        // While the app is still writing, that is the whole period: the device played it,
        // whatever was in it, and a sound card's play pointer advances over silence it was
        // handed. Reporting only the frames this pull happened to find deadlocks the two
        // against each other: every starved block would cost 78.6 ms and the device would
        // play a third of realtime, because the app's write credit *is* this position — no
        // advance, no `avail`, no writes, no audio, and round again.
        //
        // After the app's last write it is the pulled count, because a closing stream is
        // draining rather than starving and nothing should be claimed for its tail.
        //
        // The bound against inflation is elsewhere and is not this: `raw_position` caps the
        // result at the frames the app actually wrote, so the play pointer can never move
        // past the write pointer no matter what is claimed here.
        let since_write = now_ns().saturating_sub(self.last_write_ns.load(Ordering::Acquire));
        if since_write < STARVED_AFTER_WRITE_NS {
            wanted as u32
        } else {
            pulled as u32
        }
    }
}

/// One open PCM.
pub struct Pcm {
    /// Swapped at `hw_params`, because the app's buffer size is not known until
    /// then; behind a mutex rather than in an `Arc<Mutex<…>>` so that the writer
    /// thread's path (through [`Source`]) never touches it.
    ring: Mutex<Arc<Ring>>,
    /// `None` until `hw_params`, and again after `hw_free`.
    engine: Arc<Mutex<Option<Engine>>>,
    /// The app's sample format, as ALSA numbers it.
    format: AtomicI32,
    channels: AtomicU32,
    /// How long a period's render may wait for the ring: the device buffer's duration,
    /// published here because the device's buffer size is not known until it opens.
    wait_ns: Arc<AtomicU64>,
    /// Frames the engine had handed to the device when the current segment began.
    /// `position` is the difference, so a restart counts from zero again without the
    /// engine's own counter having to be reset.
    base_frames: AtomicU64,
    /// Frames that must be queued before the engine is started, so playback begins
    /// with a full buffer instead of one period.
    ///
    /// This is `start_threshold` by another name, and it is the difference between a
    /// clean stream and a stream that starves: libasound starts an ioplug on its first
    /// write, so an app that then writes one period per period holds a standing queue
    /// of exactly one period, with no margin at all. Measured on a device with a
    /// perfectly periodic 10 ms writer: one 10 ms hole every second, which is the
    /// crackle. Starting with the whole buffer queued gives the app a buffer's worth of
    /// slack, which is what a sound card gives it.
    prefill_frames: AtomicU32,
    /// When the app last handed us audio. A ring that empties because the app stopped
    /// writing is not starvation — it is the app finishing — and counting it as such
    /// made every closed stream report hundreds of "starved" frames for the tail it
    /// legitimately drained.
    last_write_ns: Arc<AtomicU64>,
    /// Whether the engine has been started and not yet stopped.
    ///
    /// Kept here rather than inferred from libasound calling [`gnaudio_alsa_start`],
    /// because libasound only calls that when `appl_ptr` reaches its start threshold —
    /// and with a ring of one period the app's writes cannot lift `appl_ptr` that far
    /// while nothing is consuming. Waiting for it is a deadlock: the buffer is full, the
    /// position never moves, and every write retries forever.
    running: AtomicBool,
    /// Set when `start` was called but the ring is not yet full enough to begin.
    awaiting_prefill: AtomicBool,
    /// When a deferred start gives up waiting and starts on whatever it has. An app that
    /// writes one short sound and stops would otherwise never be heard, and would block
    /// in `writei` once the ring filled — a wedge for the sake of a longer buffer.
    prefill_deadline_ns: AtomicU64,
    /// Whether a device was taking audio at the last write. The transition from false
    /// back to true is the moment a rebuild finished, and the moment whatever the
    /// ring still holds has gone stale.
    delivering: AtomicBool,
    /// Frames the app has handed over since the current segment began, played or not.
    ///
    /// This is the ceiling on any reported position. libasound's `avail` is
    /// `buffer_size - (appl_ptr - hw_ptr)`, so a position past what the app has written
    /// makes `avail` come back larger than the buffer — a thing no sound card can
    /// answer, and winealsa's lead-in branch reacts to it by splicing silence into the
    /// stream. Measured with the position allowed to catch up at twice real time: 55 % of
    /// all `avail` samples were above the buffer, by up to 31 632 frames.
    accepted_frames: AtomicU64,
    /// Frames accepted from the app while no device was taking audio — during a
    /// rebuild, when Android is moving output between devices.
    ///
    /// They are counted rather than handed to the engine, because the ring is not
    /// being drained and filling it would only delay the music once the device comes
    /// back. They still have to reach the read position, though: libasound decides
    /// whether the app may write from `appl_ptr - hw_ptr`, so a position that froze
    /// during the gap would make it believe the device had stopped consuming and
    /// throttle the app exactly when it should keep running.
    dropped_frames: AtomicU64,
    /// Float scratch for the non-float formats, sized at `hw_params`.
    scratch: Mutex<Vec<f32>>,
    /// The device's xrun count, mirrored out of the engine on the write path so the
    /// reporter can read it without taking the lock the audio path needs.
    xruns: Arc<AtomicU32>,
    /// The furthest the read position may advance in one report, as frames: half the
    /// app's buffer.
    ///
    /// libasound takes the difference between one reported position and the next modulo
    /// the buffer size, so a single step larger than the buffer is indistinguishable
    /// from a small backwards one — it silently under-counts, and the client is told it
    /// has room it does not have. A step this small cannot be mistaken for anything, and
    /// still catches up at twice real time after a stall.
    max_step: AtomicU32,
    /// The last position reported to libasound, which is what a step is measured from.
    last_reported: AtomicU64,
    /// Writes that could not take everything the app offered, and the frames left
    /// behind. A short write is back-pressure the app must wait out; if it happens while
    /// a device is taking audio it means the ring was full for a whole wait, and the
    /// seam that leaves is one of the few things that can crackle without any counter
    /// here moving.
    shortfalls: AtomicU64,
    short_frames: AtomicU64,
    /// A capture of what the app hands over, when `GNAUDIO_DUMP_PCM` asks for one.
    dump: crate::dump::Dump,
    /// Seams seen in the audio the app handed over: holes and splices that no drain-side
    /// counter can see, because nothing was dropped — the damage arrived already done.
    seams: Mutex<crate::gap::Seams>,
    gap_count: AtomicU32,
    gap_frames: AtomicU32,
    click_count: AtomicU32,
    /// The buffer size libasound negotiated, in frames, for the summary and for the
    /// bound on how far the reported position may lag.
    buffer_size: AtomicU32,
    /// Frames the reported position may sit behind what the device accepted, which is
    /// exactly `avail` as winealsa sees it, since `avail = buffer_size − (appl − hw)`.
    ///
    /// Zero means "not yet negotiated"; [`Pcm::position`] then falls back to half a
    /// period. At `hw_params` this becomes [`credit_frames`], which is the only value that
    /// both lets a whole block be pulled and stays inside the buffer libasound was given.
    avail_frames: AtomicU32,
    /// The period libasound negotiated, in frames.
    period_frames: AtomicU32,
    /// How far libasound's own `appl_ptr` has ever run ahead of the position it is told.
    ///
    /// This is the depth of everything between the app's write and the speaker: this
    /// plugin's ring plus the whole AAudio device buffer. libasound's arithmetic is
    /// modulo `buffer_size`, so a depth that exceeds it is not a small error — it is a
    /// number libasound cannot represent, and everything it derives from it (whether the
    /// app may write, how much room there is) becomes wrong.
    max_depth: AtomicU64,
    /// Times the depth exceeded the buffer size libasound was given.
    depth_overruns: AtomicU64,
    /// Smallest and largest write the app has made, in frames.
    ///
    /// The shape of the caller's writes: a whole period each time is one thing, an odd
    /// size that never repeats is another, and a size that is not the period at all says
    /// something between the app and here is rewriting the stream.
    transfer_min: AtomicU32,
    transfer_max: AtomicU32,
    /// When the position last moved: the device's own clock, as it reports it.
    last_advance_ns: AtomicU64,
    /// The last frame count the engine had accepted, which is what the stall check
    /// watches. Not the presented position: in shared mode that is the HAL's timestamp,
    /// published every few milliseconds at best and coarse enough to look stopped while
    /// audio is playing perfectly — measured, 96 apparent stalls in ten seconds of a
    /// clean tone. Whether the device is consuming is a question about what it accepts.
    last_delivered: AtomicU64,
    /// How long that stillness has to last before it counts.
    ///
    /// Twice the device buffer's duration, floored. A presentation timestamp is only as
    /// fine as the period the HAL publishes it for, so a tighter threshold reports the
    /// clock's own granularity as a fault: measured on a clean 8 s tone in shared mode,
    /// a fixed 20 ms turned a perfectly paced stream into 90 "stalls". A device buffer
    /// already covers its own duration, so nothing shorter can be a real stall.
    stall_after_ns: AtomicU64,
    /// Episodes where the position did not move at all while a device was taking audio.
    ///
    /// The app is told how much room it has from this position. A position that stands
    /// still for several periods says the device is not consuming, which is a lie when
    /// the listener can hear music — and it makes the app stop writing exactly when it
    /// should keep going.
    stalls: AtomicU64,
    in_stall: AtomicBool,
}

// SAFETY: every field is an atomic, a mutex, or an `Arc` of a type that is already
// `Send + Sync`. The struct is handed to C as an opaque pointer and every entry
// point below works through `&Pcm`, so no field is ever aliased mutably.
unsafe impl Send for Pcm {}
unsafe impl Sync for Pcm {}

impl Pcm {
    fn new() -> Self {
        Self {
            ring: Mutex::new(Arc::new(Ring::new(1, 2))),
            engine: Arc::new(Mutex::new(None)),
            format: AtomicI32::new(FORMAT_FLOAT_LE),
            channels: AtomicU32::new(2),
            base_frames: AtomicU64::new(0),
            prefill_frames: AtomicU32::new(0),
            last_write_ns: Arc::new(AtomicU64::new(0)),
            running: AtomicBool::new(false),
            awaiting_prefill: AtomicBool::new(false),
            prefill_deadline_ns: AtomicU64::new(0),
            delivering: AtomicBool::new(false),
            accepted_frames: AtomicU64::new(0),
            dropped_frames: AtomicU64::new(0),
            scratch: Mutex::new(Vec::new()),
            xruns: Arc::new(AtomicU32::new(0)),
            max_step: AtomicU32::new(0),
            last_reported: AtomicU64::new(0),
            shortfalls: AtomicU64::new(0),
            short_frames: AtomicU64::new(0),
            dump: crate::dump::Dump::new(),
            seams: Mutex::new(crate::gap::Seams::new()),
            gap_count: AtomicU32::new(0),
            gap_frames: AtomicU32::new(0),
            click_count: AtomicU32::new(0),
            buffer_size: AtomicU32::new(0),
            period_frames: AtomicU32::new(0),
            max_depth: AtomicU64::new(0),
            depth_overruns: AtomicU64::new(0),
            transfer_min: AtomicU32::new(0),
            transfer_max: AtomicU32::new(0),
            last_advance_ns: AtomicU64::new(0),
            wait_ns: Arc::new(AtomicU64::new(0)),
            last_delivered: AtomicU64::new(0),
            avail_frames: AtomicU32::new(0),
            stall_after_ns: AtomicU64::new(STALL_AFTER_NS),
            stalls: AtomicU64::new(0),
            in_stall: AtomicBool::new(false),
        }
    }

    fn ring(&self) -> Arc<Ring> {
        self.ring
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    fn engine(&self) -> std::sync::MutexGuard<'_, Option<Engine>> {
        self.engine
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// The frames that must be queued before the engine starts.
    ///
    /// Taken from the app's buffer size, bounded by what this ring can hold **with one
    /// period of headroom**. Both bounds matter and both were measured:
    ///
    /// * Without the ceiling the target is unreachable — the app's buffer is larger than
    ///   this ring — so the engine never starts and libasound waits on a position that
    ///   never moves.
    /// * Without the headroom the engine only starts on a completely full ring, so the
    ///   app's very next write blocks until the device has drained a period. That is the
    ///   no-margin start this whole mechanism exists to avoid, and it left every remaining
    ///   hole of the game's stream in the first 3.5 seconds of audio.
    fn prefill_target(&self) -> usize {
        let capacity = self.ring().capacity_frames();
        let headroom = (self.period_frames.load(Ordering::Acquire) as usize).max(1);
        (self.prefill_frames.load(Ordering::Acquire) as usize)
            .min(capacity.saturating_sub(headroom))
            .max(1)
    }

    /// Records how deep the pipeline ran, as libasound itself sees it.
    fn note_depth(&self, depth: u64, buffer_size: u64) {
        if buffer_size > 0 {
            self.buffer_size
                .store(buffer_size.min(u32::MAX as u64) as u32, Ordering::Relaxed);
        }
        let high = self.max_depth.load(Ordering::Relaxed);
        if depth > high {
            self.max_depth.store(depth, Ordering::Relaxed);
        }
        if buffer_size > 0 && depth > buffer_size {
            self.depth_overruns.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// The avail target to report against: what winealsa may move in one tick.
    ///
    /// `avail` is `buffer_size − (appl_ptr − hw_ptr)`, so it is entirely decided by how
    /// far behind the accepted count this plugin claims playback is. It is the number
    /// that does or does not leave winealsa's `held_frames` above one ALSA period, and a
    /// file can change it between runs.
    fn avail_frame_target(&self, room: u64, period: u64) -> u64 {
        let chosen = self.avail_frames.load(Ordering::Acquire) as u64;
        if chosen > 0 {
            return chosen.min(room);
        }
        if room > period {
            period / 2
        } else {
            0
        }
    }

    /// Records the size of one write from the app.
    fn note_transfer(&self, frames: u32) {
        let low = self.transfer_min.load(Ordering::Relaxed);
        if low == 0 || frames < low {
            self.transfer_min.store(frames, Ordering::Relaxed);
        }
        let high = self.transfer_max.load(Ordering::Relaxed);
        if frames > high {
            self.transfer_max.store(frames, Ordering::Relaxed);
        }
    }

    /// Looks over the audio the app just handed us for holes and splices.
    ///
    /// Two comparisons per sample on the app's own thread, inside a write it was
    /// already making. The alternative — trusting the drain side — cannot see this
    /// class of damage at all: a hole the caller padded is not a frame we dropped, so
    /// the ring stays fed and the device stays happy while the listener hears the seam.
    fn observe(&self, samples: &[f32], channels: usize) {
        let mut seams = self
            .seams
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        seams.observe(samples, channels);
        seams.note(&self.gap_count, &self.gap_frames, &self.click_count);
    }

    fn with_engine<T>(&self, f: impl FnOnce(&Engine) -> T) -> Option<T> {
        self.engine().as_ref().map(f)
    }

    /// Starts the engine.
    ///
    /// Deliberately does **not** touch the position base. Taking it from the engine's
    /// running frame count would make every restart jump the reported position *backwards*
    /// by everything played so far. libasound derives `appl_ptr - hw_ptr` from that
    /// position, so a backwards jump tells it a huge amount of audio is outstanding — the
    /// classic way to walk an audio client off the end of its own buffer, which faults in
    /// the app rather than here. A device rebuild (Android moving output) would trigger it.
    ///
    /// The base belongs to the *segment*, not the start: set once at `hw_params` and
    /// again only at `prepare`, which is where libasound itself resets `appl_ptr`. The
    /// engine's frame count keeps running across a rebuild, so the position stays
    /// monotone through one — which is the only property libasound actually needs.
    fn start_engine(&self) -> c_int {
        if self.running.load(Ordering::Acquire) {
            // libasound calls `start` itself once its threshold is met, which with this
            // ring can happen after the write path has already started the engine. A
            // second start is not an error and must not be reported as one.
            return 0;
        }
        let Some(result) = self.with_engine(|engine| engine.start()) else {
            return EIO;
        };
        self.awaiting_prefill.store(false, Ordering::Release);
        match result {
            Ok(()) => {
                self.running.store(true, Ordering::Release);
                trace!(
                    "start: engine running with {} frames queued",
                    self.ring().available_frames()
                );
                0
            }
            Err(error) => {
                trace!("start: {error:?}");
                EIO
            }
        }
    }

    /// The position to report, before the monotone step is applied.
    ///
    /// `avail` is `buffer_size - (appl_ptr - hw_ptr)`, so this number *is* the room the app
    /// believes it has. It is chosen, not measured, because winealsa pads with
    /// `silence_buf` whenever its own `data_in_alsa_frames` (frames moved into ALSA and not
    /// yet seen played) drains to zero while less than a period is held — and that counter
    /// drains by `avail` on every tick. `avail` at nothing and winealsa cannot move audio
    /// at all; at a whole buffer it empties every tick and the pad fires continuously.
    ///
    /// Two properties are not chosen, they are enforced:
    ///
    /// * **Never past what the app has written.** A position ahead of `appl_ptr` makes
    ///   `avail` come back larger than the buffer, which no card can answer and which
    ///   winealsa reacts to by splicing silence into the stream. This is what the ceiling
    ///   below is for, and it matters most right after a stall — the moment `step_towards`
    ///   is allowed to advance half a buffer to catch up, while the app supplies one
    ///   period per tick.
    /// * **Never negative.** `saturating_sub` throughout, because a counter that is briefly
    ///   behind a rebase must read as zero, not as four billion.
    fn raw_position(&self, counter: u64, base: u64) -> u64 {
        let room = self.buffer_size.load(Ordering::Acquire) as u64;
        let period = self.period_frames.load(Ordering::Acquire) as u64;
        let avail = self.avail_frame_target(room, period);
        let lag = room.saturating_sub(avail);
        counter
            .saturating_sub(base)
            .saturating_sub(lag)
            .saturating_add(self.dropped_frames.load(Ordering::Acquire))
            .min(self.accepted_frames.load(Ordering::Acquire))
    }

    /// Frames the device has played since the current segment began, from
    /// libasound's point of view.
    ///
    /// What the device has actually presented, plus the frames accepted while no
    /// device was taking any. Not what was handed to the device: that leads real
    /// playback by the whole device buffer, and a read position ahead of the truth
    /// tells the client it has room it does not have — libasound derives
    /// `appl_ptr - hw_ptr` from this, and the app then writes where the buffer is not.
    fn position(&self) -> u64 {
        let base = self.base_frames.load(Ordering::Acquire);
        let counter = self
            .with_engine(|engine| {
                if played_position() {
                    engine
                        .played_frames()
                        .unwrap_or_else(|| engine.stats().frames)
                } else {
                    engine.stats().frames
                }
            })
            .unwrap_or(base);
        let position = self.step_towards(self.raw_position(counter, base));
        let delivered = self
            .with_engine(|engine| engine.stats().frames)
            .unwrap_or(0);
        self.note_progress(delivered);
        position
    }

    /// Notices a position that has stopped moving while audio is supposedly playing.
    fn note_progress(&self, delivered: u64) {
        let now = now_ns();
        if delivered > self.last_delivered.load(Ordering::Relaxed)
            || self.last_advance_ns.load(Ordering::Relaxed) == 0
        {
            self.last_delivered.store(delivered, Ordering::Relaxed);
            self.last_advance_ns.store(now, Ordering::Relaxed);
            self.in_stall.store(false, Ordering::Relaxed);
            return;
        }
        if !self.delivering.load(Ordering::Relaxed) {
            // Nothing is being taken, so a still position is the truth, not a stall.
            self.last_advance_ns.store(now, Ordering::Relaxed);
            return;
        }
        let quiet = now.saturating_sub(self.last_advance_ns.load(Ordering::Relaxed));
        let threshold = self.stall_after_ns.load(Ordering::Relaxed);
        if quiet > threshold && !self.in_stall.swap(true, Ordering::AcqRel) {
            self.stalls.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// The position to report, given where playback really is.
    ///
    /// Monotone, and never further than half a buffer from the last report. Both
    /// properties are libasound's, not ours: it differences successive positions modulo
    /// the buffer size, so a larger step reads as a small backwards one and the client
    /// is told it has room it does not have. Catching up at twice real time after a
    /// stall costs nothing; being misread costs the app its buffer arithmetic.
    fn step_towards(&self, real: u64) -> u64 {
        let last = self.last_reported.load(Ordering::Acquire);
        if real <= last {
            return last;
        }
        let step = self.max_step.load(Ordering::Acquire) as u64;
        let next = if step == 0 {
            real
        } else {
            last.saturating_add((real - last).min(step))
        };
        self.last_reported.store(next, Ordering::Release);
        next
    }
}

// ---------------------------------------------------------------------------
// Lifecycle
// ---------------------------------------------------------------------------

/// Creates a PCM. Returns an opaque handle for the shim to keep in
/// `io->private_data`, or a negative errno.
///
/// # Safety
/// Called by the C shim with no arguments, so it can never be given a bad pointer.
#[no_mangle]
pub unsafe extern "C" fn gnaudio_alsa_create() -> *mut Pcm {
    let pcm = Box::new(Pcm::new());
    trace!("create: ioplug attached to the gamenative engine");
    Box::into_raw(pcm)
}

/// Releases a PCM. The engine is closed first, which is what stops its writer thread
/// before the ring and the sink are dropped.
///
/// # Safety
/// `pcm` must be a pointer from [`gnaudio_alsa_create`], and must not be used again.
#[no_mangle]
pub unsafe extern "C" fn gnaudio_alsa_destroy(pcm: *mut Pcm) {
    if pcm.is_null() {
        return;
    }
    // SAFETY: the pointer came from `Box::into_raw` and this is the only place it is
    // turned back into a `Box`.
    let pcm = unsafe { Box::from_raw(pcm) };
    if let Some(engine) = pcm.engine().take() {
        engine.close();
    }
    trace!("destroy: closed");
}

/// Opens the engine at the parameters libasound negotiated.
///
/// Everything comes from libasound's own decision — rate, channel count, sample
/// format, period, buffer — because those are the numbers the app asked for and was
/// given; second-guessing them here would make `writei` return lengths nobody
/// expects.
///
/// # Safety
/// `pcm` must be a live handle; the scalars are plain values.
#[no_mangle]
pub unsafe extern "C" fn gnaudio_alsa_hw_params(
    pcm: *mut Pcm,
    rate: c_int,
    channels: c_int,
    format: c_int,
    period_frames: u32,
    buffer_frames: u32,
) -> c_int {
    // SAFETY: the caller guarantees a live handle; `as_ref` gives `None` for null.
    let Some(pcm) = (unsafe { pcm.as_ref() }) else {
        return EINVAL;
    };
    if rate <= 0 || channels <= 0 || channels > 2 || buffer_frames == 0 {
        return EINVAL;
    }
    if !matches!(format, FORMAT_S16_LE | FORMAT_S32_LE | FORMAT_FLOAT_LE) {
        trace!("hw_params: refused format {format}");
        return EINVAL;
    }

    let mut engine = pcm.engine();
    if engine.is_some() {
        return EBUSY;
    }

    let period = period_frames.max(1);
    // The ring holds the whole credit and no more: the app cannot write past `avail`, so
    // a ring that size cannot be overflowed by any write libasound permits. A deeper ring
    // is not extra safety, because the app is never allowed to fill it — it is a number
    // libasound cannot represent, since `appl_ptr - hw_ptr` is kept modulo the buffer size.
    let ring_frames = ring_frames_for(period, buffer_frames);
    let ring = Arc::new(Ring::new(ring_frames as usize, channels as usize));

    let config = StreamConfig {
        device_id: None,
        channels: channels as u16,
        sample_rate: Some(rate),
        // The app's period is the shape it writes in; the engine turns it into whole
        // device bursts and spreads a block across the periods that cover it.
        format: None,
        device_buffer_capacity_frames: None,
        block_frames: period,
        // A ceiling on the device buffer, not a request. The backend picks within it — two
        // bursts in low-latency mode, the route's own buffer otherwise — and the reason this
        // is a ceiling is that a device buffer is charged against the buffer size libasound
        // promised the app: the engine writes as fast as the device accepts, so a device
        // buffer deeper than what the app's own buffer leaves over would take its backlog
        // out of the ring that has to fill the next block.
        device_buffer_frames: Some(buffer_frames.saturating_sub(ring_frames).max(period)),
        low_latency: low_latency(),
        sharing: sharing_mode(),
        performance: Performance::LowLatency,
    };
    let source = Source {
        ring: ring.clone(),
        last_write_ns: pcm.last_write_ns.clone(),
        wait_ns: pcm.wait_ns.clone(),
        // Always on: see the field's own note. There is no configuration in which the
        // one measurement that can see a drain-side click should be skipped.
        diagnostics: true,
    };

    match Engine::open(Box::new(source), config) {
        Ok(opened) => {
            let granted = opened.granted();
            *pcm.scratch
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()) =
                vec![0.0; buffer_frames as usize * channels as usize];
            pcm.format.store(format, Ordering::Release);
            pcm.channels.store(channels as u32, Ordering::Release);
            // The ring less one period: playback begins with the device fed and one
            // period still free, so the app's first write does not block.
            pcm.prefill_frames
                .store(ring_frames.saturating_sub(period_frames), Ordering::Release);
            pcm.base_frames.store(0, Ordering::Release);
            pcm.period_frames.store(period, Ordering::Release);
            crate::ring::note_opened();
            // Half a buffer per report: big enough to keep up with playback, small
            // enough that libasound cannot mistake it for a wrapped-around one.
            pcm.avail_frames
                .store(credit_frames(period, buffer_frames), Ordering::Release);
            pcm.max_step.store(buffer_frames / 2, Ordering::Release);
            pcm.last_reported.store(0, Ordering::Release);
            pcm.accepted_frames.store(0, Ordering::Release);
            pcm.last_advance_ns.store(now_ns(), Ordering::Release);
            pcm.stall_after_ns.store(
                stall_threshold_ns(granted.device_buffer_frames, granted.sample_rate),
                Ordering::Release,
            );
            pcm.wait_ns.store(
                device_buffer_ns(granted.device_buffer_frames, granted.sample_rate),
                Ordering::Release,
            );
            *pcm.ring
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()) = ring;
            trace!(
                "hw_params: asked {rate} Hz, {channels} ch, format {format}, period {period}, \
                 buffer {buffer_frames}; engine granted {} Hz, {} ch, burst {}, buffer {}, \
                 sharing {:?}",
                granted.sample_rate,
                granted.device_channels,
                granted.frames_per_burst,
                granted.device_buffer_frames,
                granted.sharing
            );
            *engine = Some(opened);
            0
        }
        Err(error) => {
            trace!("hw_params: the engine refused the device: {error:?}");
            match error {
                gnaudio::Error::Unsupported => EINVAL,
                gnaudio::Error::Unavailable | gnaudio::Error::Disconnected => ENODEV,
                _ => EIO,
            }
        }
    }
}

/// Releases the device but keeps the PCM, so a later `hw_params` can reopen it.
///
/// # Safety
/// `pcm` must be a live handle.
#[no_mangle]
pub unsafe extern "C" fn gnaudio_alsa_hw_free(pcm: *mut Pcm) {
    // SAFETY: `as_ref` is `None` for null.
    let Some(pcm) = (unsafe { pcm.as_ref() }) else {
        return;
    };
    pcm.dump.finish();
    let taken = pcm.engine().take();
    let mut late_blocks = 0u64;
    if let Some(engine) = taken {
        // Read the engine's own counts before the device goes: a block the client could
        // not produce inside one device period is the client-side twin of an xrun, and
        // it is the only one of the two that can happen while the device looks perfect.
        late_blocks = engine.stats().underruns;
        engine.close();
        trace!("hw_free: engine closed");
    }
    report_summary(pcm, late_blocks);
}

/// Returns the PCM to its starting state: nothing queued, position zero.
///
/// # Safety
/// `pcm` must be a live handle.
#[no_mangle]
pub unsafe extern "C" fn gnaudio_alsa_prepare(pcm: *mut Pcm) -> c_int {
    // SAFETY: `as_ref` is `None` for null.
    let Some(pcm) = (unsafe { pcm.as_ref() }) else {
        return EINVAL;
    };
    pcm.running.store(false, Ordering::Release);
    if let Some(base) = pcm.with_engine(|engine| {
        let _ = engine.stop();
        engine
            .played_frames()
            .unwrap_or_else(|| engine.stats().frames)
    }) {
        pcm.base_frames.store(base, Ordering::Release);
        // A new segment starts at zero, and so do the step arithmetic and the ceiling
        // that keeps the reported position behind the app's own write pointer.
        pcm.last_reported.store(0, Ordering::Release);
        pcm.accepted_frames.store(0, Ordering::Release);
    }
    pcm.ring().clear();
    trace!("prepare: queue cleared");
    0
}

/// Starts consuming.
///
/// # Safety
/// `pcm` must be a live handle.
#[no_mangle]
pub unsafe extern "C" fn gnaudio_alsa_start(pcm: *mut Pcm) -> c_int {
    // SAFETY: `as_ref` is `None` for null.
    let Some(pcm) = (unsafe { pcm.as_ref() }) else {
        return EINVAL;
    };
    // libasound calls this on the *first* write, when the buffer holds one period.
    // Beginning to play there leaves the app a standing queue of exactly one period and
    // no margin, which starves once a second on a 10 ms writer. If there is not enough
    // queued yet, arm the start and let `transfer` pull the trigger once the buffer is
    // worth starting on. The app is not blocked by this: with nothing consuming, its
    // writes fill the buffer up to `buffer_size` and then block in the usual way.
    // Never more than the ring can hold, and always one period less than that.
    //
    // The target comes from the app's buffer size, which is larger than this ring — so
    // without the bounds below it is unreachable and the engine never starts, leaving the
    // position at zero until libasound believes the buffer is permanently full.
    //
    // The period of headroom matters as much as the ceiling. A target equal to the
    // capacity means the engine only ever starts on a completely full ring, so the app's
    // very next write finds no room and blocks until the device has drained a period —
    // which is the "no margin" start this whole mechanism exists to avoid. Measured on the
    // game: every remaining hole fell in the first 3.5 s of audio and none after, and the
    // startup line read `engine running with 1440 frames queued` against a 1440 frame ring.
    // Already running: the write path starts the engine when the ring is worth starting
    // on, which can happen before libasound gets here. Re-entering the holding branch
    // would arm a prefill for a stream that is already playing.
    if pcm.running.load(Ordering::Acquire) {
        return 0;
    }
    let target = pcm.prefill_target();
    if target > 0 && pcm.ring().available_frames() < target {
        pcm.awaiting_prefill.store(true, Ordering::Release);
        pcm.prefill_deadline_ns.store(
            now_ns().saturating_add(PREFILL_TIMEOUT_NS),
            Ordering::Release,
        );
        trace!(
            "start: holding until {} frames are queued (have {})",
            target,
            pcm.ring().available_frames()
        );
        return 0;
    }
    pcm.start_engine()
}

/// Stops consuming, without releasing the device.
///
/// # Safety
/// `pcm` must be a live handle.
#[no_mangle]
pub unsafe extern "C" fn gnaudio_alsa_stop(pcm: *mut Pcm) -> c_int {
    // SAFETY: `as_ref` is `None` for null.
    let Some(pcm) = (unsafe { pcm.as_ref() }) else {
        return EINVAL;
    };
    pcm.running.store(false, Ordering::Release);
    match pcm.with_engine(|engine| engine.stop()) {
        Some(Ok(())) => {
            trace!("stop: engine stopped at position {}", pcm.position());
            0
        }
        Some(Err(error)) => {
            trace!("stop: {error:?}");
            EIO
        }
        // Stopping a stream that never started is not a failure: libasound calls
        // this on its way out of one.
        None => 0,
    }
}

/// Pauses or resumes the device. `enable` is libasound's own flag: non-zero pauses.
///
/// # Safety
/// `pcm` must be a live handle; `enable` is a plain value.
#[no_mangle]
pub unsafe extern "C" fn gnaudio_alsa_pause(pcm: *mut Pcm, enable: c_int) -> c_int {
    // SAFETY: `as_ref` is `None` for null.
    let Some(pcm) = (unsafe { pcm.as_ref() }) else {
        return EINVAL;
    };
    let result = pcm.with_engine(|engine| {
        if enable != 0 {
            engine.pause()
        } else {
            engine.resume()
        }
    });
    match result {
        Some(Ok(())) => {
            trace!("pause: enable={enable}");
            0
        }
        Some(Err(error)) => {
            trace!("pause: {error:?}");
            EIO
        }
        None => 0,
    }
}

/// Plays out what is queued, then returns. Bounded, because a drain that never
/// returns hangs whatever called `snd_pcm_drain` — usually a game's shutdown.
///
/// # Safety
/// `pcm` must be a live handle.
#[no_mangle]
pub unsafe extern "C" fn gnaudio_alsa_drain(pcm: *mut Pcm) -> c_int {
    // SAFETY: `as_ref` is `None` for null.
    let Some(pcm) = (unsafe { pcm.as_ref() }) else {
        return EINVAL;
    };
    let deadline = std::time::Instant::now() + std::time::Duration::from_millis(DRAIN_TIMEOUT_MS);
    loop {
        // Only the ring is watched. The engine's own queue depth counts frames handed
        // to AAudio rather than frames played, so it is not a figure a drain can wait
        // out — and waiting on a wrong one is how a shutdown hangs.
        let hold = pcm.ring().available_frames();
        if hold == 0 {
            trace!("drain: ring empty");
            return 0;
        }
        if std::time::Instant::now() >= deadline {
            trace!("drain: giving up with {hold} frames still queued");
            return 0;
        }
        std::thread::sleep(std::time::Duration::from_millis(2));
    }
}

// ---------------------------------------------------------------------------
// The audio path
// ---------------------------------------------------------------------------

/// Takes `frames` frames of the app's PCM, converts to float, and queues them.
///
/// **Blocks while the ring is full.** That is deliberate and it is what a sound card
/// does: the app's audio thread is the one that should wait when it has written faster
/// than the device plays. Returning a short count instead would turn every game's
/// write loop into a spin — measured once as 1200 frames/s where 48000 were expected.
/// The wait is bounded, so a device that stops consuming produces a short count rather
/// than a hung thread.
///
/// # Safety
/// `src` must point at `frames * channels` samples in the format the PCM was opened
/// with, and stay valid for the duration of the call.
#[no_mangle]
pub unsafe extern "C" fn gnaudio_alsa_transfer(
    pcm: *mut Pcm,
    src: *const c_void,
    frames: u32,
) -> i64 {
    // SAFETY: `as_ref` is `None` for null.
    let Some(pcm) = (unsafe { pcm.as_ref() }) else {
        return 0;
    };
    if src.is_null() || frames == 0 {
        return 0;
    }
    // No device is taking audio — Android has moved output somewhere and the backend
    // is rebuilding. Accept the frames and drop them, which is what a sound card whose
    // output has been unplugged does: the app keeps its own clock, and what it writes
    // is silence to nobody rather than a stall in its audio thread. Blocking here
    // instead would push the device outage into the app, and a game's audio thread
    // waiting on a rebuild is the whole app hitching.
    pcm.last_write_ns.store(now_ns(), Ordering::Release);
    pcm.note_transfer(frames);
    pcm.accepted_frames
        .fetch_add(frames as u64, Ordering::Release);
    let (delivering, xruns) = pcm
        .with_engine(|engine| {
            let stats = engine.stats();
            (engine.is_delivering(), stats.device_xruns)
        })
        .unwrap_or((false, 0));
    pcm.xruns.store(xruns, Ordering::Release);
    if !delivering {
        pcm.delivering.store(false, Ordering::Release);
        pcm.dropped_frames
            .fetch_add(frames as u64, Ordering::Release);
        return frames as i64;
    }
    if !pcm.delivering.swap(true, Ordering::AcqRel) {
        // A device is taking audio again, and this is the first write since it stopped.
        // What is still queued was meant to play inside the gap the listener already
        // heard; replaying it now would put a stutter on top of that gap instead of
        // continuing the music where it stopped.
        pcm.ring().clear();
    }
    let channels = pcm.channels.load(Ordering::Acquire).max(1) as usize;
    let samples = frames as usize * channels;
    let ring = pcm.ring();

    let taken = if pcm.format.load(Ordering::Acquire) == FORMAT_FLOAT_LE {
        // The device's own format: straight through, no scratch.
        // SAFETY: the shim guarantees `samples` floats at `src`.
        let input = unsafe { core::slice::from_raw_parts(src as *const f32, samples) };
        pcm.dump.write(input, channels);
        pcm.observe(input, channels);
        let deadline =
            std::time::Instant::now() + std::time::Duration::from_millis(WRITE_TIMEOUT_MS);
        ring.push_blocking(input, deadline)
    } else {
        let mut scratch = pcm
            .scratch
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if scratch.len() < samples {
            scratch.resize(samples, 0.0);
        }
        if pcm.format.load(Ordering::Acquire) == FORMAT_S16_LE {
            // SAFETY: the shim guarantees `samples` 16-bit samples at `src`.
            let input = unsafe { core::slice::from_raw_parts(src as *const i16, samples) };
            for (out, sample) in scratch[..samples].iter_mut().zip(input) {
                *out = *sample as f32 / 32_768.0;
            }
        } else {
            // SAFETY: the shim guarantees `samples` 32-bit samples at `src`.
            let input = unsafe { core::slice::from_raw_parts(src as *const i32, samples) };
            for (out, sample) in scratch[..samples].iter_mut().zip(input) {
                *out = *sample as f32 / 2_147_483_648.0;
            }
        }
        pcm.dump.write(&scratch[..samples], channels);
        pcm.observe(&scratch[..samples], channels);
        let deadline =
            std::time::Instant::now() + std::time::Duration::from_millis(WRITE_TIMEOUT_MS);
        ring.push_blocking(&scratch[..samples], deadline)
    };
    // Start from here, because libasound will not: it calls `start` when `appl_ptr`
    // reaches its threshold, and this ring is smaller than that threshold, so the call
    // never comes and the engine never runs. Starting it from the app's own thread once
    // enough is queued is what makes a ring this size viable at all.
    //
    // The same bounded target `start` uses, rather than "any audio at all": that condition
    // starts the engine on the very first period, which is precisely the no-margin start the
    // prefill exists to prevent.
    if !pcm.running.load(Ordering::Acquire) {
        let target = pcm.prefill_target();
        let available = pcm.ring().available_frames();
        let deadline = pcm.prefill_deadline_ns.load(Ordering::Acquire);
        let overdue = deadline != 0 && now_ns() >= deadline;
        if available > 0 && (available >= target || overdue) {
            // The app's thread is allowed to block, which is why this lives here.
            pcm.start_engine();
        }
    }
    if taken < frames as usize {
        pcm.shortfalls.fetch_add(1, Ordering::Relaxed);
        pcm.short_frames
            .fetch_add((frames as usize - taken) as u64, Ordering::Relaxed);
    }
    taken as i64
}

/// Records how deep the pipeline ran, from the only place that can see it: libasound's
/// own `appl_ptr` against the position it has been given.
///
/// Called from the shim's pointer callback, which runs inside libasound's mutex, so this
/// is four atomics and no allocation on a path that is already taking a lock.
///
/// # Safety
/// `pcm` must be a live handle.
#[no_mangle]
pub unsafe extern "C" fn gnaudio_alsa_note_depth(pcm: *mut Pcm, depth: u64, buffer_size: u64) {
    // SAFETY: `as_ref` is `None` for null.
    if let Some(pcm) = unsafe { pcm.as_ref() } {
        pcm.note_depth(depth, buffer_size);
    }
}

/// Reports what the ring did over the life of one open device, once, at close.
///
/// Deliberately not periodic, and deliberately not gated on a variable. A report once a
/// second from a thread of its own is work *inside* the process, and a synthetic writer
/// feeding a 40 ms buffer starves exactly one block per second — at exactly the report
/// interval — while being late once in ten seconds. Diagnostics that perturb what they
/// measure turn "healthy" into "crackling" and send the search the wrong way. Once, after
/// the audio has stopped, costs nothing and is the only moment a session's totals exist.
fn report_summary(pcm: &Pcm, late_blocks: u64) {
    // Everything below is worth printing on its own. Gating the whole report on the ring
    // having starved once meant the sessions that most needed reading — the clean-looking
    // ones — produced no output at all.
    let ring = crate::ring::take_starvation_report();
    let xruns = pcm.xruns.load(Ordering::Acquire);
    let shortfalls = pcm.shortfalls.load(Ordering::Relaxed);
    let short_frames = pcm.short_frames.load(Ordering::Relaxed);
    let gaps = pcm.gap_count.load(Ordering::Relaxed);
    let gap_frames = pcm.gap_frames.load(Ordering::Relaxed);
    let clicks = pcm.click_count.load(Ordering::Relaxed);
    let stalls = pcm.stalls.load(Ordering::Relaxed);
    let max_depth = pcm.max_depth.load(Ordering::Relaxed);
    let depth_overruns = pcm.depth_overruns.load(Ordering::Relaxed);
    let transfer = (
        pcm.transfer_min.load(Ordering::Relaxed),
        pcm.transfer_max.load(Ordering::Relaxed),
    );
    match ring {
        Some((count, frames, low)) if count > 0 => trace!(
            "summary: the ring ran dry {count} times, {frames} frames ({:.1} ms) of silence \
             played{}; low water {low} frames; device reports {xruns} xruns",
            frames as f64 * 1000.0 / 48_000.0,
            match crate::ring::starvation_span_ms() {
                Some((first, last)) => format!(", from {first} ms to {last} ms after start"),
                None => String::new(),
            }
        ),
        Some((_, _, low)) => trace!(
            "summary: the ring never ran dry; low water {low} frames; device reports \
             {xruns} xruns"
        ),
        None => trace!("summary: the ring was never measured; device reports {xruns} xruns"),
    }
    if shortfalls > 0 {
        trace!(
            "summary: {shortfalls} writes could not take everything offered, {short_frames} \
             frames ({:.1} ms) left behind",
            short_frames as f64 * 1000.0 / 48_000.0
        );
    }
    if late_blocks > 0 {
        trace!("summary: {late_blocks} blocks took longer to produce than one device period");
    }
    if stalls > 0 {
        trace!(
            "summary: the reported position stood still {stalls} times while audio was \
             supposedly playing"
        );
    }
    if max_depth > 0 {
        // Always worth a line: this is the depth libasound's own arithmetic is modulo,
        // and it is the one number that says whether the pipeline fits in the buffer the
        // app was promised. Over the buffer it is not a rounding error.
        trace!(
            "summary: the pipeline ran {max_depth} frames deep against a {buffer_size} frame \
             buffer, over it {depth_overruns} times",
            buffer_size = pcm.buffer_size.load(Ordering::Relaxed)
        );
    }
    if transfer.1 > 0 {
        trace!(
            "summary: the app wrote between {} and {} frames at a time",
            transfer.0,
            transfer.1
        );
    }
    if gaps > 0 || clicks > 0 {
        trace!(
            "summary: the audio handed over contained {gaps} gaps averaging {:.0} ms and \
             {clicks} steps — that damage arrived before it reached us",
            if gaps > 0 {
                gap_frames as f64 * 1000.0 / 48_000.0 / gaps as f64
            } else {
                0.0
            }
        );
    }
}

/// Frames the device has consumed since the current segment began: libasound's
/// hardware pointer, before it is reduced modulo the buffer size.
///
/// This is what libasound uses to decide whether the app may write, so it has to
/// advance on its own — including while the app writes nothing. That is why it is
/// the engine's count of frames *handed to the device* (silence included) rather
/// than a count of frames pulled out of the ring.
///
/// # Safety
/// `pcm` must be a live handle.
#[no_mangle]
pub unsafe extern "C" fn gnaudio_alsa_position(pcm: *mut Pcm) -> u64 {
    // SAFETY: `as_ref` is `None` for null.
    match unsafe { pcm.as_ref() } {
        Some(pcm) => pcm.position(),
        None => 0,
    }
}

/// Whether a device is open, which the shim reports through `poll`.
///
/// # Safety
/// `pcm` must be a live handle or null.
#[no_mangle]
pub unsafe extern "C" fn gnaudio_alsa_is_open(pcm: *mut Pcm) -> c_int {
    // SAFETY: `as_ref` is `None` for null.
    let Some(pcm) = (unsafe { pcm.as_ref() }) else {
        return 0;
    };
    c_int::from(pcm.with_engine(|_| ()).is_some())
}

/// Frame bytes for a format, so the shim can size its byte-based constraints without
/// duplicating the numbering.
///
/// # Safety
/// `format` is a plain value.
#[no_mangle]
pub unsafe extern "C" fn gnaudio_alsa_frame_bytes(format: c_int, channels: c_int) -> c_int {
    if channels <= 0 {
        return 0;
    }
    let width = match format {
        FORMAT_S16_LE => 2,
        FORMAT_S32_LE | FORMAT_FLOAT_LE => 4,
        _ => return 0,
    };
    width * channels
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_reported_position_never_leaps_more_than_half_a_buffer_and_never_goes_back() {
        let pcm = Pcm::new();
        pcm.max_step.store(960, Ordering::Release);

        assert_eq!(pcm.step_towards(480), 480);
        // A device buffer's worth at once is what libasound cannot read back, so it is
        // served in halves rather than in one step.
        assert_eq!(pcm.step_towards(2976), 1440);
        assert_eq!(pcm.step_towards(2976), 2400);
        // A counter that is briefly behind — a rebuild carrying its base, a backend
        // without a presentation clock — must not walk the position backwards.
        assert_eq!(pcm.step_towards(100), 2400);
        assert_eq!(pcm.step_towards(2400), 2400);
    }

    #[test]
    fn the_reported_position_never_passes_what_the_app_has_written() {
        // `avail` is `buffer_size - (appl_ptr - hw_ptr)`, so a position past `appl_ptr`
        // makes it come back larger than the buffer — which no card can answer, and which
        // winealsa's lead-in branch reacts to by splicing silence into the stream. The
        // ceiling is the frames the app has handed over in this segment.
        let pcm = Pcm::new();
        pcm.max_step.store(960, Ordering::Release);
        pcm.buffer_size.store(1920, Ordering::Release);
        pcm.period_frames.store(480, Ordering::Release);
        pcm.avail_frames.store(240, Ordering::Release);
        pcm.accepted_frames.store(480, Ordering::Release);

        // A device that claims far more than the app has written is capped at the app's
        // own total, so `avail` stays inside the buffer.
        pcm.last_reported.store(240, Ordering::Release);
        let reported = pcm.raw_position(4800, 0);
        assert_eq!(
            reported, 480,
            "capped by what the app wrote, not by the device"
        );

        // Then the app writes a period, and the position may follow it — but no further.
        pcm.accepted_frames.store(960, Ordering::Release);
        assert_eq!(pcm.raw_position(4800, 0), 960);
        assert_eq!(pcm.raw_position(4800, 0), 960, "and it does not run on");
    }

    #[test]
    fn the_credit_is_two_blocks_and_never_more_than_the_buffer() {
        // The measured geometry: a 480 frame period in a 1 920 frame buffer.
        assert_eq!(credit_frames(480, 1920), 960);
        // A buffer barely two periods wide cannot have two blocks of credit in it.
        assert_eq!(credit_frames(480, 960), 960);
        // The value that broke the pipeline — 240, half a block — must be unreachable.
        assert!(credit_frames(480, 1920) >= 480);
        // And never above the buffer, because libasound's arithmetic is modulo it.
        assert!(credit_frames(1024, 1920) <= 1920);
    }

    #[test]
    fn the_ring_holds_the_whole_credit_and_no_more() {
        // The app may write `credit` frames; a ring smaller than that would block it, and
        // a larger ring is space libasound never lets it fill.
        assert_eq!(ring_frames_for(480, 1920), 960);
        // Never deeper than the buffer libasound was given.
        assert!(ring_frames_for(1024, 1920) <= 1920);
        // Never less than one period, or a block could not be assembled at all.
        assert!(ring_frames_for(480, 480) >= 480);
    }

    
    #[test]
    fn the_prefill_target_leaves_a_period_of_room() {
        // The game's geometry: a 480 frame period, a 1920 frame buffer, and a ring of three
        // periods. The target derived from the app's buffer (1440) equals the ring exactly,
        // and starting on a completely full ring means the app's next write blocks — which
        // is where every remaining hole in the game's stream came from.
        let pcm = Pcm::new();
        pcm.period_frames.store(480, Ordering::Release);
        pcm.prefill_frames.store(1440, Ordering::Release);
        *pcm.ring.lock().unwrap() = Arc::new(Ring::new(8 * 480, 2));
        assert_eq!(
            pcm.prefill_target(),
            1440,
            "the app's buffer, and the ring can hold it"
        );

        // A ring too small for the app's buffer keeps a period of headroom, because
        // starting on a completely full ring blocks the app's very next write.
        *pcm.ring.lock().unwrap() = Arc::new(Ring::new(3 * 480, 2));
        assert_eq!(pcm.prefill_target(), 960, "two periods queued, one free");

        // A request for more than the ring can hold leaves the same headroom, rather than
        // filling the ring: the ceiling is what keeps a full ring from blocking the app.
        pcm.prefill_frames.store(4096, Ordering::Release);
        assert_eq!(pcm.prefill_target(), 960);

        // And a tiny ring still starts on something rather than deadlocking.
        *pcm.ring.lock().unwrap() = Arc::new(Ring::new(480, 2));
        assert_eq!(pcm.prefill_target(), 1);
    }

    #[test]
    fn the_stall_threshold_follows_the_device_buffer() {
        // 3744 frames at 48 kHz is 78 ms, so a clean shared-mode stream must not be
        // allowed to report a stall until it has been still for twice that.
        assert_eq!(stall_threshold_ns(3744, 48_000), 156_000_000);
        assert_eq!(stall_threshold_ns(2976, 48_000), 124_000_000);
        // A tiny buffer never makes the threshold uselessly fine.
        assert_eq!(stall_threshold_ns(96, 48_000), STALL_AFTER_NS);
    }

    #[test]
    fn without_a_buffer_size_the_position_is_passed_through() {
        let pcm = Pcm::new();
        assert_eq!(pcm.step_towards(0), 0);
        assert_eq!(pcm.step_towards(48_000), 48_000);
    }

    #[test]
    fn frame_bytes_match_the_formats_we_carry() {
        // SAFETY: plain values in, a plain value out.
        unsafe {
            assert_eq!(gnaudio_alsa_frame_bytes(FORMAT_S16_LE, 2), 4);
            assert_eq!(gnaudio_alsa_frame_bytes(FORMAT_S32_LE, 2), 8);
            assert_eq!(gnaudio_alsa_frame_bytes(FORMAT_FLOAT_LE, 2), 8);
            assert_eq!(gnaudio_alsa_frame_bytes(FORMAT_S16_LE, 0), 0);
            assert_eq!(gnaudio_alsa_frame_bytes(99, 2), 0);
        }
    }

    #[test]
    fn a_null_handle_is_refused_rather_than_followed() {
        let null = core::ptr::null_mut();
        // SAFETY: null is exactly what these entry points must survive.
        unsafe {
            assert_eq!(
                gnaudio_alsa_hw_params(null, 48_000, 2, FORMAT_FLOAT_LE, 256, 1024),
                EINVAL
            );
            assert_eq!(gnaudio_alsa_prepare(null), EINVAL);
            assert_eq!(gnaudio_alsa_start(null), EINVAL);
            assert_eq!(gnaudio_alsa_stop(null), EINVAL);
            assert_eq!(gnaudio_alsa_pause(null, 1), EINVAL);
            assert_eq!(gnaudio_alsa_drain(null), EINVAL);
            assert_eq!(gnaudio_alsa_position(null), 0);
            assert_eq!(gnaudio_alsa_is_open(null), 0);
            assert_eq!(gnaudio_alsa_transfer(null, core::ptr::null(), 0), 0);
            gnaudio_alsa_hw_free(null);
            gnaudio_alsa_destroy(null);
        }
    }

    #[test]
    fn a_created_handle_answers_questions_and_closes() {
        // SAFETY: the handle is ours for the length of the test.
        unsafe {
            let pcm = gnaudio_alsa_create();
            assert!(!pcm.is_null());
            assert_eq!(gnaudio_alsa_is_open(pcm), 0, "no device until hw_params");
            assert_eq!(
                gnaudio_alsa_stop(pcm),
                0,
                "stopping nothing is not a failure"
            );
            assert_eq!(gnaudio_alsa_pause(pcm, 0), 0, "nor is resuming nothing");
            assert_eq!(gnaudio_alsa_position(pcm), 0);
            gnaudio_alsa_destroy(pcm);
        }
    }

    #[test]
    fn an_unsupported_format_is_refused_before_a_device_is_opened() {
        // SAFETY: a live handle, used and released here.
        unsafe {
            let pcm = gnaudio_alsa_create();
            assert_eq!(
                gnaudio_alsa_hw_params(pcm, 48_000, 2, 0, 256, 1024),
                EINVAL,
                "S8 is not a format this plugin converts"
            );
            gnaudio_alsa_destroy(pcm);
        }
    }
}
