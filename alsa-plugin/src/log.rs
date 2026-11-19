//! One diagnostic channel for the plugin: logcat on Android, stderr anywhere else.
//!
//! The engine has its own channel under the tag `gnaudio`; this one uses
//! `gnaudio-alsa` so that a device pass can read the plugin's half of a session
//! (`adb logcat -s gnaudio gnaudio-alsa`) without the two being interleaved into
//! something unreadable. Nothing here is called from the render path — only from
//! the callbacks libasound makes when the app changes state.

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
        const TAG: &[u8] = b"gnaudio-alsa\0";
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
        let _ = writeln!(std::io::stderr().lock(), "gnaudio-alsa: {message}");
    }
}

pub(crate) fn log(message: core::fmt::Arguments<'_>) {
    sink::write(&format!("{message}"));
}

/// `trace!("…")` — one line, on the plugin's own channel.
macro_rules! trace {
    ($($arg:tt)*) => {
        $crate::log::log(format_args!($($arg)*))
    };
}

pub(crate) use trace;
