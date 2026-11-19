//! A 440 Hz tone written straight from an AAudio data callback, with none of this crate's
//! machinery in the way: no staging buffer, no handoff to another thread, no resampling.
//!
//! This exists to answer one question nothing else can. When a stream sounds broken while
//! every counter in the engine reads healthy, there are two possibilities needing opposite
//! fixes: the device path is at fault, or the layer between the client and the device is.
//! Ordinary apps are clean on the same route, so if this is clean too the fault is in this
//! crate's engine; if this is broken it is AAudio on that route, and the reference sink
//! would hit it as well.
//!
//!     cargo run --release --example aaudio-direct
//!
//! `GNAUDIO_DIRECT_SECONDS`, `GNAUDIO_DIRECT_FORMAT=i16`, `GNAUDIO_DIRECT_PERF=power`,
//! `GNAUDIO_DIRECT_HZ`.

#[cfg(not(target_os = "android"))]
fn main() {
    println!("aaudio-direct drives AAudio itself, so it only runs on Android");
}

#[cfg(target_os = "android")]
fn main() {
    android::run();
}

#[cfg(target_os = "android")]
mod android {
    use core::ffi::{c_int, c_void};

    type AaudioStreamBuilder = c_void;
    type AaudioStream = c_void;
    type DataCallback = extern "C" fn(*mut AaudioStream, *mut c_void, *mut c_void, c_int) -> c_int;

    const AAUDIO_OK: c_int = 0;
    const AAUDIO_UNSPECIFIED: c_int = 0;
    const AAUDIO_DIRECTION_OUTPUT: c_int = 0;
    const AAUDIO_SHARING_MODE_SHARED: c_int = 1;
    const AAUDIO_PERFORMANCE_MODE_POWER_SAVING: c_int = 10;
    const AAUDIO_PERFORMANCE_MODE_LOW_LATENCY: c_int = 12;
    const AAUDIO_FORMAT_PCM_I16: c_int = 1;
    const AAUDIO_FORMAT_PCM_FLOAT: c_int = 5;
    const CALLBACK_CONTINUE: c_int = 1;
    const CLOCK_MONOTONIC: c_int = 1;

    #[link(name = "aaudio")]
    extern "C" {
        fn AAudio_createStreamBuilder(builder: *mut *mut AaudioStreamBuilder) -> c_int;
        fn AAudioStreamBuilder_setDirection(builder: *mut AaudioStreamBuilder, direction: c_int);
        fn AAudioStreamBuilder_setPerformanceMode(builder: *mut AaudioStreamBuilder, mode: c_int);
        fn AAudioStreamBuilder_setSharingMode(builder: *mut AaudioStreamBuilder, mode: c_int);
        fn AAudioStreamBuilder_setFormat(builder: *mut AaudioStreamBuilder, format: c_int);
        fn AAudioStreamBuilder_setSampleRate(builder: *mut AaudioStreamBuilder, rate: c_int);
        fn AAudioStreamBuilder_setChannelCount(builder: *mut AaudioStreamBuilder, channels: c_int);
        fn AAudioStreamBuilder_setDataCallback(
            builder: *mut AaudioStreamBuilder,
            callback: Option<DataCallback>,
            user: *mut c_void,
        );
        fn AAudioStreamBuilder_openStream(
            builder: *mut AaudioStreamBuilder,
            stream: *mut *mut AaudioStream,
        ) -> c_int;
        fn AAudioStreamBuilder_delete(builder: *mut AaudioStreamBuilder) -> c_int;
        fn AAudioStream_getSampleRate(stream: *mut AaudioStream) -> c_int;
        fn AAudioStream_getChannelCount(stream: *mut AaudioStream) -> c_int;
        fn AAudioStream_getFramesPerBurst(stream: *mut AaudioStream) -> c_int;
        fn AAudioStream_getBufferCapacityInFrames(stream: *mut AaudioStream) -> c_int;
        fn AAudioStream_getBufferSizeInFrames(stream: *mut AaudioStream) -> c_int;
        fn AAudioStream_getFormat(stream: *mut AaudioStream) -> c_int;
        fn AAudioStream_getTimestamp(
            stream: *mut AaudioStream,
            clock: c_int,
            frame_position: *mut i64,
            time_ns: *mut i64,
        ) -> c_int;
        fn AAudioStream_getXRunCount(stream: *mut AaudioStream) -> i32;
        fn AAudioStream_requestStart(stream: *mut AaudioStream) -> c_int;
        fn AAudioStream_requestStop(stream: *mut AaudioStream) -> c_int;
        fn AAudioStream_close(stream: *mut AaudioStream) -> c_int;
    }

