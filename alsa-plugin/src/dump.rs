//! Capturing what the app hands over, so a complaint that a stream crackles can be
//! measured instead of inferred.
//!
//! Every counter in this plugin describes what happened to the frames *after* they
//! arrived: how deep the ring ran, whether the device reported an xrun, how many blocks
//! were late. None of them can answer the only question that matters when a listener
//! hears damage — whether the damage was already in the audio before this plugin touched
//! it. PulseAudio answering that question is why `parec` exists.
//!
//! So the frames the caller writes are copied to a file, as raw little-endian `f32` in
//! the caller's own layout, and analysed afterwards where the thresholds can be chosen
//! with the signal in hand rather than guessed in advance.
//!
//! Written from the caller's thread — winealsa's timer thread — and deliberately not from
//! the engine's pull path, which runs on the thread that feeds the device: file I/O there
//! would add work to exactly the path whose lateness is under investigation, which is the
//! one way a diagnostic can invent the fault it was built to find.
//!
//! **Off unless `GNAUDIO_DUMP_SECONDS` asks for it**, and then bounded to that many
//! seconds of a stream, written beside the image. `GNAUDIO_DUMP_PCM` overrides the path.
//!
//! Off by default because a capture is not free and nothing ever removed one: every
//! stream open wrote up to 8.4 MB into the app's data directory, named per pid, and a
//! device that had been used for a few sessions held 66 files and 102 MB of PCM. It also
//! perturbs what it measures — the caller's thread does the writing — so a build that
//! ships with it on is a build whose own numbers are taken with a diagnostic running.
//! A capture is a thing to ask for, in the session under investigation, and that is
//! exactly the shape of a variable that defaults to off.
//!
//! The path is derived from `ALSA_CONFIG_PATH`, which the launcher already sets and which
//! names `<data-dir>/imagefs/usr/share/alsa/alsa.conf`, so the file lands in the app's own
//! writable data directory without this crate needing to know a package name.
//!
//! The budget starts at the first sound, not at the open. A stream opened at 22:14:52 and
//! did not carry audio until after 22:15:36, so a capture measured from the open spent all
//! twenty seconds on a loading screen and had nothing left for the music — a file of pure
//! digital silence that analysis then reported as clean. Up to two seconds of pre-roll is
//! kept so the transition into sound is in the capture; the twenty seconds of budget are
//! spent from the first non-silent frame onwards.

use core::sync::atomic::{AtomicU32, Ordering};
use std::fs::File;
use std::io::{BufWriter, Write};
use std::sync::Mutex;

/// Frames per second assumed for the size bound. The rate is pinned to the device's, so
/// this only decides how much file a capture is allowed to write.
const ASSUMED_RATE: u64 = 48_000;

/// Seconds of silence kept before the first sound, so the capture holds the transition
/// into audio rather than starting mid-phrase.
const PREROLL_SECONDS: u64 = 2;

/// What counts as sound rather than the tail of a fade: -80 dBFS.
const AUDIBLE: f32 = 1.0e-4;

/// Which capture this is, so several PCMs in one process do not overwrite each other.
static INSTANCE: AtomicU32 = AtomicU32::new(0);

/// How many seconds to capture, from the variable's own text.
///
/// Off when nothing was asked for, and off when what was asked for cannot be read: a
/// capture is a diagnostic, and a diagnostic that turns itself on because a variable was
/// misspelled is worse than one that stays quiet.
fn seconds_from(value: Option<&str>) -> u64 {
    value
        .and_then(|value| value.trim().parse::<u64>().ok())
        .unwrap_or(0)
}

/// Where the capture goes when nobody said: the app's own data directory, derived from the
/// ALSA configuration path the launcher already sets.
///
/// `ALSA_CONFIG_PATH` is `<data-dir>/imagefs/usr/share/alsa/alsa.conf`, so four levels up
/// from the file is `<data-dir>` — writable by the process that is running this, and not
/// something this crate has to know the name of.
///
/// The name carries the process and a per-process counter, because one process opens
/// several PCMs — Wine probes formats before it plays — and a single path meant every one
/// of them truncated the last: a twenty second capture that was entirely digital silence,
/// from a stream that had never carried audio, while the stream that had was overwritten.
fn derive_path() -> Option<String> {
    let data = data_dir()?;
    let instance = INSTANCE.fetch_add(1, Ordering::Relaxed);
    Some(
        data.join(format!("gn-pcm-{}-{instance}.raw", std::process::id()))
            .to_string_lossy()
            .into_owned(),
    )
}

