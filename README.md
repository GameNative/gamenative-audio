# gnaudio

A native Android audio engine for direct, low-latency output, built on AAudio.

It gives an Android app a device it drives directly, a block size it chooses, a clock
it can trust, honest latency reporting, and pause/resume. Output only.

It ships as two things built from one engine: an Android library (`libgnaudio.so`), and
an **ALSA PCM plugin** (`libasound_module_pcm_gamenative.so`) that lets ordinary Linux
audio — including a Wine/Proton container — play through it without a daemon, a socket,
a helper process, or a Windows-side driver.

The point of it is the shape of the contract, not a new mixer:

* **The client owns its block size.** One `render` call per block, spread over
  however many device periods that block covers. The device's own period is
  invisible; a block is a whole number of the device's bursts, never below one.
* **The device's consumption is the clock.** `render` runs on the render thread this
  crate owns, once per block, whenever the device's data callback asks for a period and
  waits for the answer.
* **Positions are block-aligned** and zero at the first block after a start. They
  do not advance while paused, and they survive a device being rebuilt underneath
  the client, so a timeline is never silently rebased.
* **Latency is measured, not promised.** What the device granted — including
  whether exclusive sharing survived — is read back and reported, and the live
  figure is recomputed from the device's own numbers.
* **Pause is a pause.** It does not reopen the device, and it does not let the
  device replay audio it was still holding.

Android-specific honesty: sharing mode is a *request*. A device can answer `Shared`
without ever saying no, so the engine reports what it got instead of assuming what it
asked for. The default is `Shared` — AAudio's own default, and what the PulseAudio
AAudio sink uses unconditionally. `Exclusive` is reachable and shorter, since it
bypasses the AudioFlinger mixer, but on the HAL measured here it grants
`MMAP_PLAYBACK`: this writer and the DMA buffer with nothing in between, which is a
path that hides nothing and reports nothing when it goes wrong.

## One device shape: AAudio's data callback

AAudio can drive output two ways — the device pulls through a data callback, or the client
writes with a blocking call. This crate ships the **callback**, the shape PulseAudio's own
AAudio sink uses on this device, and there is no blocking path beside it.

The reason is specific rather than stylistic. `setFramesPerDataCallback` pins the period
the code sizes its buffer for, while the device's own periods are several times larger —
1 152 frames and up, from AudioFlinger — so every callback would arrive bigger than the
scratch buffer and take an early return that leaves the period silent. A stream handed
silence every period still reports the right rate, an active track and zero underruns, so
that failure is invisible from above. Sizing from `AAudioStream_getBufferCapacityInFrames`
and clamping instead of bailing is what makes the callback the whole device path.

Three properties of the shape matter, and the first is the one that changes behaviour
underneath libasound:

* **In low-latency mode the device's buffer is `burst × 2`, and it grows a burst per
  lost-audio report.** That is the reference sink's `low_latency` branch, and the branch
  GameNative runs when the container's toggle is on: `PulseAudioComponent` passes
  `low_latency=true`, so every sizing decision in `module-aaudio-sink.c` is live. Its own log
  on this device reads `AAudio buffer size set to 384 frames (burst: 192, capacity: 1 536)`.
  A burst is the route's own unit — how often that HAL wakes — so two of them is a duration
  the route defined rather than one imposed on it, and the growth rule is what lets a route
  that needs more reach it without anyone guessing a number. The caller's budget caps it,
  because the pipeline above holds its backlog out of the same buffer size the app was
  promised. With the mode off — the default, and the setting's unchecked state — nothing is
  sized and nothing grows: the stream keeps what the route chose for itself, which is what the
  reference does outside that branch.
* **The callback does not render.** It hands the period to a render thread and waits for
  it, as `pa_asyncmsgq_send` does. Rendering on AAudio's own thread would put the audio
  pipeline's lateness — a lock, a resample, a ring briefly empty — inside the device's
  deadline.
* **The wait is bounded, but by the route.** A hand-off that misses its deadline writes
  silence and counts a dropout, because a stalled callback is a stalled device — and the
  deadline is one device buffer's duration with a 20 ms floor, not a flat 20 ms, because a
  callback that answers inside one buffer has cost the listener nothing. The render thread fills a
  staging buffer which the callback copies out, so an abandoned hand-off is harmless;
  PulseAudio can render straight into the callback's buffer only because its hand-off
  cannot time out.

