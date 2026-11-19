//! GameNative Audio, reached as an ordinary ALSA PCM.
//!
//! This crate is `libasound_module_pcm_gamenative.so`: a libasound ioplug plugin
//! that takes the PCM an app writes and plays it through the engine
//! ([`gnaudio`]) on AAudio. It exists so that a Wine/Proton container needs no
//! PulseAudio daemon, no socket, no helper process and no Wine-side driver: one
//! shared object in the image's `alsa-lib` directory, and a `pcm.!default` pointing
//! at it, is the whole install.
//!
//! The split in this crate:
//!
//! * [`ring`] — the buffer between the app's writes and the device's clock.
//! * [`pcm`] — the state, the format conversion, and the C surface the shim calls.
//! * `shim/gamenative.c` — the ALSA-facing glue: the ioplug struct, the callbacks,
//!   the hw constraints, and the export macros whose symbol names libasound's own
//!   headers build out of the plugin name.
//!
//! Nothing here knows about Wine. That is the point: the same plugin serves every
//! Proton tree in the container, because it is loaded by alsa-lib rather than by a
//! Wine loader path.

mod dump;
pub mod entry;
mod gap;
mod log;
mod pcm;
mod ring;

pub use pcm::{
    gnaudio_alsa_create, gnaudio_alsa_destroy, gnaudio_alsa_drain, gnaudio_alsa_frame_bytes,
    gnaudio_alsa_hw_free, gnaudio_alsa_hw_params, gnaudio_alsa_is_open, gnaudio_alsa_pause,
    gnaudio_alsa_position, gnaudio_alsa_prepare, gnaudio_alsa_start, gnaudio_alsa_stop,
    gnaudio_alsa_transfer,
};