/// The app's own data directory, or `None` when the launcher gave us no configuration
/// path to derive it from.
///
/// The one directory this process can be sure it may write to, derived rather than
/// guessed so that a different package name or container root needs no change here.
pub(crate) fn data_dir() -> Option<std::path::PathBuf> {
    let config = std::env::var("ALSA_CONFIG_PATH").ok()?;
    let first = config.split(':').next()?;
    let alsa_dir = std::path::Path::new(first).parent()?;
    let share = alsa_dir.parent()?;
    let usr = share.parent()?;
    let imagefs = usr.parent()?;
    Some(imagefs.parent()?.to_path_buf())
}

/// Whether these frames contain anything a listener would call sound.
fn is_audible(samples: &[f32]) -> bool {
    samples.iter().any(|value| value.abs() >= AUDIBLE)
}

pub(crate) struct Dump {
    sink: Mutex<Sink>,
}

struct Sink {
    writer: Option<BufWriter<File>>,
    /// Frames left before the capture stops itself. Not spent until there is audio.
    remaining: u64,
    /// Whether audio has been seen, which is what starts the budget.
    armed: bool,
    /// The full budget, restored when the first sound arrives.
    budget: u64,
    channels: usize,
    frames: u64,
    path: String,
    /// The loudest sample and the sum of squares, so a capture that contains no audio is
    /// never reported as a clean one: a peak of zero is the difference between "nothing
    /// was wrong" and "nothing was measured".
    peak: f32,
    power: f64,
}

impl Sink {
    fn rms(&self) -> f64 {
        if self.frames == 0 {
            0.0
        } else {
            (self.power / (self.frames as f64 * self.channels.max(1) as f64)).sqrt()
        }
    }
}

impl Dump {
    fn disabled() -> Self {
        Self {
            sink: Mutex::new(Sink {
                writer: None,
                remaining: 0,
                armed: false,
                budget: 0,
                channels: 0,
                frames: 0,
                path: String::new(),
                peak: 0.0,
                power: 0.0,
            }),
        }
    }

    pub(crate) fn new() -> Self {
        // How long to capture is a diagnostic, not a property of the pipeline, so it is an
        // environment variable and not a tuning file: nothing about the audio path should
        // be settable from a file on the device.
        let seconds = seconds_from(std::env::var("GNAUDIO_DUMP_SECONDS").ok().as_deref());
        if seconds == 0 {
            return Self::disabled();
        }
        let path = match std::env::var("GNAUDIO_DUMP_PCM")
            .ok()
            .filter(|p| !p.is_empty())
        {
            Some(path) => path,
            None => match derive_path() {
                Some(path) => path,
                None => return Self::disabled(),
            },
        };
        let writer = File::create(&path).ok().map(BufWriter::new);
        if writer.is_none() {
            crate::log::trace!("dump: cannot write {path}");
        } else {
            // One line per PCM, and Wine opens twenty of them, so this says the bare
            // minimum: the file exists and where. The interesting line is the close.
            crate::log::trace!("dump: {path} up to {seconds} s");
        }
        Self {
            sink: Mutex::new(Sink {
                writer,
                remaining: PREROLL_SECONDS * ASSUMED_RATE,
                armed: false,
                budget: seconds.saturating_mul(ASSUMED_RATE),
                channels: 0,
                frames: 0,
                path,
                peak: 0.0,
                power: 0.0,
            }),
        }
    }