## Layout

| | |
|---|---|
| `src/lib.rs` | the crate's contract, and the docs a client reads first |
| `src/engine.rs` | the engine: blocks, positions, pause, dropout accounting |
| `src/backend/callback.rs` | the device: its data callback, the render thread, and rebuilding it when it goes |
| `src/supply.rs` | how much of a device's request can be filled, and how much buffer it should hold |
| `src/reconnect.rs` | what counts as a device change, and how long to wait before retrying |
| `src/backend/mock.rs` | a scripted device, for tests without hardware |
| `src/c_api.rs` | the C ABI, declared in `include/gnaudio.h` |
| `tests/` | the semantics, and the C boundary |
| `examples/gn-audio-probe.rs` | what *this* device grants, and what it costs |
| `alsa-plugin/` | the ALSA PCM plugin: Rust core, thin C shim |

## Use

```rust
use gnaudio::{Client, Engine, RenderContext, StreamConfig};

struct Tone { phase: f32 }

impl Client for Tone {
    fn render(&mut self, ctx: &RenderContext, out: &mut [f32]) {
        // Channel-major: all of channel 0, then channel 1.
        let frames = ctx.frames as usize;
        let (left, right) = out.split_at_mut(frames);
        let step = 440.0 / ctx.sample_rate as f32;
        for frame in 0..frames {
            let sample = (self.phase * core::f32::consts::TAU).sin() * 0.2;
            self.phase = (self.phase + step) % 1.0;
            left[frame] = sample;
            right[frame] = sample;
        }
    }
}

let engine = Engine::open(
    Box::new(Tone { phase: 0.0 }),
    StreamConfig { block_frames: 256, ..StreamConfig::default() },
)?;
engine.start()?;
```

Then `engine.granted()` for what the device actually gave, `engine.stats()` for
callbacks/frames/dropouts, `engine.latency_frames()` for the live figure,
`engine.queued_frames()` for what the device still holds, `engine.take_events()` to
learn that the device went away, and `engine.reopen()` to come back on it.

A C caller uses `include/gnaudio.h` instead: `gnaudio_open` with a render
function pointer, then the same calls.

## The ALSA plugin

`alsa-plugin/` builds `libasound_module_pcm_gamenative.so`: a libasound ioplug PCM whose
`transfer` converts the app's samples to float and pushes them into a ring, while a
render thread drains that ring into AAudio's data callback. The app's thread blocks for room
exactly as it would on a sound card, so no existing audio code has to know it is not
talking to hardware.

The split is deliberate — the ring, the conversion, the state machine and the engine are
Rust; the C is the ioplug struct, the ALSA-typed callbacks and the exported entry point's
name, which libasound derives from its own headers by macro.

```sh
alsa-plugin/build.sh                        # -> dist/arm64-v8a/libasound_module_pcm_gamenative.so
alsa-plugin/test.sh                         # build, install and play a tone on a device
scripts/pack-alsa-asset.sh [--check]        # the asset GameNative ships
```

`test.sh` is the check that separates "the plugin loads and plays" from "a game reaches
it": it runs a 440 Hz tone through `snd_pcm_open("default")` as the app's own uid, and
prints per-chunk throughput. Anything under 48000 frames/s means libasound is being told
something untrue about the device's progress.

## When Android moves the output

Output moves between devices constantly — headphones in and out, a Bluetooth headset
connecting, a USB DAC removed — and the two outcomes are very different:

* **Re-routed.** The stream survives and AAudio sends it to the new device. Nothing to
  do, and this is the common case; it is also why an implementation with no switching
  code at all can seem to work.
* **Disconnected.** The device the stream was routed to is gone. Every call then fails
  with `AAUDIO_ERROR_DISCONNECTED`, and per AAudio's own header the stream cannot be
  used again — it has to be closed and rebuilt, or the session is silent for good.

