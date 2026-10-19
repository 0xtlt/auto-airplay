use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use airplay_client::{LiveFrameSender, LivePcmFrame};
use anyhow::{Context, Result};

use crate::platform::linux::{OpenedInput, SampleEncoding};

/// Where captured audio goes. The capture thread always measures the input
/// level, and forwards frames only while an AirPlay stream is attached.
#[derive(Default)]
pub struct AudioSink {
    stream: Mutex<Stream>,
    /// Peak linear RMS since the last read, as `f32` bits. Levels are never
    /// negative, so the bit patterns order the same way as the values.
    level_bits: AtomicU32,
    failed: AtomicBool,
}

/// The attached stream, if any, and how long it has been refusing frames.
#[derive(Default)]
struct Stream {
    sender: Option<LiveFrameSender>,
    /// Stall detection only runs once the stream is playing: while it starts
    /// up, the queue legitimately stays full and refused frames are dropped.
    armed: bool,
    refusing_since: Option<Instant>,
}

/// How long a stream may refuse every frame before it counts as dead.
/// `try_send` cannot tell a briefly full queue from a closed one, so a single
/// refusal only drops that frame.
const STALL_LIMIT: Duration = Duration::from_secs(5);

impl AudioSink {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Starts forwarding frames to a new stream and clears any earlier failure.
    pub fn attach(&self, sender: LiveFrameSender) {
        *self.stream() = Stream {
            sender: Some(sender),
            ..Stream::default()
        };
        self.failed.store(false, Ordering::Release);
    }

    /// The stream is now playing: from here on, a stream that keeps refusing
    /// frames counts as dead.
    pub fn arm(&self) {
        let mut stream = self.stream();
        stream.armed = true;
        stream.refusing_since = None;
    }

    /// Stops forwarding frames. The capture thread keeps measuring the level.
    pub fn detach(&self) {
        *self.stream() = Stream::default();
    }

    /// Returns the loudest linear RMS (`0.0..=1.0`) seen since the last call.
    pub fn take_peak_level(&self) -> f32 {
        f32::from_bits(self.level_bits.swap(0, Ordering::AcqRel))
    }

    /// Returns whether the attached stream went away since the last call.
    pub fn take_failed(&self) -> bool {
        self.failed.swap(false, Ordering::AcqRel)
    }

    fn push(&self, samples: &[i16], channels: u8, sample_rate: u32) {
        let level = crate::silence::rms_linear(samples);
        self.level_bits.fetch_max(level.to_bits(), Ordering::AcqRel);

        let mut stream = self.stream();
        let Some(sender) = stream.sender.as_ref() else {
            return;
        };
        let frame = LivePcmFrame {
            samples: samples.to_vec(),
            channels,
            sample_rate,
        };
        if sender.try_send(frame) {
            stream.refusing_since = None;
            return;
        }
        if !stream.armed {
            return;
        }

        let now = Instant::now();
        let since = *stream.refusing_since.get_or_insert(now);
        if now.duration_since(since) < STALL_LIMIT {
            tracing::debug!("AirPlay stream is behind; dropping an audio frame");
        } else {
            tracing::debug!("AirPlay stream stopped accepting audio; detaching the sink");
            *stream = Stream::default();
            self.failed.store(true, Ordering::Release);
        }
    }

    fn stream(&self) -> MutexGuard<'_, Stream> {
        self.stream
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

pub struct CaptureHandle {
    running: Arc<AtomicBool>,
    thread: Option<JoinHandle<Result<()>>>,
}

impl CaptureHandle {
    pub fn start(input: OpenedInput, sink: Arc<AudioSink>) -> Result<Self> {
        let running = Arc::new(AtomicBool::new(true));
        let thread_running = Arc::clone(&running);
        let thread = thread::Builder::new()
            .name("audio-capture".into())
            .spawn(move || capture_loop(input, sink, thread_running))
            .context("cannot start the audio capture thread")?;

        Ok(Self {
            running,
            thread: Some(thread),
        })
    }

    pub fn is_running(&self) -> bool {
        self.running.load(Ordering::Acquire)
            && self
                .thread
                .as_ref()
                .is_some_and(|thread| !thread.is_finished())
    }

    pub fn stop(&mut self) {
        self.running.store(false, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            match thread.join() {
                Ok(Err(error)) => tracing::warn!(%error, "capture thread stopped with an error"),
                Err(_) => tracing::warn!("capture thread panicked"),
                Ok(Ok(())) => {}
            }
        }
    }
}

impl Drop for CaptureHandle {
    fn drop(&mut self) {
        self.stop();
    }
}

fn capture_loop(input: OpenedInput, sink: Arc<AudioSink>, running: Arc<AtomicBool>) -> Result<()> {
    let channels = input.format.channels as usize;
    let samples_per_period = input.period_frames * channels;

    match input.encoding {
        SampleEncoding::Signed16 => {
            let io = input.pcm.io_i16()?;
            let mut samples = vec![0_i16; samples_per_period];
            while running.load(Ordering::Acquire) {
                match io.readi(&mut samples) {
                    Ok(frames) if frames > 0 => {
                        let length = frames * channels;
                        sink.push(
                            &samples[..length],
                            input.format.channels,
                            input.format.sample_rate,
                        );
                    }
                    Ok(_) => continue,
                    Err(error) if error.errno() == libc::EAGAIN => {
                        thread::sleep(std::time::Duration::from_millis(2));
                    }
                    Err(error) => input.pcm.try_recover(error, true)?,
                }
            }
        }
        SampleEncoding::Signed32 => {
            let io = input.pcm.io_i32()?;
            let mut raw = vec![0_i32; samples_per_period];
            let mut converted = Vec::with_capacity(samples_per_period);
            while running.load(Ordering::Acquire) {
                match io.readi(&mut raw) {
                    Ok(frames) if frames > 0 => {
                        let length = frames * channels;
                        converted.clear();
                        converted.extend(raw[..length].iter().map(|sample| (sample >> 16) as i16));
                        sink.push(&converted, input.format.channels, input.format.sample_rate);
                    }
                    Ok(_) => continue,
                    Err(error) if error.errno() == libc::EAGAIN => {
                        thread::sleep(std::time::Duration::from_millis(2));
                    }
                    Err(error) => input.pcm.try_recover(error, true)?,
                }
            }
        }
    }

    running.store(false, Ordering::Release);
    Ok(())
}
