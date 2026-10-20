use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use airplay_client::LiveFrameSender;
use alsa::pcm::PCM;
use anyhow::{Context, Result};

use crate::platform::linux::{OpenedInput, SampleEncoding};
use crate::playout::{Feed, Stream};
use crate::plc::{JitterBuffer, STATS_INTERVAL};

/// The jitter buffer shared by the capture and playout threads. Critical
/// sections are one capture period of copying at most.
type SharedBuffer = Arc<Mutex<JitterBuffer>>;

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

    fn install(&self, buffer: SharedBuffer) {
        *self.buffer.lock().unwrap_or_else(|p| p.into_inner()) = Some(buffer);
    }

    fn installed(&self) -> Option<SharedBuffer> {
        self.buffer
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
    }

    /// Measures the raw captured audio, before any concealment, so that sound
    /// and silence detection see what the source really sent.
    fn measure(&self, samples: &[i16]) {
        let level = crate::silence::rms_linear(samples);
        self.level_bits.fetch_max(level.to_bits(), Ordering::AcqRel);
    }

    /// Hands the next chunk to the attached stream if it is due. Returns
    /// whether a chunk went out. Never sleeps and never keeps the stream lock
    /// beyond one chunk.
    fn feed(
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

fn lock(buffer: &Mutex<JitterBuffer>) -> MutexGuard<'_, JitterBuffer> {
    buffer
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

pub struct CaptureHandle {
    running: Arc<AtomicBool>,
    capture: Option<JoinHandle<Result<()>>>,
    playout: Option<JoinHandle<()>>,
}

impl CaptureHandle {
    /// Starts capturing into a jitter buffer holding `jitter_buffer_ms`, and
    /// the thread that plays it out to the stream attached to `sink`.
    pub fn start(input: OpenedInput, sink: Arc<AudioSink>, jitter_buffer_ms: u32) -> Result<Self> {
        let buffer: SharedBuffer = Arc::new(Mutex::new(JitterBuffer::new(
            input.format.sample_rate,
            input.format.channels,
            jitter_buffer_ms,
        )));
        sink.install(Arc::clone(&buffer));
        tracing::info!(jitter_buffer_ms, "input jitter buffer ready");

        let running = Arc::new(AtomicBool::new(true));
        let capture = {
            let (sink, buffer, running) = (Arc::clone(&sink), Arc::clone(&buffer), running.clone());
            thread::Builder::new()
                .name("audio-capture".into())
                .spawn(move || capture_loop(input, sink, buffer, running))
                .context("cannot start the audio capture thread")?
        };
        // From here on, dropping the handle stops the capture thread too.
        let mut handle = Self {
            running: Arc::clone(&running),
            capture: Some(capture),
            playout: None,
        };
        handle.playout = Some(
            thread::Builder::new()
                .name("audio-playout".into())
                .spawn(move || playout_loop(sink, buffer, running))
                .context("cannot start the audio playout thread")?,
        );
        Ok(handle)
    }

    pub fn is_running(&self) -> bool {
        self.running.load(Ordering::Acquire)
            && self
                .capture
                .as_ref()
                .is_some_and(|thread| !thread.is_finished())
    }

    pub fn stop(&mut self) {
        self.running.store(false, Ordering::Release);
        if let Some(thread) = self.capture.take() {
            match thread.join() {
                Ok(Err(error)) => tracing::warn!(%error, "capture thread stopped with an error"),
                Err(_) => tracing::warn!("capture thread panicked"),
                Ok(Ok(())) => {}
            }
        }
        if let Some(thread) = self.playout.take()
            && thread.join().is_err()
        {
            tracing::warn!("playout thread panicked");
        }
    }
}

impl Drop for CaptureHandle {
    fn drop(&mut self) {
        self.stop();
    }
}

fn capture_loop(
    input: OpenedInput,
    sink: Arc<AudioSink>,
    buffer: SharedBuffer,
    running: Arc<AtomicBool>,
) -> Result<()> {
    let channels = input.format.channels as usize;
    let samples_per_period = input.period_frames * channels;
    let epoch = Instant::now();
    let deliver = |samples: &[i16]| {
        sink.measure(samples);
        // Frames already waiting in the device are late, not lost.
        let backlog = input
            .pcm
            .avail_update()
            .map_or(0, |frames| frames.max(0) as usize);
        lock(&buffer).push_at(epoch.elapsed(), samples, backlog);
    };

    match input.encoding {
        SampleEncoding::Signed16 => {
            let io = input.pcm.io_i16()?;
            let mut samples = vec![0_i16; samples_per_period];
            while running.load(Ordering::Acquire) {
                match io.readi(&mut samples) {
                    Ok(frames) if frames > 0 => deliver(&samples[..frames * channels]),
                    Ok(_) => continue,
                    Err(error) if error.errno() == libc::EAGAIN => {
                        thread::sleep(Duration::from_millis(2));
                    }
                    Err(error) => recover(&input.pcm, &buffer, error)?,
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
                        converted.clear();
                        converted.extend(
                            raw[..frames * channels]
                                .iter()
                                .map(|sample| (sample >> 16) as i16),
                        );
                        deliver(&converted);
                    }
                    Ok(_) => continue,
                    Err(error) if error.errno() == libc::EAGAIN => {
                        thread::sleep(Duration::from_millis(2));
                    }
                    Err(error) => recover(&input.pcm, &buffer, error)?,
                }
            }
        }
    }

    running.store(false, Ordering::Release);
    Ok(())
}

/// Recovers the capture device after an error, counting overruns as losses.
fn recover(pcm: &PCM, buffer: &Mutex<JitterBuffer>, error: alsa::Error) -> Result<()> {
    if error.errno() == libc::EPIPE || error.errno() == libc::ESTRPIPE {
        lock(buffer).note_xrun();
        tracing::debug!("audio capture overrun; recovering");
    }
    pcm.try_recover(error, true)?;
    Ok(())
}

/// Feeds the attached stream from the jitter buffer, and reports the health of
/// the input once a minute while capturing, zeros included.
fn playout_loop(sink: Arc<AudioSink>, buffer: SharedBuffer, running: Arc<AtomicBool>) {
    let (channels, sample_rate, chunk_frames) = {
        let buffer = lock(&buffer);
        (
            buffer.channels(),
            buffer.sample_rate(),
            buffer.chunk_frames(),
        )
    };
    let format = (channels as u8, sample_rate);
    let mut chunk = vec![0_i16; chunk_frames * channels];
    let epoch = Instant::now();
    let mut next_report = epoch + STATS_INTERVAL;

    while running.load(Ordering::Acquire) {
        let now = Instant::now();
        if now >= next_report {
            next_report += STATS_INTERVAL;
            let stats = lock(&buffer).take_stats();
            tracing::info!(target: "auto_airplay", "{stats}");
        }
        if !sink.feed(&buffer, &mut chunk, format, now.duration_since(epoch)) {
            thread::sleep(PLAYOUT_IDLE);
        }
    }
}