    /// Copies `samples` (interleaved, `channels` wide) into the capture.
    ///
    /// Called on the caller's own thread inside a write it was already making, and only
    /// does work when the variable was set.
    pub(crate) fn write(&self, samples: &[f32], channels: usize) {
        if channels == 0 {
            return;
        }
        let mut sink = match self.sink.lock() {
            Ok(sink) => sink,
            Err(poisoned) => poisoned.into_inner(),
        };
        if sink.writer.is_none() {
            return;
        }
        // The first sound is what starts the budget: a stream spends its opening seconds on
        // a loading screen, and a capture measured from the open was twenty seconds of
        // digital silence that analysis then called clean.
        //
        // Checked before the budget is consulted, because the budget is only a few seconds
        // of pre-roll and running out of it must not be what stops this from ever noticing
        // that the music started — which is exactly how the first attempt failed.
        if !sink.armed && is_audible(samples) {
            sink.armed = true;
            sink.remaining = sink.budget;
            crate::log::trace!(
                "dump: sound has started after {} frames of silence, budgetting {} s from here",
                sink.frames,
                sink.budget / ASSUMED_RATE
            );
        }
        if sink.remaining == 0 {
            return;
        }
        let frames = (samples.len() / channels) as u64;
        let taking = frames.min(sink.remaining) as usize;
        let bytes = taking * channels;
        // Little-endian `f32`, which is `array('f')` in anything that reads it later.
        let raw = unsafe {
            core::slice::from_raw_parts(
                samples.as_ptr() as *const u8,
                bytes * core::mem::size_of::<f32>(),
            )
        };
        let failed = match sink.writer.as_mut() {
            Some(writer) => writer.write_all(raw).is_err(),
            None => false,
        };
        if failed {
            sink.writer = None;
            crate::log::trace!("dump: write failed");
            return;
        }
        for value in &samples[..bytes] {
            let level = value.abs();
            if level > sink.peak {
                sink.peak = level;
            }
            sink.power += (level as f64) * (level as f64);
        }
        sink.channels = channels;
        sink.remaining -= taking as u64;
        sink.frames += taking as u64;
    }

    /// Flushes and reports what was captured. Called once, when the PCM is released.
    pub(crate) fn finish(&self) {
        let mut sink = match self.sink.lock() {
            Ok(sink) => sink,
            Err(poisoned) => poisoned.into_inner(),
        };
        let Some(mut writer) = sink.writer.take() else {
            return;
        };
        let _ = writer.flush();
        drop(writer);
        crate::log::trace!(
            "dump: wrote {} frames ({} ch, peak {:.4}, rms {:.4}) to {} — about {:.1} s",
            sink.frames,
            sink.channels,
            sink.peak,
            sink.rms(),
            sink.path,
            sink.frames as f64 / ASSUMED_RATE as f64
        );
    }
}

#[cfg(test)]
mod tests {
    use super::{derive_path, is_audible, seconds_from};

    #[test]
    fn the_capture_is_off_unless_asked_for() {
        // The default. An unset variable, an empty one, and one that does not parse all
        // mean the same thing, and none of them may mean "capture 20 seconds": every
        // stream open wrote up to 8.4 MB into the app's data directory and nothing
        // removed it, which left 66 files and 102 MB on one device.
        assert_eq!(seconds_from(None), 0);
        assert_eq!(seconds_from(Some("")), 0);
        assert_eq!(seconds_from(Some("  ")), 0);
        assert_eq!(seconds_from(Some("yes")), 0);
        assert_eq!(seconds_from(Some("-5")), 0);
        // And an explicit request is honoured, from text a caller might actually write.
        assert_eq!(seconds_from(Some("20")), 20);
        assert_eq!(seconds_from(Some(" 5 ")), 5);
    }

    #[test]
    fn the_budget_does_not_start_until_there_is_sound() {
        // A stream spends its opening seconds on a loading screen, and a capture measured
        // from the open was twenty seconds of digital silence that analysis then called
        // clean. Silence is not sound, and a fade tail is not either.
        assert!(!is_audible(&[0.0; 480]));
        assert!(!is_audible(&[1.0e-5, -1.0e-5]));
        assert!(is_audible(&[0.0, 0.0, 0.2, 0.0]));
    }

    #[test]
    fn the_capture_lands_beside_the_image_and_only_there() {
        // One test, not two: these share a process-wide variable, and tests run in
        // parallel — the pair raced and the second one read the first one's value.
        // SAFETY: the only test that touches this variable.
        unsafe {
            std::env::set_var(
                "ALSA_CONFIG_PATH",
                "/data/user/0/app/files/imagefs/usr/share/alsa/alsa.conf",
            )
        };
        let path = derive_path().expect("a path");
        assert!(
            path.starts_with("/data/user/0/app/files/gn-pcm-"),
            "the capture must land beside the image, not inside it: {path}"
        );
        assert!(path.ends_with(".raw"), "{path}");

        // Without the launcher's own variable there is no path to derive, which is the
        // honest answer: this crate has no business guessing a package name.
        // SAFETY: as above.
        unsafe { std::env::remove_var("ALSA_CONFIG_PATH") };
        assert_eq!(derive_path(), None);
    }
}