    /// Everything the callback needs. Allocated before the stream opens — the builder needs the
    /// pointer then — and filled in from the device's answer before the stream starts, which is
    /// the whole reason it is mutable rather than built from the granted values.
    struct Tone {
        phase: f64,
        step: f64,
        channels: usize,
        format: c_int,
        periods: u64,
    }

    extern "C" fn data_callback(
        _stream: *mut AaudioStream,
        user: *mut c_void,
        audio_data: *mut c_void,
        num_frames: c_int,
    ) -> c_int {
        if user.is_null() || audio_data.is_null() || num_frames <= 0 {
            return CALLBACK_CONTINUE;
        }
        // SAFETY: `user` is the `Tone` this example keeps alive for the stream's lifetime, and
        // AAudio calls this from exactly one thread at a time.
        let tone = unsafe { &mut *(user as *mut Tone) };
        let frames = num_frames as usize;
        tone.periods += 1;
        for frame in 0..frames {
            let value = (tone.phase * std::f64::consts::TAU).sin();
            tone.phase += tone.step;
            if tone.phase >= 1.0 {
                tone.phase -= 1.0;
            }
            for channel in 0..tone.channels {
                let index = frame * tone.channels + channel;
                if tone.format == AAUDIO_FORMAT_PCM_I16 {
                    let sample = (value * 0.35 * 32_767.0) as i16;
                    // SAFETY: the device guarantees `frames × channels` samples of room.
                    unsafe {
                        core::ptr::write_unaligned((audio_data as *mut i16).add(index), sample)
                    };
                } else {
                    // SAFETY: as above, in f32 units.
                    unsafe {
                        core::ptr::write_unaligned(
                            (audio_data as *mut f32).add(index),
                            (value * 0.35) as f32,
                        )
                    };
                }
            }
        }
        CALLBACK_CONTINUE
    }

    fn env<T: std::str::FromStr>(key: &str) -> Option<T> {
        std::env::var(key).ok().and_then(|value| value.parse().ok())
    }

