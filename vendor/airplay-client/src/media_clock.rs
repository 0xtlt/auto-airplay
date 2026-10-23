//! The clock a buffered stream plays on (local addition, see VENDORED.md).

use std::fmt;
use std::sync::Arc;

/// Time on the timeline the receiver plays a buffered stream on, in ns.
///
/// For a buffered stream this is the receiver's PTP clock as followed by the
/// sender (see `ptp_follower`): audio produced at the pace of this clock is
/// played by the receiver at the same pace, so the receiver's buffer neither
/// fills up nor drains over time. Readings never go backwards.
#[derive(Clone)]
pub struct MediaClock {
    source: Arc<dyn Fn() -> u64 + Send + Sync>,
}

impl MediaClock {
    /// The current time, in ns.
    pub fn now_ns(&self) -> u64 {
        (self.source)()
    }

    /// A clock read from `source`. Public only so the host crate's tests can
    /// drive the output with a clock of their own.
    #[doc(hidden)]
    pub fn from_fn(source: impl Fn() -> u64 + Send + Sync + 'static) -> Self {
        Self {
            source: Arc::new(source),
        }
    }
}

impl fmt::Debug for MediaClock {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MediaClock")
            .field("now_ns", &self.now_ns())
            .finish()
    }
}
