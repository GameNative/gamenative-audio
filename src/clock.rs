//! A monotonic clock, and the one place a platform difference is visible.
//!
//! `CLOCK_MONOTONIC` on Android and Linux, via a `timespec` whose field widths
//! differ between 32- and 64-bit targets — getting that wrong corrupts the stack on
//! a 32-bit target, which is why the shape is spelled out per pointer width rather
//! than assumed.

#[cfg(any(target_os = "android", target_os = "linux"))]
mod monotonic {
    #[cfg(target_pointer_width = "64")]
    #[repr(C)]
    struct Timespec {
        tv_sec: i64,
        tv_nsec: i64,
    }

    #[cfg(target_pointer_width = "32")]
    #[repr(C)]
    struct Timespec {
        tv_sec: i32,
        tv_nsec: i32,
    }

    extern "C" {
        fn clock_gettime(clock_id: i32, tp: *mut Timespec) -> i32;
    }

    /// `CLOCK_MONOTONIC`, which is 1 on both Android and Linux.
    const CLOCK_MONOTONIC: i32 = 1;

    pub fn now_ns() -> u64 {
        let mut ts = Timespec {
            tv_sec: 0,
            tv_nsec: 0,
        };
        // SAFETY: `ts` is a live local of the layout `clock_gettime` expects for
        // this target.
        let result = unsafe { clock_gettime(CLOCK_MONOTONIC, &mut ts) };
        if result != 0 {
            return 0;
        }
        // Widened to `i128` so one expression is correct whether the fields are
        // 32- or 64-bit, without a cast that is a no-op on one target and not the
        // other.
        let seconds = ts.tv_sec as i128;
        let nanoseconds = ts.tv_nsec as i128;
        (seconds * 1_000_000_000 + nanoseconds).max(0) as u64
    }
}

#[cfg(not(any(target_os = "android", target_os = "linux")))]
mod monotonic {
    use std::sync::OnceLock;
    use std::time::Instant;

    static ORIGIN: OnceLock<Instant> = OnceLock::new();

    /// Monotonic nanoseconds since first use: monotonic, but not an absolute
    /// clock. Only reached on a host that is neither Android nor Linux.
    pub fn now_ns() -> u64 {
        ORIGIN.get_or_init(Instant::now).elapsed().as_nanos() as u64
    }
}

pub(crate) use monotonic::now_ns;
