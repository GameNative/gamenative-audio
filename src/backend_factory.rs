//! Which backend a build gets.
//!
//! One function, so that "is there a device?" is answered in exactly one place:
//! on Android there is AAudio, and anywhere else there is nothing unless the
//! caller supplied a backend of its own (which is how the test suite drives the
//! whole engine without a device).

use crate::backend::Backend;
use crate::types::Error;

/// The device stack this build can offer, or an error saying there is none.
///
/// The **callback** backend, which is the shape PulseAudio's own AAudio sink uses on
/// this device: the device pulls, its buffer is two bursts, and the pipeline above it
/// holds the slack. The callback is the only device shape here; a blocking writer has no
/// place in it, because the device's own periods are larger than the client's block and a
/// writer would have to choose which side to stall.
pub fn open_backend() -> Result<Box<dyn Backend>, Error> {
    #[cfg(target_os = "android")]
    {
        Ok(Box::new(crate::backend::callback::AaudioBackend::new()))
    }
    #[cfg(not(target_os = "android"))]
    {
        Err(Error::UnavailableBackend)
    }
}
