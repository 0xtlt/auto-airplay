use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use alsa::pcm::PCM;
use anyhow::{Context, Result};

use crate::platform::linux::{OpenedInput, SampleEncoding};
pub use crate::playout::AudioSink;
use crate::playout::{SharedBuffer, lock, run_playout};
use crate::plc::{JitterBuffer, STATS_INTERVAL};

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
                .spawn(move || {
                    run_playout(&sink, &buffer, &running, STATS_INTERVAL, |stats| {
                        tracing::info!(target: "auto_airplay", "{stats}");
                    });
                })
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
        // Metering and the first-audio log come first: nothing below may hide
        // that audio arrives.
        sink.observe(samples);
        // Frames already waiting in the device are late, not lost.
        let backlog = match input.pcm.avail_update() {
            Ok(frames) => frames.max(0) as usize,
            Err(_) => {
                lock(&buffer).note_avail_error();
                0
            }
        };
        sink.enqueue(&buffer, epoch.elapsed(), samples, backlog);
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