    pub(crate) fn run() {
        let seconds: u64 = env("GNAUDIO_DIRECT_SECONDS").unwrap_or(10);
        let format = match std::env::var("GNAUDIO_DIRECT_FORMAT").as_deref() {
            Ok("i16") => AAUDIO_FORMAT_PCM_I16,
            _ => AAUDIO_FORMAT_PCM_FLOAT,
        };
        let performance = match std::env::var("GNAUDIO_DIRECT_PERF").as_deref() {
            Ok("power") => AAUDIO_PERFORMANCE_MODE_POWER_SAVING,
            _ => AAUDIO_PERFORMANCE_MODE_LOW_LATENCY,
        };
        let rate: c_int = env("GNAUDIO_DIRECT_HZ").unwrap_or(AAUDIO_UNSPECIFIED);

        let tone = Box::into_raw(Box::new(Tone {
            phase: 0.0,
            step: 0.0,
            channels: 1,
            format,
            periods: 0,
        }));

        let mut builder: *mut AaudioStreamBuilder = core::ptr::null_mut();
        // SAFETY: a plain out-parameter.
        if unsafe { AAudio_createStreamBuilder(&mut builder) } != AAUDIO_OK || builder.is_null() {
            eprintln!("no stream builder");
            std::process::exit(2);
        }
        // SAFETY: every call takes the builder created above, and the tone outlives the stream.
        unsafe {
            AAudioStreamBuilder_setDirection(builder, AAUDIO_DIRECTION_OUTPUT);
            AAudioStreamBuilder_setSharingMode(builder, AAUDIO_SHARING_MODE_SHARED);
            AAudioStreamBuilder_setPerformanceMode(builder, performance);
            AAudioStreamBuilder_setFormat(builder, format);
            AAudioStreamBuilder_setSampleRate(builder, rate);
            AAudioStreamBuilder_setChannelCount(builder, 2);
            AAudioStreamBuilder_setDataCallback(builder, Some(data_callback), tone as *mut c_void);
        }

        let mut stream: *mut AaudioStream = core::ptr::null_mut();
        // SAFETY: as above.
        let opened = unsafe { AAudioStreamBuilder_openStream(builder, &mut stream) };
        // SAFETY: ours, and not needed again.
        unsafe { AAudioStreamBuilder_delete(builder) };
        if opened != AAUDIO_OK || stream.is_null() {
            eprintln!("the device refused a stream: {opened}");
            std::process::exit(2);
        }

        // SAFETY: the stream is open and nothing has started it.
        let (granted_rate, channels, burst, capacity, buffer, granted_format) = unsafe {
            (
                AAudioStream_getSampleRate(stream),
                AAudioStream_getChannelCount(stream),
                AAudioStream_getFramesPerBurst(stream),
                AAudioStream_getBufferCapacityInFrames(stream),
                AAudioStream_getBufferSizeInFrames(stream),
                AAudioStream_getFormat(stream),
            )
        };
        println!(
            "aaudio: {granted_rate} Hz, {channels} ch, burst {burst}, capacity {capacity}, \
         buffer {buffer}, format {granted_format}, performance {performance}"
        );
        // SAFETY: the stream has not been started, so nothing is reading this yet.
        unsafe {
            (*tone).channels = channels.max(1) as usize;
            (*tone).format = granted_format;
            (*tone).step = 440.0 / granted_rate.max(1) as f64;
        }

        // SAFETY: the stream is open and described; this is what begins the callbacks.
        unsafe { AAudioStream_requestStart(stream) };
        std::thread::sleep(std::time::Duration::from_millis(400));

        let mut first_position: i64 = 0;
        let mut first_time: i64 = 0;
        // SAFETY: both are live out-parameters.
        let first = unsafe {
            AAudioStream_getTimestamp(
                stream,
                CLOCK_MONOTONIC,
                &mut first_position,
                &mut first_time,
            )
        };
        let started = std::time::Instant::now();
        std::thread::sleep(std::time::Duration::from_secs(seconds));
        let elapsed = started.elapsed().as_nanos() as u64;
        let mut last_position: i64 = 0;
        let mut last_time: i64 = 0;
        // SAFETY: as above.
        let last = unsafe {
            AAudioStream_getTimestamp(stream, CLOCK_MONOTONIC, &mut last_position, &mut last_time)
        };
        // SAFETY: the stream is open.
        let xruns = unsafe { AAudioStream_getXRunCount(stream) };
        let periods = unsafe { (*tone).periods };

        // SAFETY: the documented order for a stream nobody else holds.
        unsafe {
            AAudioStream_requestStop(stream);
            AAudioStream_close(stream);
        }
        // SAFETY: the stream is closed, so nothing can call into it any more.
        unsafe { drop(Box::from_raw(tone)) };

        if first != AAUDIO_OK || last != AAUDIO_OK {
            // A stream can play perfectly and simply not publish a position — a MIXER track
            // with no fast path does exactly that — so this is reported, not taken as
            // silence.
            println!(
                "no position from the device ({} callbacks served, {} xruns)",
                periods, xruns
            );
            return;
        }
        let played = last_position.saturating_sub(first_position) as u64;
        let device_ns = last_time.saturating_sub(first_time).max(1) as u64;
        println!(
            "played {} frames in {:.2} s: {}/s by the device's own clock, {}/s by ours; \
         {} xruns over {} callbacks",
            played,
            device_ns as f64 / 1e9,
            played.saturating_mul(1_000_000_000) / device_ns,
            played.saturating_mul(1_000_000_000) / elapsed.max(1),
            xruns,
            periods
        );
    }
} // mod android