`src/backend/callback.rs` recreates it, on the render thread (the header requires a
disconnected stream to be closed somewhere other than the callback that reported it),
by the reference sink's sequence — **stop, close, forget, create, start** — with the
replacement created immediately rather than after a delay, and `src/reconnect.rs` holds
the policy: the four results that mean "rebuild", and a doubling 50 ms → 1 s backoff so
a device that stays gone costs a probe a second rather than a spin. A replacement is
opened **unpinned**, so it reaches whatever the system is routed to now rather than
asking for the device that just left.

Every create also re-reads the route rather than assuming the last one. That is not
tidiness: a stream is opened with the rate, the channel count and the buffer capacity
left **unspecified**, exactly as `pa_create_aaudio_stream` does, and the device's answers
to `getSampleRate`, `getChannelCount`, `getFramesPerBurst`, `getBufferCapacityInFrames`,
`getBufferSizeInFrames`, `getFormat` and `getDeviceId` are what the rest of the stack is
then told. Pinning the client's numbers on the builder does not make a route run at them
— it only means this crate never finds out what the route actually is.

### The route answers with its own geometry

A route's rate is its own. Bluetooth A2DP is usually 44.1 kHz, some DACs are 96 kHz, and
a device asked for 48 kHz does not start running at 48 kHz because it was asked — the
most it can do is answer that it is running at something else. So the ALSA PCM's rate and
the device's rate are two different numbers and `src/resample.rs` converts between them:
Catmull-Rom over four points, with the fractional phase carried from block to block so
there is no drift, and an identity fast path that copies when the two rates agree.

Nothing above the engine has to know. The client is asked for *its* frames at *its* rate,
the position advances in the client's timeline, and a device period is still a device
period. Before this, a replacement answering with a different rate was refused and
retried, which reads as correct in a comment and sounds like a tape at the wrong speed in
practice — 48 kHz played at 44.1 kHz is 8.8 % slow, low, and ever further behind.

The same applies to channels: a route with more channels than the client gets silence on
the extras, and a route with fewer gets the client's folded down (quad folds into stereo
two planes at a time, anything folds into mono) rather than an error.

Three things make a *rebuild* actually happen, and the second is the one that is easy to
miss.
All three are in the reference sink, `module-aaudio-sink.c`, which is why it works there:

* **The error callback wakes the render thread.** A disconnected stream delivers no data
  callbacks, so nothing else will ever ask the render thread for a period. A render loop
  that waits only for "a period is due" parks for ever, the fault is never read, and the
  rebuild never runs — silence, with every counter healthy. The predicate is set under the
  mutex the render thread waits on, then notified; setting it outside the lock leaves a
  window where the wakeup lands on nobody.
* **The stream's state is polled every period.** Some HALs end a stream when the route
  goes without calling back at all, and then the state is the only evidence. Anything but
  `STARTED`/`STARTING` is a stream that will not ask for audio, so the wait has a 40 ms
  timeout to make that checkable. A deliberate pause is excluded — that state is this
  crate's own doing.
* **A `requestStart` that cannot reach the device is a fault, not a failure.** The
  reference sink schedules a recreate and reports success, because being routed elsewhere
  is something that happens rather than a mistake by the caller. Same here, for both
  `start` and `resume` — the route may have changed while the stream was paused.

* **The device buffer grows when the device reports it lost audio.** The reference's rule
  from `sink_process_render`: on any change in `AAudioStream_getXRunCount`, raise the buffer
  by one burst, bounded by what the pipeline above allows and the route's capacity. It is
  the mechanism that adapts to a route nobody described in advance. Worth knowing: on one
  device measured here `getXRunCount` reported zero through hundreds of milliseconds of
  silence, so it can miss — the two-burst floor is what makes a route usable, and this is the
  top-up on top of it.

Two things follow from it that a caller can see:

* `Events::RESYNC` is raised, because the clock is not continuous across the swap, and
  `Events::DEVICE_LOST` when the fault was first noticed.
* `Backend::is_delivering` — and `Engine::is_delivering` — report false while a rebuild
  is pending. The ALSA plugin uses that to *drop* frames rather than block: the app
  keeps its own clock and loses a fraction of a second of audio, instead of its audio
  thread stalling for the length of the outage.

