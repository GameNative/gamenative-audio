//! Declares the C shim as a build input.
//!
//! Cargo has no idea that `shim/gamenative.c` exists: the object file reaches the
//! link through a `-C link-arg` that `build.sh` passes, which Cargo cannot see. Without
//! this file, editing the shim and re-running the build produces a **stale** plugin —
//! Cargo's fingerprint says nothing changed, so it skips the link and the old `.so` is
//! copied to `dist/` as if it were new. That is exactly how three consecutive fixes to
//! the pointer callback appeared to change nothing: the timings were identical to the
//! millisecond because the binary being tested predated all of them.
//!
//! `build.sh` compiles the object before invoking Cargo, so declaring the object's own
//! path here makes a recompile of the shim register as a change to this crate.

fn main() {
    for input in [
        "shim/gamenative.c",
        "shim/libasound-stub.c",
        "build/gamenative.o",
        "prebuilt/libasound.so.2",
    ] {
        println!("cargo:rerun-if-changed={input}");
    }
}
