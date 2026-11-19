//! The PCM's own wakeup source: the file descriptor libasound's `snd_pcm_wait`
//! polls when an app's write cannot proceed yet.
//!
//! Why this exists at all. `snd_pcm_writei` in blocking mode, when the app offers
//! more frames than `avail`, does not return a short count — it waits for room and
//! retries. Inside an ioplug that wait goes through `snd_pcm_wait_nocheck`, which
//! polls whatever the plugin's poll descriptors name. A plugin that provides no
//! `poll_descriptors`/`poll_revents` callbacks leaves libasound on the PCM's
//! `poll_fd`, and the shim `calloc`s its `snd_pcm_ioplug_t`, so that field is
//! **zero**: descriptor 0, the process's own stdin.
//!
//! Polling stdin as though it were an audio wakeup source is not a harmless
//! oddity. `snd_pcm_wait_nocheck` asks for `poll_events` (zero here) plus
//! `POLLERR`/`POLLNVAL`, and only leaves its loop on `POLLIN` or `POLLOUT` —
//! `POLLHUP` is read and ignored. A wine process's stdin is a pipe whose writer is
//! gone, `poll()` reports `POLLHUP` on such a pipe immediately and every time, and
//! the loop therefore re-polls as fast as the kernel will answer it: measured on
//! an ioplug of exactly this shape, **98 % of one core for as long as the process
//! lived**, and the app's audio thread never came back out of `writei`. With
//! stdin at `/dev/null` the same path instead sleeps out libasound's own
//! one-period-plus-200 ms timeout on every write, which is a different failure
//! and not a smaller one.
//!
//! So the PCM owns a descriptor of its own, and the consumer signals it. What the
//! signal means is precise: **room appeared**, because the only thing that frees
//! room is the ring being pulled by the engine. One signal per pull is what makes
//! `POLLOUT` truthful — reporting it while `avail` is still zero would put
//! libasound straight back into a retry loop, which is the state this file exists
//! to make impossible.

use core::ffi::{c_int, c_void};

/// `EFD_NONBLOCK | EFD_CLOEXEC`. These are the `O_*` values from `asm-generic`,
/// which is what bionic and x86-64 Linux both use: `O_NONBLOCK` is 0o4000 and
/// `O_CLOEXEC` is 0o2000000. Non-blocking because a read here follows a poll and
/// must never be the thing that parks the audio thread; close-on-exec so the
/// descriptor cannot leak into a game's own children.
const EVENTFD_FLAGS: c_int = 0o4000 | 0o2000000;

extern "C" {
    fn eventfd(initval: u32, flags: c_int) -> c_int;
    fn read(fd: c_int, buffer: *mut c_void, count: usize) -> isize;
    fn write(fd: c_int, buffer: *const c_void, count: usize) -> isize;
    fn close(fd: c_int) -> c_int;
}

/// One wakeup channel: an `eventfd` the consumer signals and the waiter drains.
pub(crate) struct Wake {
    fd: c_int,
}

impl Wake {
    /// Creates the channel, or a wake with no descriptor when the platform will not
    /// give one.
    ///
    /// A missing descriptor is not a reason to fail the open. Without it the shim
    /// reports no poll descriptors at all, and libasound answers `snd_pcm_wait` with
    /// `-EIO` — a short write the app retries on its own schedule. That is degraded
    /// but bounded, which is the property that matters: the failure this replaces was
    /// a thread that never returned.
    pub(crate) fn new() -> Self {
        // SAFETY: plain scalar arguments, and the return value is a descriptor or -1.
        let fd = unsafe { eventfd(0, EVENTFD_FLAGS) };
        Self { fd }
    }

    /// The descriptor to poll, or -1 when there is none.
    pub(crate) fn fd(&self) -> c_int {
        self.fd
    }

    /// Says that room has appeared. Called from the consumer, once per pull.
    ///
    /// Best-effort by design: a counter that could not be raised means a waiter
    /// sleeps until the next pull rather than that anything breaks, and the pull is
    /// what a waiter is waiting for anyway.
    pub(crate) fn signal(&self) {
        if self.fd < 0 {
            return;
        }
        let one: u64 = 1;
        // SAFETY: an eight-byte value for an eight-byte eventfd, and the return value
        // is only an error code — a full counter already means "signalled".
        unsafe {
            write(self.fd, &one as *const u64 as *const c_void, 8);
        }
    }

    /// Whether the channel had been signalled, clearing it.
    ///
    /// The whole history since the last call collapses into one `true`: the question
    /// being answered is "has room appeared since you last looked", not "how many
    /// times", and one wakeup is enough to make the waiter re-read its own position.
    pub(crate) fn fired(&self) -> bool {
        if self.fd < 0 {
            return false;
        }
        let mut value: u64 = 0;
        // SAFETY: an eight-byte out-parameter for an eight-byte eventfd, non-blocking,
        // so an empty channel returns `EAGAIN` rather than parking this thread.
        let read_bytes = unsafe {
            read(
                self.fd,
                &mut value as *mut u64 as *mut c_void,
                core::mem::size_of::<u64>(),
            )
        };
        read_bytes == core::mem::size_of::<u64>() as isize && value > 0
    }
}

impl Drop for Wake {
    fn drop(&mut self) {
        if self.fd >= 0 {
            // SAFETY: the descriptor was created by `eventfd` above and is closed once.
            unsafe {
                close(self.fd);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_fresh_channel_has_a_descriptor_and_nothing_pending() {
        let wake = Wake::new();
        assert!(wake.fd() >= 0, "an eventfd is what makes waiting possible");
        assert!(
            !wake.fired(),
            "an unsignalled channel has nothing to report"
        );
    }

    #[test]
    fn a_signal_is_reported_once_and_then_cleared() {
        let wake = Wake::new();
        wake.signal();
        assert!(wake.fired(), "a signalled channel must wake the waiter");
        assert!(
            !wake.fired(),
            "and must not wake it again for the same pull"
        );
    }

    #[test]
    fn many_signals_collapse_into_one_wakeup() {
        // The waiter re-reads its own position after waking, so the count is not
        // information it can act on — and coalescing is what keeps a burst of pulls
        // from turning into a burst of wakeups.
        let wake = Wake::new();
        for _ in 0..16 {
            wake.signal();
        }
        assert!(wake.fired());
        assert!(!wake.fired());
    }

    #[test]
    fn the_descriptor_is_ready_only_when_signalled() {
        // The property libasound depends on, tested through the same `poll` it uses.
        let wake = Wake::new();
        assert!(!ready(wake.fd(), 0), "nothing to report yet");
        wake.signal();
        assert!(ready(wake.fd(), 100), "the signal must reach poll");
        assert!(wake.fired());
        assert!(!ready(wake.fd(), 0), "and be cleared by reading it");
    }

    /// `poll` on one descriptor, for the given timeout in milliseconds.
    fn ready(fd: c_int, timeout_ms: c_int) -> bool {
        #[repr(C)]
        struct PollFd {
            fd: c_int,
            events: i16,
            revents: i16,
        }
        extern "C" {
            fn poll(fds: *mut PollFd, count: core::ffi::c_ulong, timeout: c_int) -> c_int;
        }
        /// `POLLIN`.
        const POLLIN: i16 = 0x1;
        let mut entry = PollFd {
            fd,
            events: POLLIN,
            revents: 0,
        };
        // SAFETY: one initialised descriptor and a count that matches it.
        let result = unsafe { poll(&mut entry, 1, timeout_ms) };
        result > 0 && entry.revents & POLLIN != 0
    }
}