`GNAUDIO_FAULT_AFTER_MS=<n>` makes the render thread report a disconnect `n`
milliseconds after the stream starts. That is a diagnostic, not a feature: a real unplug
needs a device to unplug, and an unexercised rebuild path is not one to trust. Under it,
the log reads `injecting a disconnect`, then `callback: reconnected on device N …`, and
the stream that comes back is a genuinely new one — AudioFlinger shows a new player
instance — writing at real time again within a second.

## Nothing to tune

There is no configuration file. Every number that decides the shape of this pipeline has a
correct value derivable from the geometry libasound hands over at `hw_params`, and a file
that lets those be set by hand only lets them be set wrong.

* **The credit** — how many frames the app may have outstanding, which is `avail` as
  winealsa sees it — is two blocks, capped by the buffer size. `avail = buffer −
  (appl_ptr − hw_ptr)` is simultaneously the app's write credit and the most the ring can
  hold, so at 240 it made the ring 240 frames deep while a block is 480: every block was
  short by construction. The right value follows from the block size.
* **The ring** holds exactly that credit. Deeper is not safer: the app can never fill it,
  and libasound keeps `appl_ptr − hw_ptr` modulo the buffer size, so a deeper pipeline is
  a number it cannot represent.
* **The device buffer** is two bursts, grown a burst at a time when the device reports it
  lost audio, capped by what the app's buffer leaves over. The slack belongs in the ring.
* **The render wait** is half the app's period: enough for a late timer tick, short enough
  that it can never delay a period past its own length.

One property of the stream is a choice rather than a derivation, because the two answers are
different products rather than a right one and a wrong one: `GNAUDIO_LOW_LATENCY` (off unless
set to `1`, `true`, `on` or `yes`) asks for usage `AAUDIO_USAGE_GAME`, a device buffer of two
bursts and the growth rule above. Left off — the default — the stream opens with the platform's
default usage and the route's own buffer.

What remains configurable is the capture, and that is a diagnostic rather than a property of
the audio path: `GNAUDIO_DUMP_SECONDS` (0 disables) and `GNAUDIO_DUMP_PCM` (path).

## Build and test

```sh
cargo test                 # the engine's semantics, on any host
cargo fmt --check
scripts/build-android.sh   # the .so and the probe, into dist/arm64-v8a/
scripts/pack-alsa-asset.sh --check
```

Both scripts need only the NDK (`ANDROID_NDK_HOME`, or the newest under the SDK); they
use the NDK's clang as the linker, at API 26 — GameNative's own floor, and AAudio's
minimum. No `cargo-ndk`. They build arm64-v8a, the one ABI the app ships.

## On a device

The probe is built alongside the library, and runs from a shell on the device:

```sh
adb push dist/arm64-v8a/gn-audio-probe /data/local/tmp/
adb shell /data/local/tmp/gn-audio-probe
```

For each candidate block size it prints the granted sharing, performance mode,
burst, ring size and format, and then a second of real playback with its
dropouts, the engine's latency estimate, and how far the device's own play
position lags the frames handed over. That last number is the one to compare
exclusive against shared: it is the evidence for whether the mixer was bypassed.

## What it is not

* **Not a Windows-side driver.** There is no PE module, no registry key and no Wine
  loader path. A Windows host reaches the engine the way any Linux application does:
  through libasound, which loads `alsa-plugin/` in-process.
* **Not desktop-class latency.** Android's floor is the HAL's, and a shared-mode
  stream also pays for the AudioFlinger mixer. Exclusive sharing is the one lever
  that removes the mixer, and the device decides — but the mixer is also what absorbs
  a writer that arrives late, so asking for it away is a trade and not a free win.
* **Not input.** Capture is not implemented.
* **Not a mixer.** One client per device. The ALSA PCM advertises 48000 Hz only, so an
  *app* asking for another rate is refused by libasound's own constraints rather than
  quietly resampled, with `plug` offered in front so an app that insists gets a software
  resample where it can be seen. A *device* answering another rate is a different question
  and is converted, because a route's rate is not something either side gets to choose.

## License

LGPL-2.1-or-later; see `COPYING` and `NOTICE`.
