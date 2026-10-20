//! The attached AirPlay stream and the feeding of it from the jitter buffer.
//!
//! Platform independent so the stall detection can be tested anywhere.

use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant};

use airplay_client::{LiveFrameSender, LivePcmFrame};

use crate::plc::{JitterBuffer, PLAYOUT_CHUNK_FRAMES, Pacer};

/// How long a stream may refuse every chunk before it counts as dead.
/// `try_send` cannot tell a briefly full queue from a closed one, so a single
/// refusal only delays that chunk.
pub const STALL_LIMIT: Duration = Duration::from_secs(5);

/// What one attempt to feed the stream came to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Feed {
    /// A chunk went out.
    Sent,
    /// Nothing to do now (not due, nothing to send, or refused for a while).
    Idle,
    /// The stream refused everything for [`STALL_LIMIT`]: it is gone.
    Dead,
}

/// A stream audio is forwarded to.
pub struct Stream {
    sender: LiveFrameSender,
    pacer: Pacer,
    /// Stall detection only runs once the stream is playing: while it starts
    /// up, the queue legitimately stays full and refused chunks are dropped.
    armed: bool,
    /// When the unbroken run of refusals started. Cleared by a chunk the
    /// stream accepted, and by nothing else: a queue that is not full can
    /// still belong to a receiver that is gone.
    refusing_since: Option<Instant>,
}

impl Stream {
    pub fn new(sender: LiveFrameSender, pacer: Pacer) -> Self {
        Self {
            sender,
            pacer,
            armed: false,
            refusing_since: None,
        }
    }

    /// The stream is now playing: from here on, refusing everything counts as dead.
    pub fn arm(&mut self) {
        self.armed = true;
        self.refusing_since = None;
    }

    /// Offers the stream the next chunk if one is due. `now` is the time on
    /// the clock that paces the output, `clock` the time for stall detection;
    /// the caller never sleeps with the buffer locked.
    ///
    /// The sender is only offered audio while its queue has room, at the pace
    /// of the [`Pacer`].
    pub fn feed(
        &mut self,
        buffer: &Mutex<JitterBuffer>,
        chunk: &mut [i16],
        format: (u8, u32),
        now: Duration,
        clock: Instant,
    ) -> Feed {
        if self.sender.is_full() {
            self.pacer.blocked(now);
            return self.refused(clock);
        }
        let pulled = lock(buffer).pull_chunk(&mut self.pacer, now, chunk);
        if !pulled {
            return Feed::Idle;
        }

        // The AirPlay resampler takes nothing but whole playout chunks.
        let expected = PLAYOUT_CHUNK_FRAMES * usize::from(format.0);
        debug_assert_eq!(chunk.len(), expected);
        if chunk.len() != expected {
            return Feed::Idle;
        }
        let frame = LivePcmFrame {
            samples: chunk.to_vec(),
            channels: format.0,
            sample_rate: format.1,
        };
        if self.sender.try_send(frame) {
            self.pacer.sent(PLAYOUT_CHUNK_FRAMES);
            self.refusing_since = None;
            Feed::Sent
        } else {
            self.refused(clock)
        }
    }

    fn refused(&mut self, clock: Instant) -> Feed {
        if !self.armed {
            return Feed::Idle;
        }
        let since = *self.refusing_since.get_or_insert(clock);
        if clock.saturating_duration_since(since) < STALL_LIMIT {
            Feed::Idle
        } else {
            Feed::Dead
        }
    }
}

fn lock(buffer: &Mutex<JitterBuffer>) -> MutexGuard<'_, JitterBuffer> {
    buffer
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}
