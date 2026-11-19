//! The two symbols libasound looks up when an app opens this PCM.
//!
//! `snd_pcm_open` does not call a plugin by a name anyone chose; `pcm_external.h`
//! pastes it together out of the plugin's name, and asks for
//!
//! ```text
//!     _snd_pcm_gamenative_open          (SND_PCM_PLUGIN_ENTRY)
//!     __snd_pcm_gamenative_open_dlsym_pcm_001   (SND_PCM_PLUGIN_SYMBOL)
//! ```
//!
//! Both are defined here rather than in `shim/gamenative.c` because rustc links a
//! `cdylib` with a linker version script that exports only what the Rust crate
//! exports — a symbol a companion `.o` defines ends up *local*, still present in
//! `.symtab` and invisible to `dlsym`, which is a failure mode that looks exactly
//! like a plugin that is not installed. Defining the entry point here keeps it in
//! `.dynsym`, and the shim keeps the ALSA-typed work (`gamenative_open_entry`).
//!
//! The versioned symbol exists so that libasound's own version probe finds something
//! to compare against; its value is never read as a function, which is why it is a
//! byte.

use core::ffi::{c_char, c_int, c_void};

extern "C" {
    /// `gamenative_open_entry` in `shim/gamenative.c`: the ioplug create path, with
    /// ALSA's own types on its side of the boundary.
    fn gamenative_open_entry(
        pcm: *mut *mut c_void,
        name: *const c_char,
        root: *mut c_void,
        conf: *mut c_void,
        stream: c_int,
        mode: c_int,
    ) -> c_int;
}

/// `SND_PCM_PLUGIN_ENTRY(gamenative)` — what libasound calls.
///
/// # Safety
/// Called by libasound with the handles it owns: `pcm` is where the opened PCM must
/// be stored, `name` is a NUL-terminated string that outlives the call, and `root`
/// and `conf` are config nodes. None of them is interpreted here; they are passed
/// straight to the shim.
#[no_mangle]
pub unsafe extern "C" fn _snd_pcm_gamenative_open(
    pcm: *mut *mut c_void,
    name: *const c_char,
    root: *mut c_void,
    conf: *mut c_void,
    stream: c_int,
    mode: c_int,
) -> c_int {
    // SAFETY: the arguments are libasound's own, forwarded unchanged.
    unsafe { gamenative_open_entry(pcm, name, root, conf, stream, mode) }
}

/// `SND_PCM_PLUGIN_SYMBOL(gamenative)` — the version tag libasound may look for.
///
/// A plain byte, exported under the exact mangled name the macro would have built.
#[no_mangle]
pub static __snd_pcm_gamenative_open_dlsym_pcm_001: u8 = 0;
