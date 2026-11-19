//! Diagnostics: what the engine did, where a device pass can read it.
//!
//! On Android this is logcat under the tag `gnaudio`, which is where a developer
//! already looks; everywhere else it goes to stderr. It is **on by default** and
//! deliberately cheap: nothing here is called from the real-time path, so a
//! session costs a handful of lines — the device's answer at open, each transport
//! change, and at most one line per second when audio is being dropped. Use
//! [`set_logging`] to silence it for a shipping build.
//!
//! Nothing in this module may be called from [`crate::Client::render`]: it formats
//! and it may cross into liblog, neither of which belongs on a real-time thread.

use core::sync::atomic::{AtomicBool, Ordering};

static LOGGING: AtomicBool = AtomicBool::new(true);

/// Turns the engine's own diagnostics on or off. On by default.
pub fn set_logging(enabled: bool) {
    LOGGING.store(enabled, Ordering::Relaxed);
}

pub fn logging() -> bool {
    LOGGING.load(Ordering::Relaxed)
}

#[cfg(target_os = "android")]
mod sink {
    /// `ANDROID_LOG_INFO`.
    const ANDROID_LOG_INFO: i32 = 4;

    #[link(name = "log")]
    extern "C" {
        fn __android_log_write(prio: i32, tag: *const u8, text: *const u8) -> i32;
    }

    pub fn write(message: &str) {
        let mut text = Vec::with_capacity(message.len() + 1);
        text.extend_from_slice(message.as_bytes());
        text.push(0);
        /// NUL-terminated, because liblog wants a C string.
        const TAG: &[u8] = b"gnaudio\0";
        // SAFETY: both pointers are NUL-terminated for the duration of the call.
        unsafe {
            __android_log_write(ANDROID_LOG_INFO, TAG.as_ptr(), text.as_ptr());
        }
    }
}

#[cfg(not(target_os = "android"))]
mod sink {
    use std::io::Write;

    pub fn write(message: &str) {
        let _ = writeln!(std::io::stderr().lock(), "gnaudio: {message}");
    }
}

/// Formats and emits one line, when logging is on.
pub fn log(message: core::fmt::Arguments<'_>) {
    if !logging() {
        return;
    }
    sink::write(&format!("{message}"));
}

/// `trace!("…")` — one diagnostic line, on by default.
#[macro_export]
macro_rules! trace {
    ($($arg:tt)*) => {
        $crate::trace::log(format_args!($($arg)*))
    };
}
