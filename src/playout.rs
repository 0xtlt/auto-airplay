//! The attached AirPlay stream and the feeding of it from the jitter buffer.
//!
//! Platform independent so the stall detection can be tested anywhere.

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread;
use std::time::{Duration, Instant};

use airplay_client::{LiveFrameSender, LivePcmFrame};

use crate::plc::{JitterBuffer, PLAYOUT_CHUNK_FRAMES, Pacer, StatsSnapshot};

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

pub fn lock(buffer: &Mutex<JitterBuffer>) -> MutexGuard<'_, JitterBuffer> {
    buffer
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// The jitter buffer shared by the capture and playout threads. Critical
/// sections are one capture period of copying at most.
pub type SharedBuffer = Arc<Mutex<JitterBuffer>>;

/// Where captured audio goes. The capture thread measures the input level and
/// queues the audio in a jitter buffer with packet loss concealment; the
/// playout thread forwards it, paced, while an AirPlay stream is attached.
#[derive(Default)]
pub struct AudioSink {
    stream: Mutex<Option<Stream>>,
    /// Peak linear RMS since the last read, as `f32` bits. Levels are never
    /// negative, so the bit patterns order the same way as the values.
    level_bits: AtomicU32,
    failed: AtomicBool,
    /// Set by the first audio received, which is logged once.
    receiving: AtomicBool,
    /// Installed by [`CaptureHandle::start`]. Lock order: `stream`, then the
    /// buffer.
    buffer: Mutex<Option<SharedBuffer>>,
}

/// How long the playout thread sleeps when there is nothing to send.
const PLAYOUT_IDLE: Duration = Duration::from_millis(3);

impl AudioSink {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Starts forwarding frames to a new stream and clears any earlier failure.
    /// Playback starts from the audio queued in the jitter buffer.
    pub fn attach(&self, sender: LiveFrameSender) {
        let mut stream = self.stream();
        *stream = self.installed().map(|buffer| {
            let mut buffer = lock(&buffer);
            buffer.start_playout();
            Stream::new(sender, buffer.pacer())
        });
        self.failed.store(false, Ordering::Release);
    }

    /// The stream is now playing: from here on, a stream that keeps refusing
    /// frames counts as dead.
    pub fn arm(&self) {
        if let Some(stream) = self.stream().as_mut() {
            stream.arm();
        }
    }

    /// Stops forwarding frames. The capture thread keeps measuring the level.
    pub fn detach(&self) {
        let mut stream = self.stream();
        *stream = None;
        if let Some(buffer) = self.installed() {
            lock(&buffer).stop_playout();
        }
    }

    /// Returns the loudest linear RMS (`0.0..=1.0`) seen since the last call.
    pub fn take_peak_level(&self) -> f32 {
        f32::from_bits(self.level_bits.swap(0, Ordering::AcqRel))
    }

    /// Returns whether the attached stream went away since the last call.
    pub fn take_failed(&self) -> bool {
        self.failed.swap(false, Ordering::AcqRel)
    }

    pub fn install(&self, buffer: SharedBuffer) {
        *self.buffer.lock().unwrap_or_else(|p| p.into_inner()) = Some(buffer);
    }

    pub fn installed(&self) -> Option<SharedBuffer> {
        self.buffer
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
    }

    /// The capture thread's side of one read: meters the raw audio, then
    /// queues it in `buffer`. `now` is when it arrived on the capture clock and
    /// `backlog` the frames still waiting in the device. Never waits for
    /// anything but the buffer lock, which nobody holds for long.
    pub fn deliver(
        &self,
        buffer: &Mutex<JitterBuffer>,
        now: Duration,
        samples: &[i16],
        backlog: usize,
    ) {
        self.observe(samples);
        self.enqueue(buffer, now, samples, backlog);
    }

    /// The first half of [`deliver`](Self::deliver): the audio has arrived.
    /// Logs the first arrival and meters the raw audio, before anything that
    /// could wait (the device query for the backlog, the buffer lock), so that
    /// neither can hide that reads arrive.
    pub fn observe(&self, samples: &[i16]) {
        if !self.receiving.swap(true, Ordering::AcqRel) {
            tracing::info!(
                frames = samples.len(),
                "first audio received from the input"
            );
        }
        self.measure(samples);
    }

    /// The second half of [`deliver`](Self::deliver): queues the audio.
    pub fn enqueue(
        &self,
        buffer: &Mutex<JitterBuffer>,
        now: Duration,
        samples: &[i16],
        backlog: usize,
    ) {
        lock(buffer).push_at(now, samples, backlog);
    }

    /// Measures the raw captured audio, before any concealment, so that sound
    /// and silence detection see what the source really sent.
    pub fn measure(&self, samples: &[i16]) {
        let level = crate::silence::rms_linear(samples);
        self.level_bits.fetch_max(level.to_bits(), Ordering::AcqRel);
    }

    /// Hands the next chunk to the attached stream if it is due. Returns
    /// whether a chunk went out. Never sleeps and never keeps the stream lock
    /// beyond one chunk.
    pub fn feed(
        &self,
        buffer: &Mutex<JitterBuffer>,
        chunk: &mut [i16],
        format: (u8, u32),
        now: Duration,
    ) -> bool {
        let mut guard = self.stream();
        let Some(stream) = guard.as_mut() else {
            return false;
        };
        match stream.feed(buffer, chunk, format, now, Instant::now()) {
            Feed::Sent => true,
            Feed::Idle => false,
            Feed::Dead => {
                tracing::debug!("AirPlay stream stopped accepting audio; detaching the sink");
                *guard = None;
                lock(buffer).stop_playout();
                self.failed.store(true, Ordering::Release);
                false
            }
        }
    }

    fn stream(&self) -> MutexGuard<'_, Option<Stream>> {
        self.stream
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

/// The playout thread: feeds the attached stream, if any, from `buffer` until
/// `running` is cleared, and hands `report` the health of the input every
/// `stats_every`, attached or not.
pub fn run_playout(
    sink: &AudioSink,
    buffer: &Mutex<JitterBuffer>,
    running: &AtomicBool,
    stats_every: Duration,
    mut report: impl FnMut(StatsSnapshot),
) {
    let (channels, sample_rate, chunk_frames) = {
        let buffer = lock(buffer);
        (
            buffer.channels(),
            buffer.sample_rate(),
            buffer.chunk_frames(),
        )
    };
    let format = (channels as u8, sample_rate);
    let mut chunk = vec![0_i16; chunk_frames * channels];
    let epoch = Instant::now();
    let mut next_report = epoch + stats_every;

    while running.load(Ordering::Acquire) {
        let now = Instant::now();
        if now >= next_report {
            next_report += stats_every;
            let stats = lock(buffer).take_stats();
            report(stats);
        }
        if !sink.feed(buffer, &mut chunk, format, now.duration_since(epoch)) {
            thread::sleep(PLAYOUT_IDLE);
        }
    }
}
